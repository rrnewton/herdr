//! Client-side local terminal mirror emulator.
//!
//! A [`LocalMirror`] runs a real terminal emulator locally from a mirror
//! stream (see the responsive local mirror protocol in `src/protocol/wire.rs`).
//! The client feeds it the raw PTY output and resize events replicated from the
//! server, then renders and scrolls it entirely locally, so scrollback, search,
//! and selection cost no server round-trip. It reuses herdr's own terminal
//! renderer, so the local view matches what the server would render.

use bytes::Bytes;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use super::terminal::{GhosttyPaneTerminal, PaneTerminal};
use crate::layout::PaneId;
use crate::protocol::{CursorState, FrameData, MirrorEventKind};

/// Local scrollback budget for the mirror emulator. Generous, since the point of
/// the mirror is fast local scrollback; this is client memory only.
const LOCAL_MIRROR_SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;

/// What the caller should do after applying a mirror message.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MirrorApplyOutcome {
    /// The emulator was reset or resized; the renderer must do a full redraw.
    pub needs_full_redraw: bool,
    /// The source terminal reported it ended.
    pub closed: bool,
}

/// A local terminal emulator driven by a replicated mirror stream.
pub(crate) struct LocalMirror {
    terminal: PaneTerminal,
    // Kept alive so terminal responses (which a read-only mirror discards) have a
    // valid sink; the emulator never actually writes to it.
    _response_tx: mpsc::Sender<Bytes>,
    _response_rx: mpsc::Receiver<Bytes>,
    /// Current source terminal size (columns, rows).
    cols: u16,
    rows: u16,
    /// Highest mirror sequence applied, or `None` before the first snapshot.
    last_seq: Option<u64>,
}

