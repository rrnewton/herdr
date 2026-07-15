//! Thin client mode — connects to the server's client socket.
//!
//! The client:
//! - Connects to `herdr-client.sock`, sends Hello with terminal size and protocol version
//! - Sets up the real terminal (raw mode, mouse capture, keyboard enhancements)
//! - Receives Frame messages and blits them to the terminal (diff against last frame)
//! - Reads stdin events (keystrokes, mouse, paste) and sends them as ClientMessage::Input
//! - Detects terminal resize and sends ClientMessage::Resize
//! - Restores terminal on exit (normal or error)
//! - Handles ServerShutdown gracefully (clean exit, informative message to stderr)
//! - Handles server unreachable (clear error screen, not blank/hang)
//! - Forwards OSC 52 clipboard writes from server to its own stdout
//! - Displays sound/toast notifications forwarded from server

mod input;
mod io_pump;
// The client-rendered mirror session is Unix-only for now, matching the rest of
// the interactive mirror path (`design-mirror-tui.md` §7).
#[cfg(unix)]
mod mirror_session;
#[cfg(unix)]
mod scroll;

/// Entry point for the full multi-pane mirror TUI (`herdr --mirror`). Unix-only
/// for now, matching the rest of the interactive mirror path.
#[cfg(unix)]
pub use mirror_session::run_mirror_session;

/// Non-Unix fallback: the client-rendered mirror session shares the Unix-only
/// raw-input path, so it is unavailable elsewhere.
#[cfg(not(unix))]
pub fn run_mirror_session(_session: Option<String>, _cols: u16, _rows: u16) -> io::Result<()> {
    eprintln!("herdr --mirror is not supported on this platform yet");
    Ok(())
}

use std::collections::HashSet;
use std::io::{self, BufRead, Write as _};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
#[cfg(unix)]
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
#[cfg(not(windows))]
use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use tracing::{debug, info, warn};

use crate::ipc::LocalStream;
use crate::protocol::render_ansi;
#[cfg(unix)]
use crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD;
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientKeybindings, ClientLaunchMode,
    ClientMessage, NotifyKind, RenderEncoding, ServerMessage, MAX_FRAME_SIZE,
    MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::server::socket_paths::client_socket_path;
use io_pump::{ClientEventPump, ClientLoopHandler, LoopEnd};

static RECEIVED_KITTY_GRAPHICS_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Client state
// ---------------------------------------------------------------------------

struct ClientLoopConfig {
    sound_config: crate::config::SoundConfig,
    mouse_scroll_lines: usize,
    redraw_on_focus_gained: bool,
    host_cursor: crate::config::HostCursorModeConfig,
    kitty_graphics_enabled: bool,
    mouse_capture_active: bool,
    #[cfg(unix)]
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
}

/// State tracking for the thin client.
struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    blit_encoder: render_ansi::BlitEncoder,
    /// Whether host mouse capture is currently active.
    mouse_capture_active: bool,
    /// The terminal size we reported to the server in our last Hello/Resize.
    reported_size: (u16, u16),
    /// Client-local sound playback config, refreshed on server request.
    sound_config: crate::config::SoundConfig,
    /// Whether this client may write Kitty graphics bytes to its host terminal.
    kitty_graphics_enabled: bool,
    /// Direct attach prefix escape state. None for full-app clients.
    attach_escape: Option<AttachEscapeState>,
    /// Rows scrolled for one direct-attach wheel notch.
    #[cfg(unix)]
    mouse_scroll_lines: usize,
    /// Local-client shortcut that sends a clipboard image to a remote Herdr session.
    #[cfg(unix)]
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    /// Whether outer focus gain should force a full host-terminal redraw.
    redraw_on_focus_gained: bool,
    /// Whether this client draws the cursor into frame cells instead of using the host cursor.
    draw_host_cursor: bool,
}

#[derive(Debug, Default)]
#[cfg(windows)]
struct AttachEscapeState;

#[derive(Debug, Default)]
#[cfg(unix)]
struct AttachEscapeState {
    pending_prefix: bool,
}

#[derive(Debug)]
#[cfg(unix)]
enum AttachInputAction {
    Forward(Vec<u8>),
    Scroll {
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    },
    Detach,
    None,
}

impl AttachEscapeState {
    #[cfg(unix)]
    fn filter_input(
        &mut self,
        data: Vec<u8>,
        viewport_rows: u16,
        mouse_scroll_lines: usize,
    ) -> AttachInputAction {
        const PREFIX: u8 = 0x02; // Ctrl+B

        let mut output = Vec::with_capacity(data.len());
        for byte in data {
            if self.pending_prefix {
                self.pending_prefix = false;
                match byte {
                    b'q' => return AttachInputAction::Detach,
                    PREFIX => output.push(PREFIX),
                    other => {
                        output.push(PREFIX);
                        output.push(other);
                    }
                }
                continue;
            }

            if byte == PREFIX {
                self.pending_prefix = true;
            } else {
                output.push(byte);
            }
        }

        if output.is_empty() {
            AttachInputAction::None
        } else if let Some(action) =
            attach_scroll_action(&output, viewport_rows, mouse_scroll_lines)
        {
            action
        } else {
            AttachInputAction::Forward(output)
        }
    }
}

#[cfg(unix)]
fn attach_scroll_action(
    data: &[u8],
    viewport_rows: u16,
    mouse_scroll_lines: usize,
) -> Option<AttachInputAction> {
    let mut events = crate::raw_input::parse_raw_input_bytes_sync(data);
    if events.len() != 1 {
        return None;
    }

    match events.pop()? {
        crate::raw_input::RawInputEvent::Mouse(mouse) => {
            let direction = match mouse.kind {
                MouseEventKind::ScrollUp => AttachScrollDirection::Up,
                MouseEventKind::ScrollDown => AttachScrollDirection::Down,
                _ => return Some(AttachInputAction::None),
            };
            Some(AttachInputAction::Scroll {
                source: AttachScrollSource::Wheel,
                direction,
                lines: mouse_scroll_lines.max(1).min(u16::MAX as usize) as u16,
                column: Some(mouse.column),
                row: Some(mouse.row),
                modifiers: mouse.modifiers.bits(),
            })
        }
        crate::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            let direction = match key.code {
                KeyCode::PageUp => AttachScrollDirection::Up,
                KeyCode::PageDown => AttachScrollDirection::Down,
                _ => return None,
            };
            Some(AttachInputAction::Scroll {
                source: AttachScrollSource::PageKey {
                    input: data.to_vec(),
                },
                direction,
                lines: viewport_rows.saturating_sub(1).max(1),
                column: None,
                row: None,
                modifiers: KeyModifiers::empty().bits(),
            })
        }
        crate::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && key.kind == KeyEventKind::Release
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
        {
            Some(AttachInputAction::None)
        }
        _ => None,
    }
}

