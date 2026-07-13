//! Data-plane connection manager for the mirror session.
//!
//! While the control plane ([`super::JsonApiClient`]) tracks *what* panes exist
//! and how they are laid out, the data plane replicates each terminal's *content*.
//! [`MirrorConnectionManager`] owns one binary wire connection per `terminal_id`
//! (`ClientMessage::MirrorTerminal`), each feeding a [`MirrorRuntime`] the raw PTY
//! output, resize, and close events the server replicates
//! (`design-mirror-tui.md` §2.1/§2.4, Phase 3).
//!
//! Ownership: the manager holds an `Arc<MirrorRuntime>` per terminal and shares a
//! clone with the render registry (via [`crate::terminal::TerminalRuntime::mirror`],
//! wired up by the Phase 4 app driver), so the same local emulator that the
//! manager feeds is the one the TUI renders. The manager itself only touches the
//! transport and the feed; it never renders.
//!
//! Authority model (`design-mirror-tui.md` §2.2): the connection for the focused
//! terminal is opened `writable`, so the client's keystrokes reach that PTY; all
//! other connections are read-only. Focus changes re-subscribe the affected
//! connections to move the writable bit, resuming from the last applied sequence
//! so the server sends a cheap delta rather than a fresh snapshot.
//!
//! The transport methods here need a live server, so they are exercised by the
//! end-to-end acceptance tests; the pure routing/bookkeeping (which runtime a
//! message feeds, writable/focus tracking, resume sequences) is unit-tested below
//! without a server by feeding synthetic mirror messages.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use interprocess::TryClone as _;
use tracing::warn;

use crate::client::io_pump::ClientEventPump;
use crate::client::{connect_mirror_data_stream, write_to_server, ClientError};
use crate::ipc::LocalStream;
use crate::pane::{MirrorApplyOutcome, MirrorRuntime};
use crate::protocol::{ClientMessage, ServerMessage, MAX_FRAME_SIZE};

/// A single live per-terminal mirror connection: the writable half of the wire
/// socket plus the join handle of the reader feeding the pump. The emulator being
/// fed lives in [`MirrorConnectionManager::runtimes`] so it can outlive a dropped
/// connection (and be reconnected/resumed).
struct MirrorConnection {
    /// The writer half; keystroke/resize forwarding for the focused terminal.
    write_stream: LocalStream,
    /// The tagged reader thread; joined on close/reconnect.
    reader: JoinHandle<()>,
    /// Whether this connection was subscribed `writable` (focused terminal).
    writable: bool,
}

/// The result of applying one data-plane message to a terminal's local emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorApply {
    /// Applied to the terminal's emulator; `outcome` says whether a redraw or a
    /// close resulted.
    Applied(MirrorApplyOutcome),
    /// The message was not a mirror data message (snapshot/event); ignored.
    NotMirror,
    /// No emulator exists for that terminal (already closed); ignored.
    Unknown,
}

/// Owns the per-terminal data-plane connections and the local emulators they feed.
pub struct MirrorConnectionManager {
    /// Wire socket to connect data-plane connections to (`herdr-client.sock`).
    socket_path: PathBuf,
    /// The client's current viewport size `(cols, rows)`. Used as the initial
    /// size for a freshly-opened emulator before its first snapshot arrives; the
    /// per-pane size the focused PTY is driven to is forwarded separately by the
    /// app driver (`design-mirror-tui.md` §3.3).
    size: (u16, u16),
    /// Per-pane local scrollback budget, in bytes, for new emulators
    /// (`config.mirror.local_scrollback_bytes`).
    local_scrollback_bytes: usize,
    /// Local emulators keyed by `terminal_id`, shared with the render registry.
    runtimes: BTreeMap<String, Arc<MirrorRuntime>>,
    /// Live transport per `terminal_id`; absent while a terminal is disconnected
    /// and awaiting reconnect.
    connections: BTreeMap<String, MirrorConnection>,
    /// The currently focused (writable) terminal, if any.
    focused: Option<String>,
}

