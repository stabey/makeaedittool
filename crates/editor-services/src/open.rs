use crate::{
    CancelToken, ContentIdentity, DocumentId, EncodingSpec, ErrorKind, FileFingerprint, LineEnding,
    PagedDocHandle, PagingService, ReadOnlyReason, ServiceError, SourceGeneration,
};
use ropey::{Rope, RopeBuilder};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    pub editable_byte_limit: u64,
    pub long_line_byte_limit: usize,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            editable_byte_limit: 64 * 1024 * 1024,
            long_line_byte_limit: 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug)]
pub enum OpenOutcome {
    Editable {
        rope: Rope,
        identity: ContentIdentity,
        encoding: EncodingSpec,
        line_ending: LineEnding,
        readonly_reason: Option<ReadOnlyReason>,
        fingerprint: FileFingerprint,
    },
    Paged {
        doc: PagedDocHandle,
        encoding: EncodingSpec,
        line_ending: LineEnding,
        readonly_reason: ReadOnlyReason,
        fingerprint: FileFingerprint,
        source_gen: SourceGeneration,
    },
}
#[derive(Default)]
struct EolScan {
    lf: bool,
    crlf: bool,
    isolated_cr: bool,
    previous_cr: bool,
    line_bytes: usize,
    longest: usize,
}
impl EolScan {
    fn push(&mut self, bytes: &[u8]) {
        let mut at = 0;
        if self.previous_cr && !bytes.is_empty() {
            self.previous_cr = false;
            if bytes[0] == b'\n' {
                self.crlf = true;
                self.longest = self.longest.max(self.line_bytes);
                self.line_bytes = 0;
                at = 1;
            } else {
                self.isolated_cr = true;
                self.line_bytes += 1;
            }
        }
        while let Some(relative) = memchr::memchr2(b'\r', b'\n', &bytes[at..]) {
            let index = at + relative;
            self.line_bytes += relative;
            if bytes[index] == b'\n' {
                self.lf = true;
                self.longest = self.longest.max(self.line_bytes);
                self.line_bytes = 0;
                at = index + 1;
            } else if index + 1 == bytes.len() {
                self.previous_cr = true;
                at = bytes.len();
                break;
            } else if bytes[index + 1] == b'\n' {
                self.crlf = true;
                self.longest = self.longest.max(self.line_bytes);
                self.line_bytes = 0;
                at = index + 2;
            } else {
                self.isolated_cr = true;
                self.line_bytes += 1;
                at = index + 1;
            }
        }
        self.line_bytes += bytes.len() - at;
        self.longest = self.longest.max(self.line_bytes);
    }
    fn ending(&self, eof: bool) -> LineEnding {
        if self.isolated_cr || (eof && self.previous_cr) || (self.lf && self.crlf) {
            LineEnding::MixedReadOnly
        } else if self.crlf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }
}
fn set_reason(reason: &mut Option<ReadOnlyReason>, candidate: ReadOnlyReason) {
    fn priority(reason: &ReadOnlyReason) -> u8 {
        match reason {
            ReadOnlyReason::BinaryContent => 4,
            ReadOnlyReason::UnsupportedEncoding => 3,
            ReadOnlyReason::LargeFile => 2,
            ReadOnlyReason::LongLine => 1,
            ReadOnlyReason::MixedLineEndings => 0,
        }
    }
    if reason
        .as_ref()
        .is_none_or(|current| priority(&candidate) > priority(current))
    {
        *reason = Some(candidate);
    }
}
fn append_editable(
    builder: &mut RopeBuilder,
    hash: &mut blake3::Hasher,
    reason: &Option<ReadOnlyReason>,
    text: &str,
) {
    if reason.is_none() {
        builder.append(text);
        hash.update(text.as_bytes());
    }
}
/// Synchronous service operation: dispatch on a bounded background executor, not GPUI.
/// Ordinary files are fully validated; paged-file encoding/EOL labels describe the prefix only.
pub fn open_document(
    doc_id: DocumentId,
    path: impl AsRef<Path>,
    paging: &PagingService,
    options: OpenOptions,
    cancel: &CancelToken,
) -> Result<OpenOutcome, ServiceError> {
    cancel.check()?;
    let _permit = crate::paging::acquire_open_permit()?;
    let doc = paging.open(doc_id, path)?;
    let fingerprint = doc.fingerprint().clone();
    let source_gen = doc.source_generation();
    let limit = options.editable_byte_limit.min(64 * 1024 * 1024);
    let line_limit = options.long_line_byte_limit.clamp(64 * 1024, 1024 * 1024);
    let large = doc.len() > limit;
    let mut bytes = vec![0; if large { 64 * 1024 } else { 1024 * 1024 }];
    let mut carry = Vec::with_capacity(4);
    let mut builder = RopeBuilder::new();
    let mut hash = blake3::Hasher::new();
    let mut scan = EolScan::default();
    let mut encoding = EncodingSpec::Utf8;
    let mut offset = 0;
    let mut reason = if large {
        Some(ReadOnlyReason::LargeFile)
    } else {
        None
    };
    loop {
        cancel.check()?;
        let n = doc.read_at(offset, &mut bytes, source_gen)?;
        let mut chunk = &bytes[..n];
        if offset == 0 {
            if chunk.starts_with(&[0xef, 0xbb, 0xbf]) {
                encoding = EncodingSpec::Utf8Bom;
                chunk = &chunk[3..];
            } else if chunk.starts_with(&[0xff, 0xfe]) || chunk.starts_with(&[0xfe, 0xff]) {
                encoding = EncodingSpec::UnsupportedReadOnly;
                set_reason(&mut reason, ReadOnlyReason::UnsupportedEncoding);
            }
        }
        if memchr::memchr(0, chunk).is_some() {
            set_reason(&mut reason, ReadOnlyReason::BinaryContent);
            encoding = EncodingSpec::UnsupportedReadOnly;
        }
        scan.push(chunk);
        if scan.longest > line_limit {
            set_reason(&mut reason, ReadOnlyReason::LongLine);
        }
        if encoding == EncodingSpec::UnsupportedReadOnly {
            offset += n as u64;
            break;
        }
        if !carry.is_empty() {
            let needed = (4 - carry.len()).min(chunk.len());
            carry.extend_from_slice(&chunk[..needed]);
            match std::str::from_utf8(&carry) {
                Ok(text) => {
                    append_editable(&mut builder, &mut hash, &reason, text);
                    chunk = &chunk[needed..];
                    carry.clear();
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let valid = e.valid_up_to();
                    append_editable(
                        &mut builder,
                        &mut hash,
                        &reason,
                        std::str::from_utf8(&carry[..valid])
                            .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))?,
                    );
                    // A peek can include the beginning of the next codepoint; do not consume it.
                    let consumed = valid - (carry.len() - needed);
                    chunk = &chunk[consumed..];
                    carry.clear();
                }
                Err(_) => {
                    encoding = EncodingSpec::UnsupportedReadOnly;
                    set_reason(&mut reason, ReadOnlyReason::UnsupportedEncoding);
                }
            }
        }
        match std::str::from_utf8(chunk) {
            Ok(text) => append_editable(&mut builder, &mut hash, &reason, text),
            Err(e) => {
                if e.error_len().is_some() {
                    encoding = EncodingSpec::UnsupportedReadOnly;
                    set_reason(&mut reason, ReadOnlyReason::UnsupportedEncoding);
                } else {
                    append_editable(
                        &mut builder,
                        &mut hash,
                        &reason,
                        std::str::from_utf8(&chunk[..e.valid_up_to()])
                            .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))?,
                    );
                    carry.extend_from_slice(&chunk[e.valid_up_to()..]);
                }
            }
        }
        offset += n as u64;
        if offset >= doc.len() {
            if !carry.is_empty() {
                encoding = EncodingSpec::UnsupportedReadOnly;
                set_reason(&mut reason, ReadOnlyReason::UnsupportedEncoding);
            }
            break;
        }
        if large || reason.is_some() {
            break;
        }
    }
    let ending = scan.ending(offset == doc.len());
    doc.validate(source_gen)?;
    if let Some(readonly_reason) = reason {
        return Ok(OpenOutcome::Paged {
            doc,
            encoding,
            line_ending: ending,
            readonly_reason,
            fingerprint,
            source_gen,
        });
    }
    let rope = builder.finish();
    let mut digest = [0u8; 16];
    digest.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    let identity = ContentIdentity {
        byte_len: rope.len() as u64,
        hash128: u128::from_le_bytes(digest),
    };
    Ok(OpenOutcome::Editable {
        rope,
        identity,
        encoding,
        line_ending: ending,
        readonly_reason: if ending == LineEnding::MixedReadOnly {
            Some(ReadOnlyReason::MixedLineEndings)
        } else {
            None
        },
        fingerprint,
    })
}
