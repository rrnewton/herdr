//! The shared pane rendering/query contract.
//!
//! [`PaneView`] is the read-only render + viewport surface the TUI consumes from
//! a pane's live terminal, regardless of whether that terminal is a server-owned
//! PTY runtime ([`TerminalRuntime`]) or a client-side local mirror
//! ([`crate::pane::LocalMirror`]). It is exactly the intersection of what the
//! server runtime exposes and what the mirror already does internally, so the
//! multi-pane TUI can be generic over "a pane's live terminal" (see
//! `design-mirror-tui.md` §1).
//!
//! Consistent with the project principle that *render is pure*: none of these
//! methods mutate terminal **content**; the `scroll_*` methods only move a local
//! viewport (implemented with interior mutability, hence `&self`).
//!
//! Two buckets are deliberately kept **off** this trait:
//!
//! * **Server-only lifecycle** (`spawn*`, `shutdown`, `output_log`, `handoff*`,
//!   PTY `resize`, theme/agent-detection control): these drive a real OS PTY and
//!   are meaningless for a read-only mirror, so they stay inherent on
//!   [`TerminalRuntime`].
//! * **Input byte encoding / sending** (`encode_terminal_key`, `send_bytes`,
//!   `encode_mouse_*`): on the server these write to the PTY; in a mirror session
//!   keystrokes are forwarded over the network, so input is routed by the app
//!   driver, not through this shared render trait. The state that *drives* input
//!   routing ([`PaneView::input_state`], [`PaneView::wheel_routing`],
//!   [`PaneView::is_alt_screen`]) is exposed here so callers such as
//!   [`crate::client::scroll`] can make that decision uniformly.

use ratatui::{layout::Rect, Frame};

use crate::pane::{
    InputState, ScrollMetrics, TerminalCursorState, TerminalTextMatch, TerminalTextPoint,
    TerminalWordMotion, WheelRouting,
};
use crate::selection::Selection;

use super::TerminalRuntime;

/// The read-only render + viewport surface a pane's live terminal exposes to the
/// TUI. Implemented by both the server runtime and the client-side local mirror
/// so the TUI can treat the two interchangeably.
///
/// Foundational contract for the mirror-session work in `design-mirror-tui.md`;
/// the multi-pane consumers land in later phases, so the trait itself is not yet
/// invoked outside tests.
#[allow(dead_code)]
pub(crate) trait PaneView {
    // --- draw ---
    /// Draws the current viewport in place into `frame` at `area`.
    fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool);
    /// The host cursor state for this viewport, if any.
    fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState>;

    // --- input-relevant + viewport state queries ---
    /// Terminal input modes (alt-screen, keyboard protocol, mouse mode, …).
    fn input_state(&self) -> Option<InputState>;
    /// How mouse-wheel events should be routed for this terminal.
    fn wheel_routing(&self) -> Option<WheelRouting>;
    /// Whether synchronized output (DEC 2026) is currently active.
    fn synchronized_output_active(&self) -> bool;
    /// Current terminal size as `(rows, cols)`.
    fn current_size(&self) -> (u16, u16);
    /// Local scrollback viewport metrics, if the terminal tracks them.
    fn scroll_metrics(&self) -> Option<ScrollMetrics>;

    /// Whether the terminal is on its alternate screen (a full-screen app).
    /// Drives scroll routing (see [`crate::client::scroll`]).
    fn is_alt_screen(&self) -> bool {
        self.input_state()
            .is_some_and(|state| state.alternate_screen)
    }

    // --- copy-mode / selection / search (read-only) ---
    /// Plain text of the current viewport.
    fn visible_text(&self) -> String;
    /// Hyperlinks visible in `area` as `((col, row), id, uri)`.
    fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)>;
    /// Extracts the text covered by `selection`, if any.
    fn extract_selection(&self, selection: &Selection) -> Option<String>;
    /// Finds matches for `query` across the terminal's text.
    fn search_text_matches(&self, query: &str, case_sensitive: bool) -> Vec<TerminalTextMatch>;
    /// Reports, per match, whether it still points at current content.
    fn text_matches_are_current(&self, text_matches: &[TerminalTextMatch]) -> Vec<bool>;
    /// Resolves the target of a copy-mode word motion from `(row, col)`.
    fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint>;

    // --- local viewport scroll (no network) ---
    /// Scrolls the local viewport up (into scrollback) by `lines`.
    fn scroll_up(&self, lines: usize);
    /// Scrolls the local viewport down toward the live edge by `lines`.
    fn scroll_down(&self, lines: usize);
    /// Returns the viewport to the live bottom of the stream.
    fn scroll_reset(&self);
    /// Positions the viewport `lines` above the live bottom.
    fn set_scroll_offset_from_bottom(&self, lines: usize);

    // --- soft metadata (used by chrome; may be None on a mirror) ---
    /// The pane's reported working directory, if known.
    fn cwd(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// The foreground process's working directory, if known.
    fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Server-owned PTY pane. Each method calls the identically-named inherent
/// method on [`TerminalRuntime`] (inherent methods take precedence over trait
/// methods in call syntax, so this is delegation, not recursion).
impl PaneView for TerminalRuntime {
    fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.render(frame, area, show_cursor);
    }

    fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState> {
        self.cursor_state(area, show_cursor)
    }

    fn input_state(&self) -> Option<InputState> {
        self.input_state()
    }

    fn wheel_routing(&self) -> Option<WheelRouting> {
        self.wheel_routing()
    }

    fn synchronized_output_active(&self) -> bool {
        self.synchronized_output_active()
    }

    fn current_size(&self) -> (u16, u16) {
        self.current_size()
    }

    fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.scroll_metrics()
    }

    fn visible_text(&self) -> String {
        self.visible_text()
    }

    fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.visible_hyperlinks(area)
    }

    fn extract_selection(&self, selection: &Selection) -> Option<String> {
        self.extract_selection(selection)
    }

    fn search_text_matches(&self, query: &str, case_sensitive: bool) -> Vec<TerminalTextMatch> {
        self.search_text_matches(query, case_sensitive)
    }

    fn text_matches_are_current(&self, text_matches: &[TerminalTextMatch]) -> Vec<bool> {
        self.text_matches_are_current(text_matches)
    }

    fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        self.word_motion_target(row, col, motion)
    }

    fn scroll_up(&self, lines: usize) {
        self.scroll_up(lines);
    }

    fn scroll_down(&self, lines: usize) {
        self.scroll_down(lines);
    }

    fn scroll_reset(&self) {
        self.scroll_reset();
    }

    fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.set_scroll_offset_from_bottom(lines);
    }

    fn cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd()
    }

    fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        self.foreground_cwd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiles only if `P: PaneView`; a static assertion that a type satisfies
    /// the contract.
    fn assert_pane_view<P: PaneView>(_pane: &P) {}

    /// Exercise a couple of trait methods generically so the shared contract is
    /// covered independent of any concrete backend.
    fn read_via_trait<P: PaneView>(pane: &P) -> (String, bool) {
        (pane.visible_text(), pane.is_alt_screen())
    }

    #[tokio::test]
    async fn terminal_runtime_is_a_pane_view() {
        let runtime = TerminalRuntime::test_with_screen_bytes(20, 3, b"hello pane view");
        assert_pane_view(&runtime);
        let (text, alt) = read_via_trait(&runtime);
        assert!(text.contains("hello pane view"));
        assert!(!alt);
        // current_size is (rows, cols).
        assert_eq!(PaneView::current_size(&runtime), (3, 20));
    }
}
