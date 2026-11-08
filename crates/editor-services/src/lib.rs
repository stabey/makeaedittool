//! Headless editor services. All filesystem, hashing, SQLite, indexing and save work
//! is blocking by design and belongs on background workers. Only bounded `try_*`
//! admission and channel polling are intended for UI-thread use.
//!
//! Normal text authority remains GPUI Kit's EditorState. Services receive immutable
//! committed Rope clones; they never observe IME preedit or own a competing editor.
//!
//! # Resource and durability contract
//! - `PagingService::default()` shares one process-wide 64 MiB page-data cache and
//!   16 MiB sparse-anchor budget. Viewport text is capped at 64 KiB; invalid UTF-8 is
//!   explicitly marked lossy, with its raw consumed byte range kept accurate.
//!   Anchors grow within the shared budget; pressure thins the largest index.
//!   Known-line probes read at most 1 MiB off-thread, otherwise report unknown.
//!   Two process-wide reusable indexing buffers and two ordinary-open permits
//!   bound background I/O concurrency; foreground viewports do not take permits.
//! - Ordinary hydration is limited to 64 MiB and routes lines exceeding 1 MiB to
//!   read-only paging. Paged encoding/EOL detection is prefix-only, never a claim
//!   that an unscanned multi-gigabyte source has been fully validated.
//! - Recovery has one SQLite WAL/FULL writer. A 128 MiB weighted mailbox holds
//!   immutable snapshots and sequential committed deltas; a separate 64 MiB
//!   derived-rope cache avoids reloading active baselines. First commits and queue
//!   gaps are self-contained 256 KiB-chunk checkpoints. Subsequent edits append
//!   checksummed deltas; compaction occurs at 512 rows or max(4 MiB, text_len/8).
//!   Neither queued nor visible revisions imply durability before an acknowledgement.
//!   Current metadata follows commit/layout admission order independently of text
//!   batching; saved-format associations merge without reverting newer preferences.
//!   Before logging a delta the worker replays and compares it with the captured
//!   rope (O(bytes) worst case). A valid but mismatching replay promotes to a
//!   checkpoint; invalid ranges fail closed. `recovery-bench` measures this cost
//!   with SQLite FULL commits and a separately labelled equivalent equality kernel.
//!   Durable heads remain discoverable without a layout row. Corrupt metadata
//!   isolates only that record and produces a manifest warning; its database
//!   content is preserved, but this service does not claim it can salvage that draft.
//! - Queues and admission are bounded. `Busy` requires the consumer to retain and
//!   retry its request/snapshot. Search uses a fixed worker pool and latest-wins
//!   per-document slots; same-document supersession does not consume another slot.
//!   Reaching the result cap always reports an incomplete count, including at EOF.
//!   Scanned versus read bytes are distinct and capped scans have no full-file rate.
//!   Shutdown is asynchronous and returns an error instead of pretending failed
//!   accepted recovery writes were flushed.
//! - Save captures a rope, preserves BOM and uniform LF/CRLF, and never deletes an
//!   existing target before publication. macOS uses same-directory exclusive temp
//!   creation, metadata/ACL/xattr copying, F_FULLFSYNC, rename, and directory sync.
//!   Symlinks resolve to their target; hard links require a separate Save As path.
//!   Recovery failure permits explicit rescue-save, with a distinct warning.
//! - UI preferences and recent paths use a separate 256 KiB JSON store with one
//!   latest-state slot and one atomic-publication worker. `queue` is bounded UI-safe
//!   admission, while `flush`/`shutdown` replies report writer failures independently
//!   of draft recovery. Loading does not stat recent targets or rewrite input;
//!   damaged settings are retained in a unique backup on the first queued change.
//! - Filesystem identities detect ordinary changes, not adversarial writes preserving
//!   metadata. Existing-target rename has an unavoidable external check/rename race;
//!   Windows atomic replacement and real power-loss durability remain unverified.
//!
//! The `editor-services` executable supplies generation, read-at baseline, both
//! search kernels, storage viewport quantiles, multifile budgets, cancellation,
//! dense indexing, lazy sessions, smoke and process-exit fault scenarios. Run it
//! without arguments for usage. A passing process-exit test is not a power-cut test.

mod open;
mod paging;
mod recovery;
mod save;
mod search;
mod session_layout;
mod types;
mod ui_shell;

pub use crossbeam_channel::{Receiver, TryRecvError};
pub use open::*;
pub use paging::*;
pub use recovery::*;
pub use ropey::Rope;
pub use save::*;
pub use search::*;
pub use session_layout::*;
pub use types::*;
pub use ui_shell::*;