impl LocalMirror {
    pub(crate) fn new(cols: u16, rows: u16) -> std::io::Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let (response_tx, response_rx) = mpsc::channel::<Bytes>(64);
        let mut ghostty = crate::ghostty::Terminal::new(cols, rows, LOCAL_MIRROR_SCROLLBACK_BYTES)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        ghostty
            .enable_grapheme_cluster_mode()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let pane_terminal =
            PaneTerminal::new(GhosttyPaneTerminal::new(ghostty, response_tx.clone())?);
        Ok(Self {
            terminal: pane_terminal,
            _response_tx: response_tx,
            _response_rx: response_rx,
            cols,
            rows,
            last_seq: None,
        })
    }

    /// The source terminal size the mirror is currently reproducing.
    #[cfg(test)]
    pub(crate) fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Highest mirror sequence applied so far; used as the `resume_from` value on
    /// reconnect so the stream continues without a gap.
    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Applies a `MirrorSnapshot`: (re)establish a fresh emulator at the source
    /// size and mark the base sequence.
    pub(crate) fn apply_snapshot(
        &mut self,
        base_seq: u64,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<MirrorApplyOutcome> {
        *self = Self::new(cols, rows)?;
        self.last_seq = Some(base_seq);
        Ok(MirrorApplyOutcome {
            needs_full_redraw: true,
            closed: false,
        })
    }

    /// Applies one sequenced mirror event.
    pub(crate) fn apply_event(&mut self, seq: u64, kind: MirrorEventKind) -> MirrorApplyOutcome {
        self.last_seq = Some(seq);
        match kind {
            MirrorEventKind::Output(bytes) => {
                // Feed the raw bytes through the same emulator the server uses.
                // Terminal responses are returned and discarded: the server's real
                // terminal already answered any queries.
                let _ = self.terminal.process_pty_bytes(
                    PaneId::from_raw(0),
                    0,
                    &bytes,
                    &self._response_tx,
                );
                MirrorApplyOutcome::default()
            }
            MirrorEventKind::Resize { cols, rows } => {
                let cols = cols.max(1);
                let rows = rows.max(1);
                if (cols, rows) != (self.cols, self.rows) {
                    self.cols = cols;
                    self.rows = rows;
                    self.terminal.resize(rows, cols, 0, 0);
                    MirrorApplyOutcome {
                        needs_full_redraw: true,
                        closed: false,
                    }
                } else {
                    MirrorApplyOutcome::default()
                }
            }
            MirrorEventKind::Closed { .. } => MirrorApplyOutcome {
                needs_full_redraw: false,
                closed: true,
            },
        }
    }

    /// Scrolls the local viewport up (into scrollback) by `lines`. No network.
    pub(crate) fn scroll_up(&mut self, lines: usize) {
        self.terminal.scroll_up(lines);
    }

    /// Scrolls the local viewport down toward the live edge by `lines`.
    pub(crate) fn scroll_down(&mut self, lines: usize) {
        self.terminal.scroll_down(lines);
    }

    /// Returns to the live bottom of the stream.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_reset();
    }

    /// Whether the local viewport is scrolled back from the live edge.
    pub(crate) fn is_scrolled_back(&self) -> bool {
        self.terminal
            .scroll_metrics()
            .is_some_and(|metrics| metrics.offset_from_bottom > 0)
    }

    /// Whether the mirrored terminal is on the alternate screen (a full-screen
    /// app). Used to decide whether page keys scroll locally or are forwarded to
    /// the remote application.
    pub(crate) fn is_alt_screen(&self) -> bool {
        self.terminal
            .input_state()
            .is_some_and(|state| state.alternate_screen)
    }

    /// Renders the current viewport to a [`FrameData`] at the source size, using
    /// herdr's own terminal renderer.
    pub(crate) fn render_frame(&self) -> FrameData {
        let area = Rect::new(0, 0, self.cols, self.rows);
        let backend = ratatui::backend::TestBackend::new(self.cols, self.rows);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");
        terminal
            .draw(|frame| {
                self.terminal.render(frame, area, true);
            })
            .expect("render to TestBackend should never fail");
        let buffer = terminal.backend().buffer().clone();

        let scrolled_back = self.is_scrolled_back();
        let cursor = self.terminal.cursor_state().map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !scrolled_back,
            shape: cursor.shape,
        });

        let hyperlinks = self.terminal.visible_hyperlinks(area);
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks)
    }

    /// Plain-text of the current viewport. Used by tests and fidelity checks.
    #[cfg(test)]
    pub(crate) fn visible_text(&self) -> String {
        self.terminal.visible_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_text(frame: &FrameData) -> String {
        let mut out = String::new();
        for row in 0..frame.height {
            for col in 0..frame.width {
                let idx = row as usize * frame.width as usize + col as usize;
                out.push_str(&frame.cells[idx].symbol);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn applies_output_and_renders_it() {
        let mut mirror = LocalMirror::new(20, 4).unwrap();
        let outcome = mirror.apply_snapshot(0, 20, 4).unwrap();
        assert!(outcome.needs_full_redraw);
        mirror.apply_event(1, MirrorEventKind::Output(b"hello mirror".to_vec()));
        assert_eq!(mirror.last_seq(), Some(1));
        assert!(mirror.visible_text().contains("hello mirror"));
        let frame = mirror.render_frame();
        assert_eq!((frame.width, frame.height), (20, 4));
        assert!(frame_text(&frame).contains("hello mirror"));
    }

    #[test]
    fn resize_changes_source_size_and_requests_redraw() {
        let mut mirror = LocalMirror::new(20, 4).unwrap();
        mirror.apply_snapshot(0, 20, 4).unwrap();
        let outcome = mirror.apply_event(1, MirrorEventKind::Resize { cols: 40, rows: 10 });
        assert!(outcome.needs_full_redraw);
        assert_eq!(mirror.size(), (40, 10));
        let same = mirror.apply_event(2, MirrorEventKind::Resize { cols: 40, rows: 10 });
        assert!(!same.needs_full_redraw);
    }

    #[test]
    fn closed_event_is_reported() {
        let mut mirror = LocalMirror::new(20, 4).unwrap();
        mirror.apply_snapshot(0, 20, 4).unwrap();
        let outcome = mirror.apply_event(1, MirrorEventKind::Closed { reason: None });
        assert!(outcome.closed);
    }

    #[test]
    fn local_scroll_navigates_scrollback_without_new_events() {
        // Fill well beyond the 4-row viewport so there is scrollback.
        let mut mirror = LocalMirror::new(20, 4).unwrap();
        mirror.apply_snapshot(0, 20, 4).unwrap();
        let mut seq = 0;
        for i in 0..50 {
            seq += 1;
            mirror.apply_event(
                seq,
                MirrorEventKind::Output(format!("line{i}\r\n").into_bytes()),
            );
        }
        assert!(!mirror.is_scrolled_back());
        // Scroll up into history purely locally (no further events applied).
        mirror.scroll_up(20);
        assert!(mirror.is_scrolled_back());
        let scrolled = frame_text(&mirror.render_frame());
        // An earlier line is now visible in the viewport.
        assert!(scrolled.contains("line"));
        mirror.scroll_to_bottom();
        assert!(!mirror.is_scrolled_back());
    }
}