impl ClientState {
    fn request_full_redraw(&mut self) {
        self.blit_encoder = render_ansi::BlitEncoder::new();
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during client operation.
#[derive(Debug)]
pub enum ClientError {
    /// Could not connect to the server's client socket.
    ConnectionFailed(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected { version: u32, error: String },
    /// Server shut down.
    ServerShutdown { reason: Option<String> },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(protocol::FramingError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::ConnectionFailed(err) => {
                write!(f, "failed to connect to server: {err}")?;
                let path = client_socket_path();
                write!(
                    f,
                    "\nIs herdr server running? Start it with `herdr server`."
                )?;
                write!(f, "\nSocket path: {}", path.display())
            }
            ClientError::HandshakeRejected { version, error } => {
                write!(f, "server rejected handshake (version {version}): {error}")
            }
            ClientError::ServerShutdown { reason } => {
                match reason.as_deref() {
                    Some("detached") => {
                        if let Ok(reattach_command) =
                            std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                        {
                            write!(f, "detached from remote server")?;
                            write!(f, "\nRun `{reattach_command}` to reattach")?;
                        } else {
                            write!(f, "detached from server")?;
                            write!(
                                f,
                                "\nRun `{}` to reattach",
                                crate::session::local_attach_command()
                            )?;
                        }
                    }
                    _ => {
                        write!(f, "server shut down")?;
                        if let Some(reason) = reason {
                            write!(f, ": {reason}")?;
                        }
                    }
                }
                Ok(())
            }
            ClientError::ConnectionLost(err) => {
                if let Ok(reattach_command) = std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                {
                    write!(f, "lost connection to remote Herdr: {err}")?;
                    write!(f, "\nIf the remote server survived the SSH or network drop, its panes may still be running.")?;
                    write!(f, "\nRun `{reattach_command}` to reattach")
                } else {
                    write!(f, "lost connection to server: {err}")
                }
            }
            ClientError::Protocol(err) => {
                write!(f, "protocol error: {err}")
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::ConnectionFailed(err) => Some(err),
            ClientError::ConnectionLost(err) => Some(err),
            ClientError::Protocol(err) => Some(err),
            _ => None,
        }
    }
}

impl From<protocol::FramingError> for ClientError {
    fn from(err: protocol::FramingError) -> Self {
        match err {
            protocol::FramingError::UnexpectedEof => ClientError::ConnectionLost(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            )),
            protocol::FramingError::Io(err) => ClientError::ConnectionLost(err),
            err => ClientError::Protocol(err),
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Sets up the terminal for client mode (raw mode, optional mouse, keyboard enhancements).
///
/// Returns a guard that restores the terminal when dropped.
fn setup_terminal(mouse_capture: bool) -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(true, mouse_capture)
}

/// Sets up a direct attach terminal.
///
/// Direct attach forwards stdin to the attached PTY. It enables mouse capture
/// so wheel events can drive the attached viewport or be forwarded to child
/// programs that requested mouse input.
fn setup_direct_attach_terminal() -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(false, true)
}

fn setup_terminal_with_capabilities(
    enable_client_protocols: bool,
    mouse_capture: bool,
) -> io::Result<TerminalGuard> {
    ratatui::init();
    apply_client_terminal_capabilities(enable_client_protocols, mouse_capture)
}

/// Sets up a ratatui terminal for the full mirror TUI and returns it alongside
/// the restore guard. Unlike [`setup_terminal_with_capabilities`] (which discards
/// the terminal because the single-pane attach/mirror paths blit to stdout
/// directly), the multi-pane mirror renders through `terminal.draw`, so it needs
/// the `DefaultTerminal` (`design-mirror-tui.md` §3.2).
#[cfg(unix)]
pub(crate) fn setup_mirror_terminal(
    mouse_capture: bool,
) -> io::Result<(ratatui::DefaultTerminal, TerminalGuard)> {
    let terminal = ratatui::init();
    let guard = apply_client_terminal_capabilities(true, mouse_capture)?;
    Ok((terminal, guard))
}

/// Applies client input/mouse/paste/keyboard capabilities on the already-init'd
/// terminal and returns the guard that restores them on drop. Shared by the
/// attach/mirror setups so their capability handling can't drift.
fn apply_client_terminal_capabilities(
    enable_client_protocols: bool,
    mouse_capture: bool,
) -> io::Result<TerminalGuard> {
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let host_color_scheme_reports =
        should_enable_host_color_scheme_reports(enable_client_protocols);

    if enable_client_protocols {
        if mouse_capture {
            set_mouse_capture(true)?;
        } else {
            set_mouse_capture(false)?;
        }
        execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
        if host_color_scheme_reports {
            write_host_color_scheme_report_mode(&mut io::stdout(), true)?;
        }
        push_keyboard_enhancement_flags()?;
    } else {
        if should_query_host_terminal_theme() {
            write_host_color_scheme_report_mode(&mut io::stdout(), false)?;
        }
        if mouse_capture {
            set_mouse_capture(true)?;
        } else {
            set_mouse_capture(false)?;
        }
    }

    #[cfg(windows)]
    let windows_virtual_terminal_input =
        if enable_client_protocols && windows_vti_input_backend_enabled() {
            enable_windows_virtual_terminal_input()
        } else {
            WindowsVirtualTerminalInputSetup::default()
        };

    #[cfg(windows)]
    if enable_client_protocols
        && windows_vti_input_backend_enabled()
        && windows_virtual_terminal_input.active
        && windows_win32_input_mode_enabled()
    {
        if let Err(err) = enable_windows_win32_input_mode(&mut io::stdout()) {
            if let Some(mode) = windows_virtual_terminal_input.restore_mode {
                restore_windows_input_mode_value(mode);
            }
            return Err(err);
        }
    }

    let modify_other_keys_mode = enable_client_protocols
        .then(crate::input::host_modify_other_keys_mode)
        .flatten();
    if let Some(mode) = modify_other_keys_mode {
        io::stdout().write_all(mode.set_sequence())?;
        io::stdout().flush()?;
    }

    Ok(TerminalGuard {
        reset_modify_other_keys: modify_other_keys_mode.is_some(),
        reset_host_color_scheme_reports: host_color_scheme_reports,
        #[cfg(windows)]
        restore_windows_input_mode: windows_virtual_terminal_input.restore_mode,
    })
}

fn should_enable_host_color_scheme_reports(enable_client_protocols: bool) -> bool {
    enable_client_protocols && should_query_host_terminal_theme()
}

/// Guard that restores the terminal when dropped.
pub(crate) struct TerminalGuard {
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
    #[cfg(windows)]
    restore_windows_input_mode: Option<u32>,
}

fn write_host_color_scheme_report_mode(
    writer: &mut impl io::Write,
    enabled: bool,
) -> io::Result<()> {
    let sequence = if enabled {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE
    } else {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE
    };
    writer.write_all(sequence.as_bytes())?;
    writer.flush()
}

fn write_terminal_restore_postlude(
    writer: &mut impl io::Write,
    reset_host_color_scheme_reports: bool,
) -> io::Result<()> {
    if reset_host_color_scheme_reports {
        writer.write_all(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        )?;
    }
    // Restore a visible cursor and reset DECSCUSR back to the terminal default.
    writer.write_all(b"\x1b[?25h\x1b[0 q")?;
    writer.flush()
}

fn should_draw_host_cursor(mode: crate::config::HostCursorModeConfig) -> bool {
    match mode {
        crate::config::HostCursorModeConfig::Auto => {
            crate::platform::should_draw_host_cursor_by_default()
        }
        crate::config::HostCursorModeConfig::Native => false,
        crate::config::HostCursorModeConfig::Drawn => true,
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsVirtualTerminalInputSetup {
    active: bool,
    restore_mode: Option<u32>,
}

#[cfg(windows)]
fn enable_windows_virtual_terminal_input() -> WindowsVirtualTerminalInputSetup {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
        STD_INPUT_HANDLE,
    };

    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        tracing::warn!("failed to get Windows console input handle for VT input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        tracing::warn!("failed to read Windows console input mode for VT input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let desired = windows_virtual_terminal_input_mode(mode);
    if desired == mode {
        return WindowsVirtualTerminalInputSetup {
            active: true,
            restore_mode: None,
        };
    }

    if unsafe { SetConsoleMode(handle, desired) } == 0 {
        tracing::warn!("failed to enable Windows virtual terminal input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let mut applied = 0;
    if unsafe { GetConsoleMode(handle, &mut applied) } == 0 {
        tracing::warn!("failed to verify Windows virtual terminal input mode");
        let _ = unsafe { SetConsoleMode(handle, mode) };
        return WindowsVirtualTerminalInputSetup::default();
    }
    if applied & ENABLE_VIRTUAL_TERMINAL_INPUT == 0 {
        tracing::warn!("Windows virtual terminal input bit did not stick");
        let _ = unsafe { SetConsoleMode(handle, mode) };
        return WindowsVirtualTerminalInputSetup::default();
    }

    WindowsVirtualTerminalInputSetup {
        active: true,
        restore_mode: Some(mode),
    }
}

#[cfg(windows)]
fn windows_vti_input_backend_enabled() -> bool {
    std::env::var("HERDR_WINDOWS_INPUT_BACKEND")
        .map(|backend| !backend.eq_ignore_ascii_case("crossterm"))
        .unwrap_or(true)
}

#[cfg(any(windows, test))]
fn windows_virtual_terminal_input_mode(mode: u32) -> u32 {
    mode | 0x0200
}

#[cfg(windows)]
fn restore_windows_input_mode_value(mode: u32) {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};

    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return;
    }
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        tracing::warn!("failed to restore Windows console input mode");
    }
}

fn set_mouse_capture(enabled: bool) -> io::Result<()> {
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    if enabled {
        execute!(io::stdout(), EnableMouseCapture)
    } else {
        match execute!(io::stdout(), DisableMouseCapture) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(err) if err.to_string() == "Initial console modes not set" => Ok(()),
            Err(err) => Err(err),
        }
    }
}

fn restore_terminal_state(
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
    #[cfg(windows)] restore_windows_input_mode: Option<u32>,
) {
    let _ = clear_received_kitty_graphics(&mut io::stdout());

    // Reset modifyOtherKeys if we enabled it.
    if reset_modify_other_keys {
        let _ = io::stdout().write_all(b"\x1b[>4;0m");
        let _ = io::stdout().flush();
    }

    let _ = pop_keyboard_enhancement_flags();

    let _ = execute!(
        io::stdout(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture
    );
    let _ = crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout());
    #[cfg(windows)]
    if let Some(mode) = restore_windows_input_mode {
        restore_windows_input_mode_value(mode);
    }

    ratatui::restore();
    let _ = write_terminal_restore_postlude(&mut io::stdout(), reset_host_color_scheme_reports);

    #[cfg(windows)]
    if windows_vti_input_backend_enabled() && windows_win32_input_mode_enabled() {
        let _ = disable_windows_win32_input_mode(&mut io::stdout());
    }
}

#[cfg(not(windows))]
fn push_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(crate::input::ime_compatible_keyboard_enhancement_flags())
    )
}

#[cfg(windows)]
fn push_keyboard_enhancement_flags() -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(io::stdout(), PopKeyboardEnhancementFlags)
}

#[cfg(windows)]
fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn windows_win32_input_mode_enabled() -> bool {
    std::env::var("HERDR_WINDOWS_INPUT_PROBE")
        .map(|probe| probe.eq_ignore_ascii_case("win32"))
        .unwrap_or(true)
}

#[cfg(windows)]
fn enable_windows_win32_input_mode(writer: &mut impl std::io::Write) -> io::Result<()> {
    writer.write_all(b"\x1b[?9001h")?;
    writer.flush()
}

#[cfg(windows)]
fn disable_windows_win32_input_mode(writer: &mut impl std::io::Write) -> io::Result<()> {
    writer.write_all(b"\x1b[?9001l")?;
    writer.flush()
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_state(
            self.reset_modify_other_keys,
            self.reset_host_color_scheme_reports,
            #[cfg(windows)]
            self.restore_windows_input_mode,
        );
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

fn requested_render_encoding() -> RenderEncoding {
    match std::env::var("HERDR_RENDER_ENCODING").ok().as_deref() {
        Some("terminal-ansi" | "terminal_ansi" | "ansi") => RenderEncoding::TerminalAnsi,
        _ => RenderEncoding::SemanticFrame,
    }
}

#[cfg(unix)]
fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

/// Time to wait for the server's Welcome reply during the handshake.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`herdr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget. See issue #753.
const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

fn handshake_read_timeout() -> Duration {
    #[cfg(unix)]
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

fn requested_keybindings() -> ClientKeybindings {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("local") => crate::config::Config::load()
            .config
            .local_keybindings_profile_toml()
            .map(|keys_toml| ClientKeybindings::Local { keys_toml })
            .unwrap_or(ClientKeybindings::Server),
        _ => ClientKeybindings::Server,
    }
}

#[cfg(windows)]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
) -> Result<(), ClientError> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(err = %err, context, "client socket receive timeout unavailable");
            Ok(())
        }
        Err(err) => Err(ClientError::ConnectionFailed(err)),
    }
}

#[cfg(not(windows))]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
) -> Result<(), ClientError> {
    stream
        .set_recv_timeout(timeout)
        .map_err(ClientError::ConnectionFailed)
}

/// Performs the client→server handshake.
///
/// Sends Hello with the terminal size and protocol version, reads the Welcome
/// response. Returns Ok(()) on success, or an error if the server rejects us.
fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    requested_encoding: RenderEncoding,
    direct_attach_requested: bool,
) -> Result<RenderEncoding, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    // Send Hello.
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        cols,
        rows,
        cell_width_px,
        cell_height_px,
        requested_encoding,
        keybindings: requested_keybindings(),
        launch_mode: if direct_attach_requested {
            ClientLaunchMode::TerminalAttach
        } else {
            ClientLaunchMode::App
        },
    };
    protocol::write_message(stream, &hello)
        .map_err(|e| ClientError::ConnectionFailed(io::Error::other(e.to_string())))?;

    // Read Welcome.
    set_handshake_recv_timeout(
        stream,
        Some(handshake_read_timeout()),
        "client handshake read timeout unavailable",
    )?;
    let welcome: ServerMessage = protocol::read_message(stream, MAX_FRAME_SIZE)?;
    set_handshake_recv_timeout(
        stream,
        None,
        "failed to clear client handshake read timeout",
    )?;

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(encoding)
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

/// Internal events for the client event loop.
enum ClientLoopEvent {
    /// Raw input bytes from stdin.
    #[cfg(unix)]
    StdinInput(Vec<u8>),
    /// Structured input events from platforms without Unix-style stdin bytes.
    #[cfg(windows)]
    StdinEvents(Vec<crate::protocol::ClientInputEvent>),
    /// Terminal resize detected.
    Resize(u16, u16, u32, u32),
    /// Server message received.
    ServerMessage(ServerMessage),
    /// Server reader thread exited (connection lost).
    ServerDisconnected,
    /// A mirror data-plane message tagged with the terminal it belongs to.
    ///
    /// The full mirror session opens one wire connection per terminal, so its
    /// server readers tag each message with a `terminal_id` and the session
    /// routes it to that terminal's local emulator (`design-mirror-tui.md` §2.1,
    /// Phase 3). Unlike [`Self::ServerMessage`], which carries no terminal
    /// identity, this variant lets multiple per-terminal readers share one pump.
    ///
    /// Produced by [`mirror_server_reader_thread`]; the fields are read by the
    /// mirror app driver's handler (Phase 4), which routes them through
    /// [`mirror_session::MirrorConnectionManager::apply`], so they read as unused
    /// until that lands.
    #[cfg(unix)]
    #[allow(dead_code)]
    ServerMirror {
        terminal_id: String,
        msg: ServerMessage,
    },
    /// A per-terminal mirror reader exited (its data connection dropped).
    ///
    /// The mirror app driver's handler (Phase 4) reads `terminal_id` to reconnect
    /// that terminal via [`mirror_session::MirrorConnectionManager::reconnect`];
    /// unused until then.
    #[cfg(unix)]
    #[allow(dead_code)]
    ServerMirrorDisconnected { terminal_id: String },
    /// A structural event from the JSON API control plane (`events.subscribe`).
    ///
    /// The full mirror session subscribes to workspace/tab/pane/layout/focus
    /// events to keep its replica current (`design-mirror-tui.md` §2.4); a reader
    /// thread feeds each one here so the app loop reconciles it alongside stdin,
    /// resize, and per-terminal mirror data. Consumed by the mirror app driver.
    #[cfg(unix)]
    #[allow(dead_code)]
    ControlEvent(Box<crate::api::schema::EventEnvelope>),
    /// The mirror control-plane event stream ended or failed; the full mirror
    /// session should rebuild its replica from a fresh snapshot and resubscribe.
    #[cfg(unix)]
    #[allow(dead_code)]
    ControlDisconnected,
    /// Timer tick.
    Timer,
}

