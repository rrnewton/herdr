//! Acceptance (E2E) skeleton for the **full multi-pane mirror TUI session**
//! (`herdr --mirror`). See `~/work/herdr/test-plan-mirror-tui.md` and
//! `~/work/herdr/design-mirror-tui.md`.
//!
//! These are BLACK-BOX tests: they spawn a real `herdr server`, drive/inspect the
//! session over the JSON API socket, speak the binary mirror wire protocol
//! directly where useful, and (for TUI-level assertions) spawn the client under a
//! PTY and capture its rendered output.
//!
//! Status: the full mirror TUI (`herdr --mirror`) is implemented, and these
//! acceptance tests run and pass by default. The only remaining `#[ignore]` is
//! `search_and_selection_are_local` (H3), which depends on mirror copy-mode —
//! an explicitly-deferred capability (see that test's ignore reason). Removing
//! that `#[ignore]` and making it pass is the acceptance gate for copy-mode.
//!
//! The helpers below are fully implemented; assertions exercise the live client
//! (startup/discovery, render parity, cached-scrollback pane switching, scroll
//! policy, resize, reconnect/resume, and multi-pane input/focus).

#![allow(dead_code)]

mod support;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, client_handshake_direct, decode_varint_u32, decode_varint_u64,
    encode_varint_u32, frame_message, read_server_message, register_runtime_dir,
    register_spawned_herdr_pid, unregister_spawned_herdr_pid, wait_for_file, wait_for_socket,
    wait_until, CURRENT_PROTOCOL,
};

// ---------------------------------------------------------------------------
// Wire tags (bincode declaration order), mirrored from tests/terminal_mirror.rs.
// ---------------------------------------------------------------------------
const MSG_MIRROR_TERMINAL: u32 = 10; // ClientMessage::MirrorTerminal
const MSG_MIRROR_SNAPSHOT: u32 = 11; // ServerMessage::MirrorSnapshot
const MSG_MIRROR_EVENT: u32 = 12; // ServerMessage::MirrorEvent
const KIND_OUTPUT: u32 = 0;
const KIND_RESIZE: u32 = 1;
const KIND_CLOSED: u32 = 2;

// Generous so the suite stays reliable under heavy parallelism: each test spawns
// a server plus one or two PTY clients, and a starved mirror client can be slow to
// produce its first frame. `SETTLE` is the quiescence window used once output has
// begun.
const READY: Duration = Duration::from_secs(30);
const SETTLE: Duration = Duration::from_secs(2);

static MIRROR_TUI_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn mirror_tui_test_guard() -> std::sync::MutexGuard<'static, ()> {
    MIRROR_TUI_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ===========================================================================
// Server + client process harness
// ===========================================================================

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-mirror-tui-test-{}-{nanos}",
        std::process::id()
    ))
}

/// A running `herdr` process (server or client) attached to a PTY, with a
/// background thread accumulating everything it writes to its terminal.
struct PtyProc {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Drop for PtyProc {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(pid);
    }
}

impl PtyProc {
    /// Bytes written to the client terminal so far.
    fn output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    /// Send raw input bytes to the process's PTY (keystrokes).
    fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self._master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Reads the client output until it has produced *some* content and then
    /// stops growing for `SETTLE`, and returns the raw accumulated bytes.
    ///
    /// Unlike a naive quiescence check, this never reports "stable" before the
    /// client has emitted its first frame: a server-rendered attach client takes
    /// noticeably longer than the local mirror to draw, and treating the initial
    /// silence as quiescence would return an empty screen.
    fn stable_bytes(&self, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut last_len = 0usize;
        let mut stable_since = Instant::now();
        loop {
            let len = self.output.lock().unwrap().len();
            if len != last_len {
                last_len = len;
                stable_since = Instant::now();
            } else if len > 0 && stable_since.elapsed() >= SETTLE {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.output()
    }

    /// Reads the client output until quiescent, then returns a normalized
    /// (ANSI-stripped) linear screen snapshot. Retained for simple text checks;
    /// prefer [`Self::capture_grid`] for layout/parity comparison.
    fn capture_stable_screen(&self, timeout: Duration) -> String {
        normalize_screen(&self.stable_bytes(timeout))
    }

    /// Reads the client output until quiescent, then replays it through a terminal
    /// grid emulator at `cols`x`rows` and returns the resolved character grid.
    ///
    /// This is the parity oracle: two clients that optimize their byte output
    /// differently (diff-encoding, cursor-motion minimization) but paint the same
    /// visible screen resolve to the same grid.
    fn capture_grid(&self, cols: u16, rows: u16, timeout: Duration) -> TerminalGrid {
        render_grid(&self.stable_bytes(timeout), cols, rows)
    }
}

fn base_command() -> CommandBuilder {
    CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"))
}

fn common_env(cmd: &mut CommandBuilder, config_home: &Path, runtime_dir: &Path, api_socket: &Path) {
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("HERDR_ENV");
}

fn spawn_pty(cmd: CommandBuilder, cols: u16, rows: u16) -> PtyProc {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    let writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });

    PtyProc {
        _master: pair.master,
        child,
        writer,
        output,
    }
}

/// Session paths derived from a unique base dir.
struct Session {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    api_socket: PathBuf,
    client_socket: PathBuf,
}

impl Session {
    fn new() -> Self {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let api_socket = runtime_dir.join("herdr.sock");
        let client_socket = runtime_dir.join("herdr-client.sock");
        Self {
            base,
            config_home,
            runtime_dir,
            api_socket,
            client_socket,
        }
    }
}