impl MirrorConnectionManager {
    /// Creates a manager targeting `socket_path` (the wire/client socket) with an
    /// initial `cols`x`rows` viewport size and a per-pane local scrollback budget.
    ///
    /// Reader threads are owned by the [`ClientEventPump`] passed to [`Self::open`]
    /// and stop on that pump's shared quit flag, so the manager holds no quit
    /// state of its own.
    pub fn new(socket_path: PathBuf, cols: u16, rows: u16, local_scrollback_bytes: usize) -> Self {
        Self {
            socket_path,
            size: (cols.max(1), rows.max(1)),
            local_scrollback_bytes,
            runtimes: BTreeMap::new(),
            connections: BTreeMap::new(),
            focused: None,
        }
    }

    /// The local emulator for `terminal_id`, shared with the render registry.
    ///
    /// The Phase 4 app driver clones this into a
    /// [`crate::terminal::TerminalRuntime::mirror`] so the pane renders the very
    /// emulator this manager feeds.
    pub fn runtime(&self, terminal_id: &str) -> Option<Arc<MirrorRuntime>> {
        self.runtimes.get(terminal_id).cloned()
    }

    /// The terminals with a live emulator, in stable order.
    pub fn terminal_ids(&self) -> impl Iterator<Item = &String> {
        self.runtimes.keys()
    }

    /// Whether a live transport connection exists for `terminal_id`.
    pub fn is_connected(&self, terminal_id: &str) -> bool {
        self.connections.contains_key(terminal_id)
    }

    /// The currently focused (writable) terminal.
    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Opens a data connection for `terminal_id`, creating its emulator if needed
    /// and spawning a tagged reader on `pump`.
    ///
    /// Idempotent: opening an already-connected terminal only updates its writable
    /// state via [`Self::set_focus`]. `writable` marks the focused terminal, whose
    /// keystrokes are forwarded to the real PTY.
    pub fn open(
        &mut self,
        pump: &ClientEventPump,
        terminal_id: &str,
        writable: bool,
    ) -> Result<(), ClientError> {
        if self.connections.contains_key(terminal_id) {
            if writable {
                self.set_focus(Some(terminal_id))?;
            }
            return Ok(());
        }

        // Reuse an existing emulator (a reconnect) so its `last_seq` drives a
        // resume; otherwise start a fresh one at the current viewport size.
        let runtime = match self.runtimes.get(terminal_id) {
            Some(runtime) => Arc::clone(runtime),
            None => {
                let runtime = Arc::new(
                    MirrorRuntime::with_scrollback(
                        self.size.0,
                        self.size.1,
                        self.local_scrollback_bytes,
                    )
                    .map_err(ClientError::ConnectionFailed)?,
                );
                self.runtimes
                    .insert(terminal_id.to_owned(), Arc::clone(&runtime));
                runtime
            }
        };

        let resume_from = runtime.last_seq();
        let stream = connect_mirror_data_stream(
            &self.socket_path,
            terminal_id,
            self.size.0,
            self.size.1,
            resume_from,
            writable,
        )?;

        let read_stream = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
        let reader = pump.spawn_mirror_reader(read_stream, MAX_FRAME_SIZE, terminal_id.to_owned());

        self.connections.insert(
            terminal_id.to_owned(),
            MirrorConnection {
                write_stream: stream,
                reader,
                writable,
            },
        );
        if writable {
            // This connection was subscribed writable; move focus here and demote
            // any previously-focused, still-open terminal so only one connection
            // is ever writable (`design-mirror-tui.md` §2.2).
            let previous = self.focused.replace(terminal_id.to_owned());
            if let Some(previous) = previous {
                if previous != terminal_id && self.connections.contains_key(&previous) {
                    self.resubscribe(&previous, false)?;
                }
            }
        }
        Ok(())
    }