/// Runs the thin client: connects to the server, performs the handshake,
/// and enters the main event loop.
///
/// This is the entry point called from `main.rs` when running in client mode.
pub fn run_client() -> io::Result<()> {
    run_client_with_mode(
        requested_render_encoding(),
        None,
        None,
        "connecting to server",
    )
}

/// Runs a direct terminal attach client.
#[cfg(unix)]
pub fn run_terminal_attach(terminal_id: String, takeover: bool) -> io::Result<()> {
    run_client_with_mode(
        RenderEncoding::TerminalAnsi,
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::default()),
        "attaching to terminal",
    )
}

/// Direct terminal attach is Unix raw-byte input only until Windows gets a semantic attach path.
#[cfg(windows)]
pub fn run_terminal_attach(_terminal_id: String, _takeover: bool) -> io::Result<()> {
    debug_assert!(!crate::platform::capabilities().direct_terminal_attach);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "direct terminal attach is not supported on Windows yet",
    ))
}

/// Runs a read-only terminal session observer and prints one JSON envelope per frame.
pub fn run_terminal_session_observe(target: String, cols: u16, rows: u16) -> io::Result<()> {
    let mut stream =
        connect_terminal_session_stream(target.clone(), cols, rows, "observing terminal session")?;
    write_to_server(&mut stream, &ClientMessage::ObserveTerminal { target })?;
    write_terminal_session_output(stream)
}

/// Runs a read-only terminal mirror consumer and prints one JSON envelope per
/// mirror message (snapshot, output/resize event, or close).
///
/// This is the headless/machine-readable face of the responsive local mirror:
/// it lets tools and tests consume the raw replication stream without a live
/// TUI, and backs `terminal session mirror --json`.
pub fn run_terminal_session_mirror_json(
    target: String,
    cols: u16,
    rows: u16,
    resume_from: Option<u64>,
) -> io::Result<()> {
    let mut stream =
        connect_terminal_session_stream(target.clone(), cols, rows, "mirroring terminal session")?;
    write_to_server(
        &mut stream,
        &ClientMessage::MirrorTerminal {
            target,
            resume_from,
            writable: false,
        },
    )?;
    write_terminal_mirror_output(stream)
}

/// Runs the interactive local mirror: a full-screen client that maintains its
/// own terminal emulator from the replicated stream and renders it locally, so
/// scrollback (mouse wheel and page keys off the alternate screen) is instant
/// with no server round-trip. Keystrokes are forwarded to the remote terminal.
///
/// If the connection drops (a flaky link), the client keeps the local mirror on
/// screen and transparently reconnects, resuming the stream from where it left
/// off. Detach with `ctrl+b q`.
#[cfg(unix)]
pub fn run_terminal_session_mirror(
    target: String,
    cli_cols: u16,
    cli_rows: u16,
    resume_from: Option<u64>,
) -> io::Result<()> {
    init_logging();
    // The interactive mirror is a full-screen client, so it renders at the real
    // terminal size, not the JSON-mode default. Fall back to the CLI values only
    // when the size cannot be detected.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((cli_cols, cli_rows));

    // Connect once before entering the alt screen so connection errors print
    // normally rather than into a full-screen UI we then have to tear down.
    let first = connect_mirror_stream(&target, cols, rows, resume_from, true)?;

    let _guard = setup_direct_attach_terminal()?;
    let should_quit = Arc::new(AtomicBool::new(false));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_terminal_mirror_loop(
        target,
        cols,
        rows,
        first,
        should_quit,
    ));
    result.map_err(|err| io::Error::other(err.to_string()))
}

/// Connects, handshakes, and subscribes a mirror stream, returning the ready
/// stream. Unlike the JSON path this never exits the process, so it can be
/// retried for transparent reconnect.
#[cfg(unix)]
fn connect_mirror_stream(
    target: &str,
    cols: u16,
    rows: u16,
    resume_from: Option<u64>,
    writable: bool,
) -> io::Result<LocalStream> {
    let socket_path = client_socket_path();
    let mut stream = crate::ipc::connect_local_stream(&socket_path)?;
    match do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::TerminalAnsi,
        true,
    ) {
        Ok(_) => {}
        Err(err) => return Err(io::Error::other(err.to_string())),
    }
    stream.set_nonblocking(false)?;
    write_to_server(
        &mut stream,
        &ClientMessage::MirrorTerminal {
            target: target.to_owned(),
            resume_from,
            writable,
        },
    )?;
    Ok(stream)
}

/// Connects, handshakes, and subscribes one terminal's mirror data connection on
/// an explicit wire socket, returning the ready stream.
///
/// Unlike [`connect_mirror_stream`] (which derives the socket from the active
/// session), the full mirror session's connection manager passes the socket it
/// resolved for the target session and opens one of these per terminal
/// (`design-mirror-tui.md` §2.1, Phase 3). Errors are typed as [`ClientError`] so
/// the manager can distinguish connect vs. subscribe failures.
#[cfg(unix)]
pub(crate) fn connect_mirror_data_stream(
    socket_path: &std::path::Path,
    target: &str,
    cols: u16,
    rows: u16,
    resume_from: Option<u64>,
    writable: bool,
) -> Result<LocalStream, ClientError> {
    let mut stream =
        crate::ipc::connect_local_stream(socket_path).map_err(ClientError::ConnectionFailed)?;
    do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::TerminalAnsi,
        true,
    )?;
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;
    write_to_server(
        &mut stream,
        &ClientMessage::MirrorTerminal {
            target: target.to_owned(),
            resume_from,
            writable,
        },
    )
    .map_err(ClientError::ConnectionLost)?;
    Ok(stream)
}

/// Interactive mirror is Unix-only for now (it shares the direct-attach raw input
/// path). Non-Unix falls back to the JSON stream.
#[cfg(not(unix))]
pub fn run_terminal_session_mirror(
    target: String,
    cols: u16,
    rows: u16,
    resume_from: Option<u64>,
) -> io::Result<()> {
    run_terminal_session_mirror_json(target, cols, rows, resume_from)
}

/// Outcome of one connected mirror session, deciding what the reconnect loop
/// does next.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum MirrorSessionEnd {
    /// The user detached or the terminal ended; stop.
    Done,
    /// The connection dropped; reconnect and resume.
    Reconnect,
}

/// The interactive mirror driver: keeps the local emulator alive across
/// reconnects, rendering and scrolling locally while the link is up or down.
#[cfg(unix)]
async fn run_terminal_mirror_loop(
    target: String,
    cols: u16,
    rows: u16,
    first_stream: LocalStream,
    should_quit: Arc<AtomicBool>,
) -> Result<(), ClientError> {
    use crate::pane::LocalMirror;

    let mut mirror = LocalMirror::new(cols, rows).map_err(ClientError::ConnectionFailed)?;
    let mut escape = AttachEscapeState::default();
    // Terminal rows moved per mouse-wheel notch during local scrollback.
    let mouse_scroll_lines = 3usize;
    let mut viewport_rows = rows;
    let mut client_size = (cols, rows);

    // The pump (event channel) and its stdin/resize threads live for the whole
    // session, across reconnects. Each connection adds its own server reader.
    let mut pump = ClientEventPump::new(should_quit.clone());
    let mouse_active = Arc::new(AtomicBool::new(true));
    pump.spawn_stdin(false, mouse_active);
    pump.spawn_resize(cols, rows, false);

    let mut stream = Some(first_stream);
    let mut backoff = Duration::from_millis(200);
    while !should_quit.load(Ordering::Acquire) {
        // Obtain a connected stream, reconnecting with resume on a dropped link.
        let connected = match stream.take() {
            Some(stream) => stream,
            None => {
                match connect_mirror_stream(
                    &target,
                    client_size.0,
                    client_size.1,
                    mirror.last_seq(),
                    true,
                ) {
                    Ok(stream) => {
                        backoff = Duration::from_millis(200);
                        stream
                    }
                    Err(_) => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(3));
                        continue;
                    }
                }
            }
        };

        let end = run_connected_mirror_session(
            connected,
            &mut mirror,
            &mut escape,
            &mut pump,
            mouse_scroll_lines,
            &mut viewport_rows,
            &mut client_size,
        )
        .await?;
        match end {
            MirrorSessionEnd::Done => break,
            MirrorSessionEnd::Reconnect => continue,
        }
    }
    Ok(())
}

/// Runs one connected mirror session until the link drops or the user leaves.
///
/// Owns the connection's writer and server reader; local input/resize/timer and
/// server messages all arrive through the shared [`ClientEventPump`]. Per-event
/// behavior lives in [`MirrorHandler`].
#[cfg(unix)]
async fn run_connected_mirror_session(
    stream: LocalStream,
    mirror: &mut crate::pane::LocalMirror,
    escape: &mut AttachEscapeState,
    pump: &mut ClientEventPump,
    mouse_scroll_lines: usize,
    viewport_rows: &mut u16,
    client_size: &mut (u16, u16),
) -> Result<MirrorSessionEnd, ClientError> {
    let mut blit = render_ansi::BlitEncoder::new();

    // Per-connection server reader feeds the shared pump channel.
    let read_stream = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let reader = pump.spawn_server_reader(read_stream, MAX_FRAME_SIZE);

    let mut write_stream = stream;
    write_stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    // Sync the source terminal to our size, then repaint from current state.
    let _ = write_to_server(
        &mut write_stream,
        &ClientMessage::Resize {
            cols: client_size.0,
            rows: client_size.1,
            cell_width_px: 0,
            cell_height_px: 0,
        },
    );
    repaint_mirror(mirror, &mut blit, true)?;

    let mut handler = MirrorHandler {
        mirror,
        escape,
        blit,
        write_stream,
        mouse_scroll_lines,
        viewport_rows,
        client_size,
        end: MirrorSessionEnd::Done,
    };
    let end = match pump.run(&mut handler).await {
        Ok(LoopEnd::Quit) => MirrorSessionEnd::Done,
        Ok(LoopEnd::Handler) => handler.end,
        // On error, abandon both writer and reader (they die with the process),
        // matching the pre-refactor early-return behavior.
        Err(err) => return Err(err),
    };

    // Tear down this connection's writer. On reconnect the reader has already
    // exited (it emitted ServerDisconnected), so join it; on a clean exit the
    // reader may still be blocked on a read, so abandon it — it dies with the
    // process when the command returns.
    drop(handler);
    match end {
        MirrorSessionEnd::Reconnect => {
            let _ = reader.join();
        }
        MirrorSessionEnd::Done => {}
    }
    Ok(end)
}

/// Per-connection mirror event handling driven by [`ClientEventPump::run`].
#[cfg(unix)]
struct MirrorHandler<'a> {
    mirror: &'a mut crate::pane::LocalMirror,
    escape: &'a mut AttachEscapeState,
    blit: render_ansi::BlitEncoder,
    write_stream: LocalStream,
    mouse_scroll_lines: usize,
    viewport_rows: &'a mut u16,
    client_size: &'a mut (u16, u16),
    /// How the session ended once the handler breaks the loop.
    end: MirrorSessionEnd,
}