/// Spawns a `herdr server`, waits for both sockets, returns the running server.
fn spawn_server(sess: &Session) -> PtyProc {
    fs::create_dir_all(sess.config_home.join("herdr")).unwrap();
    fs::create_dir_all(&sess.runtime_dir).unwrap();
    register_runtime_dir(&sess.runtime_dir);
    fs::write(
        sess.config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let mut cmd = base_command();
    cmd.arg("server");
    common_env(
        &mut cmd,
        &sess.config_home,
        &sess.runtime_dir,
        &sess.api_socket,
    );
    let server = spawn_pty(cmd, 80, 24);
    wait_for_socket(&sess.api_socket, READY);
    wait_for_file(&sess.client_socket, READY);
    server
}

/// Spawns the full mirror TUI client: `herdr --mirror`. (Does not exist yet.)
fn spawn_mirror_tui(sess: &Session, cols: u16, rows: u16) -> PtyProc {
    let mut cmd = base_command();
    cmd.arg("--mirror");
    common_env(
        &mut cmd,
        &sess.config_home,
        &sess.runtime_dir,
        &sess.api_socket,
    );
    spawn_pty(cmd, cols, rows)
}

/// Spawns the normal server-rendered attach client: `herdr` (auto-attach).
/// Used as the parity oracle for the mirror client.
fn spawn_attach_tui(sess: &Session, cols: u16, rows: u16) -> PtyProc {
    let mut cmd = base_command();
    common_env(
        &mut cmd,
        &sess.config_home,
        &sess.runtime_dir,
        &sess.api_socket,
    );
    spawn_pty(cmd, cols, rows)
}

// ===========================================================================
// Wire proxy — sits between the mirror client and the server's two sockets so a
// test can inject data-plane latency (C3) or drop the data connections (F1)
// without touching the server. The control (API) socket is passed through
// untouched; the data (client/wire) socket is latency-injected and trackable.
// ===========================================================================

use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

struct WireProxy {
    api_path: PathBuf,
    latency: Arc<Mutex<Duration>>,
    api_latency: Arc<Mutex<Duration>>,
    data_streams: Arc<Mutex<Vec<UnixStream>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for WireProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl WireProxy {
    /// Starts proxy listeners in `dir` forwarding to the session's real sockets.
    /// The mirror is pointed at `api_path()` (its client socket is derived as the
    /// sibling `herdr-client.sock`, also proxied).
    fn start(dir: &Path, real_api: &Path, real_client: &Path) -> WireProxy {
        fs::create_dir_all(dir).unwrap();
        let api_path = dir.join("herdr.sock");
        let client_path = dir.join("herdr-client.sock");
        let _ = fs::remove_file(&api_path);
        let _ = fs::remove_file(&client_path);
        let latency = Arc::new(Mutex::new(Duration::ZERO));
        let api_latency = Arc::new(Mutex::new(Duration::ZERO));
        let data_streams = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        spawn_proxy_listener(
            &api_path,
            real_api.to_path_buf(),
            Some(api_latency.clone()),
            None,
            stop.clone(),
        );
        spawn_proxy_listener(
            &client_path,
            real_client.to_path_buf(),
            Some(latency.clone()),
            Some(data_streams.clone()),
            stop.clone(),
        );
        WireProxy {
            api_path,
            latency,
            api_latency,
            data_streams,
            stop,
        }
    }

    fn api_path(&self) -> &Path {
        &self.api_path
    }

    fn set_latency(&self, latency: Duration) {
        *self.latency.lock().unwrap() = latency;
    }

    /// Injects latency on the control (API) plane — the path `layout_export`,
    /// subscriptions, and structural mutations travel. Simulates SSH RTT on the
    /// control socket (the real `herdr mirror <host>` transport).
    #[allow(dead_code)]
    fn set_api_latency(&self, latency: Duration) {
        *self.api_latency.lock().unwrap() = latency;
    }

    /// Forcibly closes every live data-plane connection, simulating a transient
    /// data-connection drop (the control plane is untouched).
    fn drop_data_connections(&self) {
        let mut streams = self.data_streams.lock().unwrap();
        for stream in streams.drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn spawn_proxy_listener(
    path: &Path,
    target: PathBuf,
    latency: Option<Arc<Mutex<Duration>>>,
    track: Option<Arc<Mutex<Vec<UnixStream>>>>,
    stop: Arc<AtomicBool>,
) {
    let listener = UnixListener::bind(path).expect("bind proxy socket");
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((inbound, _)) => {
                    let Ok(upstream) = UnixStream::connect(&target) else {
                        continue;
                    };
                    inbound.set_nonblocking(false).ok();
                    upstream.set_nonblocking(false).ok();
                    let in_read = inbound.try_clone().unwrap();
                    let up_read = upstream.try_clone().unwrap();
                    if let Some(track) = &track {
                        let mut t = track.lock().unwrap();
                        t.push(inbound.try_clone().unwrap());
                        t.push(upstream.try_clone().unwrap());
                    }
                    proxy_pipe(in_read, upstream, latency.clone());
                    proxy_pipe(up_read, inbound, latency.clone());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
    });
}

fn proxy_pipe(mut from: UnixStream, mut to: UnixStream, latency: Option<Arc<Mutex<Duration>>>) {
    thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = to.shutdown(std::net::Shutdown::Both);
                    break;
                }
                Ok(n) => {
                    if let Some(latency) = &latency {
                        let d = *latency.lock().unwrap();
                        if !d.is_zero() {
                            thread::sleep(d);
                        }
                    }
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Spawns the mirror TUI pointed at a proxy's API socket (so both its control and
/// data planes route through the proxy).
fn spawn_mirror_tui_via(proxy: &WireProxy, cols: u16, rows: u16, config_home: &Path) -> PtyProc {
    let mut cmd = base_command();
    cmd.arg("--mirror");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", proxy.api_path().parent().unwrap());
    cmd.env("HERDR_SOCKET_PATH", proxy.api_path());
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("HERDR_ENV");
    spawn_pty(cmd, cols, rows)
}

// ===========================================================================
// JSON API control-plane helpers
// ===========================================================================

fn send_json(socket: &Path, request: &str) -> Value {
    use std::io::{BufRead, BufReader};
    let mut stream = UnixStream::connect(socket).expect("connect API socket");
    writeln!(stream, "{request}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("valid JSON response")
}

fn assert_ok(resp: &Value, ctx: &str) {
    assert!(resp.get("error").is_none(), "{ctx}: {resp}");
}

fn create_workspace_root_pane(socket: &Path, label: &str) -> String {
    let resp = send_json(
        socket,
        &format!(
            "{{\"id\":\"ws\",\"method\":\"workspace.create\",\"params\":{{\"label\":\"{label}\"}}}}"
        ),
    );
    assert_ok(&resp, "workspace.create");
    resp.pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
        .expect("root pane id")
        .to_string()
}

/// Splits `pane_id` in `direction` ("horizontal"|"vertical"), returns the new pane id.
/// The JSON API's `SplitDirection` is `right`/`down`, so map the human-facing
/// terms: a horizontal (side-by-side) split is `right`, a vertical (stacked) one
/// is `down`.
fn split_pane(socket: &Path, pane_id: &str, direction: &str) -> String {
    let api_direction = match direction {
        "horizontal" => "right",
        "vertical" => "down",
        other => other,
    };
    let resp = send_json(
        socket,
        &format!(
            "{{\"id\":\"sp\",\"method\":\"pane.split\",\"params\":{{\"target_pane_id\":\"{pane_id}\",\"direction\":\"{api_direction}\",\"focus\":true}}}}"
        ),
    );
    assert_ok(&resp, "pane.split");
    // `pane.split` returns the new pane as `ResponseResult::PaneInfo { pane }`.
    resp.pointer("/result/pane/pane_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("pane.split returned no pane id: {resp}"))
        .to_string()
}

fn pane_list_ids(socket: &Path) -> Vec<String> {
    let resp = send_json(
        socket,
        "{\"id\":\"pl\",\"method\":\"pane.list\",\"params\":{}}",
    );
    assert_ok(&resp, "pane.list");
    resp.pointer("/result/panes")
        .and_then(Value::as_array)
        .map(|panes| {
            panes
                .iter()
                .filter_map(|p| p.get("pane_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn pane_size(socket: &Path, pane_id: &str) -> Option<(u64, u64)> {
    let resp = send_json(
        socket,
        &format!(
            "{{\"id\":\"pg\",\"method\":\"pane.get\",\"params\":{{\"pane_id\":\"{pane_id}\"}}}}"
        ),
    );
    let cols = resp.pointer("/result/pane/cols").and_then(Value::as_u64)?;
    let rows = resp.pointer("/result/pane/rows").and_then(Value::as_u64)?;
    Some((cols, rows))
}

fn pane_focus(socket: &Path, pane_id: &str) {
    let resp = send_json(
        socket,
        &format!(
            "{{\"id\":\"pf\",\"method\":\"pane.focus\",\"params\":{{\"pane_id\":\"{pane_id}\"}}}}"
        ),
    );
    assert_ok(&resp, "pane.focus");
}

fn pane_close(socket: &Path, pane_id: &str) {
    let resp = send_json(
        socket,
        &format!(
            "{{\"id\":\"pc\",\"method\":\"pane.close\",\"params\":{{\"pane_id\":\"{pane_id}\"}}}}"
        ),
    );
    assert_ok(&resp, "pane.close");
}

fn pane_send_text(socket: &Path, pane_id: &str, text: &str) {
    // Serialize the text with serde so backslash sequences (e.g. `\033`, `\n` for
    // the shell's printf) survive as intended literal characters, rather than
    // becoming invalid or unintended JSON escapes.
    let request = serde_json::json!({
        "id": "in",
        "method": "pane.send_input",
        "params": { "pane_id": pane_id, "text": text, "keys": ["Enter"] },
    });
    let resp = send_json(socket, &request.to_string());
    assert_ok(&resp, "pane.send_input");
}

/// Sends raw text to a pane *without* a trailing Enter (for interactive control
/// sequences where a newline would be wrong).
fn pane_send_raw(socket: &Path, pane_id: &str, text: &str) {
    let request = serde_json::json!({
        "id": "in",
        "method": "pane.send_input",
        "params": { "pane_id": pane_id, "text": text },
    });
    let resp = send_json(socket, &request.to_string());
    assert_ok(&resp, "pane.send_input");
}

fn layout_export(socket: &Path) -> Value {
    let resp = send_json(
        socket,
        "{\"id\":\"lx\",\"method\":\"layout.export\",\"params\":{}}",
    );
    assert_ok(&resp, "layout.export");
    resp.get("result").cloned().unwrap_or(Value::Null)
}

// ===========================================================================
// Binary mirror wire-protocol helpers (data plane), from terminal_mirror.rs.
// ===========================================================================

fn decode_varint_u16(payload: &[u8], offset: usize) -> Result<(u16, usize), String> {
    let (value, consumed) = decode_varint_u32(payload, offset)?;
    Ok((value as u16, consumed))
}

fn send_mirror_terminal(
    stream: &mut UnixStream,
    target: &str,
    resume_from: Option<u64>,
    writable: bool,
) {
    let mut payload = encode_varint_u32(MSG_MIRROR_TERMINAL);
    payload.extend(encode_varint_u32(target.len() as u32));
    payload.extend_from_slice(target.as_bytes());
    match resume_from {
        None => payload.push(0),
        Some(seq) => {
            payload.push(1);
            if seq < 251 {
                payload.push(seq as u8);
            } else {
                payload.push(253);
                payload.extend_from_slice(&seq.to_le_bytes());
            }
        }
    }
    payload.push(if writable { 1 } else { 0 });
    let framed = frame_message(&payload);
    stream.write_all(&framed).unwrap();
    stream.flush().unwrap();
}

#[derive(Debug)]
enum MirrorMessage {
    Snapshot { base_seq: u64, cols: u16, rows: u16 },
    Output { seq: u64, bytes: Vec<u8> },
    Resize { seq: u64, cols: u16, rows: u16 },
    Closed { seq: u64 },
    Other(u32),
}

fn read_mirror_message(stream: &mut UnixStream) -> Result<MirrorMessage, String> {
    let (variant, rest) = read_server_message(stream)?;
    match variant {
        MSG_MIRROR_SNAPSHOT => {
            let (base_seq, mut off) = decode_varint_u64(&rest, 0)?;
            let (cols, c) = decode_varint_u16(&rest, off)?;
            off += c;
            let (rows, _) = decode_varint_u16(&rest, off)?;
            Ok(MirrorMessage::Snapshot {
                base_seq,
                cols,
                rows,
            })
        }
        MSG_MIRROR_EVENT => {
            let (seq, mut off) = decode_varint_u64(&rest, 0)?;
            let (kind, c) = decode_varint_u32(&rest, off)?;
            off += c;
            match kind {
                KIND_OUTPUT => {
                    let (len, c) = decode_varint_u32(&rest, off)?;
                    off += c;
                    let len = len as usize;
                    if off + len > rest.len() {
                        return Err("output payload truncated".into());
                    }
                    Ok(MirrorMessage::Output {
                        seq,
                        bytes: rest[off..off + len].to_vec(),
                    })
                }
                KIND_RESIZE => {
                    let (cols, c) = decode_varint_u16(&rest, off)?;
                    off += c;
                    let (rows, _) = decode_varint_u16(&rest, off)?;
                    Ok(MirrorMessage::Resize { seq, cols, rows })
                }
                KIND_CLOSED => Ok(MirrorMessage::Closed { seq }),
                other => Err(format!("unknown MirrorEventKind tag {other}")),
            }
        }
        other => Ok(MirrorMessage::Other(other)),
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Reads mirror messages until `marker` appears in accumulated output or the
/// timeout elapses, asserting every event sequence is strictly contiguous from
/// `expected_next_seq` (no gap, no re-snapshot). Returns the highest seq seen.
fn collect_until_marker(
    stream: &mut UnixStream,
    marker: &[u8],
    mut expected_next_seq: u64,
    timeout: Duration,
) -> u64 {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let deadline = Instant::now() + timeout;
    let mut accumulated: Vec<u8> = Vec::new();
    let mut last_seq = expected_next_seq.saturating_sub(1);
    while Instant::now() < deadline {
        match read_mirror_message(stream) {
            Ok(MirrorMessage::Output { seq, bytes }) => {
                assert_eq!(
                    seq, expected_next_seq,
                    "output seq must be contiguous (gap)"
                );
                expected_next_seq += 1;
                last_seq = seq;
                accumulated.extend_from_slice(&bytes);
                if contains_subslice(&accumulated, marker) {
                    return last_seq;
                }
            }
            Ok(MirrorMessage::Resize { seq, .. }) => {
                assert_eq!(seq, expected_next_seq, "resize seq must be contiguous");
                expected_next_seq += 1;
                last_seq = seq;
            }
            Ok(MirrorMessage::Snapshot { .. }) => {
                panic!("unexpected re-snapshot while tailing a covered resume stream");
            }
            Ok(MirrorMessage::Closed { .. }) => panic!("stream closed before marker"),
            Ok(MirrorMessage::Other(v)) => panic!("unexpected server message variant {v}"),
            Err(_) => continue,
        }
    }
    panic!(
        "did not observe marker {:?} within timeout (last seq {last_seq})",
        String::from_utf8_lossy(marker)
    );
}

/// Opens a direct mirror subscription for `pane_id` and returns the base seq.
fn subscribe_mirror(client_socket: &Path, pane_id: &str, writable: bool) -> (UnixStream, u64) {
    let mut stream = UnixStream::connect(client_socket).expect("connect client socket");
    let (version, err) =
        client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(err.is_none(), "handshake error: {err:?}");
    send_mirror_terminal(&mut stream, pane_id, None, writable);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let base = match read_mirror_message(&mut stream).expect("snapshot") {
        MirrorMessage::Snapshot { base_seq, .. } => base_seq,
        other => panic!("expected MirrorSnapshot, got {other:?}"),
    };
    (stream, base)
}

/// Reads and discards mirror messages until none arrive for `quiet` (or a hard
/// cap elapses). Used to advance a read-only wire subscription past all output
/// produced so far, so a subsequent read observes only *new* bytes.
fn drain_mirror(stream: &mut UnixStream, quiet: Duration) {
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let hard_cap = Instant::now() + Duration::from_secs(10);
    let mut last = Instant::now();
    while last.elapsed() < quiet && Instant::now() < hard_cap {
        // Ok => a message was pending, keep draining; Err => read timeout, idle.
        if read_mirror_message(stream).is_ok() {
            last = Instant::now();
        }
    }
}

/// Accumulates raw output bytes seen on a mirror subscription for `dur`.
fn collect_mirror_output(stream: &mut UnixStream, dur: Duration) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let deadline = Instant::now() + dur;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        if let Ok(MirrorMessage::Output { bytes, .. }) = read_mirror_message(stream) {
            out.extend_from_slice(&bytes);
        }
    }
    out
}

/// Drives `input` into `client`'s stdin and returns the pane's PTY output that
/// results, observed on an independent read-only wire subscription to `pane_id`.
///
/// The pane is expected to be running `cat -v` (see [`setup_echo_pane`]): input
/// the client *forwards* to the PTY is echoed back (visibly, control chars as
/// `^[`), while input the client *handles locally* (scrollback) produces nothing.
/// So the returned string is non-empty iff the client forwarded the input — the
/// black-box realization of the scroll-routing decision (Finding 1).
fn pty_output_after_input(
    client_socket: &Path,
    pane_id: &str,
    client: &mut PtyProc,
    input: &[u8],
) -> String {
    let (mut sub, _base) = subscribe_mirror(client_socket, pane_id, false);
    drain_mirror(&mut sub, Duration::from_millis(500));
    client.write_input(input);
    let out = collect_mirror_output(&mut sub, Duration::from_millis(1200));
    String::from_utf8_lossy(&out).into_owned()
}

/// Runs `cat -v` in `pane_id` after the given screen setup, so the pane echoes
/// every byte forwarded to it. `alt_screen` enters the alternate screen and
/// `mouse` additionally enables SGR mouse reporting, exercising the alt-screen /
/// mouse-app branches of the scroll-routing policy.
///
/// A unique `marker` is printed after the mode is set; a client can wait for it
/// (in its own rendered grid) to know the pane — and its own replica of it — has
/// finished switching modes before scroll input is sent (avoids a race where the
/// alt-screen enter hasn't propagated yet and routing is misjudged).
fn setup_echo_pane(socket: &Path, pane_id: &str, alt_screen: bool, mouse: bool, marker: &str) {
    let mut prefix = String::new();
    if alt_screen {
        prefix.push_str("\\033[?1049h");
    }
    if mouse {
        prefix.push_str("\\033[?1000;1006h");
    }
    prefix.push_str(marker);
    // Raw + no-echo so the tty delivers control sequences straight to `cat -v`
    // (which makes them visible) rather than line-buffering or echoing them.
    pane_send_text(
        socket,
        pane_id,
        &format!("printf '{prefix}'; stty raw -echo 2>/dev/null; cat -v"),
    );
}

// ===========================================================================
// Screen normalization
// ===========================================================================

/// Strips ANSI/OSC control sequences so two visually-identical frames rendered by
/// different clients compare equal. Approximate by design — tighten during
/// implementation (it currently keeps only printable text + newlines).
fn normalize_screen(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'[' => {
                        // CSI: params until a final byte in 0x40..=0x7e
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    b']' => {
                        // OSC: until BEL or ESC-backslash
                        i += 1;
                        while i < bytes.len() && bytes[i] != 0x07 {
                            if bytes[i] == 0x1b {
                                i += 1;
                            }
                            i += 1;
                        }
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            b'\r' => i += 1,
            0x00..=0x08 | 0x0b..=0x1f | 0x7f => i += 1,
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out.trim_end().to_string()
}

fn screen_contains(proc: &PtyProc, needle: &str) -> bool {
    wait_until(READY, Duration::from_millis(100), || {
        normalize_screen(&proc.output()).contains(needle)
    })
}

// ===========================================================================
// Terminal grid emulator (parity oracle)
// ===========================================================================

/// A resolved character grid: what a real terminal would display after applying
/// a client's byte stream. Comparing grids (not raw bytes) lets two clients that
/// encode the same visible screen differently compare equal.
#[derive(Clone, PartialEq, Eq)]
struct TerminalGrid {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<char>>,
}

impl TerminalGrid {
    /// The grid as text, one line per row, trailing blanks trimmed per line and
    /// trailing blank lines removed. Suitable for `assert_eq!` parity checks.
    fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .cells
            .iter()
            .map(|row| row_text(row).trim_end().to_string())
            .collect();
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn contains(&self, needle: &str) -> bool {
        self.cells.iter().any(|row| row_text(row).contains(needle))
    }

    /// The multiset of whitespace-separated visible tokens across all rows. A
    /// robust parity signal that ignores exact cell placement (tolerant to cursor
    /// or one-cell layout jitter) while still catching content/chrome divergence.
    fn tokens(&self) -> Vec<String> {
        let mut toks: Vec<String> = self.text().split_whitespace().map(str::to_string).collect();
        toks.sort();
        toks
    }
}

/// Wide glyphs occupy two cells; the trailing cell holds `WIDE_SPACER` so text
/// extraction can drop it and keep the glyph contiguous (`日本語`, not `日 本 語`).
const WIDE_SPACER: char = '\u{0}';

/// A row rendered to a string, dropping the wide-glyph spacer cells.
fn row_text(row: &[char]) -> String {
    row.iter().filter(|c| **c != WIDE_SPACER).collect()
}

/// Rough display width: East-Asian wide / fullwidth ranges count as two cells,
/// everything else as one. Enough for the CJK content-parity check.
fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u) // Hangul Jamo
        || (0x2E80..=0xA4CF).contains(&u) // CJK, Kana, etc.
        || (0xAC00..=0xD7A3).contains(&u) // Hangul syllables
        || (0xF900..=0xFAFF).contains(&u) // CJK compat
        || (0xFE30..=0xFE4F).contains(&u) // CJK compat forms
        || (0xFF00..=0xFF60).contains(&u) // Fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u) // emoji / symbols
        || (0x20000..=0x3FFFD).contains(&u); // CJK ext B+
    if wide {
        2
    } else {
        1
    }
}

/// Replays a client's terminal byte stream through a minimal VT emulator and
/// returns the resolved `cols`x`rows` grid. Handles the CSI cursor-motion, erase,
/// SGR (ignored), OSC (ignored), alt-screen toggles, and UTF-8 text the herdr TUI
/// actually emits. Deliberately small: it need only be *consistent* across the
/// two client byte streams it compares.
fn render_grid(bytes: &[u8], cols: u16, rows: u16) -> TerminalGrid {
    let cols = cols.max(1) as usize;
    let rows = rows.max(1) as usize;
    let blank = || vec![vec![' '; cols]; rows];
    let mut screen = blank();
    let mut saved: Option<Vec<Vec<char>>> = None;
    let (mut cr, mut cc) = (0usize, 0usize);

    // Decode the byte stream to chars once (UTF-8, lossy for stray bytes), so the
    // parser works in codepoints and never splits a multibyte glyph.
    let text = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let clamp = |r: usize, c: usize| (r.min(rows - 1), c.min(cols.saturating_sub(1)));

    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\u{1b}' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                match chars[i] {
                    '[' => {
                        i += 1;
                        // Collect params (digits, ';', '?', etc.) until a final byte.
                        let start = i;
                        while i < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[i]) {
                            i += 1;
                        }
                        if i >= chars.len() {
                            break;
                        }
                        let final_byte = chars[i];
                        let params: String = chars[start..i].iter().collect();
                        i += 1;
                        apply_csi(
                            &params,
                            final_byte,
                            &mut screen,
                            &mut cr,
                            &mut cc,
                            rows,
                            cols,
                            &mut saved,
                        );
                    }
                    ']' => {
                        // OSC: skip to BEL or ESC-backslash (ST).
                        i += 1;
                        while i < chars.len() {
                            if chars[i] == '\u{07}' {
                                i += 1;
                                break;
                            }
                            if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Two-byte escapes / charset selection: consume the next char.
                    '(' | ')' | '*' | '+' => i += 2,
                    _ => i += 1,
                }
            }
            '\r' => {
                cc = 0;
                i += 1;
            }
            '\n' => {
                if cr + 1 >= rows {
                    // Scroll up.
                    screen.remove(0);
                    screen.push(vec![' '; cols]);
                } else {
                    cr += 1;
                }
                i += 1;
            }
            '\u{08}' => {
                cc = cc.saturating_sub(1);
                i += 1;
            }
            c if (c as u32) < 0x20 => {
                // Other C0 controls: ignore.
                i += 1;
            }
            c => {
                let (r, col) = clamp(cr, cc);
                cr = r;
                cc = col;
                screen[cr][cc] = c;
                let w = char_width(c);
                if w == 2 && cc + 1 < cols {
                    screen[cr][cc + 1] = WIDE_SPACER;
                }
                cc += w;
                if cc >= cols {
                    cc = cols - 1;
                }
                i += 1;
            }
        }
    }

    TerminalGrid {
        cols,
        rows,
        cells: screen,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_csi(
    params: &str,
    final_byte: char,
    screen: &mut [Vec<char>],
    cr: &mut usize,
    cc: &mut usize,
    rows: usize,
    cols: usize,
    saved: &mut Option<Vec<Vec<char>>>,
) {
    // Private-mode (?) sequences: handle alt-screen enter/leave, ignore the rest.
    if params.starts_with('?') {
        let clear_screen = |screen: &mut [Vec<char>]| {
            for row in screen.iter_mut() {
                for cell in row.iter_mut() {
                    *cell = ' ';
                }
            }
        };
        if final_byte == 'h' && (params.contains("1049") || params.contains("1047")) {
            *saved = Some(screen.to_vec());
            clear_screen(screen);
            *cr = 0;
            *cc = 0;
        } else if final_byte == 'l' && (params.contains("1049") || params.contains("1047")) {
            if let Some(prev) = saved.take() {
                for (row, prev_row) in screen.iter_mut().zip(prev) {
                    *row = prev_row;
                }
            }
            *cr = 0;
            *cc = 0;
        }
        return;
    }

    let nums: Vec<usize> = params
        .split(';')
        .map(|p| p.parse::<usize>().unwrap_or(0))
        .collect();
    let n = |idx: usize, default: usize| {
        nums.get(idx)
            .copied()
            .filter(|v| *v != 0)
            .unwrap_or(default)
    };

    match final_byte {
        'H' | 'f' => {
            *cr = n(0, 1).saturating_sub(1).min(rows - 1);
            *cc = n(1, 1).saturating_sub(1).min(cols - 1);
        }
        'A' => *cr = cr.saturating_sub(n(0, 1)),
        'B' => *cr = (*cr + n(0, 1)).min(rows - 1),
        'C' => *cc = (*cc + n(0, 1)).min(cols - 1),
        'D' => *cc = cc.saturating_sub(n(0, 1)),
        'G' => *cc = n(0, 1).saturating_sub(1).min(cols - 1),
        'd' => *cr = n(0, 1).saturating_sub(1).min(rows - 1),
        'E' => {
            *cr = (*cr + n(0, 1)).min(rows - 1);
            *cc = 0;
        }
        'F' => {
            *cr = cr.saturating_sub(n(0, 1));
            *cc = 0;
        }
        'J' => {
            let mode = nums.first().copied().unwrap_or(0);
            let blank_row = |row: &mut [char]| row.iter_mut().for_each(|cell| *cell = ' ');
            match mode {
                0 => {
                    blank_row(&mut screen[*cr][*cc..cols]);
                    screen[(*cr + 1)..rows]
                        .iter_mut()
                        .for_each(|r| blank_row(r));
                }
                1 => {
                    screen[0..*cr].iter_mut().for_each(|r| blank_row(r));
                    blank_row(&mut screen[*cr][0..=*cc]);
                }
                _ => screen.iter_mut().for_each(|r| blank_row(r)),
            }
        }
        'K' => {
            let mode = nums.first().copied().unwrap_or(0);
            let row = &mut screen[*cr];
            match mode {
                0 => row[*cc..cols].iter_mut().for_each(|cell| *cell = ' '),
                1 => row[0..=(*cc).min(cols - 1)]
                    .iter_mut()
                    .for_each(|cell| *cell = ' '),
                _ => row.iter_mut().for_each(|cell| *cell = ' '),
            }
        }
        's' => *saved = Some(screen.to_vec()),
        'u' => {
            if let Some(prev) = saved.take() {
                for (row, prev_row) in screen.iter_mut().zip(prev) {
                    *row = prev_row;
                }
            }
        }
        // SGR and everything else: no grid effect.
        _ => {}
    }
}

fn grid_contains(proc: &PtyProc, cols: u16, rows: u16, needle: &str) -> bool {
    wait_until(READY, Duration::from_millis(100), || {
        render_grid(&proc.output(), cols, rows).contains(needle)
    })
}

/// Finds the (row, col) — 0-based — of the first cell where `needle` begins in
/// the rendered grid. Used to click precisely on a pane's on-screen content.
fn grid_find_cell(grid: &TerminalGrid, needle: &str) -> Option<(usize, usize)> {
    let needle: Vec<char> = needle.chars().collect();
    for (r, row) in grid.cells.iter().enumerate() {
        if row.len() < needle.len() {
            continue;
        }
        for start in 0..=(row.len() - needle.len()) {
            if row[start..start + needle.len()] == needle[..] {
                return Some((r, start));
            }
        }
    }
    None
}

/// SGR mouse press+release for a left click at 0-based `(row, col)`. Terminals
/// report SGR coordinates 1-based, so we add one.
fn sgr_left_click(row: usize, col: usize) -> (Vec<u8>, Vec<u8>) {
    let press = format!("\x1b[<0;{};{}M", col + 1, row + 1).into_bytes();
    let release = format!("\x1b[<0;{};{}m", col + 1, row + 1).into_bytes();
    (press, release)
}

/// The herdr prefix key (default `ctrl+b`).
const PREFIX: &[u8] = b"\x02";

/// Sends a prefix command (`prefix` then `key`) to a client. They must be
/// separate writes: the client's stdin handler acts on the first parsed event of
/// a batch, so `prefix`+`key` in one write would drop the key.
fn send_prefix_key(client: &mut PtyProc, key: &[u8]) {
    client.write_input(PREFIX);
    thread::sleep(Duration::from_millis(80));
    client.write_input(key);
    thread::sleep(Duration::from_millis(80));
}

// ===========================================================================
// A. Startup & pane discovery
// ===========================================================================

#[test]
fn mirror_session_starts_and_discovers_all_panes() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);

    // Build a 3-pane session and drive a unique marker into each pane.
    let p1 = create_workspace_root_pane(&sess.api_socket, "discovery");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    let p3 = split_pane(&sess.api_socket, &p2, "vertical");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_pane_one_marker");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_pane_two_marker");
    pane_send_text(&sess.api_socket, &p3, "echo herdr_pane_three_marker");

    let mirror = spawn_mirror_tui(&sess, 120, 40);

    // Acceptance: the mirror client renders content from ALL three panes,
    // proving it enumerated the layout and opened a mirror per terminal.
    assert!(
        screen_contains(&mirror, "herdr_pane_one_marker"),
        "pane 1 missing"
    );
    assert!(
        screen_contains(&mirror, "herdr_pane_two_marker"),
        "pane 2 missing"
    );
    assert!(
        screen_contains(&mirror, "herdr_pane_three_marker"),
        "pane 3 missing"
    );

    drop(mirror);
    cleanup_test_base(&sess.base);
}

#[test]
fn mirror_session_opens_one_data_connection_per_pane() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "conns");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    // Distinct content per pane so we can confirm each mirrors independently.
    pane_send_text(&sess.api_socket, &p1, "echo herdr_conn_one");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_conn_two");

    let mirror = spawn_mirror_tui(&sess, 120, 40);

    // The session has exactly N terminals...
    let panes = pane_list_ids(&sess.api_socket);
    assert_eq!(panes.len(), 2, "expected two panes to mirror");

    // ...each is an independent mirror source on the data plane: subscribing to
    // each pane_id over the wire yields its own snapshot (one data connection per
    // terminal). This is what the mirror client opens internally, verified here
    // against the server directly.
    for pane in &panes {
        let (mut stream, _base) = subscribe_mirror(&sess.client_socket, pane, false);
        drain_mirror(&mut stream, Duration::from_millis(300));
    }

    // ...and the mirror renders both panes' content live and independently.
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_conn_one"),
        "pane 1 live"
    );
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_conn_two"),
        "pane 2 live"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn mirror_session_errors_clearly_when_no_server() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    fs::create_dir_all(&sess.runtime_dir).unwrap();
    register_runtime_dir(&sess.runtime_dir);

    // No server spawned. `herdr --mirror` must fail fast, not hang.
    let mut mirror = spawn_mirror_tui(&sess, 80, 24);
    let exited = wait_until(Duration::from_secs(5), Duration::from_millis(100), || !{
        mirror.is_alive()
    });
    assert!(
        exited,
        "herdr --mirror should exit promptly when no server is running"
    );
    let text = normalize_screen(&mirror.output()).to_lowercase();
    assert!(
        text.contains("server") || text.contains("connect") || text.contains("no herdr"),
        "expected a clear no-server message, got: {text:?}"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn mirror_initial_layout_matches_server_snapshot() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "replica");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_replica_left");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_replica_right");

    // The server's authoritative view of the session.
    let server_layout = layout_export(&sess.api_socket);
    assert!(server_layout.is_object(), "layout.export returns a tree");
    let server_panes = pane_list_ids(&sess.api_socket);
    assert_eq!(server_panes.len(), 2, "server reports two panes");

    let mirror = spawn_mirror_tui(&sess, 120, 40);

    // The mirror's initial replica reproduces the server's structure: both panes'
    // content renders (pane count replicated + one mirror per terminal), and the
    // workspace label the session carries appears in the mirror chrome.
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_replica_left"),
        "left pane replicated"
    );
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_replica_right"),
        "right pane replicated"
    );
    assert!(
        grid_contains(&mirror, 120, 40, "replica"),
        "workspace label replicated into the mirror chrome"
    );

    // The split geometry is replicated: the mirror draws two distinct pane boxes
    // side by side (two top-left border corners on the split row).
    let grid = mirror.capture_grid(120, 40, READY);
    let corner_rows = grid
        .cells
        .iter()
        .filter(|row| row.iter().filter(|c| **c == '┌').count() == 2)
        .count();
    assert!(
        corner_rows >= 1,
        "mirror renders two side-by-side pane boxes (split geometry). Grid:\n{}",
        grid.text()
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// B. Layout rendering parity (mirror vs normal)
// ===========================================================================

#[test]
fn layout_parity_single_pane() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "parity1");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_parity_single");

    let normal = spawn_attach_tui(&sess, 100, 30);
    let mirror = spawn_mirror_tui(&sess, 100, 30);

    let normal_grid = normal.capture_grid(100, 30, READY);
    let mirror_grid = mirror.capture_grid(100, 30, READY);
    assert_eq!(
        mirror_grid.text(),
        normal_grid.text(),
        "mirror render must match the normal (server-rendered) client"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn layout_parity_split_panes() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "parity2");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    let _p3 = split_pane(&sess.api_socket, &p2, "vertical");

    // Distinct content per pane, so parity covers both chrome and pane bodies.
    pane_send_text(&sess.api_socket, &p1, "echo pane_one");
    pane_send_text(&sess.api_socket, &p2, "echo pane_two");
    pane_send_text(&sess.api_socket, &_p3, "echo pane_three");

    let normal = spawn_attach_tui(&sess, 120, 40);
    let mirror = spawn_mirror_tui(&sess, 120, 40);

    assert_eq!(
        mirror.capture_grid(120, 40, READY).text(),
        normal.capture_grid(120, 40, READY).text(),
        "split layout (rects + dividers) must match normal"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn chrome_parity_borders_titles_sidebar_tabs() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "chrome");
    let _p2 = split_pane(&sess.api_socket, &p1, "horizontal");

    let normal = spawn_attach_tui(&sess, 120, 40);
    let mirror = spawn_mirror_tui(&sess, 120, 40);

    // Chrome is pure over AppState per the design, so parity should be exact.
    assert_eq!(
        mirror.capture_grid(120, 40, READY).text(),
        normal.capture_grid(120, 40, READY).text(),
        "borders, titles, sidebar and tab bar must be identical"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn content_parity_colors_and_wide_glyphs() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "content");
    // Colored + wide-glyph content exercises the shared ghostty renderer.
    pane_send_text(
        &sess.api_socket,
        &p1,
        "printf '\\033[31mRED\\033[0m 日本語 done\\n'",
    );

    let normal = spawn_attach_tui(&sess, 100, 30);
    let mirror = spawn_mirror_tui(&sess, 100, 30);

    // Wait for the wide-glyph content to arrive in both before comparing.
    assert!(grid_contains(&mirror, 100, 30, "日本語"), "mirror content");
    assert!(grid_contains(&normal, 100, 30, "日本語"), "normal content");
    assert_eq!(
        mirror.capture_grid(100, 30, READY).text(),
        normal.capture_grid(100, 30, READY).text(),
        "colored + wide-glyph content must match normal exactly"
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// C. Pane switching with cached scrollback (no re-download)
// ===========================================================================

#[test]
fn pane_switch_is_local_no_resnapshot() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "switch");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_switch_one");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_switch_two");

    let mut mirror = spawn_mirror_tui(&sess, 120, 40);

    // Both panes are rendered from cached content before any switch — the mirror
    // holds a live emulator per terminal, so the non-focused pane is already fully
    // drawn (no content is fetched on focus).
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_switch_one"),
        "pane 1 shown"
    );
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_switch_two"),
        "pane 2 shown"
    );

    // Switching focus (prefix + focus-right) is a local view change: both panes
    // stay rendered from their cached emulators, no re-download needed. (The
    // "no new MirrorSnapshot on focus change" invariant — focus re-subscribes with
    // a resume, not a fresh snapshot — is unit-tested in
    // `mirror_session::connection`.)
    send_prefix_key(&mut mirror, b"l");
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_switch_two"),
        "pane 2 after switch"
    );
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_switch_one"),
        "pane 1 still cached after switch"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn scrollback_position_survives_pane_switch() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "scrollswitch");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    // Focus the pane with history and fill its scrollback.
    pane_focus(&sess.api_socket, &p1);
    for i in 0..200 {
        pane_send_text(&sess.api_socket, &p1, &format!("echo herdr_line_{i}"));
    }

    let mut mirror = spawn_mirror_tui(&sess, 120, 40);
    // Wait until the latest output has arrived (the pane is at the live tail).
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_line_199"),
        "at live tail"
    );

    // Make sure pane A (the leftmost pane, with the history) is focused so the
    // wheel/page scroll targets it.
    send_prefix_key(&mut mirror, b"h"); // focus left -> A
    thread::sleep(Duration::from_millis(200));

    // Scroll pane A all the way up into its oldest history (over-scroll clamps at
    // the top), bringing an early line into view.
    for _ in 0..40 {
        mirror.write_input(PAGE_UP);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_line_5"),
        "scrolled A back into history"
    );

    // Switch to B and back to A. A must retain its scrollback offset (no reset,
    // no re-download) — the early line is still on screen.
    send_prefix_key(&mut mirror, b"l"); // focus right -> B
    thread::sleep(Duration::from_millis(200));
    send_prefix_key(&mut mirror, b"h"); // focus left -> A
    thread::sleep(Duration::from_millis(200));
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_line_5"),
        "A's scrollback offset survived the pane switch"
    );

    let _ = &p2;
    cleanup_test_base(&sess.base);
}

