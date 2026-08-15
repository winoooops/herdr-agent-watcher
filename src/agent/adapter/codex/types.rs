//! Private codex-internal types. Visibility intentionally `pub(super)` so
//! these types do not leak through the `AgentAdapter` trait surface.

use std::path::Path;
use std::time::SystemTime;

/// Bag of attach-time facts the codex locator needs. Built fresh on each
/// `status_source` call from the adapter's stored `pid`/`pty_start` plus
/// the trait method's `cwd` parameter.
///
/// Note: `session_id` is intentionally not a field. The codex locator gates
/// queries by `pid` + `pty_start` + `cwd` only.
#[derive(Debug, Clone, Copy)]
pub(super) struct BindContext<'a> {
    pub(super) cwd: &'a Path,
    pub(super) pid: u32,
    pub(super) pty_start: SystemTime,
}
