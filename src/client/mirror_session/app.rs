//! The `MirrorApp` driver: a PTY-free, client-side sibling of `App::run`.
//!
//! It renders the existing pure TUI over `(&AppState, &TerminalRuntimeRegistry)`
//! (`design-mirror-tui.md` §3.2) where the registry is populated with
//! [`crate::terminal::TerminalRuntime::mirror`] runtimes fed by the data plane.
//! Its event loop is built on the shared [`ClientEventPump`] and selects over
//! local stdin/resize, per-terminal mirror data, and JSON API structural events.
//!
//! Input is classified per §3.4: view-local actions mutate the local replica
//! (zero round-trips), shell keystrokes forward to the focused writable mirror
//! connection, and structural mutations go to the server over the JSON API (the
//! server stays authoritative; the replica updates from the resulting events).

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;
use tracing::warn;

use crate::api::client::ApiClientError;
use crate::api::schema::{Method, PaneTarget};
use crate::app::{
    prefix_indexed_navigation_action, prefix_non_indexed_navigation_action,
    terminal_direct_indexed_navigation_action, terminal_direct_non_indexed_navigation_action,
    ActionContext, App, Mode, NavigateAction,
};
use crate::client::io_pump::{ClientEventPump, ClientLoopHandler, LoopEnd};
use crate::client::{ClientError, ClientLoopEvent, TerminalGuard};
use crate::input::TerminalKey;
use crate::layout::{PaneId, PaneInfo};
use crate::pane::{MirrorRuntime, WheelRouting};
use crate::protocol::{AttachScrollSource, ServerMessage};
use crate::raw_input::{parse_raw_input_bytes_sync, RawInputEvent};
use crate::terminal::{TerminalId, TerminalRuntime};

use super::action::{JsonApiSink, MirrorActionSink};
use super::control::{JsonApiClient, JsonApiError};
use super::projection::{rebuild_app_state, MirrorTabChannels};
use super::replica::{ReplicaChange, SessionReplica};
use super::MirrorConnectionManager;

/// The full multi-pane mirror session driver.
pub(crate) struct MirrorApp {
    /// Config-populated app shell: its `state` is the replica we render and its
    /// `terminal_runtimes` registry holds the mirror runtimes.
    app: App,
    control: JsonApiClient,
    data: MirrorConnectionManager,
    replica: SessionReplica,
    channels: MirrorTabChannels,
    terminal: DefaultTerminal,
    /// Restores terminal modes on drop.
    _guard: TerminalGuard,
    client_size: (u16, u16),
    /// The last `(terminal_id, cols, rows)` we drove the focused pane's PTY to,
    /// to avoid re-sending an identical resize while the echo is in flight.
    last_forwarded_pane_size: Option<(String, u16, u16)>,
    mouse_scroll_lines: usize,
    needs_redraw: bool,
}