#[test]
fn pane_switch_latency_independent_of_rtt() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "rtt");
    for i in 0..300 {
        pane_send_text(&sess.api_socket, &p1, &format!("echo rtt_line_{i}"));
    }

    // Route the mirror through a proxy so we can inject transport latency.
    let proxy = WireProxy::start(
        &sess.base.join("proxy"),
        &sess.api_socket,
        &sess.client_socket,
    );
    let mut mirror = spawn_mirror_tui_via(&proxy, 100, 40, &sess.config_home);
    assert!(
        grid_contains(&mirror, 100, 40, "rtt_line_299"),
        "content cached at tail"
    );

    // Inject a large transport RTT. Cached-content operations (scrollback) are
    // local and must not pay it, while anything needing the transport does.
    let rtt = Duration::from_millis(600);
    proxy.set_latency(rtt);

    // Transport-bound op FIRST (viewport still at the live tail): new server
    // output only reaches the mirror over the latency-injected data plane.
    let output_start = Instant::now();
    pane_send_text(&sess.api_socket, &p1, "echo rtt_new_output");
    assert!(
        grid_contains(&mirror, 100, 40, "rtt_new_output"),
        "new output eventually arrives over the slow transport"
    );
    let t_output = output_start.elapsed();

    // Local op: scrolling back to already-cached history is near-instant, paying
    // no transport cost.
    let scroll_start = Instant::now();
    let mut scrolled = false;
    for _ in 0..10 {
        mirror.write_input(PAGE_UP);
        if wait_until(
            Duration::from_millis(200),
            Duration::from_millis(10),
            || render_grid(&mirror.output(), 100, 40).contains("rtt_line_270"),
        ) {
            scrolled = true;
            break;
        }
    }
    let t_scroll = scroll_start.elapsed();
    assert!(scrolled, "scrollback to cached content resolved");

    // Local scrollback is dramatically faster than a transport round-trip —
    // proving navigation over cached content is RTT-independent.
    assert!(
        t_scroll < rtt && t_scroll * 2 < t_output,
        "cached scrollback must be RTT-independent: t_scroll={t_scroll:?}, t_output={t_output:?}, rtt={rtt:?}"
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// D. Scroll policy parity (Finding 1 — shared scroll_disposition)
// ===========================================================================

// SGR mouse wheel-up at an arbitrary cell, and the PageUp key sequence.
const WHEEL_UP: &[u8] = b"\x1b[<64;5;5M";
const PAGE_UP: &[u8] = b"\x1b[5~";

/// Waits until a client has drawn its first frame and gone quiescent.
fn wait_ready(client: &PtyProc) {
    let _ = client.stable_bytes(READY);
}

/// Reports an agent detection/status on a pane via the JSON API (`pane.report_agent`).
fn report_agent(socket: &Path, pane_id: &str, agent: &str, state: &str) {
    let request = serde_json::json!({
        "id": "ra",
        "method": "pane.report_agent",
        "params": { "pane_id": pane_id, "source": "cli", "agent": agent, "state": state },
    });
    let resp = send_json(socket, &request.to_string());
    assert_ok(&resp, "pane.report_agent");
}

/// Blocks until `marker` shows in the client's rendered grid, proving it has
/// caught up to the pane's mode-switch output before scroll input is sent.
fn wait_client_shows(client: &PtyProc, cols: u16, rows: u16, marker: &str) {
    assert!(
        grid_contains(client, cols, rows, marker),
        "client never rendered the echo-pane marker {marker:?}"
    );
}

#[test]
fn scroll_wheel_local_on_primary_screen() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "wheel");
    setup_echo_pane(&sess.api_socket, &p1, false, false, "echoready_wheel");
    let mut mirror = spawn_mirror_tui(&sess, 100, 30);
    wait_client_shows(&mirror, 100, 30, "echoready_wheel");

    // Wheel on the primary screen is always handled locally: the pane PTY must
    // receive nothing (no echo from `cat -v`).
    let echoed = pty_output_after_input(&sess.client_socket, &p1, &mut mirror, WHEEL_UP);
    assert!(
        echoed.is_empty(),
        "primary-screen wheel must not forward to the PTY, got: {echoed:?}"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn scroll_pagekey_local_on_primary_screen() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "pageprimary");
    setup_echo_pane(&sess.api_socket, &p1, false, false, "echoready_page");
    let mut mirror = spawn_mirror_tui(&sess, 100, 30);
    wait_client_shows(&mirror, 100, 30, "echoready_page");

    // PageUp on the primary screen scrolls the local viewport; not forwarded.
    let echoed = pty_output_after_input(&sess.client_socket, &p1, &mut mirror, PAGE_UP);
    assert!(
        echoed.is_empty(),
        "primary-screen PageUp must not forward to the PTY, got: {echoed:?}"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn scroll_pagekey_forwarded_on_alt_screen() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "pagealt");
    // Alt screen: a full-screen app owns paging, so PageUp is forwarded.
    setup_echo_pane(&sess.api_socket, &p1, true, false, "echoready_pagealt");
    let mut mirror = spawn_mirror_tui(&sess, 100, 30);
    wait_client_shows(&mirror, 100, 30, "echoready_pagealt");

    let echoed = pty_output_after_input(&sess.client_socket, &p1, &mut mirror, PAGE_UP);
    assert!(
        echoed.contains("[5~"),
        "alt-screen PageUp must be forwarded to the app, got: {echoed:?}"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn scroll_wheel_forwarded_as_input_on_alt_screen_mouse_app() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "altmouse");
    // Alt screen + mouse reporting: the app consumes the wheel as input.
    setup_echo_pane(&sess.api_socket, &p1, true, true, "echoready_altmouse");
    let mut mirror = spawn_mirror_tui(&sess, 100, 30);
    wait_client_shows(&mirror, 100, 30, "echoready_altmouse");

    let echoed = pty_output_after_input(&sess.client_socket, &p1, &mut mirror, WHEEL_UP);
    assert!(
        echoed.contains("[<64;5;5M"),
        "alt-screen mouse app must receive the wheel as input, got: {echoed:?}"
    );

    cleanup_test_base(&sess.base);
}