#[cfg(unix)]
impl ClientLoopHandler for MirrorHandler<'_> {
    fn on_event(
        &mut self,
        event: ClientLoopEvent,
        pump: &mut ClientEventPump,
    ) -> Result<ControlFlow<()>, ClientError> {
        match event {
            ClientLoopEvent::StdinInput(data) => {
                match self
                    .escape
                    .filter_input(data, *self.viewport_rows, self.mouse_scroll_lines)
                {
                    AttachInputAction::Forward(bytes) => {
                        if self.mirror.is_scrolled_back() {
                            self.mirror.scroll_to_bottom();
                            repaint_mirror(self.mirror, &mut self.blit, false)?;
                        }
                        let _ = write_to_server(
                            &mut self.write_stream,
                            &ClientMessage::Input { data: bytes },
                        );
                    }
                    AttachInputAction::Scroll {
                        source,
                        direction,
                        lines,
                        ..
                    } => {
                        let handle_locally = matches!(
                            scroll::scroll_disposition(&source, self.mirror.input_state()),
                            scroll::ScrollDisposition::Local
                        );
                        if handle_locally {
                            match direction {
                                AttachScrollDirection::Up => self.mirror.scroll_up(lines as usize),
                                AttachScrollDirection::Down => {
                                    self.mirror.scroll_down(lines as usize)
                                }
                            }
                            repaint_mirror(self.mirror, &mut self.blit, false)?;
                        } else if let AttachScrollSource::PageKey { input } = source {
                            let _ = write_to_server(
                                &mut self.write_stream,
                                &ClientMessage::Input { data: input },
                            );
                        }
                    }
                    AttachInputAction::Detach => {
                        let _ = write_to_server(&mut self.write_stream, &ClientMessage::Detach);
                        self.end = MirrorSessionEnd::Done;
                        return Ok(ControlFlow::Break(()));
                    }
                    AttachInputAction::None => {}
                }
            }
            ClientLoopEvent::Resize(new_cols, new_rows, cell_width_px, cell_height_px) => {
                *self.viewport_rows = new_rows;
                *self.client_size = (new_cols, new_rows);
                let _ = write_to_server(
                    &mut self.write_stream,
                    &ClientMessage::Resize {
                        cols: new_cols,
                        rows: new_rows,
                        cell_width_px,
                        cell_height_px,
                    },
                );
                self.blit = render_ansi::BlitEncoder::new();
            }
            ClientLoopEvent::ServerMessage(ServerMessage::MirrorSnapshot {
                base_seq,
                cols: src_cols,
                rows: src_rows,
            }) => {
                let outcome = self
                    .mirror
                    .apply_snapshot(base_seq, src_cols, src_rows)
                    .map_err(ClientError::ConnectionFailed)?;
                repaint_mirror(self.mirror, &mut self.blit, outcome.needs_full_redraw)?;
            }
            ClientLoopEvent::ServerMessage(ServerMessage::MirrorEvent { seq, kind }) => {
                let mut outcome = self.mirror.apply_event(seq, kind);
                // Coalesce a burst of pending events (e.g. the initial history
                // replay) into a single repaint to avoid flicker and wasted work.
                while !outcome.closed {
                    match pump.try_next() {
                        Some(ClientLoopEvent::ServerMessage(ServerMessage::MirrorEvent {
                            seq,
                            kind,
                        })) => {
                            let next = self.mirror.apply_event(seq, kind);
                            outcome.needs_full_redraw |= next.needs_full_redraw;
                            outcome.closed |= next.closed;
                        }
                        Some(ClientLoopEvent::ServerMessage(ServerMessage::MirrorSnapshot {
                            base_seq,
                            cols: src_cols,
                            rows: src_rows,
                        })) => {
                            let next = self
                                .mirror
                                .apply_snapshot(base_seq, src_cols, src_rows)
                                .map_err(ClientError::ConnectionFailed)?;
                            outcome.needs_full_redraw |= next.needs_full_redraw;
                        }
                        Some(ClientLoopEvent::ServerMessage(ServerMessage::ServerShutdown {
                            ..
                        })) => {
                            outcome.closed = true;
                        }
                        // Not part of this burst (input/resize/disconnect): put it
                        // back so the next loop iteration handles it in order.
                        Some(other) => {
                            pump.push_front(other);
                            break;
                        }
                        None => break,
                    }
                }
                if outcome.closed {
                    self.end = MirrorSessionEnd::Done;
                    return Ok(ControlFlow::Break(()));
                }
                if outcome.needs_full_redraw || !self.mirror.is_scrolled_back() {
                    repaint_mirror(self.mirror, &mut self.blit, outcome.needs_full_redraw)?;
                }
            }
            ClientLoopEvent::ServerMessage(ServerMessage::ServerShutdown { .. }) => {
                self.end = MirrorSessionEnd::Done;
                return Ok(ControlFlow::Break(()));
            }
            ClientLoopEvent::ServerMessage(_) => {}
            ClientLoopEvent::ServerDisconnected => {
                self.end = MirrorSessionEnd::Reconnect;
                return Ok(ControlFlow::Break(()));
            }
            // Tagged per-terminal mirror events belong to the full multi-pane
            // mirror session, not this single-connection interactive mirror.
            ClientLoopEvent::ServerMirror { .. }
            | ClientLoopEvent::ServerMirrorDisconnected { .. }
            | ClientLoopEvent::ControlEvent(_)
            | ClientLoopEvent::ControlDisconnected => {}
            ClientLoopEvent::Timer => {}
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// Renders the mirror's current viewport and writes the diff to stdout.
#[cfg(unix)]
fn repaint_mirror(
    mirror: &crate::pane::LocalMirror,
    blit: &mut render_ansi::BlitEncoder,
    full_redraw: bool,
) -> Result<(), ClientError> {
    let frame = mirror.render_frame();
    write_frame_to_stdout(
        &mut io::stdout(),
        blit,
        frame,
        FrameWriteOptions {
            full_redraw,
            draw_host_cursor: false,
            graphics_enabled: false,
        },
    )
    .map_err(ClientError::ConnectionLost)
}

/// Runs a writable terminal session controller.
pub fn run_terminal_session_control(
    target: String,
    takeover: bool,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let mut stream = connect_terminal_session_stream(
        target.clone(),
        cols,
        rows,
        "controlling terminal session",
    )?;
    write_to_server(
        &mut stream,
        &ClientMessage::ControlTerminal { target, takeover },
    )?;

    let mut write_stream = stream.try_clone()?;
    let _input_thread = std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            match terminal_control_command_from_json(&line) {
                Ok(message) => {
                    let release = matches!(message, ClientMessage::Detach);
                    if write_to_server(&mut write_stream, &message).is_err() {
                        return;
                    }
                    if release {
                        return;
                    }
                }
                Err(err) => eprintln!("herdr: terminal session control input ignored: {err}"),
            }
        }
        let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
    });

    write_terminal_session_output(stream)
}

fn connect_terminal_session_stream(
    target: String,
    cols: u16,
    rows: u16,
    log_message: &'static str,
) -> io::Result<LocalStream> {
    init_logging();

    let socket_path = client_socket_path();
    crate::logging::startup("client");
    info!(path = %socket_path.display(), target = %target, cols, rows, "{log_message}");

    let mut stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("herdr: {}", ClientError::ConnectionFailed(err));
            std::process::exit(1);
        }
    };

    match do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::TerminalAnsi,
        true,
    ) {
        Ok(RenderEncoding::TerminalAnsi) => {}
        Ok(encoding) => {
            eprintln!(
                "herdr: terminal session observe negotiated unsupported encoding {encoding:?}"
            );
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("herdr: {err}");
            std::process::exit(1);
        }
    }

    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn write_terminal_session_output(mut stream: LocalStream) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    loop {
        match protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE) {
            Ok(ServerMessage::Terminal(frame)) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&frame.bytes);
                let line = serde_json::json!({
                    "type": "terminal.frame",
                    "seq": frame.seq,
                    "encoding": "ansi",
                    "width": frame.width,
                    "height": frame.height,
                    "full": frame.full,
                    "bytes": encoded,
                });
                serde_json::to_writer(&mut stdout, &line)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            Ok(ServerMessage::ServerShutdown { reason }) => {
                let line = serde_json::json!({
                    "type": "terminal.closed",
                    "reason": reason,
                });
                serde_json::to_writer(&mut stdout, &line)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
                return Ok(());
            }
            Ok(ServerMessage::Graphics { .. }) => {}
            Ok(_) => {}
            Err(protocol::FramingError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(io::Error::other(err.to_string())),
        }
    }
}

fn write_terminal_mirror_output(mut stream: LocalStream) -> io::Result<()> {
    use crate::protocol::MirrorEventKind;
    let mut stdout = io::stdout().lock();
    let mut emit = |value: &serde_json::Value| -> io::Result<()> {
        serde_json::to_writer(&mut stdout, value)?;
        stdout.write_all(b"\n")?;
        stdout.flush()
    };
    loop {
        match protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE) {
            Ok(ServerMessage::MirrorSnapshot {
                base_seq,
                cols,
                rows,
            }) => {
                emit(&serde_json::json!({
                    "type": "mirror.snapshot",
                    "base_seq": base_seq,
                    "cols": cols,
                    "rows": rows,
                }))?;
            }
            Ok(ServerMessage::MirrorEvent { seq, kind }) => match kind {
                MirrorEventKind::Output(bytes) => {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    emit(&serde_json::json!({
                        "type": "mirror.event",
                        "event": "output",
                        "seq": seq,
                        "bytes": encoded,
                    }))?;
                }
                MirrorEventKind::Resize { cols, rows } => {
                    emit(&serde_json::json!({
                        "type": "mirror.event",
                        "event": "resize",
                        "seq": seq,
                        "cols": cols,
                        "rows": rows,
                    }))?;
                }
                MirrorEventKind::Closed { reason } => {
                    emit(&serde_json::json!({
                        "type": "mirror.closed",
                        "seq": seq,
                        "reason": reason,
                    }))?;
                    return Ok(());
                }
            },
            Ok(ServerMessage::ServerShutdown { reason }) => {
                emit(&serde_json::json!({
                    "type": "mirror.closed",
                    "reason": reason,
                }))?;
                return Ok(());
            }
            Ok(_) => {}
            Err(protocol::FramingError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(io::Error::other(err.to_string())),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum TerminalControlCommand {
    #[serde(rename = "terminal.input")]
    Input {
        text: Option<String>,
        bytes: Option<String>,
    },
    #[serde(rename = "terminal.resize")]
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
    },
    #[serde(rename = "terminal.scroll")]
    Scroll {
        direction: TerminalControlScrollDirection,
        lines: u16,
        #[serde(default)]
        source: TerminalControlScrollSource,
        #[serde(default)]
        column: Option<u16>,
        #[serde(default)]
        row: Option<u16>,
        #[serde(default)]
        modifiers: u8,
    },
    #[serde(rename = "terminal.release")]
    Release {},
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollSource {
    #[default]
    Wheel,
    PageKey,
}

fn terminal_control_command_from_json(raw: &str) -> Result<ClientMessage, String> {
    let command = serde_json::from_str::<TerminalControlCommand>(raw)
        .map_err(|err| format!("invalid json command: {err}"))?;
    match command {
        TerminalControlCommand::Input { text, bytes } => {
            let data = match (text, bytes) {
                (Some(_), Some(_)) => {
                    return Err("terminal.input accepts text or bytes, not both".into())
                }
                (Some(text), None) => text.into_bytes(),
                (None, Some(bytes)) => base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|err| format!("invalid terminal.input bytes: {err}"))?,
                (None, None) => Vec::new(),
            };
            Ok(ClientMessage::Input { data })
        }
        TerminalControlCommand::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } => {
            if cols == 0 || rows == 0 {
                return Err("terminal.resize cols and rows must be greater than 0".into());
            }
            Ok(ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            })
        }
        TerminalControlCommand::Scroll {
            direction,
            lines,
            source,
            column,
            row,
            modifiers,
        } => {
            if lines == 0 {
                return Err("terminal.scroll lines must be greater than 0".into());
            }
            let direction = match direction {
                TerminalControlScrollDirection::Up => AttachScrollDirection::Up,
                TerminalControlScrollDirection::Down => AttachScrollDirection::Down,
            };
            let source = match source {
                TerminalControlScrollSource::Wheel => AttachScrollSource::Wheel,
                TerminalControlScrollSource::PageKey => AttachScrollSource::PageKey {
                    input: match direction {
                        AttachScrollDirection::Up => b"\x1b[5~".to_vec(),
                        AttachScrollDirection::Down => b"\x1b[6~".to_vec(),
                    },
                },
            };
            Ok(ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            })
        }
        TerminalControlCommand::Release {} => Ok(ClientMessage::Detach),
    }
}

