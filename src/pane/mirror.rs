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

/// Default local scrollback budget for the mirror emulator when no explicit size
/// is configured. Generous, since the point of the mirror is fast local
/// scrollback; this is client memory only. The interactive mirror session
/// overrides this per `config.mirror.local_scrollback_bytes`
/// (`design-mirror-tui.md` §5).
const LOCAL_MIRROR_SCROLLBACK_BYTES: usize = crate::config::DEFAULT_MIRROR_LOCAL_SCROLLBACK_BYTES;

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
    /// Local scrollback budget in bytes, preserved across re-snapshots so a
    /// configured size survives an `apply_snapshot` reset.
    scrollback_bytes: usize,
    /// Highest mirror sequence applied, or `None` before the first snapshot.
    last_seq: Option<u64>,
}

impl LocalMirror {
    /// Creates a mirror emulator with the default local scrollback budget.
    pub(crate) fn new(cols: u16, rows: u16) -> std::io::Result<Self> {
        Self::with_scrollback(cols, rows, LOCAL_MIRROR_SCROLLBACK_BYTES)
    }

    /// Creates a mirror emulator with an explicit local scrollback budget
    /// (`config.mirror.local_scrollback_bytes`).
    pub(crate) fn with_scrollback(
        cols: u16,
        rows: u16,
        scrollback_bytes: usize,
    ) -> std::io::Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let (response_tx, response_rx) = mpsc::channel::<Bytes>(64);
        let mut ghostty = crate::ghostty::Terminal::new(cols, rows, scrollback_bytes)
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
            scrollback_bytes,
            last_seq: None,
        })
    }

    /// The source terminal size the mirror is currently reproducing.
    #[cfg(test)]
    pub(crate) fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The local scrollback budget, in bytes, this emulator was built with.
    #[cfg(test)]
    pub(crate) fn scrollback_bytes(&self) -> usize {
        self.scrollback_bytes
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
        // Preserve the configured scrollback budget across the reset.
        *self = Self::with_scrollback(cols, rows, self.scrollback_bytes)?;
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

    /// Current terminal input modes reported by the mirrored emulator.
    pub(crate) fn input_state(&self) -> Option<crate::pane::InputState> {
        self.terminal.input_state()
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

/// Client-side local mirror pane. All methods delegate to the wrapped
/// [`PaneTerminal`] (herdr's own renderer), so a mirror pane draws and queries
/// identically to a server-owned pane (`design-mirror-tui.md` §1.4). The
/// server-only lifecycle and input-encoding surface is intentionally absent (see
/// [`crate::terminal::PaneView`]).
impl crate::terminal::PaneView for LocalMirror {
    fn render(&self, frame: &mut ratatui::Frame, area: Rect, show_cursor: bool) {
        self.terminal.render(frame, area, show_cursor);
    }

    fn cursor_state(
        &self,
        _area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        let mut cursor = self.terminal.cursor_state()?;
        // The mirror renders at its full source size, so `area` is unused; hide
        // the cursor when the caller opts out or the viewport is scrolled back,
        // matching `render_frame`.
        if !show_cursor || self.is_scrolled_back() {
            cursor.visible = false;
        }
        Some(cursor)
    }

    fn input_state(&self) -> Option<crate::pane::InputState> {
        self.terminal.input_state()
    }

    fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.terminal.wheel_routing()
    }

    fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    fn current_size(&self) -> (u16, u16) {
        // Matches the server runtime's ordering: (rows, cols).
        (self.rows, self.cols)
    }

    fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        self.terminal.scroll_metrics()
    }

    fn visible_text(&self) -> String {
        self.terminal.visible_text()
    }

    fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.terminal.visible_hyperlinks(area)
    }

    fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        self.terminal.search_text_matches(query, case_sensitive)
    }

    fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        self.terminal.text_matches_are_current(text_matches)
    }

    fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.word_motion_target(row, col, motion)
    }

    fn scroll_up(&self, lines: usize) {
        self.terminal.scroll_up(lines);
    }

    fn scroll_down(&self, lines: usize) {
        self.terminal.scroll_down(lines);
    }

    fn scroll_reset(&self) {
        self.terminal.scroll_reset();
    }

    fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.terminal.set_scroll_offset_from_bottom(lines);
    }
    // cwd/foreground_cwd default to None: a read-only mirror has no PTY cwd.
}

/// A [`LocalMirror`] behind interior mutability so it can serve as the `Mirror`
/// variant of [`crate::terminal::TerminalRuntime`].
///
/// The shared render trait ([`crate::terminal::PaneView`]) takes `&self`, but the
/// mirror is also fed sequenced data (`apply_snapshot`/`apply_event`) which needs
/// `&mut LocalMirror`. Wrapping the emulator in a `Mutex` lets both the render
/// path and the data-plane feed share one runtime through shared references, the
/// same way the PTY [`crate::pane::PaneRuntime`] uses interior mutability
/// (`design-mirror-tui.md` §1b). Methods here mirror the read/render/input-encode
/// surface of `PaneRuntime` so the enum runtime can dispatch to either backend.
///
/// Constructed by the mirror connection manager in a later phase; the feed API is
/// `#[allow(dead_code)]` until then.
#[cfg(unix)]
pub struct MirrorRuntime {
    inner: std::sync::Mutex<LocalMirror>,
}