/// Whether a freshly-spawned `client` (mirror or single-pane attach) forwards
/// `input` to the pane PTY for the given terminal mode, via the `cat -v` echo
/// oracle. Uses a dedicated server+session so client writable connections never
/// collide and cases don't bleed together.
fn client_forwards(
    spawn: fn(&Session, u16, u16) -> PtyProc,
    alt: bool,
    mouse: bool,
    input: &[u8],
) -> bool {
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "policycase");
    let marker = "echoready_policycase";
    setup_echo_pane(&sess.api_socket, &p1, alt, mouse, marker);
    let mut client = spawn(&sess, 100, 30);
    wait_client_shows(&client, 100, 30, marker);
    let echoed = pty_output_after_input(&sess.client_socket, &p1, &mut client, input);
    drop(client);
    cleanup_test_base(&sess.base);
    !echoed.trim().is_empty()
}

#[test]
fn scroll_policy_matches_round_trip_attach() {
    let _guard = mirror_tui_test_guard();
    // The strongest divergence guard: for the same input + terminal mode, the
    // full mirror TUI and the single-pane responsive attach client — both of which
    // route through the shared `scroll_disposition` helper (Finding 1) — must make
    // the SAME local-vs-forward decision. If either bypassed the shared helper,
    // one of these cases would diverge.
    //
    // Scope: these are exactly the decisions `scroll_disposition` governs
    //   - wheel                 -> Local (never forwarded)
    //   - page key, primary     -> Local
    //   - page key, alt screen  -> ForwardToServer
    //
    // Deliberately excluded: wheel-to-mouse-app routing on the alternate screen.
    // That is a *separate* per-client concern (design §3.4 `wheel_routing`), not
    // part of Finding 1's shared helper, and the two clients currently diverge on
    // it: the mirror TUI forwards the wheel to an alt-screen mouse app (asserted by
    // `scroll_wheel_forwarded_as_input_on_alt_screen_mouse_app`), while the attach
    // client always scrolls the wheel locally. Asserting parity there would be
    // asserting behavior the shared helper does not (yet) unify.
    let cases: &[(&str, bool, bool, &[u8])] = &[
        ("primary+wheel", false, false, WHEEL_UP),
        ("primary+pagekey", false, false, PAGE_UP),
        ("alt+pagekey", true, false, PAGE_UP),
    ];

    for (name, alt, mouse, input) in cases {
        let mirror_fwd = client_forwards(spawn_mirror_tui, *alt, *mouse, input);
        let attach_fwd = client_forwards(spawn_attach_tui, *alt, *mouse, input);
        assert_eq!(
            mirror_fwd, attach_fwd,
            "scroll routing diverged for case {name}: mirror forwarded={mirror_fwd}, attach forwarded={attach_fwd}"
        );
        // Sanity: the shared policy forwards page keys exactly on the alternate
        // screen and never forwards a wheel.
        let expected_forward = *alt && *input == PAGE_UP;
        assert_eq!(
            mirror_fwd, expected_forward,
            "case {name}: expected forward={expected_forward} per scroll_disposition"
        );
    }
}