fn run_client_with_mode(
    requested_encoding: RenderEncoding,
    attach_request: Option<(String, bool)>,
    attach_escape: Option<AttachEscapeState>,
    log_message: &'static str,
) -> io::Result<()> {
    init_logging();

    let loaded_config = crate::config::Config::load();
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let mouse_capture = loaded_config.config.ui.mouse_capture;
    let mouse_scroll_lines = loaded_config.config.ui.mouse_scroll_lines();
    let redraw_on_focus_gained = loaded_config.config.ui.redraw_on_focus_gained;
    let host_cursor = loaded_config.config.ui.host_cursor;
    let direct_attach_requested = attach_request.is_some();
    #[cfg(unix)]
    let remote_image_paste_key = client_remote_image_paste_key(&loaded_config.config);
    let kitty_graphics_enabled =
        loaded_config.config.experimental.kitty_graphics && !direct_attach_requested;
    let loop_config = ClientLoopConfig {
        sound_config: loaded_config.config.ui.sound,
        mouse_scroll_lines,
        redraw_on_focus_gained,
        host_cursor,
        kitty_graphics_enabled,
        mouse_capture_active: mouse_capture,
        #[cfg(unix)]
        remote_image_paste_key,
    };

    let socket_path = client_socket_path();
    crate::logging::startup("client");
    info!(path = %socket_path.display(), "{log_message}");

    // Try to connect to the server.
    let mut stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(s) => s,
        Err(err) => {
            // Server unreachable — show clear error and exit.
            let client_err = ClientError::ConnectionFailed(err);
            eprintln!("herdr: {client_err}");
            std::process::exit(1);
        }
    };

    // Get the terminal geometry before handshake (before raw mode).
    let (cols, rows, cell_width_px, cell_height_px) =
        current_terminal_geometry(kitty_graphics_enabled);

    // Perform handshake while the stream is still in blocking mode.
    let negotiated_encoding = match do_handshake(
        &mut stream,
        cols,
        rows,
        cell_width_px,
        cell_height_px,
        requested_encoding,
        direct_attach_requested,
    ) {
        Ok(encoding) => encoding,
        Err(err) => {
            eprintln!("herdr: {err}");
            std::process::exit(1);
        }
    };

    if let Some((terminal_id, takeover)) = attach_request {
        let attach = ClientMessage::AttachTerminal {
            terminal_id,
            takeover,
        };
        if let Err(err) = write_to_server(&mut stream, &attach) {
            eprintln!("herdr: failed to request terminal attach: {err}");
            std::process::exit(1);
        }
    }

    // Now set up the terminal. This must happen AFTER the handshake succeeds,
    // so we don't leave the terminal in raw mode if the server rejects us.
    let direct_attach = attach_escape.is_some();
    let terminal_guard = if direct_attach {
        setup_direct_attach_terminal()
    } else {
        setup_terminal(mouse_capture)
    }
    .map_err(|err| {
        eprintln!("herdr: failed to set up terminal: {err}");
        err
    })?;

    // Install a panic hook to restore the terminal on panic (same as monolithic).
    let panic_resets_modify_other_keys = terminal_guard.reset_modify_other_keys;
    let panic_resets_host_color_scheme_reports = terminal_guard.reset_host_color_scheme_reports;
    #[cfg(windows)]
    let panic_restore_windows_input_mode = terminal_guard.restore_windows_input_mode;
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_state(
            panic_resets_modify_other_keys,
            panic_resets_host_color_scheme_reports,
            #[cfg(windows)]
            panic_restore_windows_input_mode,
        );
        original_hook(info);
    }));

    // Create the tokio runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let should_quit = Arc::new(AtomicBool::new(false));

    // Install Ctrl+C handler.
    let quit_flag = should_quit.clone();
    let _ = ctrlc::set_handler(move || {
        quit_flag.store(true, Ordering::Release);
    });

    let result = rt.block_on(async {
        run_client_loop(
            stream,
            cols,
            rows,
            should_quit,
            loop_config,
            negotiated_encoding,
            attach_escape,
        )
        .await
    });

    // Restore the terminal before printing any final status message.
    drop(terminal_guard);

    if let Err(err) = result {
        eprintln!("herdr: {err}");
        rt.shutdown_timeout(Duration::from_millis(100));
        crate::logging::shutdown("client");

        if matches!(
            err,
            ClientError::ServerShutdown {
                reason: Some(reason)
            } if reason == "detached"
        ) {
            return Ok(());
        }

        std::process::exit(1);
    }

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("client");
    Ok(())
}

/// The main client event loop.
///
/// Uses a threaded architecture:
/// - stdin reader thread → sends raw input bytes to main loop
/// - resize poller thread → sends resize events to main loop
/// - server reader thread → reads ServerMessages and sends to main loop
/// - main loop: coordinates input, output, and server communication
async fn run_client_loop(
    stream: LocalStream,
    cols: u16,
    rows: u16,
    should_quit: Arc<AtomicBool>,
    config: ClientLoopConfig,
    negotiated_encoding: RenderEncoding,
    attach_escape: Option<AttachEscapeState>,
) -> Result<(), ClientError> {
    #[cfg(windows)]
    let _ = config.mouse_scroll_lines;
    let draw_host_cursor = attach_escape.is_none() && should_draw_host_cursor(config.host_cursor);
    #[cfg(unix)]
    let is_remote_client = is_remote_client_process();

    let state = ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
        mouse_capture_active: config.mouse_capture_active,
        reported_size: (cols, rows),
        sound_config: config.sound_config,
        kitty_graphics_enabled: config.kitty_graphics_enabled,
        attach_escape,
        #[cfg(unix)]
        mouse_scroll_lines: config.mouse_scroll_lines,
        #[cfg(unix)]
        remote_image_paste_key: config.remote_image_paste_key,
        redraw_on_focus_gained: config.redraw_on_focus_gained,
        draw_host_cursor,
    };
    debug!(?negotiated_encoding, "client render encoding active");
    let host_mouse_capture_active = Arc::new(AtomicBool::new(state.mouse_capture_active));

    // The pump owns the event channel and its stdin/resize/server-reader threads.
    let mut pump = ClientEventPump::new(should_quit.clone());

    // Spawn the stdin reader thread.
    let will_query_host_terminal_theme =
        state.attach_escape.is_none() && should_query_host_terminal_theme();
    pump.spawn_stdin(
        will_query_host_terminal_theme,
        host_mouse_capture_active.clone(),
    );

    if will_query_host_terminal_theme {
        query_host_terminal_theme();
    }

    // Spawn the resize poller thread.
    let kitty_graphics_enabled = state.kitty_graphics_enabled;
    pump.spawn_resize(cols, rows, kitty_graphics_enabled);

    // Spawn the server reader thread (blocking reads from the socket).
    // Clone the stream's file descriptor so we can read from a blocking stream.
    let read_stream = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let max_frame_size = if kitty_graphics_enabled {
        MAX_GRAPHICS_FRAME_SIZE
    } else {
        MAX_FRAME_SIZE
    };
    pump.spawn_server_reader(read_stream, max_frame_size);

    // Use the original stream for writing (blocking is fine since we write
    // from the async loop).
    let write_stream = stream;
    write_stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    // This (foreground) client owns the prefix ASCII input-source switch; a no-op on non-macOS.
    let prefix_input_source = crate::platform::RealPrefixInputSource::default();

    let mut handler = RoundTripHandler {
        state,
        write_stream,
        host_mouse_capture_active,
        prefix_input_source,
        #[cfg(unix)]
        is_remote_client,
    };

    // The single shared event loop drives the handler.
    match pump.run(&mut handler).await? {
        LoopEnd::Quit => {
            // Clean exit (Ctrl+C). Send Detach before closing.
            let _ = write_to_server(&mut handler.write_stream, &ClientMessage::Detach);
            let _ = io::stdout().flush();
        }
        LoopEnd::Handler => {}
    }

    Ok(())
}

/// Round-trip (server-rendered) client event handling driven by
/// [`ClientEventPump::run`].
struct RoundTripHandler {
    state: ClientState,
    write_stream: LocalStream,
    host_mouse_capture_active: Arc<AtomicBool>,
    prefix_input_source: crate::platform::RealPrefixInputSource,
    #[cfg(unix)]
    is_remote_client: bool,
}

