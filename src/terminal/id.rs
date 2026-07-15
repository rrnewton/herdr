use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

impl TerminalId {
    pub fn alloc() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!("term_{micros:x}{counter:x}"))
    }

    /// Reconstruct a terminal id from a server-authoritative string (e.g. a
    /// `terminal_id` returned over the JSON API). This preserves the server's
    /// existing identity for a replicated/mirror client; it is **not** a way to
    /// derive an id from a pane id or layout position (see the type doc).
    #[cfg(unix)]
    pub(crate) fn from_string(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