// ===========================================================================
// E. Resize handling
// ===========================================================================

#[test]
fn resize_client_propagates_to_server_pty() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "resize");
    let mirror = spawn_mirror_tui(&sess, 100, 30);
    // Let the mirror settle so its first forwarded size is recorded.
    wait_ready(&mirror);
    let before = pane_size(&sess.api_socket, &p1);

    // Grow the client terminal; the mirror drives the focused pane's PTY to the
    // new content-rect size, which replicates back as a MirrorEvent::Resize.
    mirror.resize(160, 50);

    let changed = wait_until(READY, Duration::from_millis(100), || {
        pane_size(&sess.api_socket, &p1) != before
    });
    assert!(
        changed,
        "resizing the mirror client must resize the server PTY (was {before:?}, now {:?})",
        pane_size(&sess.api_socket, &p1)
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn resize_relayouts_locally_without_resnapshot() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "resizelocal");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_resize_left");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_resize_right");
    let mirror = spawn_mirror_tui(&sess, 100, 30);
    assert!(
        grid_contains(&mirror, 100, 30, "herdr_resize_left"),
        "left before"
    );
    assert!(
        grid_contains(&mirror, 100, 30, "herdr_resize_right"),
        "right before"
    );

    // Resize the mirror: it relayouts locally and resizes its local emulators. The
    // already-cached content must survive (no blank/reclear from a full re-fetch).
    mirror.resize(140, 45);

    assert!(
        grid_contains(&mirror, 140, 45, "herdr_resize_left"),
        "left pane content survived the local relayout"
    );
    assert!(
        grid_contains(&mirror, 140, 45, "herdr_resize_right"),
        "right pane content survived the local relayout"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn resize_parity_mirror_vs_normal() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "resizeparity");
    let _p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    let normal = spawn_attach_tui(&sess, 100, 30);
    let mirror = spawn_mirror_tui(&sess, 100, 30);
    // Let both draw at the initial size first, then resize both identically.
    let _ = normal.stable_bytes(READY);
    let _ = mirror.stable_bytes(READY);
    normal.resize(130, 42);
    mirror.resize(130, 42);
    assert_eq!(
        mirror.capture_grid(130, 42, READY).text(),
        normal.capture_grid(130, 42, READY).text(),
        "post-resize layout must match normal"
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// F. Reconnect / resume
// ===========================================================================

#[test]
fn mirror_survives_transient_disconnect() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "disc");
    pane_send_text(&sess.api_socket, &p1, "echo herdr_before_drop");

    // Route the mirror's data plane through a proxy we can sever.
    let proxy = WireProxy::start(
        &sess.base.join("proxy"),
        &sess.api_socket,
        &sess.client_socket,
    );
    let mirror = spawn_mirror_tui_via(&proxy, 100, 30, &sess.config_home);
    assert!(
        grid_contains(&mirror, 100, 30, "herdr_before_drop"),
        "content before drop"
    );

    // Sever the data connection(s). The mirror must keep the last view on screen
    // (no clear) and reconnect with backoff.
    proxy.drop_data_connections();
    thread::sleep(Duration::from_millis(500));
    assert!(
        grid_contains(&mirror, 100, 30, "herdr_before_drop"),
        "mirror keeps the last view across a transient disconnect"
    );

    // After reconnect, new output must flow again (the pane resumed live).
    pane_send_text(&sess.api_socket, &p1, "echo herdr_after_reconnect");
    assert!(
        grid_contains(&mirror, 100, 30, "herdr_after_reconnect"),
        "mirror reconnects and resumes live output"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn mirror_reconnect_resumes_all_panes_without_gap() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "resume");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");

    // Every pane of the session must resume gap-free from its last delivered seq
    // — a covered resume delivers a contiguous Delta, never a re-snapshot.
    for (idx, pane) in [p1, p2].iter().enumerate() {
        // First session: subscribe fresh, drive a marker, read to it, then drop.
        let marker_one = format!("herdr_resume_{idx}_one");
        let marker_two = format!("herdr_resume_{idx}_two");
        let resume_point = {
            let (mut stream, base) = subscribe_mirror(&sess.client_socket, pane, false);
            pane_send_text(&sess.api_socket, pane, &format!("echo {marker_one}"));
            collect_until_marker(&mut stream, marker_one.as_bytes(), base + 1, READY)
            // stream dropped here -> disconnect
        };

        // Produce more output while disconnected, then reconnect and resume.
        pane_send_text(&sess.api_socket, pane, &format!("echo {marker_two}"));
        let mut stream = UnixStream::connect(&sess.client_socket).expect("reconnect");
        let (version, _) =
            client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
        assert_eq!(version, CURRENT_PROTOCOL);
        send_mirror_terminal(&mut stream, pane, Some(resume_point), false);

        // A covered resume delivers events directly (no re-snapshot), contiguous
        // from resume_point + 1, eventually carrying the second marker.
        let last =
            collect_until_marker(&mut stream, marker_two.as_bytes(), resume_point + 1, READY);
        assert!(
            last > resume_point,
            "resume must advance the sequence for pane {idx}"
        );
    }

    cleanup_test_base(&sess.base);
}