impl MirrorApp {
    /// Assembles the driver: opens a data connection per terminal, wires each
    /// into the render registry, and projects the replica into the app state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut app: App,
        control: JsonApiClient,
        mut data: MirrorConnectionManager,
        replica: SessionReplica,
        pump: &ClientEventPump,
        terminal: DefaultTerminal,
        guard: TerminalGuard,
        cols: u16,
        rows: u16,
        mouse_scroll_lines: usize,
    ) -> Result<Self, ClientError> {
        let channels = MirrorTabChannels {
            events: app.event_tx.clone(),
            render_notify: app.render_notify.clone(),
            render_dirty: app.render_dirty.clone(),
        };

        // Data plane: one connection per terminal, writable only for the focused
        // one; then bridge each shared emulator into the render registry.
        let terminal_ids = replica.terminal_ids();
        data.open_all(pump, &terminal_ids, replica.focused_terminal_id())?;
        for terminal_id in &terminal_ids {
            if let Some(runtime) = data.runtime(terminal_id) {
                register_runtime(&mut app, terminal_id, runtime);
            }
        }

        // Control plane: project the replica structure into the real AppState.
        rebuild_app_state(
            &mut app.state,
            &control,
            &replica,
            &channels,
            /* preserve_overlay_mode = */ false,
        );

        // Reuse the server app's exact effect logic: enable the capture seam so
        // any structural mutation the shared input pipeline issues is recorded
        // (and later forwarded to the authoritative server) instead of being
        // applied to this PTY-less replica.
        app.captured_runtime_mutations = Some(Vec::new());

        Ok(Self {
            app,
            control,
            data,
            replica,
            channels,
            terminal,
            _guard: guard,
            client_size: (cols, rows),
            last_forwarded_pane_size: None,
            mouse_scroll_lines,
            needs_redraw: true,
        })
    }

    /// Advances the agent-panel spinner on the shared 100ms client timer, matching
    /// the server TUI's animation gating: only when a pane is `Working` (so an idle
    /// mirror never repaints) and only then requesting a redraw. Without this the
    /// projected Working state would render as a frozen spinner. The client pump's
    /// tick is coarser than the server's 16ms `ANIMATION_INTERVAL`, so the spinner
    /// animates a little slower, but it is no longer static.
    fn tick_animation(&mut self) {
        let has_working = self
            .app
            .state
            .workspaces
            .iter()
            .any(|ws| ws.has_working_pane(&self.app.state.terminals));
        if has_working {
            self.app.state.spinner_tick = self.app.state.spinner_tick.wrapping_add(1);
            self.needs_redraw = true;
        }
    }

    fn state(&self) -> &crate::app::AppState {
        &self.app.state
    }

    fn state_mut(&mut self) -> &mut crate::app::AppState {
        &mut self.app.state
    }

    /// Draws one frame using the existing pure render path. Uses
    /// `compute_view_without_resizing_panes` so the shared mirror runtimes are
    /// never resized locally (their size follows the replicated stream, §3.3).
    fn draw(&mut self) -> Result<(), ClientError> {
        let state = &mut self.app.state;
        let registry = &self.app.terminal_runtimes;
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                crate::ui::compute_view_without_resizing_panes(state, registry, area);
                crate::ui::render_with_runtime_registry(state, registry, frame);
            })
            .map_err(ClientError::ConnectionLost)?;
        self.sync_focused_pane_size();
        Ok(())
    }

    /// Drives the focused pane's PTY to the size the mirror renders it at, so the
    /// remote shell reflows to match this client's layout (`design-mirror-tui.md`
    /// §3.3). Called after each [`Self::draw`], once the layout is recomputed.
    ///
    /// The forwarded size is the focused pane's *content rect* (`inner_rect`), not
    /// the whole client viewport — in a multi-pane layout the focused pane is only
    /// part of the screen. When the focused emulator already reports that size
    /// (this client's layout matches the server's, the common case) no round-trip
    /// is made, so parity costs nothing. Runs only for the focused writable
    /// connection; read-only panes follow the server's size.
    fn sync_focused_pane_size(&mut self) {
        let Some(info) = self
            .app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.is_focused)
        else {
            return;
        };
        let cols = info.inner_rect.width.max(1);
        let rows = info.inner_rect.height.max(1);
        // `current_size` is `(rows, cols)`. If the emulator is already this size,
        // the server PTY matches and nothing needs forwarding.
        if let Some(runtime) = self.focused_runtime() {
            if runtime.current_size() == (rows, cols) {
                self.last_forwarded_pane_size = None;
                return;
            }
        }
        let Some(focused) = self.data.focused().map(str::to_owned) else {
            return;
        };
        // Skip re-sending an identical resize while the echo is still in flight.
        if self.last_forwarded_pane_size.as_ref() == Some(&(focused.clone(), cols, rows)) {
            return;
        }
        if let Err(err) = self.data.forward_resize(cols, rows) {
            warn!(err = %err, "mirror focused-pane resize forward failed");
            return;
        }
        self.last_forwarded_pane_size = Some((focused, cols, rows));
    }

    /// The focused terminal's local emulator, if any.
    fn focused_runtime(&self) -> Option<Arc<MirrorRuntime>> {
        self.data
            .focused()
            .and_then(|terminal_id| self.data.runtime(terminal_id))
    }

    fn focused_input_state(&self) -> Option<crate::pane::InputState> {
        self.focused_runtime().and_then(|rt| rt.input_state())
    }

    // --- input routing (§3.4) -------------------------------------------------

    fn handle_stdin(&mut self, data: Vec<u8>) -> Result<(), ClientError> {
        for event in parse_raw_input_bytes_sync(&data) {
            match event {
                RawInputEvent::Key(key) => self.handle_key(key)?,
                RawInputEvent::Mouse(mouse) => self.handle_mouse(mouse)?,
                RawInputEvent::Paste(text) => self.forward_paste(text)?,
                // Focus/host-color events: nothing to forward, but a redraw keeps
                // the view fresh (e.g. cursor visibility on focus change).
                _ => self.needs_redraw = true,
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: TerminalKey) -> Result<(), ClientError> {
        // Route by the current interaction mode, mirroring the server app's
        // mode-aware dispatch (`src/app/input/mod.rs`). The prefix menu and the
        // other command overlays are driven by `state.mode`, so the shared render
        // path shows them exactly as the normal TUI does.
        match self.state().mode {
            Mode::Prefix => return self.handle_prefix_key(key),
            Mode::Terminal => {}
            _ => return self.handle_overlay_key(key),
        }

        if self.state().is_prefix_key(key) {
            self.arm_prefix();
            return Ok(());
        }

        if key.as_key_event().kind == KeyEventKind::Release || is_modifier_only_key(&key) {
            return Ok(());
        }

        // Direct (non-prefix) keybindings behave like the server app.
        if let Some(action) = terminal_direct_non_indexed_navigation_action(self.state(), key)
            .or_else(|| terminal_direct_indexed_navigation_action(self.state(), key))
        {
            self.dispatch_action(action, ActionContext::Direct);
            return Ok(());
        }

        // Page keys drive local scrollback only when the focused pane looks like a
        // shell transcript; otherwise the child application owns them.
        let key_event = key.as_key_event();
        if matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
            && key_event.modifiers.is_empty()
        {
            let source = AttachScrollSource::PageKey { input: Vec::new() };
            if matches!(
                crate::client::scroll::scroll_disposition(&source, self.focused_input_state()),
                crate::client::scroll::ScrollDisposition::Local
            ) {
                let up = key_event.code == KeyCode::PageUp;
                self.scroll_focused(up, self.focused_pane_scroll_lines());
                return Ok(());
            }
        }

        self.forward_key(key)
    }

    /// Arms the herdr prefix: the next key is a command and the prefix menu
    /// overlay renders (`Mode::Prefix`), exactly as in the server TUI
    /// (`src/app/input/terminal.rs`).
    fn arm_prefix(&mut self) {
        self.state_mut().mode = Mode::Prefix;
        self.needs_redraw = true;
    }

    /// Resolves the key that follows the prefix (`Mode::Prefix`). Dismisses the
    /// prefix menu, then dispatches the bound action (a view-local action may open
    /// its own mode immediately afterwards).
    fn handle_prefix_key(&mut self, key: TerminalKey) -> Result<(), ClientError> {
        // Ignore releases and lone modifier keys so lifting the prefix chord
        // (e.g. releasing Ctrl) does not dismiss the menu before a command key.
        if key.as_key_event().kind == KeyEventKind::Release || is_modifier_only_key(&key) {
            return Ok(());
        }
        self.needs_redraw = true;

        if key.as_key_event().code == KeyCode::Esc {
            self.leave_prefix_menu();
            return Ok(());
        }
        if self.state().is_prefix_key(key) {
            // Double prefix: send a literal prefix key to the focused pane.
            self.leave_prefix_menu();
            return self.forward_key(key);
        }
        if let Some(action) = prefix_non_indexed_navigation_action(self.state(), key)
            .or_else(|| prefix_indexed_navigation_action(self.state(), key))
        {
            // Keep `Mode::Prefix` set while executing: the shared
            // `execute_tui_navigate_action` returns to `Terminal` itself when the
            // action does not open its own mode (via `finish_action_context`),
            // and leaves the new mode in place when it does (e.g. Settings, copy
            // mode) — exactly as the server app's `execute_prefix_key_action`.
            self.dispatch_action(action, ActionContext::Prefix);
        } else {
            // Unrecognized command key: dismiss the menu without leaking to the pane.
            self.leave_prefix_menu();
        }
        Ok(())
    }

    /// Dismisses the prefix menu back to terminal interaction.
    fn leave_prefix_menu(&mut self) {
        self.state_mut().mode = Mode::Terminal;
        self.needs_redraw = true;
    }

    /// Handles a key while a non-terminal interaction mode is active (copy mode,
    /// navigate/workspace picker, settings, help, the navigator, rename/worktree/
    /// confirm modals, resize, context menus). Delegates to the server app's exact
    /// per-mode handlers via [`App::handle_non_terminal_key`], so these modes
    /// behave identically to the normal TUI. Structural effects (e.g. a rename or
    /// resize committed from a modal) are captured and forwarded to the server.
    fn handle_overlay_key(&mut self, key: TerminalKey) -> Result<(), ClientError> {
        self.app.handle_non_terminal_key(key);
        self.forward_captured_mutations();
        self.needs_redraw = true;
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<(), ClientError> {
        let Some(info) = self.pane_info_at(mouse.column, mouse.row, true) else {
            return self.forward_focused_reported_mouse(mouse);
        };
        let Some((pane_id, terminal_id)) = self.public_and_terminal_for_local_pane(info.id) else {
            return Ok(());
        };

        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::Down(MouseButton::Left | MouseButton::Middle)
        ) {
            self.focus_pane_for_input(pane_id, &terminal_id);
        }

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.handle_wheel(mouse, &info, &terminal_id)
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                self.forward_mouse_wheel(mouse, &info, &terminal_id)
            }
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                if rect_contains(info.inner_rect, mouse.column, mouse.row) {
                    self.forward_mouse_button(mouse, &info, &terminal_id)?;
                }
                Ok(())
            }
            MouseEventKind::Moved => {
                if rect_contains(info.inner_rect, mouse.column, mouse.row) {
                    self.forward_mouse_motion(mouse, &info, &terminal_id)?;
                }
                Ok(())
            }
        }
    }

    fn forward_focused_reported_mouse(&mut self, mouse: MouseEvent) -> Result<(), ClientError> {
        let Some(terminal_id) = self.data.focused().map(str::to_owned) else {
            self.needs_redraw = true;
            return Ok(());
        };
        let Some(runtime) = self.data.runtime(&terminal_id) else {
            return Ok(());
        };
        if !runtime
            .input_state()
            .map(|state| state.mouse_reporting_enabled())
            .unwrap_or(false)
        {
            self.needs_redraw = true;
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(bytes) =
                    runtime.encode_mouse_wheel(mouse.kind, mouse.column, mouse.row, mouse.modifiers)
                {
                    runtime.scroll_reset();
                    self.forward_input_to(&terminal_id, bytes)?;
                }
            }
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                if let Some(bytes) = runtime.encode_mouse_button(
                    mouse.kind,
                    mouse.column,
                    mouse.row,
                    mouse.modifiers,
                ) {
                    runtime.scroll_reset();
                    self.forward_input_to(&terminal_id, bytes)?;
                }
            }
            MouseEventKind::Moved => {
                if let Some(bytes) = runtime.encode_mouse_motion(
                    mouse.kind,
                    mouse.column,
                    mouse.row,
                    mouse.modifiers,
                ) {
                    self.forward_input_to(&terminal_id, bytes)?;
                }
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {}
        }
        Ok(())
    }

    fn handle_wheel(
        &mut self,
        mouse: MouseEvent,
        info: &PaneInfo,
        terminal_id: &str,
    ) -> Result<(), ClientError> {
        let Some(runtime) = self.data.runtime(terminal_id) else {
            return Ok(());
        };
        match runtime.wheel_routing() {
            Some(WheelRouting::MouseReport) => self.forward_mouse_wheel(mouse, info, terminal_id),
            Some(WheelRouting::AlternateScroll) => {
                if let Some(bytes) = runtime.encode_alternate_scroll(mouse.kind) {
                    runtime.scroll_reset();
                    self.forward_input_to(terminal_id, bytes)?;
                }
                Ok(())
            }
            Some(WheelRouting::HostScroll) | None => {
                self.scroll_runtime(
                    &runtime,
                    matches!(mouse.kind, MouseEventKind::ScrollUp),
                    self.mouse_scroll_lines,
                );
                Ok(())
            }
        }
    }

    fn forward_mouse_button(
        &mut self,
        mouse: MouseEvent,
        info: &PaneInfo,
        terminal_id: &str,
    ) -> Result<(), ClientError> {
        let Some(runtime) = self.data.runtime(terminal_id) else {
            return Ok(());
        };
        let column = mouse.column.saturating_sub(info.inner_rect.x);
        let row = mouse.row.saturating_sub(info.inner_rect.y);
        if let Some(bytes) = runtime.encode_mouse_button(mouse.kind, column, row, mouse.modifiers) {
            runtime.scroll_reset();
            self.forward_input_to(terminal_id, bytes)?;
        }
        Ok(())
    }

    fn forward_mouse_motion(
        &mut self,
        mouse: MouseEvent,
        info: &PaneInfo,
        terminal_id: &str,
    ) -> Result<(), ClientError> {
        let Some(runtime) = self.data.runtime(terminal_id) else {
            return Ok(());
        };
        let column = mouse.column.saturating_sub(info.inner_rect.x);
        let row = mouse.row.saturating_sub(info.inner_rect.y);
        if let Some(bytes) = runtime.encode_mouse_motion(mouse.kind, column, row, mouse.modifiers) {
            self.forward_input_to(terminal_id, bytes)?;
        }
        Ok(())
    }

    fn forward_mouse_wheel(
        &mut self,
        mouse: MouseEvent,
        info: &PaneInfo,
        terminal_id: &str,
    ) -> Result<(), ClientError> {
        let Some(runtime) = self.data.runtime(terminal_id) else {
            return Ok(());
        };
        let column = mouse.column.saturating_sub(info.inner_rect.x);
        let row = mouse.row.saturating_sub(info.inner_rect.y);
        if let Some(bytes) = runtime.encode_mouse_wheel(mouse.kind, column, row, mouse.modifiers) {
            runtime.scroll_reset();
            self.forward_input_to(terminal_id, bytes)?;
        }
        Ok(())
    }

    fn scroll_focused(&mut self, up: bool, lines: usize) {
        if let Some(runtime) = self.focused_runtime() {
            self.scroll_runtime(&runtime, up, lines);
        }
    }

    fn scroll_runtime(&mut self, runtime: &MirrorRuntime, up: bool, lines: usize) {
        if up {
            runtime.scroll_up(lines);
        } else {
            runtime.scroll_down(lines);
        }
        self.needs_redraw = true;
    }

    fn focused_pane_scroll_lines(&self) -> usize {
        self.app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.is_focused)
            .map(|info| info.inner_rect.height as usize)
            .unwrap_or(10)
            .max(1)
    }

    fn forward_key(&mut self, key: TerminalKey) -> Result<(), ClientError> {
        let Some(runtime) = self.focused_runtime() else {
            return Ok(());
        };
        let bytes = runtime.encode_terminal_key(key);
        if bytes.is_empty() {
            return Ok(());
        }
        runtime.scroll_reset();
        self.data
            .forward_input(bytes)
            .map_err(ClientError::ConnectionLost)?;
        self.needs_redraw = true;
        Ok(())
    }

    fn forward_paste(&mut self, text: String) -> Result<(), ClientError> {
        // In a non-terminal mode, a paste belongs to the active overlay (rename,
        // navigator/worktree search, copy-mode search), not the focused pane —
        // route it through the shared text-input handler the server app uses.
        if self.state().mode != Mode::Terminal {
            self.app.paste_into_active_text_input(&text);
            self.forward_captured_mutations();
            self.needs_redraw = true;
            return Ok(());
        }
        let Some(runtime) = self.focused_runtime() else {
            return Ok(());
        };
        let data = if runtime
            .input_state()
            .map(|state| state.bracketed_paste)
            .unwrap_or(false)
        {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        runtime.scroll_reset();
        self.data
            .forward_input(data)
            .map_err(ClientError::ConnectionLost)?;
        self.needs_redraw = true;
        Ok(())
    }

    fn forward_input_to(&mut self, terminal_id: &str, data: Vec<u8>) -> Result<(), ClientError> {
        self.data
            .forward_input_to(terminal_id, data)
            .map_err(ClientError::ConnectionLost)?;
        self.needs_redraw = true;
        Ok(())
    }

    fn focus_pane_for_input(&mut self, pane_id: String, terminal_id: &str) {
        // Clicking or scrolling within the already-focused pane must not re-issue
        // a focus handoff or a server mutation: a wheel burst would otherwise
        // flood the control plane with one redundant `changes_ui` pane.focus
        // request per tick. The data-plane `set_focus` already no-ops on an
        // unchanged focus; this guard extends that to the control-plane mutation.
        if self.data.focused() == Some(terminal_id) {
            return;
        }
        if let Err(err) = self.data.set_focus(Some(terminal_id)) {
            warn!(pane_id, terminal_id, err = %err, "mirror writable handoff for mouse target failed");
        }
        let sink = JsonApiSink {
            control: &self.control,
        };
        if let Err(err) = sink.dispatch(
            "tui.mirror.pane.focus",
            Method::PaneFocus(PaneTarget { pane_id }),
        ) {
            warn!(err = %err, "mirror pane focus mutation failed");
        }
    }

    fn pane_info_at(&self, col: u16, row: u16, include_frame: bool) -> Option<PaneInfo> {
        self.app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| {
                let rect = if include_frame {
                    info.rect
                } else {
                    info.inner_rect
                };
                rect_contains(rect, col, row)
            })
            .cloned()
    }

    fn public_and_terminal_for_local_pane(&self, local_id: PaneId) -> Option<(String, String)> {
        let public_id = self
            .state()
            .public_pane_id_aliases
            .iter()
            .find_map(|(public_id, &pane_id)| (pane_id == local_id).then(|| public_id.clone()))?;
        let terminal_id = self.replica.pane(&public_id)?.terminal_id.clone();
        Some((public_id, terminal_id))
    }

    /// Runs a resolved [`NavigateAction`] through the server app's shared effect
    /// logic ([`App::execute_tui_navigate_action`]). View-local actions (copy
    /// mode, help, settings, navigator, resize/workspace-picker entry, sidebar
    /// toggle, rename-modal open, detach) mutate the replica `AppState` directly;
    /// structural actions (new/close/move/focus/split/zoom/reload across pane,
    /// tab, workspace) are captured by the replica `App` and forwarded to the
    /// authoritative server here. Because the effect layer is shared, *every*
    /// action the main TUI supports works in the mirror with no per-action
    /// curation — new keybindings are picked up automatically.
    fn dispatch_action(&mut self, action: NavigateAction, context: ActionContext) {
        self.app.execute_tui_navigate_action(action, context);
        self.forward_captured_mutations();
        self.needs_redraw = true;
    }

    /// Drains the structural mutations the shared input pipeline captured on the
    /// replica `App` and forwards each to the authoritative server over the JSON
    /// API. The replica updates only when the resulting event arrives (server
    /// stays authoritative), matching the mirror's control-plane model.
    fn forward_captured_mutations(&mut self) {
        let methods = match self.app.captured_runtime_mutations.as_mut() {
            Some(buffer) if !buffer.is_empty() => std::mem::take(buffer),
            _ => return,
        };
        let sink = JsonApiSink {
            control: &self.control,
        };
        for method in methods {
            if let Err(err) = sink.dispatch("tui.mirror.action", method) {
                warn!(err = %err, "mirror captured mutation dispatch failed");
            }
        }
        self.needs_redraw = true;
    }

    // --- data plane -----------------------------------------------------------

    fn handle_mirror(
        &mut self,
        pump: &mut ClientEventPump,
        terminal_id: String,
        msg: ServerMessage,
    ) {
        let mut redraw = self.apply_mirror(&terminal_id, msg);
        // Coalesce an available burst of mirror messages into one redraw to avoid
        // flicker during the initial history replay (mirrors the single-pane
        // handler's coalescing).
        while let Some(event) = pump.try_next() {
            match event {
                ClientLoopEvent::ServerMirror { terminal_id, msg } => {
                    redraw |= self.apply_mirror(&terminal_id, msg);
                }
                other => {
                    pump.push_front(other);
                    break;
                }
            }
        }
        if redraw {
            self.needs_redraw = true;
        }
    }

    /// Routes one mirror message to its emulator; returns whether a redraw is due.
    fn apply_mirror(&mut self, terminal_id: &str, msg: ServerMessage) -> bool {
        use super::MirrorApply;
        match self.data.apply(terminal_id, msg) {
            MirrorApply::Applied(outcome) => {
                // Repaint on a reset/resize or close, or whenever this terminal is
                // currently on screen — new output in *any* visible pane (not just
                // the focused one) must repaint, or background panes go stale.
                outcome.needs_full_redraw || outcome.closed || self.is_visible_terminal(terminal_id)
            }
            MirrorApply::NotMirror | MirrorApply::Unknown => false,
        }
    }

    /// Whether `terminal_id` backs a pane currently laid out on screen (the active
    /// tab's visible panes). Panes on other tabs are not visible, so their output
    /// need not force a repaint.
    fn is_visible_terminal(&self, terminal_id: &str) -> bool {
        self.app.state.view.pane_infos.iter().any(|info| {
            self.public_and_terminal_for_local_pane(info.id)
                .is_some_and(|(_, tid)| tid == terminal_id)
        })
    }

    // --- control plane --------------------------------------------------------

    fn handle_control_event(
        &mut self,
        pump: &ClientEventPump,
        envelope: crate::api::schema::EventEnvelope,
    ) {
        let changes = self.replica.apply_event(envelope);
        if changes.is_empty() {
            return;
        }
        let mut structural = false;
        for change in changes {
            match change {
                ReplicaChange::PaneAdded { terminal_id, .. } => {
                    if let Err(err) = self.data.open(pump, &terminal_id, false) {
                        warn!(terminal_id, err = %err, "mirror open for new pane failed");
                    }
                    if let Some(runtime) = self.data.runtime(&terminal_id) {
                        register_runtime(&mut self.app, &terminal_id, runtime);
                    }
                    structural = true;
                }
                ReplicaChange::PaneRemoved { terminal_id, .. } => {
                    self.data.close(&terminal_id);
                    self.app
                        .terminal_runtimes
                        .remove(&TerminalId::from_string(terminal_id));
                    structural = true;
                }
                ReplicaChange::FocusChanged { terminal_id, .. } => {
                    if let Err(err) = self.data.set_focus(terminal_id.as_deref()) {
                        warn!(err = %err, "mirror focus handover failed");
                    }
                    structural = true;
                }
                ReplicaChange::LayoutChanged { .. } | ReplicaChange::Structural => {
                    structural = true;
                }
            }
        }
        if structural {
            self.reproject();
        }
        self.needs_redraw = true;
    }

    /// Rebuilds the projected `AppState` from the current replica and re-syncs the
    /// render registry with the live set of mirror runtimes.
    fn reproject(&mut self) {
        rebuild_app_state(
            &mut self.app.state,
            &self.control,
            &self.replica,
            &self.channels,
            /* preserve_overlay_mode = */ true,
        );
        // Ensure every live terminal has a registry entry (defensive: a pane that
        // appeared without a PaneAdded, e.g. via a moved pane).
        let terminal_ids: Vec<String> = self.data.terminal_ids().cloned().collect();
        for terminal_id in terminal_ids {
            if self
                .app
                .terminal_runtimes
                .get(&TerminalId::from_string(terminal_id.clone()))
                .is_none()
            {
                if let Some(runtime) = self.data.runtime(&terminal_id) {
                    register_runtime(&mut self.app, &terminal_id, runtime);
                }
            }
        }
    }

    fn handle_control_disconnected(&mut self, pump: &ClientEventPump) {
        warn!("mirror control event stream disconnected; rebuilding replica");
        let replica = match self.control.build_replica() {
            Ok(replica) => replica,
            Err(err) => {
                warn!(err = %err, "mirror control-plane resync failed");
                return;
            }
        };

        let desired_terminals: BTreeSet<String> = replica.terminal_ids().into_iter().collect();
        let existing_terminals: Vec<String> = self.data.terminal_ids().cloned().collect();
        for terminal_id in existing_terminals {
            if !desired_terminals.contains(&terminal_id) {
                self.data.close(&terminal_id);
                self.app
                    .terminal_runtimes
                    .remove(&TerminalId::from_string(terminal_id));
            }
        }

        self.replica = replica;
        let focused = self.replica.focused_terminal_id().map(str::to_owned);
        for terminal_id in &desired_terminals {
            if !self.data.is_connected(terminal_id) {
                if let Err(err) = self.data.open(
                    pump,
                    terminal_id,
                    focused.as_deref() == Some(terminal_id.as_str()),
                ) {
                    warn!(terminal_id, err = %err, "mirror open during control resync failed");
                }
            }
            if let Some(runtime) = self.data.runtime(terminal_id) {
                register_runtime(&mut self.app, terminal_id, runtime);
            }
        }
        if let Err(err) = self.data.set_focus(focused.as_deref()) {
            warn!(err = %err, "mirror focus handover after control resync failed");
        }
        self.reproject();
        self.needs_redraw = true;

        match self.control.subscribe(Some(Duration::from_millis(200))) {
            Ok(stream) => spawn_control_reader(pump, stream),
            Err(err) => warn!(err = %err, "mirror control event resubscribe failed"),
        }
    }
}

