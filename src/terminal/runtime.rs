use std::sync::{atomic::AtomicBool, Arc};

use bytes::Bytes;
use ratatui::{layout::Rect, Frame};
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;

/// Live runtime for a pane's terminal.
///
/// A terminal is either a server-owned PTY ([`Self::Pty`]) or a client-side
/// local mirror replica ([`Self::Mirror`]). The whole TUI depends on this single
/// terminal-layer type; the read/render/viewport surface (see
/// [`crate::terminal::PaneView`]) works uniformly across both variants, while
/// server-only lifecycle operations (`spawn*`, PTY `resize`, `output_log`,
/// handoff, byte sending) are meaningless for a read-only mirror and therefore
/// no-op / return `None` on it, guarded by a `debug_assert!` so a stray call is
/// caught in development (`design-mirror-tui.md` §1c).
pub enum TerminalRuntime {
    /// A server-owned PTY terminal backed by the legacy pane runtime.
    Pty(crate::pane::PaneRuntime),
    /// A client-side local mirror replica, driven by a replicated mirror stream.
    ///
    /// Constructed via [`Self::mirror`] by the mirror connection manager
    /// (`design-mirror-tui.md` §6, Phase 3), which owns the same
    /// [`crate::pane::MirrorRuntime`] to feed it replicated bytes while this
    /// registry entry renders it — hence the shared `Arc` rather than a `Box`. The
    /// `Arc` is a single pointer, so the common [`Self::Pty`] variant stays small.
    ///
    /// Built via [`Self::mirror`] by the mirror app driver (Phase 4); until that
    /// wires the connection manager's emulators into the registry it is only
    /// matched, never constructed outside tests.
    #[cfg(unix)]
    #[allow(dead_code)]
    Mirror(Arc<crate::pane::MirrorRuntime>),
}