#[cfg(unix)]
impl MirrorRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, LocalMirror> {
        // A poisoned lock only means a panic happened while rendering/feeding; the
        // emulator state is still coherent enough to read, so recover it rather
        // than propagate the panic (matches `MirrorLog`).
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // --- draw ---
    pub(crate) fn render(&self, frame: &mut ratatui::Frame, area: Rect, show_cursor: bool) {
        self.lock().terminal.render(frame, area, show_cursor);
    }

    pub(crate) fn cursor_state(
        &self,
        _area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        let guard = self.lock();
        let mut cursor = guard.terminal.cursor_state()?;
        // The mirror renders at its full source size, so `area` is unused; hide the
        // cursor when the caller opts out or the viewport is scrolled back, matching
        // `LocalMirror::render_frame`.
        if !show_cursor || guard.is_scrolled_back() {
            cursor.visible = false;
        }
        Some(cursor)
    }

    // --- input-relevant + viewport state queries ---
    pub(crate) fn input_state(&self) -> Option<crate::pane::InputState> {
        self.lock().terminal.input_state()
    }

    pub(crate) fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.lock().terminal.wheel_routing()
    }

    pub(crate) fn synchronized_output_active(&self) -> bool {
        self.lock().terminal.synchronized_output_active()
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        let guard = self.lock();
        // Matches the server runtime's ordering: (rows, cols).
        (guard.rows, guard.cols)
    }

    pub(crate) fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        self.lock().terminal.scroll_metrics()
    }

    // --- copy-mode / selection / search (read-only) ---
    pub(crate) fn visible_text(&self) -> String {
        self.lock().terminal.visible_text()
    }

    pub(crate) fn visible_ansi(&self) -> String {
        self.lock().terminal.visible_ansi()
    }

    pub(crate) fn detection_text(&self) -> String {
        self.lock().terminal.detection_text()
    }

    pub(crate) fn agent_osc_title(&self) -> String {
        self.lock().terminal.agent_osc_title()
    }

    pub(crate) fn agent_osc_progress(&self) -> String {
        self.lock().terminal.agent_osc_progress()
    }

    pub(crate) fn recent_text(&self, lines: usize) -> String {
        self.lock().terminal.recent_text(lines)
    }

    pub(crate) fn recent_ansi(&self, lines: usize) -> String {
        self.lock().terminal.recent_ansi(lines)
    }

    pub(crate) fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.lock().terminal.recent_unwrapped_text(lines)
    }

    pub(crate) fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        self.lock().terminal.recent_unwrapped_ansi(lines)
    }

    pub(crate) fn extract_selection(
        &self,
        selection: &crate::selection::Selection,
    ) -> Option<String> {
        self.lock().terminal.extract_selection(selection)
    }

    pub(crate) fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        self.lock()
            .terminal
            .search_text_matches(query, case_sensitive)
    }

    pub(crate) fn text_match_is_current(&self, text_match: crate::pane::TerminalTextMatch) -> bool {
        self.lock().terminal.text_match_is_current(text_match)
    }

    pub(crate) fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        self.lock().terminal.text_matches_are_current(text_matches)
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.lock().terminal.word_motion_target(row, col, motion)
    }

    pub(crate) fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.lock().terminal.visible_hyperlinks(area)
    }

    pub(crate) fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        self.lock()
            .terminal
            .kitty_image_placements_with_data_filter(needs_data)
    }

    // --- local viewport scroll (no network) ---
    pub(crate) fn scroll_up(&self, lines: usize) {
        self.lock().terminal.scroll_up(lines);
    }

    pub(crate) fn scroll_down(&self, lines: usize) {
        self.lock().terminal.scroll_down(lines);
    }

    pub(crate) fn scroll_reset(&self) {
        self.lock().terminal.scroll_reset();
    }

    pub(crate) fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.lock().terminal.set_scroll_offset_from_bottom(lines);
    }

    // --- input byte encoding ---
    //
    // The mirror never writes to a PTY; the app driver forwards these encoded
    // bytes over the network for the focused pane (`design-mirror-tui.md` §3.4).
    // The encoding still depends on the mirrored terminal's own modes, so it is
    // computed from the local emulator.
    pub(crate) fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        self.lock()
            .terminal
            .keyboard_protocol(crate::input::KeyboardProtocol::Legacy)
    }

    pub(crate) fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        let guard = self.lock();
        let protocol = guard
            .terminal
            .keyboard_protocol(crate::input::KeyboardProtocol::Legacy);
        guard.terminal.encode_terminal_key(key, protocol)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.lock()
            .terminal
            .encode_mouse_button(kind, column, row, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.lock()
            .terminal
            .encode_mouse_motion(kind, column, row, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.lock()
            .terminal
            .encode_mouse_wheel(kind, column, row, modifiers)
    }

    pub(crate) fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        let guard = self.lock();
        if guard.terminal.wheel_routing()? != crate::pane::WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        let protocol = guard
            .terminal
            .keyboard_protocol(crate::input::KeyboardProtocol::Legacy);
        Some(guard.terminal.encode_terminal_key(
            crate::input::TerminalKey::new(key, crossterm::event::KeyModifiers::empty()),
            protocol,
        ))
    }
}