    /// Opens connections for every `terminal_id`, marking `focused` writable.
    pub fn open_all(
        &mut self,
        pump: &ClientEventPump,
        terminal_ids: &[String],
        focused: Option<&str>,
    ) -> Result<(), ClientError> {
        for terminal_id in terminal_ids {
            let writable = focused == Some(terminal_id.as_str());
            self.open(pump, terminal_id, writable)?;
        }
        Ok(())
    }

    /// Closes and forgets a terminal's connection and emulator (e.g. on
    /// `PaneClosed`). The reader exits once the server closes the mirror; its
    /// handle is joined best-effort without blocking the caller.
    pub fn close(&mut self, terminal_id: &str) {
        self.runtimes.remove(terminal_id);
        if let Some(connection) = self.connections.remove(terminal_id) {
            let mut write_stream = connection.write_stream;
            let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
            drop(write_stream);
            if connection.reader.is_finished() {
                let _ = connection.reader.join();
            }
        }
        if self.focused.as_deref() == Some(terminal_id) {
            self.focused = None;
        }
    }

    /// Routes a data-plane [`ServerMessage`] to its terminal's local emulator.
    ///
    /// Called by the app loop for each [`crate::client::ClientLoopEvent::ServerMirror`].
    /// Non-mirror messages and messages for unknown terminals are ignored.
    pub fn apply(&mut self, terminal_id: &str, msg: ServerMessage) -> MirrorApply {
        let Some(runtime) = self.runtimes.get(terminal_id) else {
            return MirrorApply::Unknown;
        };
        match msg {
            ServerMessage::MirrorSnapshot {
                base_seq,
                cols,
                rows,
            } => match runtime.apply_snapshot(base_seq, cols, rows) {
                Ok(outcome) => MirrorApply::Applied(outcome),
                Err(err) => {
                    warn!(terminal_id, err = %err, "mirror snapshot apply failed");
                    MirrorApply::Applied(MirrorApplyOutcome::default())
                }
            },
            ServerMessage::MirrorEvent { seq, kind } => {
                MirrorApply::Applied(runtime.apply_event(seq, kind))
            }
            _ => MirrorApply::NotMirror,
        }
    }

    /// Handles a dropped data connection by reconnecting and resuming from the
    /// last applied sequence, keeping the emulator on screen across the blip
    /// (`design-mirror-tui.md` §2.4). Returns `Ok(false)` if the terminal is no
    /// longer tracked (it was closed) so the caller can stop retrying.
    pub fn reconnect(
        &mut self,
        pump: &ClientEventPump,
        terminal_id: &str,
    ) -> Result<bool, ClientError> {
        // Drop any stale connection first so `open` re-subscribes with a resume.
        if let Some(connection) = self.connections.remove(terminal_id) {
            drop(connection.write_stream);
            let _ = connection.reader.join();
        }
        if !self.runtimes.contains_key(terminal_id) {
            return Ok(false);
        }
        let writable = self.focused.as_deref() == Some(terminal_id);
        self.open(pump, terminal_id, writable)?;
        Ok(true)
    }

    /// Moves the writable bit to `terminal_id` (or clears it when `None`) on a
    /// focus change: promote the newly-focused connection before demoting the old
    /// one so a keystroke never falls between two read-only connections
    /// (`design-mirror-tui.md` §7). Each side re-subscribes on its existing
    /// connection, resuming from the last applied sequence.
    pub fn set_focus(&mut self, terminal_id: Option<&str>) -> Result<(), ClientError> {
        if self.focused.as_deref() == terminal_id {
            return Ok(());
        }
        let previous = self.focused.take();

        // Promote the new focus first.
        if let Some(new_focus) = terminal_id {
            self.resubscribe(new_focus, true)?;
            self.focused = Some(new_focus.to_owned());
        }

        // Then demote the old one, if it is a different, still-open terminal.
        if let Some(previous) = previous {
            if Some(previous.as_str()) != terminal_id && self.connections.contains_key(&previous) {
                self.resubscribe(&previous, false)?;
            }
        }
        Ok(())
    }