impl TerminalRuntime {
    /// Builds a mirror-backed runtime sharing `runtime` with the data-plane
    /// connection manager that feeds it (`design-mirror-tui.md` §2.1, Phase 3).
    ///
    /// The manager keeps its own `Arc` clone so it can apply replicated snapshots
    /// and events into the same local emulator this registry entry renders. Called
    /// by the mirror app driver (Phase 4); unused until then.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn mirror(runtime: Arc<crate::pane::MirrorRuntime>) -> Self {
        Self::Mirror(runtime)
    }

    pub fn shutdown(self) {
        match self {
            Self::Pty(rt) => rt.shutdown(),
            // Dropping the mirror tears down its local emulator; nothing else to do.
            #[cfg(unix)]
            Self::Mirror(_) => {}
        }
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        match self {
            Self::Pty(rt) => rt.duplicate_handoff_fd(),
            Self::Mirror(_) => {
                debug_assert!(false, "duplicate_handoff_fd called on a mirror runtime");
                Err(std::io::Error::other("mirror runtime has no handoff fd"))
            }
        }
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(self) {
        match self {
            Self::Pty(rt) => rt.preserve_for_handoff(),
            Self::Mirror(_) => {
                debug_assert!(false, "preserve_for_handoff called on a mirror runtime")
            }
        }
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        match self {
            Self::Pty(rt) => rt.assume_handoff_ownership(),
            Self::Mirror(_) => {
                debug_assert!(false, "assume_handoff_ownership called on a mirror runtime")
            }
        }
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        match self {
            Self::Pty(rt) => rt.set_handoff_reader_paused(paused),
            Self::Mirror(_) => {
                debug_assert!(
                    false,
                    "set_handoff_reader_paused called on a mirror runtime"
                )
            }
        }
    }

    #[cfg(unix)]
    pub fn pause_handoff_reader(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        match self {
            Self::Pty(rt) => rt.pause_handoff_reader(timeout),
            Self::Mirror(_) => {
                debug_assert!(false, "pause_handoff_reader called on a mirror runtime");
                Ok(())
            }
        }
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        match self {
            Self::Pty(rt) => rt.handoff_runtime_state(pane_id),
            Self::Mirror(_) => {
                debug_assert!(false, "handoff_runtime_state called on a mirror runtime");
                crate::handoff_runtime::HandoffRuntimeState {
                    pane_id,
                    child_pid: 0,
                    rows: 0,
                    cols: 0,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    keyboard_protocol_flags: 0,
                    keyboard_protocol_ansi: None,
                    input_state: None,
                    terminal_title: None,
                    initial_history_ansi: None,
                }
            }
        }
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        match self {
            Self::Pty(rt) => rt.handoff_history_ansi(),
            Self::Mirror(_) => {
                debug_assert!(false, "handoff_history_ansi called on a mirror runtime");
                None
            }
        }
    }

    #[cfg(unix)]
    pub fn from_handoff_fd(
        import: crate::handoff_runtime::ImportedHandoffRuntime,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::from_handoff_fd(
            import,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            initial_history_ansi,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        command: &str,
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_shell_command(
            pane_id,
            rows,
            cols,
            cwd,
            command,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments, including detection policy.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd,
            argv,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        match self {
            Self::Pty(rt) => rt.apply_host_terminal_theme(theme),
            // A mirror inherits theming through the replicated stream, not locally.
            #[cfg(unix)]
            Self::Mirror(_) => {}
        }
    }

    pub fn begin_graceful_release(&self, agent: crate::detect::Agent) {
        match self {
            Self::Pty(rt) => rt.begin_graceful_release(agent),
            // Agent lifecycle is server-authoritative; a mirror has nothing to release.
            #[cfg(unix)]
            Self::Mirror(_) => {}
        }
    }

    pub fn reset_agent_detection(&self) {
        match self {
            Self::Pty(rt) => rt.reset_agent_detection(),
            // Agent detection is server-authoritative on a mirror.
            #[cfg(unix)]
            Self::Mirror(_) => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_reset_notify_for_test(
        &self,
    ) -> std::sync::Arc<tokio::sync::Notify> {
        match self {
            Self::Pty(rt) => rt.agent_detection_reset_notify_for_test(),
            #[cfg(unix)]
            Self::Mirror(_) => std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_enabled_for_test(&self) -> bool {
        match self {
            Self::Pty(rt) => rt.agent_detection_enabled_for_test(),
            #[cfg(unix)]
            Self::Mirror(_) => false,
        }
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        match self {
            Self::Pty(rt) => rt.set_full_lifecycle_authority_active(active),
            // Lifecycle authority is a server-side concept.
            #[cfg(unix)]
            Self::Mirror(_) => {}
        }
    }

    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        match self {
            Self::Pty(rt) => rt.resize(rows, cols, cell_width_px, cell_height_px),
            #[cfg(unix)]
            Self::Mirror(_) => {
                // The mirror's size follows the replicated stream (a `Resize` event);
                // resizing it locally would desync it from the server.
                debug_assert!(false, "PTY resize called on a mirror runtime");
            }
        }
    }

    /// Shared handle to this terminal's raw output replication log, used to
    /// stream the responsive local mirror.
    pub fn output_log(&self) -> std::sync::Arc<crate::terminal::MirrorLog> {
        match self {
            Self::Pty(rt) => rt.output_log(),
            #[cfg(unix)]
            Self::Mirror(m) => {
                // A mirror is a consumer of a replication log, not a producer.
                debug_assert!(false, "output_log called on a mirror runtime");
                let (rows, cols) = m.current_size();
                std::sync::Arc::new(crate::terminal::MirrorLog::new(cols, rows))
            }
        }
    }

    #[cfg(unix)]
    pub fn nudge_child_redraw_after_handoff(&self) {
        match self {
            Self::Pty(rt) => rt.nudge_child_redraw_after_handoff(),
            Self::Mirror(_) => {
                debug_assert!(
                    false,
                    "nudge_child_redraw_after_handoff called on a mirror runtime"
                )
            }
        }
    }

    pub fn scroll_up(&self, lines: usize) {
        match self {
            Self::Pty(rt) => rt.scroll_up(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.scroll_up(lines),
        }
    }

    pub fn scroll_down(&self, lines: usize) {
        match self {
            Self::Pty(rt) => rt.scroll_down(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.scroll_down(lines),
        }
    }

    pub fn scroll_reset(&self) {
        match self {
            Self::Pty(rt) => rt.scroll_reset(),
            #[cfg(unix)]
            Self::Mirror(m) => m.scroll_reset(),
        }
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        match self {
            Self::Pty(rt) => rt.set_scroll_offset_from_bottom(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.set_scroll_offset_from_bottom(lines),
        }
    }

    pub fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        match self {
            Self::Pty(rt) => rt.scroll_metrics(),
            #[cfg(unix)]
            Self::Mirror(m) => m.scroll_metrics(),
        }
    }

    pub(crate) fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        match self {
            Self::Pty(rt) => rt.search_text_matches(query, case_sensitive),
            #[cfg(unix)]
            Self::Mirror(m) => m.search_text_matches(query, case_sensitive),
        }
    }

    pub(crate) fn text_match_is_current(&self, text_match: crate::pane::TerminalTextMatch) -> bool {
        match self {
            Self::Pty(rt) => rt.text_match_is_current(text_match),
            #[cfg(unix)]
            Self::Mirror(m) => m.text_match_is_current(text_match),
        }
    }

    pub(crate) fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        match self {
            Self::Pty(rt) => rt.text_matches_are_current(text_matches),
            #[cfg(unix)]
            Self::Mirror(m) => m.text_matches_are_current(text_matches),
        }
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        match self {
            Self::Pty(rt) => rt.word_motion_target(row, col, motion),
            #[cfg(unix)]
            Self::Mirror(m) => m.word_motion_target(row, col, motion),
        }
    }

    pub fn input_state(&self) -> Option<crate::pane::InputState> {
        match self {
            Self::Pty(rt) => rt.input_state(),
            #[cfg(unix)]
            Self::Mirror(m) => m.input_state(),
        }
    }

    pub fn cursor_state(
        &self,
        area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        match self {
            Self::Pty(rt) => rt.cursor_state(area, show_cursor),
            #[cfg(unix)]
            Self::Mirror(m) => m.cursor_state(area, show_cursor),
        }
    }

    pub fn synchronized_output_active(&self) -> bool {
        match self {
            Self::Pty(rt) => rt.synchronized_output_active(),
            #[cfg(unix)]
            Self::Mirror(m) => m.synchronized_output_active(),
        }
    }

    pub fn visible_text(&self) -> String {
        match self {
            Self::Pty(rt) => rt.visible_text(),
            #[cfg(unix)]
            Self::Mirror(m) => m.visible_text(),
        }
    }

    pub fn visible_ansi(&self) -> String {
        match self {
            Self::Pty(rt) => rt.visible_ansi(),
            #[cfg(unix)]
            Self::Mirror(m) => m.visible_ansi(),
        }
    }

    pub fn detection_text(&self) -> String {
        match self {
            Self::Pty(rt) => rt.detection_text(),
            #[cfg(unix)]
            Self::Mirror(m) => m.detection_text(),
        }
    }

    pub fn terminal_title(&self) -> Option<String> {
        match self {
            Self::Pty(rt) => rt.terminal_title(),
            #[cfg(unix)]
            Self::Mirror(_) => None,
        }
    }

    pub fn agent_osc_title(&self) -> String {
        match self {
            Self::Pty(rt) => rt.agent_osc_title(),
            #[cfg(unix)]
            Self::Mirror(m) => m.agent_osc_title(),
        }
    }

    pub fn agent_osc_progress(&self) -> String {
        match self {
            Self::Pty(rt) => rt.agent_osc_progress(),
            #[cfg(unix)]
            Self::Mirror(m) => m.agent_osc_progress(),
        }
    }

    pub fn recent_text(&self, lines: usize) -> String {
        match self {
            Self::Pty(rt) => rt.recent_text(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.recent_text(lines),
        }
    }

    pub fn recent_ansi(&self, lines: usize) -> String {
        match self {
            Self::Pty(rt) => rt.recent_ansi(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.recent_ansi(lines),
        }
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        match self {
            Self::Pty(rt) => rt.recent_unwrapped_text(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.recent_unwrapped_text(lines),
        }
    }

    pub fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        match self {
            Self::Pty(rt) => rt.recent_unwrapped_ansi(lines),
            #[cfg(unix)]
            Self::Mirror(m) => m.recent_unwrapped_ansi(lines),
        }
    }

    pub fn snapshot_history(&self) -> Option<String> {
        match self {
            Self::Pty(rt) => rt.snapshot_history(),
            // History snapshots drive server-side restore/handoff, not a mirror.
            #[cfg(unix)]
            Self::Mirror(_) => None,
        }
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        match self {
            Self::Pty(rt) => rt.extract_selection(selection),
            #[cfg(unix)]
            Self::Mirror(m) => m.extract_selection(selection),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        match self {
            Self::Pty(rt) => rt.render(frame, area, show_cursor),
            #[cfg(unix)]
            Self::Mirror(m) => m.render(frame, area, show_cursor),
        }
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> crate::pane::TerminalDirtyPatchOutcome {
        match self {
            Self::Pty(rt) => rt.collect_dirty_patch(area_width, area_height),
            #[cfg(unix)]
            Self::Mirror(_) => {
                // Dirty-patch collection feeds the server's mirror-streaming path; a
                // mirror never streams to sub-clients. Fall back to a full redraw.
                debug_assert!(false, "collect_dirty_patch called on a mirror runtime");
                crate::pane::TerminalDirtyPatchOutcome::Fallback
            }
        }
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        match self {
            Self::Pty(rt) => rt.visible_hyperlinks(area),
            #[cfg(unix)]
            Self::Mirror(m) => m.visible_hyperlinks(area),
        }
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        match self {
            Self::Pty(rt) => rt.kitty_image_placements_with_data_filter(needs_data),
            #[cfg(unix)]
            Self::Mirror(m) => m.kitty_image_placements_with_data_filter(needs_data),
        }
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        match self {
            Self::Pty(rt) => rt.keyboard_protocol(),
            #[cfg(unix)]
            Self::Mirror(m) => m.keyboard_protocol(),
        }
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        match self {
            Self::Pty(rt) => rt.encode_terminal_key(key),
            #[cfg(unix)]
            Self::Mirror(m) => m.encode_terminal_key(key),
        }
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            Self::Pty(rt) => rt.send_bytes(bytes).await,
            #[cfg(unix)]
            Self::Mirror(_) => {
                // Mirror keystrokes are forwarded over the network by the app driver,
                // not written to a local PTY.
                debug_assert!(false, "send_bytes called on a mirror runtime");
                Ok(())
            }
        }
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            Self::Pty(rt) => rt.try_send_bytes(bytes),
            #[cfg(unix)]
            Self::Mirror(_) => {
                debug_assert!(false, "try_send_bytes called on a mirror runtime");
                Ok(())
            }
        }
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            Self::Pty(rt) => rt.send_paste(text).await,
            #[cfg(unix)]
            Self::Mirror(_) => {
                debug_assert!(false, "send_paste called on a mirror runtime");
                Ok(())
            }
        }
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            Self::Pty(rt) => rt.try_send_paste(text),
            #[cfg(unix)]
            Self::Mirror(_) => {
                debug_assert!(false, "try_send_paste called on a mirror runtime");
                Ok(())
            }
        }
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        match self {
            Self::Pty(rt) => rt.try_send_focus_event(event),
            #[cfg(unix)]
            Self::Mirror(_) => {
                debug_assert!(false, "try_send_focus_event called on a mirror runtime");
                false
            }
        }
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        match self {
            Self::Pty(rt) => rt.wheel_routing(),
            #[cfg(unix)]
            Self::Mirror(m) => m.wheel_routing(),
        }
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        match self {
            Self::Pty(rt) => rt.encode_mouse_button(kind, column, row, modifiers),
            #[cfg(unix)]
            Self::Mirror(m) => m.encode_mouse_button(kind, column, row, modifiers),
        }
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        match self {
            Self::Pty(rt) => rt.encode_mouse_motion(kind, column, row, modifiers),
            #[cfg(unix)]
            Self::Mirror(m) => m.encode_mouse_motion(kind, column, row, modifiers),
        }
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        match self {
            Self::Pty(rt) => rt.encode_mouse_wheel(kind, column, row, modifiers),
            #[cfg(unix)]
            Self::Mirror(m) => m.encode_mouse_wheel(kind, column, row, modifiers),
        }
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        match self {
            Self::Pty(rt) => rt.encode_alternate_scroll(kind),
            #[cfg(unix)]
            Self::Mirror(m) => m.encode_alternate_scroll(kind),
        }
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::Pty(rt) => rt.cwd(),
            // A read-only mirror has no local PTY cwd.
            #[cfg(unix)]
            Self::Mirror(_) => None,
        }
    }

    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::Pty(rt) => rt.foreground_cwd(),
            #[cfg(unix)]
            Self::Mirror(_) => None,
        }
    }

    pub fn child_pid(&self) -> Option<u32> {
        match self {
            Self::Pty(rt) => rt.child_pid(),
            // A mirror does not own the child process.
            #[cfg(unix)]
            Self::Mirror(_) => None,
        }
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        match self {
            Self::Pty(rt) => rt.current_size(),
            #[cfg(unix)]
            Self::Mirror(m) => m.current_size(),
        }
    }
}

#[cfg(test)]
impl TerminalRuntime {
    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel(cols, rows);
        (Self::Pty(runtime), rx)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) =
            crate::pane::PaneRuntime::test_with_channel_capacity(cols, rows, capacity);
        (Self::Pty(runtime), rx)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::Pty(crate::pane::PaneRuntime::test_with_screen_bytes(
            cols, rows, bytes,
        ))
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        match self {
            Self::Pty(rt) => rt.test_process_pty_bytes(bytes),
            #[cfg(unix)]
            Self::Mirror(_) => {
                debug_assert!(false, "test_process_pty_bytes called on a mirror runtime")
            }
        }
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::Pty(crate::pane::PaneRuntime::test_with_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
        ))
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
            channel_capacity,
        );
        (Self::Pty(runtime), rx)
    }
}

