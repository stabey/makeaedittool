use std::fs::OpenOptions;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

pub type ScratchLease = Arc<Mutex<Option<TempDir>>>;

pub fn new_scratch_lease() -> ScratchLease {
    Arc::new(Mutex::new(None))
}

/// Worker-only: atomically creates an owned temporary directory via `tempfile::Builder` (`0700`
/// non-recursive `mkdir`), canonicalizes its path for symlink-safe `--report` aliasing checks,
/// and records ownership in `lease` only after creation succeeds.
pub fn acquire_scratch_tempdir(lease: &ScratchLease) -> Result<(PathBuf, PathBuf), String> {
    let dir = tempfile::Builder::new()
        .prefix("makeaedittool-multiview-paged-")
        .tempdir()
        .map_err(|e| format!("failed to acquire owned TempDir: {e}"))?;
    let raw_path = dir.path().to_path_buf();
    let canonical_path = std::fs::canonicalize(&raw_path).map_err(|e| {
        format!(
            "failed to canonicalize owned TempDir '{}': {e}",
            raw_path.display()
        )
    })?;
    let mut guard = lease.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Err("internal bug: ScratchLease acquired more than once".to_string());
    }
    *guard = Some(dir);
    Ok((raw_path, canonical_path))
}

/// Disarms `TempDir::drop` (`TempDir::keep`) when aborting on panic or worker-drain timeout so
/// stack unwinding never runs `remove_dir_all` while detached workers may still hold open files.
pub fn disarm_scratch_on_unclean_abort(lease: &ScratchLease) -> Option<PathBuf> {
    let mut guard = lease.lock().unwrap_or_else(|e| e.into_inner());
    guard.take().map(TempDir::keep)
}

/// Worker-only (called strictly AFTER all background operations have drained):
/// - Returns `Ok(None)` without touching the filesystem if no `TempDir` was ever acquired.
/// - Otherwise closes the owned `TempDir` (surfacing any I/O error) and verifies `NotFound` via
///   `std::fs::symlink_metadata` so `PermissionDenied` never masquerades as a clean deletion.
pub fn close_and_verify_drained_scratch(lease: &ScratchLease) -> Result<Option<bool>, String> {
    let maybe_dir = {
        let mut guard = lease.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    let Some(dir) = maybe_dir else {
        return Ok(None);
    };
    let leased_path = dir.path().to_path_buf();
    dir.close()
        .map_err(|e| format!("TempDir::close failed for '{}': {e}", leased_path.display()))?;
    match std::fs::symlink_metadata(&leased_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(true)),
        Ok(_) => Ok(Some(false)),
        Err(e) => Err(format!(
            "failed to verify removal of owned TempDir '{}': {e}",
            leased_path.display()
        )),
    }
}

pub const DENSE_BLOCKS: usize = 8;
pub const DENSE_BLOCK_BYTES: u64 = 1024 * 1024;
pub const DENSE_TOTAL_BYTES: u64 = (DENSE_BLOCKS as u64) * DENSE_BLOCK_BYTES;
pub const DENSE_SEARCH_NEEDLE: &str = "NEEDLE_MULTIVIEW_TARGET";

/// Logical length of the sparse 64-bit fixture: `4 GiB + 4 MiB` (`> u32::MAX` and `> 64 MiB`).
pub const SPARSE_LOGICAL_BYTES: u64 = (1u64 << 32) + (4u64 << 20);
pub const SPARSE_HIGH_STRIDE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct DenseFixtureSpec {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub newline_count: u64,
    pub expected_index_total_lines: u64,
    pub search_needle: &'static str,
    pub expected_match_offsets: [u64; DENSE_BLOCKS],
}

#[derive(Clone, Debug)]
pub struct SparseFixtureSpec {
    pub path: PathBuf,
    pub logical_bytes: u64,
    /// Anchor 0 is `0`; anchors `1..=8` are `>= (1u64 << 32) + 262_144` (`> u32::MAX`).
    pub island_anchors: [u64; 9],
}