    /// Forwards raw input bytes to the focused terminal's writable connection.
    /// Input for a terminal with no writable connection is dropped (there is no
    /// authoritative PTY to receive it).
    pub fn forward_input(&mut self, data: Vec<u8>) -> io::Result<()> {
        let Some(terminal_id) = self.focused.clone() else {
            return Ok(());
        };
        self.forward_input_to(&terminal_id, data)
    }

    /// Forwards encoded input bytes to a specific writable mirror connection.
    /// Used for pane-targeted mouse events after the app driver promotes that
    /// terminal to the writable mirror.
    pub fn forward_input_to(&mut self, terminal_id: &str, data: Vec<u8>) -> io::Result<()> {
        match self.connections.get_mut(terminal_id) {
            Some(connection) if connection.writable => {
                write_to_server(&mut connection.write_stream, &ClientMessage::Input { data })
            }
            _ => Ok(()),
        }
    }

    /// Records the client's viewport size, used as the initial size for
    /// emulators opened later (before their first snapshot). This does not touch
    /// the server: the focused PTY is sized per-pane via [`Self::forward_resize`].
    pub fn set_viewport(&mut self, cols: u16, rows: u16) {
        self.size = (cols.max(1), rows.max(1));
    }

    /// Drives the focused (writable) terminal's PTY to `cols`x`rows` — the size
    /// the mirror renders that pane at — so the server reflows the real shell to
    /// match (`design-mirror-tui.md` §3.3). The resulting `MirrorEvent::Resize`
    /// replicates the new size back into the local emulator.
    ///
    /// Crucially this is the *focused pane's content rect*, not the whole client
    /// viewport: in a multi-pane layout the focused pane is only part of the
    /// screen, so forwarding the viewport would wrongly size that one PTY to the
    /// full terminal and corrupt every client's layout.
    pub fn forward_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let Some(terminal_id) = self.focused.clone() else {
            return Ok(());
        };
        match self.connections.get_mut(&terminal_id) {
            Some(connection) if connection.writable => write_to_server(
                &mut connection.write_stream,
                &ClientMessage::Resize {
                    cols,
                    rows,
                    cell_width_px: 0,
                    cell_height_px: 0,
                },
            ),
            _ => Ok(()),
        }
    }

    /// Re-subscribes an already-open connection with a new `writable` bit,
    /// resuming from its last applied sequence.
    fn resubscribe(&mut self, terminal_id: &str, writable: bool) -> Result<(), ClientError> {
        let resume_from = self.runtimes.get(terminal_id).and_then(|rt| rt.last_seq());
        let Some(connection) = self.connections.get_mut(terminal_id) else {
            return Ok(());
        };
        write_to_server(
            &mut connection.write_stream,
            &ClientMessage::MirrorTerminal {
                target: terminal_id.to_owned(),
                resume_from,
                writable,
            },
        )
        .map_err(ClientError::ConnectionLost)?;
        connection.writable = writable;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{bind_local_listener, connect_local_stream};
    use crate::protocol::MirrorEventKind;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A manager with no live server, for exercising the emulator-feeding and
    /// bookkeeping paths. Connections are never opened; `runtimes` is populated
    /// directly, exactly as a successful `open` would.
    fn manager_with_terminals(terminals: &[&str]) -> MirrorConnectionManager {
        let mut manager = MirrorConnectionManager::new(
            PathBuf::from("/nonexistent/herdr-client.sock"),
            80,
            24,
            64 * 1024,
        );
        for terminal_id in terminals {
            manager.runtimes.insert(
                (*terminal_id).to_owned(),
                Arc::new(MirrorRuntime::new(80, 24).unwrap()),
            );
        }
        manager
    }

    fn unique_socket_path() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "herdr-mirror-conn-test-{}-{n}.sock",
            std::process::id()
        ))
    }

    /// Inserts a fake but *live* connection backed by a real connected socket
    /// pair, whose peer end is continuously drained so `resubscribe` writes never
    /// block. This exercises the writable-bit handoff bookkeeping (which needs
    /// entries in `connections`) without a herdr server.
    fn insert_live_connection(
        manager: &mut MirrorConnectionManager,
        terminal_id: &str,
        writable: bool,
    ) {
        let path = unique_socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = bind_local_listener(&path).expect("bind test listener");
        let client = connect_local_stream(&path).expect("connect test stream");
        let peer = listener.accept().expect("accept test stream");
        // Drain the peer so the socket stays open and writes never block; the
        // loop ends (and the thread exits) when the client end is dropped.
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let mut buf = [0u8; 256];
            while peer.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
        });
        let _ = std::fs::remove_file(&path);
        manager.connections.insert(
            terminal_id.to_owned(),
            MirrorConnection {
                write_stream: client,
                reader,
                writable,
            },
        );
    }

    fn writable_count(manager: &MirrorConnectionManager) -> usize {
        manager
            .connections
            .values()
            .filter(|connection| connection.writable)
            .count()
    }

    #[test]
    fn apply_routes_snapshot_and_output_to_the_right_emulator() {
        let mut manager = manager_with_terminals(&["term_a", "term_b"]);

        assert!(matches!(
            manager.apply(
                "term_a",
                ServerMessage::MirrorSnapshot {
                    base_seq: 0,
                    cols: 80,
                    rows: 24,
                },
            ),
            MirrorApply::Applied(_)
        ));
        manager.apply(
            "term_a",
            ServerMessage::MirrorEvent {
                seq: 1,
                kind: MirrorEventKind::Output(b"hello from a".to_vec()),
            },
        );
        manager.apply(
            "term_b",
            ServerMessage::MirrorSnapshot {
                base_seq: 0,
                cols: 80,
                rows: 24,
            },
        );
        manager.apply(
            "term_b",
            ServerMessage::MirrorEvent {
                seq: 1,
                kind: MirrorEventKind::Output(b"greetings from b".to_vec()),
            },
        );

        let a = manager.runtime("term_a").unwrap();
        let b = manager.runtime("term_b").unwrap();
        assert!(a.visible_text().contains("hello from a"));
        assert!(!a.visible_text().contains("greetings from b"));
        assert!(b.visible_text().contains("greetings from b"));
    }

    #[test]
    fn apply_reports_close_and_ignores_unknown_terminal() {
        let mut manager = manager_with_terminals(&["term_a"]);
        manager.apply(
            "term_a",
            ServerMessage::MirrorSnapshot {
                base_seq: 0,
                cols: 80,
                rows: 24,
            },
        );

        let closed = manager.apply(
            "term_a",
            ServerMessage::MirrorEvent {
                seq: 1,
                kind: MirrorEventKind::Closed { reason: None },
            },
        );
        assert!(matches!(
            closed,
            MirrorApply::Applied(MirrorApplyOutcome { closed: true, .. })
        ));

        assert_eq!(
            manager.apply(
                "ghost",
                ServerMessage::MirrorEvent {
                    seq: 1,
                    kind: MirrorEventKind::Output(b"x".to_vec()),
                },
            ),
            MirrorApply::Unknown
        );
    }

    #[test]
    fn apply_ignores_non_mirror_messages() {
        let mut manager = manager_with_terminals(&["term_a"]);
        assert_eq!(
            manager.apply("term_a", ServerMessage::ReloadSoundConfig),
            MirrorApply::NotMirror
        );
    }

    #[test]
    fn resume_from_tracks_last_applied_sequence() {
        let mut manager = manager_with_terminals(&["term_a"]);
        manager.apply(
            "term_a",
            ServerMessage::MirrorSnapshot {
                base_seq: 5,
                cols: 80,
                rows: 24,
            },
        );
        manager.apply(
            "term_a",
            ServerMessage::MirrorEvent {
                seq: 6,
                kind: MirrorEventKind::Output(b"more".to_vec()),
            },
        );
        // The emulator's last_seq is what `open`/`reconnect` resume from.
        assert_eq!(manager.runtime("term_a").unwrap().last_seq(), Some(6));
    }

    #[test]
    fn close_forgets_runtime_and_clears_focus() {
        let mut manager = manager_with_terminals(&["term_a", "term_b"]);
        manager.focused = Some("term_a".to_owned());

        manager.close("term_a");
        assert!(manager.runtime("term_a").is_none());
        assert!(manager.runtime("term_b").is_some());
        assert_eq!(manager.focused(), None);
    }

    #[test]
    fn set_focus_is_a_noop_when_focus_is_unchanged() {
        // With no live connections, re-focusing the already-focused terminal must
        // not attempt any network I/O (which would fail against the dead socket).
        let mut manager = manager_with_terminals(&["term_a"]);
        manager.focused = Some("term_a".to_owned());
        assert!(manager.set_focus(Some("term_a")).is_ok());
        assert_eq!(manager.focused(), Some("term_a"));
    }

    #[test]
    fn forward_input_without_focus_is_dropped() {
        // No focused/writable connection: input has nowhere authoritative to go,
        // and must be silently dropped rather than erroring.
        let mut manager = manager_with_terminals(&["term_a"]);
        assert!(manager.forward_input(b"ls\n".to_vec()).is_ok());
    }

    #[test]
    fn set_focus_moves_the_single_writable_bit_to_the_new_focus() {
        // The handoff must leave exactly one writable connection — the new focus —
        // so a keystroke can never fall between two read-only connections
        // (`design-mirror-tui.md` §7: promote-new-before-demote-old).
        let mut manager = manager_with_terminals(&["a", "b"]);
        insert_live_connection(&mut manager, "a", true);
        insert_live_connection(&mut manager, "b", false);
        manager.focused = Some("a".to_owned());

        manager.set_focus(Some("b")).unwrap();

        assert_eq!(manager.focused(), Some("b"));
        assert!(
            manager.connections.get("b").unwrap().writable,
            "new focus is promoted to writable"
        );
        assert!(
            !manager.connections.get("a").unwrap().writable,
            "old focus is demoted to read-only"
        );
        assert_eq!(
            writable_count(&manager),
            1,
            "exactly one writable connection after the handoff"
        );
    }

    #[test]
    fn forward_resize_only_writes_to_the_focused_writable_connection() {
        let mut manager = manager_with_terminals(&["a", "b"]);
        insert_live_connection(&mut manager, "a", true);
        insert_live_connection(&mut manager, "b", false);

        // No focus: nothing to drive, but not an error.
        manager.focused = None;
        assert!(manager.forward_resize(40, 12).is_ok());

        // Focused + writable: the resize is forwarded to that connection's PTY.
        manager.focused = Some("a".to_owned());
        assert!(manager.forward_resize(40, 12).is_ok());

        // Focused but read-only (should not happen post-handoff, but must be a
        // safe no-op rather than driving a non-writable PTY).
        manager.focused = Some("b".to_owned());
        assert!(manager.forward_resize(40, 12).is_ok());
    }

    #[test]
    fn set_viewport_does_not_touch_connections() {
        // Updating the client viewport records the default size for future
        // emulator opens; it must never drive an existing PTY (that is done
        // per-pane via `forward_resize`).
        let mut manager = manager_with_terminals(&["a"]);
        insert_live_connection(&mut manager, "a", true);
        manager.focused = Some("a".to_owned());
        manager.set_viewport(120, 40);
        assert_eq!(manager.size, (120, 40));
        assert!(manager.connections.get("a").unwrap().writable);
    }

    #[test]
    fn set_focus_none_demotes_the_writable_connection() {
        let mut manager = manager_with_terminals(&["a"]);
        insert_live_connection(&mut manager, "a", true);
        manager.focused = Some("a".to_owned());

        manager.set_focus(None).unwrap();

        assert_eq!(manager.focused(), None);
        assert!(!manager.connections.get("a").unwrap().writable);
        assert_eq!(writable_count(&manager), 0);
    }
}