#[cfg(all(test, unix))]
impl TerminalRuntime {
    /// Builds a `Mirror`-variant runtime seeded with `bytes`, for exercising enum
    /// dispatch across the mirror backend.
    pub(crate) fn test_mirror_with_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        let mirror = crate::pane::MirrorRuntime::new(cols, rows)
            .expect("mirror runtime construction should not fail in tests");
        mirror
            .apply_snapshot(0, cols, rows)
            .expect("mirror snapshot should not fail in tests");
        mirror.apply_event(1, crate::protocol::MirrorEventKind::Output(bytes.to_vec()));
        Self::mirror(Arc::new(mirror))
    }
}

#[cfg(all(test, unix))]
mod mirror_dispatch_tests {
    use super::*;
    use crate::terminal::PaneView;

    #[test]
    fn mirror_variant_dispatches_read_surface_through_pane_view() {
        let runtime = TerminalRuntime::test_mirror_with_bytes(20, 4, b"hello mirror");
        // The read/render surface routes to the mirror backend through the enum.
        assert_eq!(PaneView::current_size(&runtime), (4, 20));
        assert!(!PaneView::is_alt_screen(&runtime));
        assert!(PaneView::visible_text(&runtime).contains("hello mirror"));
        // Local scroll flows through the enum to the mirror's viewport, no network.
        PaneView::scroll_up(&runtime, 10);
        PaneView::scroll_reset(&runtime);
    }

    #[test]
    fn mirror_variant_reports_no_pty_metadata() {
        let runtime = TerminalRuntime::test_mirror_with_bytes(20, 4, b"x");
        // Server-only PTY metadata is absent on a read-only mirror.
        assert_eq!(runtime.cwd(), None);
        assert_eq!(runtime.foreground_cwd(), None);
        assert_eq!(runtime.child_pid(), None);
        assert_eq!(runtime.snapshot_history(), None);
    }
}