#[derive(Clone, Debug)]
pub enum SparseCreateError {
    SetLenRejected(String),
    Io(String),
}

impl SparseCreateError {
    pub fn message(&self) -> &str {
        match self {
            Self::SetLenRejected(msg) | Self::Io(msg) => msg,
        }
    }
}

/// Deterministic byte offset for disjoint view `slot` (`0..8`) in the 8 MiB dense fixture.
#[inline]
pub fn dense_disjoint_offset(slot: usize) -> u64 {
    debug_assert!(slot < DENSE_BLOCKS);
    (slot as u64) * DENSE_BLOCK_BYTES
}

/// Deterministic expected marker prefix at `dense_disjoint_offset(slot)`.
pub fn expected_dense_block_marker(slot: usize) -> String {
    let base = dense_disjoint_offset(slot);
    format!("DENSE_BLK_{slot:02}_ORIGIN_0x{base:08X}_")
}

/// Deterministic island anchor for sparse 64-bit correctness checks (`slot == 0` -> `0`,
/// `slot in 1..=8` -> `(1u64 << 32) + slot * 256 KiB`).
#[inline]
pub fn sparse_island_anchor(slot: usize) -> u64 {
    if slot == 0 {
        0
    } else {
        (1u64 << 32) + (slot as u64) * SPARSE_HIGH_STRIDE_BYTES
    }
}

/// Deterministic expected marker prefix at `sparse_island_anchor(slot)`.
pub fn expected_sparse_island_marker(slot: usize) -> String {
    let anchor = sparse_island_anchor(slot);
    format!("SPARSE64_ISLAND_{slot:02}_AT_0x{anchor:010X}_")
}

/// Worker-only: streams the bounded 8 MiB dense fixture into an already-leased `TempDir` using
/// `OpenOptions::create_new(true)` and a 64 KiB `BufWriter` (no `create_dir_all`, no 8 MiB Vec).
pub fn create_dense_fixture(scratch_dir: &Path) -> Result<DenseFixtureSpec, String> {
    let path = scratch_dir.join("dense-paged-fixture.txt");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("failed to create dense fixture '{}': {e}", path.display()))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);

    let mut written_total: u64 = 0;
    let mut newline_count: u64 = 0;
    let mut expected_match_offsets = [0u64; DENSE_BLOCKS];
    let mut line_buf = Vec::with_capacity(512);

    for blk in 0..DENSE_BLOCKS {
        let block_base = (blk as u64) * DENSE_BLOCK_BYTES;

        // Line 0 (exact 64 bytes, aligned at block_base = blk * 1 MiB):
        let origin_prefix = expected_dense_block_marker(blk);
        write_padded_line(&mut line_buf, &origin_prefix, b'-', 64)?;
        writer
            .write_all(&line_buf)
            .map_err(|e| format!("dense fixture write failed: {e}"))?;
        written_total += 64;
        newline_count += 1;

        // Line 1 (exact 512 bytes, > 180 chars with multi-byte UTF-8 so build_display_segments
        // produces continuation rows with `is_continuation == true`):
        let long_prefix =
            format!("DENSE_BLK_{blk:02}_LONGLINE_WRAP_中文段落αβγ_CONTINUATION_PROBE_");
        write_padded_line(&mut line_buf, &long_prefix, b'=', 512)?;
        writer
            .write_all(&line_buf)
            .map_err(|e| format!("dense fixture write failed: {e}"))?;
        written_total += 512;
        newline_count += 1;

        // Line 2 (exact 64 bytes, containing the unique per-block search needle):
        let search_lead = format!("DENSE_BLK_{blk:02}_SEARCH_");
        expected_match_offsets[blk] = written_total + (search_lead.len() as u64);
        let search_prefix = format!("{search_lead}{DENSE_SEARCH_NEEDLE}_B{blk:02}_");
        write_padded_line(&mut line_buf, &search_prefix, b'_', 64)?;
        writer
            .write_all(&line_buf)
            .map_err(|e| format!("dense fixture write failed: {e}"))?;
        written_total += 64;
        newline_count += 1;

        // Remaining (1_048_576 - 640) = 1_047_936 bytes = 16,374 rows of exact 64 bytes:
        let remaining_rows = ((DENSE_BLOCK_BYTES - 640) / 64) as usize;
        for row in 0..remaining_rows {
            let abs_off = written_total;
            let row_prefix = format!("DENSE_BLK_{blk:02}_ROW_{row:05}_AT_0x{abs_off:08X}_");
            write_padded_line(&mut line_buf, &row_prefix, b'.', 64)?;
            writer
                .write_all(&line_buf)
                .map_err(|e| format!("dense fixture write failed: {e}"))?;
            written_total += 64;
            newline_count += 1;
        }

        if written_total != block_base + DENSE_BLOCK_BYTES {
            return Err(format!(
                "dense block {blk} byte accounting mismatch: got {written_total}, expected {}",
                block_base + DENSE_BLOCK_BYTES
            ));
        }
    }

    writer
        .flush()
        .map_err(|e| format!("dense fixture flush failed: {e}"))?;
    let file = writer
        .into_inner()
        .map_err(|e| format!("dense fixture writer finalize failed: {e}"))?;
    file.sync_data()
        .map_err(|e| format!("dense fixture sync_data failed: {e}"))?;

    if written_total != DENSE_TOTAL_BYTES {
        return Err(format!(
            "dense fixture size mismatch: wrote {written_total}, expected {DENSE_TOTAL_BYTES}"
        ));
    }

    Ok(DenseFixtureSpec {
        path,
        total_bytes: DENSE_TOTAL_BYTES,
        newline_count,
        expected_index_total_lines: newline_count + 1,
        search_needle: DENSE_SEARCH_NEEDLE,
        expected_match_offsets,
    })
}