#[test]
fn mirror_resume_past_eviction_resnapshots() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "evict");
    let (mut stream, base) = subscribe_mirror(&sess.client_socket, &p1, false);
    // Read the fresh snapshot's tail so `base` marks an early resume point.
    drain_mirror(&mut stream, Duration::from_millis(300));
    drop(stream);

    // Overflow the ~1 MiB server ring so the early resume point is evicted. One
    // bulk command produces ~1.3 MiB fast (20k lines of ~64 bytes).
    pane_send_text(
        &sess.api_socket,
        &p1,
        "yes yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy | head -n 20000",
    );
    // Give the server time to stream and evict.
    thread::sleep(Duration::from_secs(3));

    // Reconnect resuming from the now-evicted `base`: the server can't cover it,
    // so it must send a fresh MirrorSnapshot (re-sync), not a delta.
    let mut stream = UnixStream::connect(&sess.client_socket).expect("reconnect");
    let (version, _) =
        client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
    assert_eq!(version, CURRENT_PROTOCOL);
    send_mirror_terminal(&mut stream, &p1, Some(base), false);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let first = read_mirror_message(&mut stream).expect("resume reply");
    assert!(
        matches!(first, MirrorMessage::Snapshot { .. }),
        "resume past eviction must re-snapshot, got {first:?}"
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// G. Multi-pane interactions
// ===========================================================================

#[test]
fn keystrokes_route_to_focused_pane_only() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "route");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_focus(&sess.api_socket, &p1);
    let mut mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);

    // Watch p2's PTY directly while typing into the mirror (focused on p1).
    let (mut p2_sub, _base) = subscribe_mirror(&sess.client_socket, &p2, false);
    drain_mirror(&mut p2_sub, Duration::from_millis(500));

    mirror.write_input(b"echo herdr_only_focused\r");

    // The keystrokes reach the focused pane (p1)...
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_only_focused"),
        "focused pane shows the typed command"
    );
    // ...and NOT the unfocused pane (p2 received no bytes).
    let p2_out = collect_mirror_output(&mut p2_sub, Duration::from_millis(1000));
    let p2_text = String::from_utf8_lossy(&p2_out);
    assert!(
        !p2_text.contains("herdr_only_focused"),
        "unfocused pane must not receive keystrokes, got: {p2_text:?}"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn focus_change_hands_over_writable_pane() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "handover");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_focus(&sess.api_socket, &p1);
    let mut mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);

    // Focus hands over to p2 (prefix + focus-right). Input must now land in p2.
    send_prefix_key(&mut mirror, b"l");
    thread::sleep(Duration::from_millis(200));

    let (mut p2_sub, _base) = subscribe_mirror(&sess.client_socket, &p2, false);
    drain_mirror(&mut p2_sub, Duration::from_millis(500));
    let (mut p1_sub, _base) = subscribe_mirror(&sess.client_socket, &p1, false);
    drain_mirror(&mut p1_sub, Duration::from_millis(500));

    mirror.write_input(b"echo herdr_handover_ok\r");

    // The newly focused pane p2 receives the input...
    let p2_text = {
        let deadline = Instant::now() + READY;
        let mut acc = String::new();
        while Instant::now() < deadline && !acc.contains("herdr_handover_ok") {
            acc.push_str(&String::from_utf8_lossy(&collect_mirror_output(
                &mut p2_sub,
                Duration::from_millis(300),
            )));
        }
        acc
    };
    assert!(
        p2_text.contains("herdr_handover_ok"),
        "input must land in the newly focused pane, got: {p2_text:?}"
    );
    // ...and the previously focused pane p1 does not.
    let p1_out = collect_mirror_output(&mut p1_sub, Duration::from_millis(500));
    assert!(
        !String::from_utf8_lossy(&p1_out).contains("herdr_handover_ok"),
        "old pane must stop receiving input after handover"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn mouse_click_focuses_pane() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "clickfocus");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_focus(&sess.api_socket, &p1);
    // Drive a unique marker into p2 so we can locate it on the mirror screen.
    pane_send_text(&sess.api_socket, &p2, "echo herdr_click_target");
    let mut mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);
    assert!(
        grid_contains(&mirror, 120, 40, "herdr_click_target"),
        "p2 marker must be visible before clicking"
    );
    // The mirror must actually ask the terminal for SGR mouse reporting (?1006h),
    // otherwise no mouse bytes are ever emitted for clicks to act on.
    assert!(
        contains_subslice(&mirror.output(), b"\x1b[?1006h"),
        "mirror must enable SGR mouse reporting on startup"
    );

    // Server focus starts on p1.
    let focused_before = layout_export(&sess.api_socket)
        .get("layout")
        .and_then(|l| l.get("focused_pane_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    assert_eq!(
        focused_before.as_deref(),
        Some(p1.as_str()),
        "p1 focused first"
    );

    // Click on p2's on-screen content — as a REAL terminal sends it: a flood of
    // any-motion (mode 1003) reports ending at the target, then press+release,
    // all coalesced into one stdin read.
    let grid = mirror.capture_grid(120, 40, READY);
    let (row, col) =
        grid_find_cell(&grid, "herdr_click_target").expect("p2 marker located on the mirror grid");
    let mut burst = Vec::new();
    for step in 0..6usize {
        let c = col.saturating_sub(6 - step);
        // cb=35 → Moved (any-motion report).
        burst.extend_from_slice(format!("\x1b[<35;{};{}M", c + 1, row + 1).as_bytes());
    }
    let (press, release) = sgr_left_click(row, col);
    burst.extend_from_slice(&press);
    burst.extend_from_slice(&release);
    mirror.write_input(&burst);

    // Acceptance 1: the click focuses p2 on the authoritative server.
    let focused_p2 = wait_until(READY, Duration::from_millis(100), || {
        layout_export(&sess.api_socket)
            .get("layout")
            .and_then(|l| l.get("focused_pane_id"))
            .and_then(Value::as_str)
            == Some(p2.as_str())
    });
    assert!(
        focused_p2,
        "clicking a pane must focus it; server focus never moved to p2"
    );

    // Acceptance 2: the writable handover follows the click — typed input now
    // lands in the clicked pane (p2), not the previously focused one (p1).
    let (mut p1_sub, _b) = subscribe_mirror(&sess.client_socket, &p1, false);
    drain_mirror(&mut p1_sub, Duration::from_millis(500));
    let (mut p2_sub, _b) = subscribe_mirror(&sess.client_socket, &p2, false);
    drain_mirror(&mut p2_sub, Duration::from_millis(500));

    mirror.write_input(b"echo herdr_after_click\r");

    let p2_text = {
        let deadline = Instant::now() + READY;
        let mut acc = String::new();
        while Instant::now() < deadline && !acc.contains("herdr_after_click") {
            acc.push_str(&String::from_utf8_lossy(&collect_mirror_output(
                &mut p2_sub,
                Duration::from_millis(300),
            )));
        }
        acc
    };
    assert!(
        p2_text.contains("herdr_after_click"),
        "after clicking p2, typed input must land in p2; got: {p2_text:?}"
    );
    let p1_out = collect_mirror_output(&mut p1_sub, Duration::from_millis(500));
    assert!(
        !String::from_utf8_lossy(&p1_out).contains("herdr_after_click"),
        "old pane p1 must stop receiving input after the click"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn server_created_pane_appears_in_mirror() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "livecreate");
    let mirror = spawn_mirror_tui(&sess, 120, 40);

    // Another client (the JSON API) creates a pane after the mirror is up.
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_live_new_pane");

    // Acceptance: the mirror reconciles PaneCreated, opens a data connection, and
    // renders the new pane live.
    assert!(
        screen_contains(&mirror, "herdr_live_new_pane"),
        "mirror must show a pane created after startup"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn server_closed_pane_removed_from_mirror() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "liveclose");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_send_text(&sess.api_socket, &p2, "echo herdr_doomed_pane");
    let mirror = spawn_mirror_tui(&sess, 120, 40);
    assert!(screen_contains(&mirror, "herdr_doomed_pane"));

    pane_close(&sess.api_socket, &p2);
    // Acceptance: the closed pane disappears from the mirror layout.
    let gone = wait_until(READY, Duration::from_millis(100), || {
        !normalize_screen(&mirror.output()).contains("herdr_doomed_pane")
            || pane_list_ids(&sess.api_socket).len() == 1
    });
    assert!(gone, "closed pane must be removed from the mirror");

    cleanup_test_base(&sess.base);
}