/// Bridges a shared mirror emulator into the render registry under the server's
/// terminal id (the same id the projected `PaneState` references).
fn register_runtime(app: &mut App, terminal_id: &str, runtime: Arc<MirrorRuntime>) {
    app.terminal_runtimes.insert(
        TerminalId::from_string(terminal_id.to_owned()),
        TerminalRuntime::mirror(runtime),
    );
}

impl ClientLoopHandler for MirrorApp {
    fn on_event(
        &mut self,
        event: ClientLoopEvent,
        pump: &mut ClientEventPump,
    ) -> Result<ControlFlow<()>, ClientError> {
        match event {
            ClientLoopEvent::StdinInput(data) => self.handle_stdin(data)?,
            ClientLoopEvent::Resize(cols, rows, _, _) => {
                self.client_size = (cols, rows);
                // Record the viewport for future emulator opens; the focused
                // pane's PTY is driven per-pane after the redraw recomputes the
                // layout (see `sync_focused_pane_size`).
                self.data.set_viewport(cols, rows);
                self.needs_redraw = true;
            }
            ClientLoopEvent::ServerMirror { terminal_id, msg } => {
                self.handle_mirror(pump, terminal_id, msg);
            }
            ClientLoopEvent::ServerMirrorDisconnected { terminal_id } => {
                if let Err(err) = self.data.reconnect(pump, &terminal_id) {
                    warn!(terminal_id, err = %err, "mirror reconnect failed");
                }
            }
            ClientLoopEvent::ControlEvent(envelope) => self.handle_control_event(pump, *envelope),
            ClientLoopEvent::ControlDisconnected => self.handle_control_disconnected(pump),
            // The mirror holds no untagged server connection; these can't occur.
            ClientLoopEvent::ServerMessage(_) | ClientLoopEvent::ServerDisconnected => {}
            ClientLoopEvent::Timer => self.tick_animation(),
        }

        if self.state().should_quit || self.state().detach_requested {
            return Ok(ControlFlow::Break(()));
        }
        if self.needs_redraw {
            self.draw()?;
            self.needs_redraw = false;
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// Spawns the JSON API structural-event reader, feeding each event into the pump
/// as a [`ClientLoopEvent::ControlEvent`] so the loop reconciles it alongside
/// stdin/resize/mirror data.
fn spawn_control_reader(pump: &ClientEventPump, mut stream: super::control::StructuralEventStream) {
    let sender = pump.event_sender();
    let quit = pump.quit_flag();
    std::thread::spawn(move || {
        while !quit.load(Ordering::Acquire) {
            match stream.next_event() {
                Ok(Some(envelope)) => {
                    if sender
                        .blocking_send(ClientLoopEvent::ControlEvent(Box::new(envelope)))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = sender.blocking_send(ClientLoopEvent::ControlDisconnected);
                    break;
                }
                Err(JsonApiError::Client(ApiClientError::Io(err)))
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    // Idle read timeout; poll again so we can honor `quit`.
                }
                Err(err) => {
                    warn!(err = %err, "mirror control event reader failed");
                    let _ = sender.blocking_send(ClientLoopEvent::ControlDisconnected);
                    break;
                }
            }
        }
    });
}

/// Runs the full mirror TUI session against a running herdr server
/// (`herdr --mirror`). Connects the control plane, discovers the session, opens a
/// data connection per terminal, and drives the render loop until the user
/// detaches or the server goes away.
pub fn run_mirror_session(session: Option<String>, cols: u16, rows: u16) -> std::io::Result<()> {
    // The mirror is dispatched before `main`'s `init_logging`, so set up its own
    // client log (matching the other client modes) for diagnosability.
    crate::logging::init_file_logging("herdr-client.log");
    let control = JsonApiClient::local(session);

    // Fail fast (and legibly) if no server is reachable, before touching the
    // terminal, so `herdr --mirror` with no server exits promptly (acceptance A3).
    let replica = match control.build_replica() {
        Ok(replica) => replica,
        Err(err) => {
            eprintln!(
                "herdr --mirror: could not connect to a herdr server ({err}). Is one running?"
            );
            return Ok(());
        }
    };

    let loaded = crate::config::Config::load();
    let config_diagnostic = crate::config::config_diagnostic_summary(&loaded.diagnostics);
    let mouse_capture = loaded.config.ui.mouse_capture;
    let local_scrollback_bytes = loaded.config.mirror.local_scrollback_bytes;
    let mouse_scroll_lines = loaded.config.ui.mouse_scroll_lines();
    let wire_socket = crate::server::socket_paths::client_socket_path();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        // The mirror never services this channel; it exists only to satisfy
        // `App::new`, which builds the config-populated (PTY-free) replica state.
        drop(api_tx);
        let app = App::new(
            &loaded.config,
            /* no_session = */ true,
            config_diagnostic,
            api_rx,
            crate::api::EventHub::default(),
        );

        let should_quit = Arc::new(AtomicBool::new(false));
        let mut pump = ClientEventPump::new(should_quit);
        let mouse_active = Arc::new(AtomicBool::new(mouse_capture));
        pump.spawn_stdin(false, mouse_active);
        pump.spawn_resize(cols, rows, false);

        // Control-plane structural event feed.
        match control.subscribe(Some(Duration::from_millis(200))) {
            Ok(stream) => spawn_control_reader(&pump, stream),
            Err(err) => warn!(err = %err, "mirror control event subscription failed"),
        }

        let data = MirrorConnectionManager::new(wire_socket, cols, rows, local_scrollback_bytes);

        let (terminal, guard) = crate::client::setup_mirror_terminal(mouse_capture)?;

        let mut mirror = MirrorApp::new(
            app,
            control,
            data,
            replica,
            &pump,
            terminal,
            guard,
            cols,
            rows,
            mouse_scroll_lines,
        )
        .map_err(client_error_to_io)?;

        mirror.draw().map_err(client_error_to_io)?;

        let result = pump.run(&mut mirror).await;
        // Drop the driver (and its terminal guard) before reporting so the
        // terminal is restored even on error.
        drop(mirror);
        match result {
            Ok(LoopEnd::Quit | LoopEnd::Handler) => Ok(()),
            Err(err) => Err(client_error_to_io(err)),
        }
    })
}

fn client_error_to_io(err: ClientError) -> std::io::Error {
    std::io::Error::other(format!("{err}"))
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn is_modifier_only_key(key: &TerminalKey) -> bool {
    matches!(key.as_key_event().code, KeyCode::Modifier(_))
}