impl ClientLoopHandler for RoundTripHandler {
    fn on_event(
        &mut self,
        event: ClientLoopEvent,
        _pump: &mut ClientEventPump,
    ) -> Result<ControlFlow<()>, ClientError> {
        use crate::platform::PrefixInputSource;
        match event {
            #[cfg(unix)]
            ClientLoopEvent::StdinInput(data) => {
                let data = if let Some(attach_escape) = &mut self.state.attach_escape {
                    match attach_escape.filter_input(
                        data,
                        self.state.reported_size.1,
                        self.state.mouse_scroll_lines,
                    ) {
                        AttachInputAction::Forward(data) => data,
                        AttachInputAction::Scroll {
                            source,
                            direction,
                            lines,
                            column,
                            row,
                            modifiers,
                        } => {
                            let msg = ClientMessage::AttachScroll {
                                source,
                                direction,
                                lines,
                                column,
                                row,
                                modifiers,
                            };
                            if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                                return Err(ClientError::ConnectionLost(e));
                            }
                            return Ok(ControlFlow::Continue(()));
                        }
                        AttachInputAction::Detach => {
                            let _ = write_to_server(&mut self.write_stream, &ClientMessage::Detach);
                            return Ok(ControlFlow::Break(()));
                        }
                        AttachInputAction::None => return Ok(ControlFlow::Continue(())),
                    }
                } else {
                    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                    if crate::raw_input::events_require_host_surface_redraw(
                        &events,
                        self.state.redraw_on_focus_gained,
                    ) {
                        self.state.request_full_redraw();
                    }
                    if crate::raw_input::events_require_host_terminal_theme_query(&events) {
                        query_host_terminal_theme();
                    }
                    data
                };
                if should_bridge_clipboard_image_paste(
                    &data,
                    self.is_remote_client,
                    self.state.remote_image_paste_key,
                ) {
                    if let Some(image) = crate::platform::read_clipboard_image() {
                        if image.bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
                            warn!(
                                bytes = image.bytes.len(),
                                max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                                "local clipboard image is too large to bridge"
                            );
                            return Ok(ControlFlow::Continue(()));
                        }
                        info!(
                            bytes = image.bytes.len(),
                            extension = image.extension,
                            "bridging local clipboard image paste to remote server"
                        );
                        let msg = ClientMessage::ClipboardImage {
                            extension: image.extension.to_owned(),
                            data: image.bytes,
                        };
                        if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                            return Err(ClientError::ConnectionLost(e));
                        }
                        return Ok(ControlFlow::Continue(()));
                    }
                    info!(
                        "clipboard image paste trigger received, but local clipboard has no image"
                    );
                }
                if let Some(image) =
                    read_image_file_from_terminal_drop(&data, self.is_remote_client)
                {
                    info!(
                        bytes = image.bytes.len(),
                        extension = image.extension,
                        "bridging local image file drop to remote server"
                    );
                    let msg = ClientMessage::ClipboardImage {
                        extension: image.extension.to_owned(),
                        data: image.bytes,
                    };
                    if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                        return Err(ClientError::ConnectionLost(e));
                    }
                    return Ok(ControlFlow::Continue(()));
                }
                let msg = ClientMessage::Input { data };
                if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                    return Err(ClientError::ConnectionLost(e));
                }
            }
            #[cfg(windows)]
            ClientLoopEvent::StdinEvents(events) => {
                if self.state.attach_escape.is_some() {
                    return Ok(ControlFlow::Continue(()));
                }
                let raw_events = events
                    .iter()
                    .map(crate::protocol::ClientInputEvent::to_raw_input_event)
                    .collect::<Vec<_>>();
                if crate::raw_input::events_require_host_surface_redraw(
                    &raw_events,
                    self.state.redraw_on_focus_gained,
                ) {
                    self.state.request_full_redraw();
                }
                let msg = ClientMessage::InputEvents { events };
                if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                    return Err(ClientError::ConnectionLost(e));
                }
            }
            ClientLoopEvent::Resize(new_cols, new_rows, cell_width_px, cell_height_px) => {
                self.state.reported_size = (new_cols, new_rows);
                let msg = ClientMessage::Resize {
                    cols: new_cols,
                    rows: new_rows,
                    cell_width_px,
                    cell_height_px,
                };
                if let Err(e) = write_to_server(&mut self.write_stream, &msg) {
                    return Err(ClientError::ConnectionLost(e));
                }
            }
            ClientLoopEvent::ServerMessage(msg) => match msg {
                ServerMessage::Frame(frame_data) => {
                    let _ = write_frame_to_stdout(
                        &mut io::stdout(),
                        &mut self.state.blit_encoder,
                        frame_data,
                        FrameWriteOptions {
                            full_redraw: false,
                            draw_host_cursor: self.state.draw_host_cursor,
                            graphics_enabled: self.state.kitty_graphics_enabled,
                        },
                    );
                }
                ServerMessage::Terminal(frame) => {
                    if self.state.kitty_graphics_enabled
                        && contains_kitty_graphics_bytes(&frame.bytes)
                    {
                        record_received_kitty_graphics(&frame.bytes);
                    }
                    let mut stdout = io::stdout();
                    let _ = stdout.write_all(&frame.bytes);
                    let _ = stdout.flush();
                }
                ServerMessage::Graphics { bytes } => {
                    if self.state.kitty_graphics_enabled {
                        record_received_kitty_graphics(&bytes);
                        let mut stdout = io::stdout();
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
                ServerMessage::ServerShutdown { reason } => {
                    return Err(ClientError::ServerShutdown { reason });
                }
                ServerMessage::Notify {
                    kind,
                    message,
                    body,
                } => {
                    handle_notify(kind, &message, body.as_deref(), &self.state.sound_config);
                }
                ServerMessage::Clipboard { data } => {
                    forward_clipboard(&data);
                    let _ = io::stdout().flush();
                }
                ServerMessage::WindowTitle { title } => {
                    write_window_title(title.as_deref());
                    let _ = io::stdout().flush();
                }
                ServerMessage::ReloadSoundConfig => {
                    reload_local_client_config(
                        &mut self.state.sound_config,
                        &mut self.state.redraw_on_focus_gained,
                        &mut self.state.draw_host_cursor,
                        #[cfg(unix)]
                        &mut self.state.remote_image_paste_key,
                    );
                }
                ServerMessage::MouseCapture { enabled } => {
                    let desired = enabled;
                    if desired != self.state.mouse_capture_active {
                        set_mouse_capture(desired).map_err(ClientError::ConnectionFailed)?;
                        #[cfg(windows)]
                        if windows_vti_input_backend_enabled() {
                            let _ = enable_windows_virtual_terminal_input();
                        }
                        self.state.mouse_capture_active = desired;
                        self.host_mouse_capture_active
                            .store(desired, Ordering::Release);
                    }
                }
                ServerMessage::PrefixInputSource { active } => {
                    if active {
                        self.prefix_input_source.switch_to_ascii();
                    } else {
                        self.prefix_input_source.restore();
                    }
                }
                ServerMessage::Welcome { .. } => {
                    debug!("received unexpected Welcome in main loop");
                }
                ServerMessage::MirrorSnapshot { .. } | ServerMessage::MirrorEvent { .. } => {
                    debug!("received unexpected mirror message in app client loop");
                }
            },
            ClientLoopEvent::ServerDisconnected => {
                return Err(ClientError::ConnectionLost(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                )));
            }
            // Tagged per-terminal mirror events are only produced by the full
            // mirror session's connection manager, never on the round-trip path.
            #[cfg(unix)]
            ClientLoopEvent::ServerMirror { .. }
            | ClientLoopEvent::ServerMirrorDisconnected { .. }
            | ClientLoopEvent::ControlEvent(_)
            | ClientLoopEvent::ControlDisconnected => {}
            ClientLoopEvent::Timer => {}
        }
        Ok(ControlFlow::Continue(()))
    }
}

// ---------------------------------------------------------------------------
// Server reader thread
// ---------------------------------------------------------------------------

/// Blocking thread that reads ServerMessages from the server and sends them
/// to the main event loop.
fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    max_frame_size: usize,
) {
    // Ensure the read stream is in blocking mode to avoid WouldBlock errors
    // from read_exact inside read_message. The stream should already be
    // blocking after handshake, but we enforce it here as a safety measure.
    if stream.set_nonblocking(false).is_err() {
        // If we can't set blocking mode, the stream is likely broken.
        let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
        return;
    }

    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        match protocol::read_message(&mut stream, max_frame_size) {
            Ok(msg) => {
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMessage(msg))
                    .is_err()
                {
                    break; // Main loop gone.
                }
            }
            Err(protocol::FramingError::UnexpectedEof) => {
                // Server closed connection.
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
                break;
            }
            Err(protocol::FramingError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                // Should not happen with blocking mode, but handle gracefully
                // in case the stream was set nonblocking by another clone.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => {
                warn!(err = %err, "server read error");
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
                break;
            }
        }
    }
}

/// Blocking thread that reads a single terminal's mirror data-plane messages and
/// forwards them to the shared pump tagged with `terminal_id`.
///
/// The full mirror session runs one of these per open terminal so the pump can
/// route each terminal's snapshot/output/close to the right local emulator
/// (`design-mirror-tui.md` §2.1, Phase 3). It mirrors [`server_reader_thread`]
/// but emits the tagged [`ClientLoopEvent::ServerMirror`] /
/// [`ClientLoopEvent::ServerMirrorDisconnected`] variants.
#[cfg(unix)]
fn mirror_server_reader_thread(
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    max_frame_size: usize,
    terminal_id: String,
) {
    let disconnect = |tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>| {
        let _ = tx.blocking_send(ClientLoopEvent::ServerMirrorDisconnected {
            terminal_id: terminal_id.clone(),
        });
    };

    if stream.set_nonblocking(false).is_err() {
        disconnect(&event_tx);
        return;
    }

    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        match protocol::read_message(&mut stream, max_frame_size) {
            Ok(msg) => {
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMirror {
                        terminal_id: terminal_id.clone(),
                        msg,
                    })
                    .is_err()
                {
                    break; // Main loop gone.
                }
            }
            Err(protocol::FramingError::UnexpectedEof) => {
                disconnect(&event_tx);
                break;
            }
            Err(protocol::FramingError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => {
                warn!(err = %err, terminal_id = %terminal_id, "mirror server read error");
                disconnect(&event_tx);
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Write helper
// ---------------------------------------------------------------------------

/// Writes a message to the server stream (blocking).
fn write_to_server(stream: &mut LocalStream, msg: &ClientMessage) -> io::Result<()> {
    protocol::write_message(stream, msg).map_err(|e| io::Error::other(e.to_string()))
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn client_remote_image_paste_key(
    config: &crate::config::Config,
) -> Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)> {
    if !is_remote_client_process() {
        return None;
    }

    match config.remote_image_paste_key() {
        Ok(key) => key,
        Err(diagnostic) => {
            warn!(diagnostic = %diagnostic, "local remote image paste key config diagnostic");
            None
        }
    }
}

fn reload_local_client_config(
    sound_config: &mut crate::config::SoundConfig,
    redraw_on_focus_gained: &mut bool,
    draw_host_cursor: &mut bool,
    #[cfg(unix)] remote_image_paste_key: &mut Option<(
        crossterm::event::KeyCode,
        crossterm::event::KeyModifiers,
    )>,
) {
    match crate::config::load_live_config() {
        Ok(loaded) => {
            for diagnostic in loaded.config.ui.sound.diagnostics() {
                warn!(diagnostic = %diagnostic, "local sound config diagnostic");
            }
            #[cfg(unix)]
            let loaded_remote_image_paste_key = client_remote_image_paste_key(&loaded.config);
            *sound_config = loaded.config.ui.sound;
            *redraw_on_focus_gained = loaded.config.ui.redraw_on_focus_gained;
            *draw_host_cursor = should_draw_host_cursor(loaded.config.ui.host_cursor);
            #[cfg(unix)]
            {
                *remote_image_paste_key = loaded_remote_image_paste_key;
            }
            debug!("reloaded local client config");
        }
        Err(diagnostics) => {
            warn!(diagnostics = ?diagnostics, "failed to reload local client config; keeping current client config");
        }
    }
}

fn handle_notify(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
) {
    handle_notify_with_notifiers(
        kind,
        message,
        body,
        sound_config,
        crate::terminal_notify::show_notification,
        crate::platform::show_desktop_notification,
    );
}

fn handle_notify_with_notifiers(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
    mut show_terminal_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
    mut show_system_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
) {
    match kind {
        NotifyKind::Sound => {
            let Some(sound) = sound_from_notify_message(message) else {
                warn!(
                    message = message,
                    "received unknown sound notification from server"
                );
                return;
            };
            if sound_config.enabled {
                crate::sound::play(sound, sound_config);
            }
        }
        NotifyKind::Toast => {
            debug!(
                message = message,
                "received terminal toast notification from server"
            );
            if let Err(err) = show_terminal_notification(message, body) {
                warn!(err = %err, "failed to emit terminal notification");
            }
        }
        NotifyKind::SystemToast => {
            debug!(
                message = message,
                "received system toast notification from server"
            );
            if let Err(err) = show_system_notification(message, body) {
                warn!(err = %err, "failed to emit system notification");
            }
        }
    }
}

fn sound_from_notify_message(message: &str) -> Option<crate::sound::Sound> {
    match message {
        "agent done" => Some(crate::sound::Sound::Done),
        "agent attention" => Some(crate::sound::Sound::Request),
        _ => None,
    }
}

#[cfg(unix)]
fn should_bridge_clipboard_image_paste(
    data: &[u8],
    is_remote_client: bool,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
) -> bool {
    if data == b"\x1b[200~\x1b[201~" {
        return is_remote_client;
    }

    let Some(remote_image_paste_key) = remote_image_paste_key else {
        return false;
    };

    let events = crate::raw_input::parse_raw_input_bytes_sync(data);
    matches!(
        events.as_slice(),
        [crate::raw_input::RawInputEvent::Key(key)]
            if key.kind == crossterm::event::KeyEventKind::Press
                && crate::config::terminal_key_matches_combo(*key, remote_image_paste_key)
    )
}

#[cfg(unix)]
fn read_image_file_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<crate::platform::ClipboardImage> {
    let (path, extension) = image_path_from_terminal_drop(data, is_remote_client)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let file = std::fs::File::open(&path).ok()?;
    let bytes =
        match crate::platform::read_limited_reader(file, MAX_CLIPBOARD_IMAGE_PAYLOAD).ok()? {
            crate::platform::LimitedRead::Complete(bytes) => bytes,
            crate::platform::LimitedRead::Empty => return None,
            crate::platform::LimitedRead::Oversized => {
                warn!(
                    max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                    "local image file drop is too large to bridge"
                );
                return None;
            }
        };

    Some(crate::platform::ClipboardImage { bytes, extension })
}

#[cfg(unix)]
fn image_path_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<(std::path::PathBuf, &'static str)> {
    if !is_remote_client {
        return None;
    }

    let bytes = bracketed_paste_payload(data).unwrap_or(data);
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return None;
    }

    let text = unescape_terminal_drop_path(strip_matching_path_quotes(text));
    let path = std::path::PathBuf::from(text);
    if !path.is_absolute() {
        return None;
    }

    let extension = recognized_image_extension(path.extension()?.to_str()?)?;
    Some((path, extension))
}

#[cfg(unix)]
fn bracketed_paste_payload(data: &[u8]) -> Option<&[u8]> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    data.strip_prefix(START)?.strip_suffix(END)
}