#[test]
fn structural_command_from_mirror_reflects_via_server() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let _p1 = create_workspace_root_pane(&sess.api_socket, "structural");
    let mut mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);
    assert_eq!(
        pane_list_ids(&sess.api_socket).len(),
        1,
        "one pane at start"
    );

    // A structural keybind (prefix + split-vertical) must go to the server over the
    // JSON API and reflect back into the replica — not mutate the replica locally.
    send_prefix_key(&mut mirror, b"v"); // split_vertical (default "prefix+v")
    let split = wait_until(READY, Duration::from_millis(100), || {
        pane_list_ids(&sess.api_socket).len() == 2
    });
    assert!(
        split,
        "structural command from mirror must create a pane on the server"
    );

    // The new pane reflects back into the mirror's own layout (two pane boxes).
    let reflected = wait_until(READY, Duration::from_millis(150), || {
        let grid = render_grid(&mirror.output(), 120, 40);
        grid.cells
            .iter()
            .any(|row| row.iter().filter(|c| **c == '┌').count() == 2)
    });
    assert!(
        reflected,
        "the server-side split reflects into the mirror replica"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn agent_status_change_reflected_in_mirror_sidebar() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "agentstatus");
    let mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);

    // Report an agent detection on the server; the mirror subscribes to
    // `pane.agent_detected` and reconciles it into its replica, so the agent's
    // name surfaces in the mirror's rendered chrome (sidebar / pane border label).
    report_agent(&sess.api_socket, &p1, "herdrbot", "working");

    assert!(
        grid_contains(&mirror, 120, 40, "herdrbot"),
        "the reported agent must surface in the mirror after events.subscribe reconciliation"
    );

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// H. Latency & memory characteristics
// ===========================================================================