/// Data-plane feed and lifecycle. Not yet wired up: the mirror connection manager
/// that drives these lands in a later phase (`design-mirror-tui.md` §6, Phase 3).
#[cfg(unix)]
#[allow(dead_code)]
impl MirrorRuntime {
    /// Creates a mirror runtime with a fresh emulator at the given source size
    /// and the default local scrollback budget.
    pub(crate) fn new(cols: u16, rows: u16) -> std::io::Result<Self> {
        Ok(Self {
            inner: std::sync::Mutex::new(LocalMirror::new(cols, rows)?),
        })
    }

    /// Creates a mirror runtime with an explicit local scrollback budget
    /// (`config.mirror.local_scrollback_bytes`).
    pub(crate) fn with_scrollback(
        cols: u16,
        rows: u16,
        scrollback_bytes: usize,
    ) -> std::io::Result<Self> {
        Ok(Self {
            inner: std::sync::Mutex::new(LocalMirror::with_scrollback(
                cols,
                rows,
                scrollback_bytes,
            )?),
        })
    }

    /// Wraps an already-constructed [`LocalMirror`].
    pub(crate) fn from_local_mirror(mirror: LocalMirror) -> Self {
        Self {
            inner: std::sync::Mutex::new(mirror),
        }
    }

    /// Applies a `MirrorSnapshot`, re-establishing the emulator at the source size.
    pub(crate) fn apply_snapshot(
        &self,
        base_seq: u64,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<MirrorApplyOutcome> {
        self.lock().apply_snapshot(base_seq, cols, rows)
    }

    /// Applies one sequenced mirror event.
    pub(crate) fn apply_event(&self, seq: u64, kind: MirrorEventKind) -> MirrorApplyOutcome {
        self.lock().apply_event(seq, kind)
    }

    /// Highest mirror sequence applied so far; used as `resume_from` on reconnect.
    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.lock().last_seq()
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
    fn configured_scrollback_survives_resnapshot() {
        // A configured (non-default) scrollback budget must be preserved when a
        // fresh snapshot resets the emulator, so a reconnect/resnapshot does not
        // silently revert a mirror pane to the default memory budget.
        let custom = 512 * 1024;
        let mut mirror = LocalMirror::with_scrollback(20, 4, custom).unwrap();
        assert_eq!(mirror.scrollback_bytes(), custom);
        mirror.apply_snapshot(0, 40, 10).unwrap();
        assert_eq!(mirror.scrollback_bytes(), custom);
        assert_eq!(mirror.size(), (40, 10));
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

    #[test]
    fn scroll_metrics_drive_the_scrollbar_decoration() {
        use crate::terminal::PaneView;

        // The pane scrollbar is rendered from `PaneView::scroll_metrics`; a mirror
        // pane must report a growing offset as it is scrolled back so the existing
        // `render_pane_scrollbar` path decorates it exactly like a PTY pane.
        let mut mirror = LocalMirror::new(20, 4).unwrap();
        mirror.apply_snapshot(0, 20, 4).unwrap();
        for i in 0..50u64 {
            mirror.apply_event(
                i + 1,
                MirrorEventKind::Output(format!("line{i}\r\n").into_bytes()),
            );
        }

        // At the live edge, there is no scrollback offset.
        let bottom = PaneView::scroll_metrics(&mirror).expect("metrics at bottom");
        assert_eq!(bottom.offset_from_bottom, 0);
        assert!(bottom.max_offset_from_bottom > 0, "scrollback is available");

        // Scrolling back reports a non-zero offset that the scrollbar renders.
        PaneView::scroll_up(&mirror, 10);
        let scrolled = PaneView::scroll_metrics(&mirror).expect("metrics while scrolled back");
        assert!(scrolled.offset_from_bottom > 0);
        assert!(scrolled.offset_from_bottom <= scrolled.max_offset_from_bottom);
    }

    #[test]
    fn local_mirror_is_a_pane_view() {
        use crate::terminal::PaneView;

        let mut mirror = LocalMirror::new(20, 4).unwrap();
        mirror.apply_snapshot(0, 20, 4).unwrap();
        for i in 0..50u64 {
            mirror.apply_event(
                i + 1,
                MirrorEventKind::Output(format!("line{i}\r\n").into_bytes()),
            );
        }

        // Read surface works through the shared trait; size is (rows, cols).
        assert_eq!(PaneView::current_size(&mirror), (4, 20));
        assert!(!PaneView::is_alt_screen(&mirror));
        assert!(PaneView::visible_text(&mirror).contains("line49"));

        // Local scroll through the trait moves the viewport with no new events.
        assert!(!mirror.is_scrolled_back());
        PaneView::scroll_up(&mirror, 20);
        assert!(mirror.is_scrolled_back());
        PaneView::scroll_reset(&mirror);
        assert!(!mirror.is_scrolled_back());
    }
}