#[cfg(unix)]
fn strip_matching_path_quotes(text: &str) -> &str {
    if text.len() < 2 {
        return text;
    }

    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) => &text[1..text.len() - 1],
        _ => text,
    }
}

#[cfg(unix)]
fn unescape_terminal_drop_path(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                unescaped.push(escaped);
            } else {
                unescaped.push(ch);
            }
        } else {
            unescaped.push(ch);
        }
    }
    unescaped
}

#[cfg(unix)]
fn recognized_image_extension(extension: &str) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case("png") {
        Some("png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("jpg")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("webp")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("bmp")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Clipboard forwarding
// ---------------------------------------------------------------------------

/// Decode a clipboard payload forwarded by the server.
fn decode_clipboard_payload(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Forwards a clipboard write from the server to the local client clipboard.
fn forward_clipboard(data: &str) {
    let Some(bytes) = decode_clipboard_payload(data) else {
        warn!("received invalid clipboard payload from server");
        return;
    };

    crate::selection::write_osc52_bytes(&bytes);
}

fn window_title_osc(title: Option<&str>) -> Vec<u8> {
    let title = title.unwrap_or("herdr");
    let safe_title = title
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .collect::<String>();
    format!("\x1b]0;{safe_title}\x07").into_bytes()
}

fn write_window_title(title: Option<&str>) {
    let _ = io::stdout().write_all(&window_title_osc(title));
}

// ---------------------------------------------------------------------------
// Frame output
// ---------------------------------------------------------------------------

/// Options controlling how a [`FrameData`] is blitted to the host terminal.
struct FrameWriteOptions {
    /// Reset the diff encoder first so the whole frame is redrawn.
    full_redraw: bool,
    /// Draw the cursor into the frame cells instead of using the host cursor.
    draw_host_cursor: bool,
    /// Emit inline Kitty graphics carried by the frame.
    graphics_enabled: bool,
}

/// The single frame blit/write/flush path shared by every client output site
/// (the round-trip `ServerMessage::Frame` arm and the local mirror repaint).
///
/// Encodes `frame` against `blit` (honoring host-cursor drawing), writes the
/// diff plus any graphics to `out`, flushes, then commits the frame to the
/// encoder so the next diff is computed against it (code-review Finding 3).
fn write_frame_to_stdout(
    out: &mut impl io::Write,
    blit: &mut render_ansi::BlitEncoder,
    frame: crate::protocol::FrameData,
    opts: FrameWriteOptions,
) -> io::Result<()> {
    if opts.full_redraw {
        *blit = render_ansi::BlitEncoder::new();
    }
    let frame = if opts.draw_host_cursor {
        render_ansi::frame_with_drawn_cursor(frame)
    } else {
        frame
    };
    let encoded = if opts.draw_host_cursor {
        blit.encode_with_suppressed_visible_cursor(&frame, false)
    } else {
        blit.encode(&frame, false)
    };
    let graphics: &[u8] = if opts.graphics_enabled {
        frame.graphics.as_slice()
    } else {
        &[]
    };
    write_encoded_frame_with_graphics(&mut *out, &encoded.bytes, graphics)?;
    out.flush()?;
    blit.commit(frame, encoded);
    Ok(())
}

fn write_encoded_frame_with_graphics(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
) -> io::Result<()> {
    writer.write_all(encoded)?;
    if graphics.is_empty() {
        return Ok(());
    }

    record_received_kitty_graphics(graphics);
    writer.write_all(b"\x1b7")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b8")
}

fn contains_kitty_graphics_bytes(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|window| window == b"\x1b_G")
}

fn record_received_kitty_graphics(bytes: &[u8]) {
    let ids = kitty_graphics_image_ids(bytes);
    if ids.is_empty() {
        return;
    }
    let set = RECEIVED_KITTY_GRAPHICS_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = set.lock() {
        set.extend(ids);
    }
}

fn clear_received_kitty_graphics(mut writer: impl io::Write) -> io::Result<()> {
    let Some(set) = RECEIVED_KITTY_GRAPHICS_IDS.get() else {
        return Ok(());
    };
    let Ok(mut set) = set.lock() else {
        return Ok(());
    };
    for id in set.drain() {
        write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
    }
    writer.flush()
}

fn kitty_graphics_image_ids(bytes: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            ids.push(id);
        }
        index = command_start + end + 2;
    }
    ids
}