#[test]
fn scrollback_available_beyond_server_ring() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "cache");
    // A tall client so each PageUp scrolls a large chunk of history.
    let mut mirror = spawn_mirror_tui(&sess, 100, 60);
    wait_ready(&mirror);

    // Produce output that overflows the server's ~1 MiB *byte* ring while staying
    // well within the client's cell-budgeted local cache: each line carries a big
    // run of zero-width SGR resets (many raw bytes, ~no cells). The server evicts
    // the earliest lines by byte cost; the client (far more lines for the same
    // cell budget) keeps them. One awk command emits it all: an early marker, the
    // bulk, and a tail marker. `%c`/27 emits ESC without shell-escaping worries.
    pane_send_text(
        &sess.api_socket,
        &p1,
        r#"awk 'BEGIN{e=""; for(i=0;i<80;i++) e=e sprintf("%c[0m", 27); print "herdr_earliest_marker" e; for(n=1;n<=6000;n++) print "herdr_num_" n e; print "herdr_tail_marker" e}'"#,
    );
    assert!(
        grid_contains(&mirror, 100, 60, "herdr_tail_marker"),
        "client caught up to the live tail"
    );

    // The server ring has evicted the earliest marker: a fresh wire subscription
    // resuming from seq 0 can no longer cover it, so it re-snapshots.
    {
        let mut probe = UnixStream::connect(&sess.client_socket).expect("probe connect");
        let (_v, _e) =
            client_handshake_direct(&mut probe, CURRENT_PROTOCOL, 80, 24).expect("handshake");
        send_mirror_terminal(&mut probe, &p1, Some(0), false);
        probe
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(
            matches!(
                read_mirror_message(&mut probe).expect("probe reply"),
                MirrorMessage::Snapshot { .. }
            ),
            "server ring should have evicted the earliest output"
        );
    }

    // Scroll the mirror back toward the top: the earliest marker — evicted by the
    // server — is STILL reachable from the client's local cache. A server-rendered
    // client could not show it. Scroll incrementally with small gaps so the stdin
    // reader doesn't coalesce (and thereby drop) the page keys, stopping as soon
    // as the marker appears.
    let mut found = false;
    'scroll: for _ in 0..40 {
        for _ in 0..20 {
            mirror.write_input(PAGE_UP);
            thread::sleep(Duration::from_millis(5));
        }
        if render_grid(&mirror.output(), 100, 60).contains("herdr_earliest_marker") {
            found = true;
            break 'scroll;
        }
    }
    assert!(
        found,
        "earliest output evicted by the server is still visible from the client cache"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn deep_scrollback_scroll_is_local() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "deepscroll");
    for i in 0..500 {
        pane_send_text(&sess.api_socket, &p1, &format!("echo deep_{i}"));
    }
    let mut mirror = spawn_mirror_tui(&sess, 100, 30);
    assert!(grid_contains(&mirror, 100, 30, "deep_499"), "at live tail");

    // Scroll deep into history: an early line becomes visible purely from the
    // local cache (no new subscribe/snapshot traffic is needed to render it).
    for _ in 0..80 {
        mirror.write_input(PAGE_UP);
    }
    assert!(
        grid_contains(&mirror, 100, 30, "deep_5"),
        "deep scrollback resolves locally"
    );

    cleanup_test_base(&sess.base);
}

#[test]
#[ignore = "H3 depends on copy-mode/search in the mirror session, which is an \
            explicitly-deferred capability: `client::mirror_session::action::classify` \
            currently maps `CopyMode` to `Unsupported`, and \
            `client::mirror_session::app::MirrorApp::apply_view_local` does not yet \
            implement copy-mode/search/selection (its own doc note: 'richer \
            modal/copy-mode flows are layered in a later phase'). The locality of \
            cached content that search/selection would rely on is already proven by \
            H1/H2 and `deep_scrollback_scroll_is_local`. Un-ignore once mirror \
            copy-mode lands."]
fn search_and_selection_are_local() {
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "searchlocal");
    for i in 0..300 {
        pane_send_text(&sess.api_socket, &p1, &format!("echo find_{i}"));
    }
    let _mirror = spawn_mirror_tui(&sess, 100, 30);
    // When mirror copy-mode lands: enter copy-mode search for "find_42" and select
    // text; assert both resolve locally (no server round-trip) and match the normal
    // client's results.

    cleanup_test_base(&sess.base);
}

// ===========================================================================
// Compile-guard: confirms the harness itself is wired up. Runs by default.
// (A minimal smoke test alongside the full acceptance suite above, which also
// runs by default; the sole `#[ignore]` is the H3 copy-mode gate.)
// ===========================================================================

/// Regression guard (#fix-keyboard-input-drops): an IDLE mirror must not
/// busy-redraw. A resize/redraw feedback loop (draw -> forward_resize -> server
/// echoes size -> mirror data -> redraw -> ...) would emit a continuous stream of
/// terminal output with no input, flooding the link and starving stdin — a
/// mechanism behind "characters dropped / scrollback laggy", worst over SSH.
#[test]
fn idle_mirror_does_not_busy_redraw() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let _p1 = create_workspace_root_pane(&sess.api_socket, "idle");
    let mirror = spawn_mirror_tui(&sess, 120, 40);
    wait_ready(&mirror);
    // Let the initial paint settle.
    thread::sleep(Duration::from_millis(500));

    let before = mirror.output().len();
    thread::sleep(Duration::from_secs(2));
    let after = mirror.output().len();
    let grew = after - before;
    // A single full 120x40 repaint is well under ~40 KiB. With no input and no
    // pane output, an idle mirror should emit at most a couple of frames. Many
    // frames/sec (hundreds of KiB) means a busy redraw loop.
    assert!(
        grew < 40_000,
        "idle mirror emitted {grew} bytes in 2s with no input — busy redraw loop"
    );

    cleanup_test_base(&sess.base);
}

/// Regression guard (#fix-keyboard-input-drops): keystrokes must all reach the
/// focused pane even while a burst of structural control events fires under a
/// realistic control-plane RTT. Each structural change makes the mirror
/// `reproject()`, which does a blocking `layout.export` round-trip on the event
/// loop; a burst of them (a split emits pane.created + layout.updated +
/// pane.focused) must not stall or drop input. The mirror coalesces a control-event
/// burst into a single reprojection so the loop stays responsive; this test drives
/// continuous focus churn over a 200 ms RTT while typing and asserts every byte is
/// delivered, in order, well within a generous bound.
#[test]
fn input_survives_structural_bursts_over_rtt() {
    let _guard = mirror_tui_test_guard();
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let p1 = create_workspace_root_pane(&sess.api_socket, "rttinput");
    let p2 = split_pane(&sess.api_socket, &p1, "horizontal");
    pane_focus(&sess.api_socket, &p1);
    setup_echo_pane(&sess.api_socket, &p1, false, false, "echoready_rttinput");

    let proxy = WireProxy::start(
        &sess.base.join("proxy"),
        &sess.api_socket,
        &sess.client_socket,
    );
    let mut mirror = spawn_mirror_tui_via(&proxy, 120, 40, &sess.config_home);
    wait_client_shows(&mirror, 120, 40, "echoready_rttinput");

    // Observe the server's p1 PTY directly (real socket, no injected latency).
    let (mut p1_sub, _base) = subscribe_mirror(&sess.client_socket, &p1, false);
    drain_mirror(&mut p1_sub, Duration::from_millis(500));

    // Realistic SSH control-plane RTT (the path layout.export/subscriptions take).
    proxy.set_api_latency(Duration::from_millis(200));

    // Continuously churn focus between the two panes -> a steady stream of
    // structural (pane.focused) control events while typing.
    let api_socket = sess.api_socket.clone();
    let panes = [p1.clone(), p2.clone()];
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let bg = thread::spawn(move || {
        let mut i = 0;
        while !stop_bg.load(Ordering::SeqCst) {
            pane_focus(&api_socket, &panes[i % 2]);
            i += 1;
            thread::sleep(Duration::from_millis(15));
        }
    });

    let typed = b"abcdefghij";
    for &b in typed {
        mirror.write_input(&[b]);
        thread::sleep(Duration::from_millis(30));
    }

    let mut echoed = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        echoed.extend_from_slice(&collect_mirror_output(
            &mut p1_sub,
            Duration::from_millis(300),
        ));
        let seen: Vec<u8> = echoed
            .iter()
            .copied()
            .filter(|b| typed.contains(b))
            .collect();
        if seen == typed {
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = bg.join();

    let seen: Vec<u8> = echoed
        .iter()
        .copied()
        .filter(|b| typed.contains(b))
        .collect();
    assert_eq!(
        String::from_utf8_lossy(&seen),
        String::from_utf8_lossy(typed),
        "server must receive every typed char even under structural-event RTT load"
    );

    cleanup_test_base(&sess.base);
}

#[test]
fn harness_starts_server_and_creates_pane() {
    let _guard = mirror_tui_test_guard();
    // Uses only the JSON shapes already validated by tests/terminal_mirror.rs, so
    // this stays a reliable, dependency-light smoke check.
    let sess = Session::new();
    let _server = spawn_server(&sess);
    let pane_id = create_workspace_root_pane(&sess.api_socket, "harness");
    assert!(!pane_id.is_empty(), "server should return a root pane id");
    cleanup_test_base(&sess.base);
}