/// Worker-only: creates a separate sparse `> 4 GiB` (`4 GiB + 4 MiB`) file inside an already-leased
/// `TempDir` via `OpenOptions::create_new(true)` solely for 64-bit byte offset correctness.
pub fn create_sparse_fixture(scratch_dir: &Path) -> Result<SparseFixtureSpec, SparseCreateError> {
    let path = scratch_dir.join("sparse-64bit-markers.txt");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| SparseCreateError::Io(format!("open sparse fixture failed: {e}")))?;

    file.set_len(SPARSE_LOGICAL_BYTES).map_err(|e| {
        let _ = std::fs::remove_file(&path);
        SparseCreateError::SetLenRejected(format!(
            "filesystem rejected sparse set_len({SPARSE_LOGICAL_BYTES}): {e}"
        ))
    })?;

    let mut island_anchors = [0u64; 9];
    let mut island_buf = Vec::with_capacity(73_728);
    let mut line_buf = Vec::with_capacity(512);

    for (slot, anchor_out) in island_anchors.iter_mut().enumerate() {
        let anchor = sparse_island_anchor(slot);
        *anchor_out = anchor;

        island_buf.clear();
        let write_start = if slot == 0 {
            0
        } else {
            // Cover [anchor - 4096 .. anchor + 69_632] with clean 64-byte newline-terminated UTF-8
            // rows so PagingService's 4-byte boundary probes before/after the 64 KiB slice never
            // touch sparse NUL bytes and land cleanly on exact `anchor`.
            anchor - 4096
        };

        if slot > 0 {
            // 4096 bytes = 64 lead-in rows of 64 bytes before `anchor`
            for lead in 0..64usize {
                let off = write_start + (lead as u64) * 64;
                let prefix = format!("SPARSE64_LEAD_{slot:02}_{lead:02}_0x{off:010X}_");
                write_padded_line(&mut line_buf, &prefix, b'-', 64)
                    .map_err(SparseCreateError::Io)?;
                island_buf.extend_from_slice(&line_buf);
            }
        }

        // Exact `anchor`: 64-byte origin marker line
        let marker = expected_sparse_island_marker(slot);
        write_padded_line(&mut line_buf, &marker, b'-', 64).map_err(SparseCreateError::Io)?;
        island_buf.extend_from_slice(&line_buf);

        // Exact `anchor + 64`: 512-byte >180-char continuation line at >= 4 GiB
        let long_marker =
            format!("SPARSE64_ISLAND_{slot:02}_LONGLINE_0x{anchor:010X}_中文64位偏移验证_");
        write_padded_line(&mut line_buf, &long_marker, b'=', 512).map_err(SparseCreateError::Io)?;
        island_buf.extend_from_slice(&line_buf);

        // Fill remaining up to 69_632 bytes past `anchor` (1079 rows * 64 = 69_056 bytes; 64 + 512 + 69_056 = 69_632)
        for row in 0..1079usize {
            let off = anchor + 576 + (row as u64) * 64;
            let prefix = format!("SPARSE64_ISLAND_{slot:02}_R{row:04}_0x{off:010X}_");
            write_padded_line(&mut line_buf, &prefix, b'.', 64).map_err(SparseCreateError::Io)?;
            island_buf.extend_from_slice(&line_buf);
        }

        file.seek(SeekFrom::Start(write_start))
            .map_err(|e| SparseCreateError::Io(format!("sparse seek failed: {e}")))?;
        file.write_all(&island_buf)
            .map_err(|e| SparseCreateError::Io(format!("sparse write_all failed: {e}")))?;
    }

    file.sync_data()
        .map_err(|e| SparseCreateError::Io(format!("sparse sync_data failed: {e}")))?;

    let observed_len = file
        .metadata()
        .map_err(|e| SparseCreateError::Io(format!("sparse metadata failed: {e}")))?
        .len();
    if observed_len != SPARSE_LOGICAL_BYTES {
        let _ = std::fs::remove_file(&path);
        return Err(SparseCreateError::SetLenRejected(format!(
            "sparse file length mismatch: got {observed_len}, expected {SPARSE_LOGICAL_BYTES}"
        )));
    }

    Ok(SparseFixtureSpec {
        path,
        logical_bytes: SPARSE_LOGICAL_BYTES,
        island_anchors,
    })
}