fn kitty_graphics_command_image_id(command: &[u8]) -> Option<u32> {
    let header_end = command
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(command.len());
    for part in command[..header_end].split(|byte| *byte == b',') {
        let Some(value) = part.strip_prefix(b"i=") else {
            continue;
        };
        let text = std::str::from_utf8(value).ok()?;
        if let Ok(id) = text.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Resize polling
// ---------------------------------------------------------------------------

fn current_terminal_geometry(kitty_graphics_enabled: bool) -> (u16, u16, u32, u32) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if !kitty_graphics_enabled {
        return (cols, rows, 0, 0);
    }
    let Ok(size) = crossterm::terminal::window_size() else {
        return (cols, rows, 8, 16);
    };
    if size.columns == 0 || size.rows == 0 || size.width == 0 || size.height == 0 {
        return (cols, rows, 8, 16);
    }
    (
        cols,
        rows,
        (size.width as u32 / size.columns as u32).max(1),
        (size.height as u32 / size.rows as u32).max(1),
    )
}

/// Polls the terminal size and sends resize events when it changes.
fn resize_poll_loop(
    resize_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    initial_cols: u16,
    initial_rows: u16,
    kitty_graphics_enabled: bool,
    should_quit: &Arc<AtomicBool>,
) {
    let (_, _, initial_cell_width, initial_cell_height) =
        current_terminal_geometry(kitty_graphics_enabled);
    let mut last_size = (
        initial_cols,
        initial_rows,
        initial_cell_width,
        initial_cell_height,
    );
    while !should_quit.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
        let new_size = current_terminal_geometry(kitty_graphics_enabled);
        if new_size != last_size {
            last_size = new_size;
            if resize_tx
                .blocking_send(ClientLoopEvent::Resize(
                    new_size.0, new_size.1, new_size.2, new_size.3,
                ))
                .is_err()
            {
                break; // Main loop gone.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Initialize logging for the client process.
fn query_host_terminal_theme() {
    let _ = write_host_terminal_theme_query(io::stdout());
}

fn should_query_host_terminal_theme() -> bool {
    !cfg!(windows)
}

fn write_host_terminal_theme_query(mut writer: impl io::Write) -> io::Result<()> {
    writer.write_all(crate::terminal_theme::HOST_COLOR_QUERY_SEQUENCE.as_bytes())?;
    writer.flush()
}

fn init_logging() {
    crate::logging::init_file_logging("herdr-client.log");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn restore_env_var(key: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            restore_env_var(self.key, self.previous.clone());
        }
    }

    #[test]
    fn windows_virtual_terminal_input_mode_sets_only_vti_bit() {
        assert_eq!(windows_virtual_terminal_input_mode(0x01f0), 0x03f0);
        assert_eq!(windows_virtual_terminal_input_mode(0x03f0), 0x03f0);
    }

    struct EnvVarsRemovedGuard {
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvVarsRemovedGuard {
        fn new(keys: &[&'static str]) -> Self {
            let previous: Vec<_> = keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            for key in keys {
                std::env::remove_var(key);
            }
            Self { previous }
        }
    }

    impl Drop for EnvVarsRemovedGuard {
        fn drop(&mut self) {
            for (key, value) in self.previous.clone() {
                restore_env_var(key, value);
            }
        }
    }

    #[test]
    fn host_cursor_policy_auto_uses_platform_default() {
        assert_eq!(
            should_draw_host_cursor(crate::config::HostCursorModeConfig::Auto),
            crate::platform::should_draw_host_cursor_by_default()
        );
    }

    #[test]
    fn host_cursor_policy_native_and_drawn_override_auto_detection() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarGuard::set("TERM_PROGRAM", "WezTerm");

        assert!(!should_draw_host_cursor(
            crate::config::HostCursorModeConfig::Native
        ));
        assert!(should_draw_host_cursor(
            crate::config::HostCursorModeConfig::Drawn
        ));
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_image_paste_bridge_triggers_on_configured_key_and_empty_paste() {
        let ctrl_v = crate::config::parse_key_combo("ctrl+v").unwrap();
        assert!(should_bridge_clipboard_image_paste(
            &[0x16],
            true,
            Some(ctrl_v)
        ));
        assert!(should_bridge_clipboard_image_paste(
            b"\x1b[118;5u",
            true,
            Some(ctrl_v)
        ));
        assert!(should_bridge_clipboard_image_paste(
            b"\x1b[200~\x1b[201~",
            true,
            None
        ));
        assert!(!should_bridge_clipboard_image_paste(
            b"\x1b[200~\x1b[201~",
            false,
            Some(ctrl_v)
        ));
        assert!(!should_bridge_clipboard_image_paste(
            b"\x1b[200~text\x1b[201~",
            true,
            Some(ctrl_v)
        ));
        assert!(!should_bridge_clipboard_image_paste(&[0x16], true, None));
        assert!(!should_bridge_clipboard_image_paste(
            b"v",
            true,
            Some(ctrl_v)
        ));
    }

    #[cfg(unix)]
    struct TempImageFile {
        path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl TempImageFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            Self::with_name_fragment("test", extension, bytes)
        }

        fn with_name_fragment(name_fragment: &str, extension: &str, bytes: &[u8]) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "herdr-client-drop-{name_fragment}-{}-{nanos}.{extension}",
                std::process::id()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self { path }
        }
    }

    #[cfg(unix)]
    impl Drop for TempImageFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_reads_bracketed_absolute_image_path() {
        let file = TempImageFile::new("PNG", b"image-bytes");
        let input = format!("\x1b[200~{}\x1b[201~", file.path.display());

        let image = read_image_file_from_terminal_drop(input.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "png");
        assert_eq!(image.bytes, b"image-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_reads_plain_quoted_path_with_newline() {
        let file = TempImageFile::new("jpeg", b"jpeg-bytes");
        let input = format!("'{}'\n", file.path.display());

        let image = read_image_file_from_terminal_drop(input.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "jpg");
        assert_eq!(image.bytes, b"jpeg-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_unescapes_spaces_in_paths() {
        let file = TempImageFile::with_name_fragment("space test", "png", b"image-bytes");
        let escaped_path = file.path.display().to_string().replace(' ', "\\ ");

        let image = read_image_file_from_terminal_drop(escaped_path.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "png");
        assert_eq!(image.bytes, b"image-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_ignores_non_remote_and_non_image_input() {
        let file = TempImageFile::new("png", b"image-bytes");
        let path = file.path.display().to_string();

        assert!(read_image_file_from_terminal_drop(path.as_bytes(), false).is_none());
        assert!(read_image_file_from_terminal_drop(b"relative.png\n", true).is_none());
        assert!(read_image_file_from_terminal_drop(b"/tmp/file.txt\n", true).is_none());
        assert!(read_image_file_from_terminal_drop(
            format!("{}\nextra", file.path.display()).as_bytes(),
            true
        )
        .is_none());
    }

    #[test]
    fn graphics_bytes_are_written_after_blit_with_saved_cursor() {
        let mut output = Vec::new();
        write_encoded_frame_with_graphics(
            &mut output,
            b"\x1b[?2026htext\x1b[?2026lcursor",
            b"graphics",
        )
        .unwrap();

        assert_eq!(
            output,
            b"\x1b[?2026htext\x1b[?2026lcursor\x1b7graphics\x1b8"
        );
    }

    #[test]
    fn empty_graphics_writes_only_blit_frame() {
        let mut output = Vec::new();
        write_encoded_frame_with_graphics(&mut output, b"text", b"").unwrap();

        assert_eq!(output, b"text");
    }

    #[test]
    fn terminal_frame_kitty_detection_matches_apc_prefix() {
        assert!(contains_kitty_graphics_bytes(b"text\x1b_Ga=p;\x1b\\"));
        assert!(!contains_kitty_graphics_bytes(b"text\x1b[?2026h"));
    }

    #[test]
    fn kitty_graphics_image_id_parser_tracks_herdr_ids_only() {
        let ids = kitty_graphics_image_ids(
            b"text\x1b_Ga=t,t=d,f=32,s=1,v=1,i=10023,q=2;AAAA\x1b\\\x1b_Ga=p,i=10023,p=7;\x1b\\",
        );
        assert_eq!(ids, vec![10023, 10023]);
    }

    #[test]
    fn kitty_graphics_cleanup_deletes_tracked_images_not_all_images() {
        record_received_kitty_graphics(b"\x1b_Ga=t,i=123,q=2;AAAA\x1b\\");
        let mut output = Vec::new();
        clear_received_kitty_graphics(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("a=d,d=I,i=123"));
        assert!(!text.contains("d=A"));
    }

    #[test]
    fn write_host_terminal_theme_query_emits_osc_queries() {
        let mut output = Vec::new();
        write_host_terminal_theme_query(&mut output).unwrap();
        assert_eq!(
            output,
            crate::terminal_theme::HOST_COLOR_QUERY_SEQUENCE.as_bytes()
        );
    }

    #[test]
    fn write_host_color_scheme_report_mode_emits_mode_sequences() {
        let mut output = Vec::new();
        write_host_color_scheme_report_mode(&mut output, true).unwrap();
        write_host_color_scheme_report_mode(&mut output, false).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE.as_bytes(),
        );
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        );
        assert_eq!(output, expected);
    }

    #[test]
    fn color_scheme_change_event_requests_host_theme_query() {
        let events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[?997;1n");

        assert!(crate::raw_input::events_require_host_terminal_theme_query(
            &events
        ));
    }

    #[test]
    fn host_terminal_theme_query_is_disabled_on_windows() {
        assert_eq!(should_query_host_terminal_theme(), !cfg!(windows));
    }

    #[test]
    fn color_scheme_reports_are_enabled_only_for_full_clients() {
        assert_eq!(
            should_enable_host_color_scheme_reports(true),
            !cfg!(windows)
        );
        assert!(!should_enable_host_color_scheme_reports(false));
    }

    #[test]
    fn terminal_restore_postlude_restores_visible_default_cursor() {
        let mut output = Vec::new();
        write_terminal_restore_postlude(&mut output, false).unwrap();
        assert_eq!(output, b"\x1b[?25h\x1b[0 q");
    }

    #[test]
    fn terminal_restore_postlude_disables_color_scheme_reports_when_enabled() {
        let mut output = Vec::new();
        write_terminal_restore_postlude(&mut output, true).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        );
        expected.extend_from_slice(b"\x1b[?25h\x1b[0 q");
        assert_eq!(output, expected);
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_detaches_on_prefix_q() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_sends_literal_prefix_on_double_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        match escape.filter_input(vec![0x02], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02]),
            other => panic!("expected forwarded prefix, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_forwards_prefix_before_non_escape_key() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![b'a', 0x02], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"a"
        ));
        match escape.filter_input(vec![b'x'], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02, b'x']),
            other => panic!("expected forwarded bytes, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_turns_wheel_into_scroll_action() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[<64;11;6M".to_vec(), 24, 7) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                column,
                row,
                ..
            } => {
                assert_eq!(source, AttachScrollSource::Wheel);
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 7);
                assert_eq!(column, Some(10));
                assert_eq!(row, Some(5));
            }
            other => panic!("expected scroll action, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_swallows_non_wheel_mouse_reports() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[<0;11;6M".to_vec(), 24, 7),
            AttachInputAction::None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_turns_plain_page_keys_into_scroll_actions() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5~".to_vec(), 12, 3) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                ..
            } => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[5~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-up scroll action, got {other:?}"),
        }

        match escape.filter_input(b"\x1b[6~".to_vec(), 12, 3) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                ..
            } => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[6~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Down);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-down scroll action, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_forwards_modified_page_key() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5;5~".to_vec(), 12, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, b"\x1b[5;5~"),
            other => panic!("expected modified page key to forward, got {other:?}"),
        }
    }

    #[test]
    fn client_error_display_connection_failed() {
        let err = ClientError::ConnectionFailed(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("failed to connect to server"),
            "should mention connection failure: {msg}"
        );
        assert!(
            msg.contains("herdr server"),
            "should suggest starting server: {msg}"
        );
    }

    #[test]
    fn client_error_display_handshake_rejected() {
        let err = ClientError::HandshakeRejected {
            version: 1,
            error: "incompatible".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("rejected handshake"),
            "should mention rejection: {msg}"
        );
        assert!(msg.contains("incompatible"), "should include error: {msg}");
    }

    #[test]
    fn client_error_display_server_shutdown() {
        let err = ClientError::ServerShutdown {
            reason: Some("maintenance".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("server shut down"),
            "should mention shutdown: {msg}"
        );
        assert!(msg.contains("maintenance"), "should include reason: {msg}");
    }

    #[test]
    fn client_error_display_server_shutdown_no_reason() {
        let err = ClientError::ServerShutdown { reason: None };
        let msg = err.to_string();
        assert!(
            msg.contains("server shut down"),
            "should mention shutdown: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_default_session_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarsRemovedGuard::new(&[
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            crate::session::SESSION_ENV_VAR,
        ]);
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr` to reattach"),
            "should suggest default reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_named_session_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
        let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr session attach work` to reattach"),
            "should suggest named session reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_remote_reattach_hint_takes_precedence() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarGuard::set(
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            "herdr --remote host --session work",
        );
        let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr --remote host --session work` to reattach"),
            "should prefer remote reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_connection_lost() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
        let err =
            ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
        let msg = err.to_string();
        assert!(
            msg.contains("lost connection to server"),
            "should mention lost connection: {msg}"
        );
    }

    #[test]
    fn client_error_display_remote_connection_lost_has_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarGuard::set(
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            "herdr --remote host --session work",
        );
        let err =
            ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
        let msg = err.to_string();
        assert!(
            msg.contains("lost connection to remote Herdr"),
            "should mention remote connection loss: {msg}"
        );
        assert!(
            msg.contains("panes may still be running"),
            "should explain possible persistence: {msg}"
        );
        assert!(
            msg.contains("Run `herdr --remote host --session work` to reattach"),
            "should show remote reattach command: {msg}"
        );
    }

    #[test]
    fn sound_from_notify_message_maps_done() {
        assert_eq!(
            sound_from_notify_message("agent done"),
            Some(crate::sound::Sound::Done)
        );
    }

    #[test]
    fn sound_from_notify_message_maps_attention() {
        assert_eq!(
            sound_from_notify_message("agent attention"),
            Some(crate::sound::Sound::Request)
        );
    }

    #[test]
    fn sound_from_notify_message_rejects_unknown_payloads() {
        assert_eq!(sound_from_notify_message("toast"), None);
    }

    #[test]
    fn reload_local_client_config_refreshes_local_client_presentation_state() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-client-config-reload-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[ui]\nredraw_on_focus_gained = false\nhost_cursor = \"drawn\"\n",
        )
        .unwrap();
        let path_string = path.to_string_lossy().to_string();
        let _env = EnvVarGuard::set(crate::config::CONFIG_PATH_ENV_VAR, &path_string);
        let mut sound_config = crate::config::SoundConfig::default();
        let mut redraw_on_focus_gained = true;
        let mut draw_host_cursor = false;
        #[cfg(unix)]
        let mut remote_image_paste_key = None;

        reload_local_client_config(
            &mut sound_config,
            &mut redraw_on_focus_gained,
            &mut draw_host_cursor,
            #[cfg(unix)]
            &mut remote_image_paste_key,
        );

        assert!(!redraw_on_focus_gained);
        assert!(draw_host_cursor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn toast_notify_from_server_is_emitted_even_when_attach_config_was_off() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::Toast,
            "pi finished",
            Some("workspace 1"),
            &sound_config,
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
            |_, _| Ok(false),
        );

        assert_eq!(
            emitted,
            Some(("pi finished".to_string(), Some("workspace 1".to_string())))
        );
    }

    #[test]
    fn system_toast_notify_from_server_uses_system_notifier() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::SystemToast,
            "pi finished",
            Some("workspace 1"),
            &sound_config,
            |_, _| Ok(false),
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
        );

        assert_eq!(
            emitted,
            Some(("pi finished".to_string(), Some("workspace 1".to_string())))
        );
    }

    #[test]
    fn system_toast_notify_preserves_colon_in_title() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::SystemToast,
            "build: failed",
            Some("api workspace"),
            &sound_config,
            |_, _| Ok(false),
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
        );

        assert_eq!(
            emitted,
            Some((
                "build: failed".to_string(),
                Some("api workspace".to_string())
            ))
        );
    }

    #[test]
    fn decode_clipboard_payload_decodes_base64() {
        assert_eq!(decode_clipboard_payload("dGVzdA=="), Some(b"test".to_vec()));
    }

    #[test]
    fn decode_clipboard_payload_rejects_invalid_base64() {
        assert_eq!(decode_clipboard_payload("not-base64!!!"), None);
    }

    #[test]
    fn terminal_control_input_command_accepts_text() {
        let action =
            terminal_control_command_from_json(r#"{"type":"terminal.input","text":"hello"}"#)
                .unwrap();
        let ClientMessage::Input { data } = action else {
            panic!("expected input command");
        };
        assert_eq!(data, b"hello");
    }

    #[test]
    fn terminal_control_input_command_accepts_base64_bytes() {
        let action =
            terminal_control_command_from_json(r#"{"type":"terminal.input","bytes":"G1tB"}"#)
                .unwrap();
        let ClientMessage::Input { data } = action else {
            panic!("expected input command");
        };
        assert_eq!(data, b"\x1b[A");
    }

    #[test]
    fn terminal_control_resize_command_maps_to_client_resize() {
        let action = terminal_control_command_from_json(
            r#"{"type":"terminal.resize","cols":100,"rows":30,"cell_width_px":8,"cell_height_px":16}"#,
        )
        .unwrap();
        let ClientMessage::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } = action
        else {
            panic!("expected resize command");
        };
        assert_eq!(
            (cols, rows, cell_width_px, cell_height_px),
            (100, 30, 8, 16)
        );
    }

    #[test]
    fn terminal_control_scroll_command_maps_to_attach_scroll() {
        let action = terminal_control_command_from_json(
            r#"{"type":"terminal.scroll","direction":"up","lines":3}"#,
        )
        .unwrap();
        let ClientMessage::AttachScroll {
            source,
            direction,
            lines,
            ..
        } = action
        else {
            panic!("expected scroll command");
        };
        assert_eq!(source, AttachScrollSource::Wheel);
        assert_eq!(direction, AttachScrollDirection::Up);
        assert_eq!(lines, 3);
    }

    #[test]
    fn forward_clipboard_uses_local_clipboard_path() {
        unsafe {
            std::env::set_var("SSH_CONNECTION", "1 2 3 4");
        }
        forward_clipboard("dGVzdA==");
        unsafe {
            std::env::remove_var("SSH_CONNECTION");
        }
    }

    #[test]
    fn window_title_osc_strips_terminators_and_defaults_to_herdr() {
        assert_eq!(
            window_title_osc(Some("herdr\x1b api\u{7}\u{9c}")),
            b"\x1b]0;herdr api\x07"
        );
        assert_eq!(window_title_osc(None), b"\x1b]0;herdr\x07");
    }
}