/// Worker-only: deterministically truncates the dense fixture by 4096 bytes on disk to trigger
/// `FileFingerprint` invalidation (`ErrorKind::SourceChanged`) without mtime sleep races.
pub fn mutate_dense_fixture_truncate(path: &Path) -> Result<u64, String> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("failed to open dense fixture for mutation: {e}"))?;
    let cur_len = file
        .metadata()
        .map_err(|e| format!("failed to stat dense fixture before mutation: {e}"))?
        .len();
    let new_len = cur_len.saturating_sub(4096);
    file.set_len(new_len)
        .map_err(|e| format!("failed to truncate dense fixture: {e}"))?;
    file.sync_data()
        .map_err(|e| format!("failed to sync mutated dense fixture: {e}"))?;
    Ok(new_len)
}

/// Worker-only: removes a single fixture file if present.
pub fn remove_fixture_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "failed to remove fixture '{}': {e}",
            path.display()
        )),
    }
}

fn write_padded_line(
    out: &mut Vec<u8>,
    prefix: &str,
    pad_byte: u8,
    exact_len: usize,
) -> Result<(), String> {
    let prefix_bytes = prefix.as_bytes();
    if prefix_bytes.len() >= exact_len {
        return Err(format!(
            "fixture line prefix ({} bytes) exceeds target exact_len ({exact_len})",
            prefix_bytes.len()
        ));
    }
    out.clear();
    out.extend_from_slice(prefix_bytes);
    out.resize(exact_len - 1, pad_byte);
    out.push(b'\n');
    Ok(())
}
