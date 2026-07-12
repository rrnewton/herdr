//! Integration tests for the responsive local mirror protocol.
//!
//! These exercise the full server path — PTY output tap, per-terminal
//! replication log, subscribe/flush, and the wire messages — over the real
//! client socket, without a live TUI client. Output is driven through the JSON
//! control API and asserted byte-for-byte on the mirror stream.

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, client_handshake_direct, decode_varint_u32, decode_varint_u64,
    encode_varint_u32, frame_message, read_server_message, register_runtime_dir,
    register_spawned_herdr_pid, unregister_spawned_herdr_pid, wait_for_file, wait_for_socket,
    CURRENT_PROTOCOL,
};

/// Decodes a bincode varint-encoded `u16` (same wire form as `encode_varint_u16`).
fn decode_varint_u16(payload: &[u8], offset: usize) -> Result<(u16, usize), String> {
    let (value, consumed) = decode_varint_u32(payload, offset)?;
    Ok((value as u16, consumed))
}

// Server -> client mirror message wire tags (bincode declaration order).
const MSG_MIRROR_SNAPSHOT: u32 = 11;
const MSG_MIRROR_EVENT: u32 = 12;
// MirrorEventKind wire tags.
const KIND_OUTPUT: u32 = 0;
const KIND_RESIZE: u32 = 1;
const KIND_CLOSED: u32 = 2;
// ClientMessage::MirrorTerminal wire tag.
const MSG_MIRROR_TERMINAL: u32 = 10;

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-mirror-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(pid);
    }
}

fn spawn_server(config_home: &Path, runtime_dir: &Path, api_socket_path: &Path) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn send_json_request(socket_path: &Path, request: &str) -> Value {
    use std::io::{BufRead, BufReader};
    let mut stream = UnixStream::connect(socket_path).expect("connect API socket");
    writeln!(stream, "{request}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("valid JSON response")
}

fn create_root_pane(socket_path: &Path, label: &str) -> String {
    let response = send_json_request(
        socket_path,
        &format!(
            "{{\"id\":\"ws\",\"method\":\"workspace.create\",\"params\":{{\"label\":\"{label}\"}}}}"
        ),
    );
    assert!(
        response.get("error").is_none(),
        "workspace.create: {response}"
    );
    response
        .pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
        .expect("root pane id")
        .to_string()
}

fn pane_send_text(socket_path: &Path, pane_id: &str, text: &str) {
    let request = format!(
        "{{\"id\":\"in\",\"method\":\"pane.send_input\",\"params\":{{\"pane_id\":\"{pane_id}\",\"text\":\"{}\",\"keys\":[\"Enter\"]}}}}",
        text.replace('"', "\\\"")
    );
    let response = send_json_request(socket_path, &request);
    assert!(
        response.get("error").is_none(),
        "pane.send_input: {response}"
    );
}

/// Sends `ClientMessage::MirrorTerminal { target, resume_from }`.
fn send_mirror_terminal(stream: &mut UnixStream, target: &str, resume_from: Option<u64>) {
    let mut payload = encode_varint_u32(MSG_MIRROR_TERMINAL);
    payload.extend(encode_varint_u32(target.len() as u32));
    payload.extend_from_slice(target.as_bytes());
    match resume_from {
        None => payload.push(0),
        Some(seq) => {
            payload.push(1);
            // bincode u64 varint: small values fit in one byte.
            if seq < 251 {
                payload.push(seq as u8);
            } else {
                payload.push(253);
                payload.extend_from_slice(&seq.to_le_bytes());
            }
        }
    }
    let framed = frame_message(&payload);
    stream.write_all(&framed).unwrap();
    stream.flush().unwrap();
}

// A decoded mirror message. Not every field is asserted by every test; the
// decoder captures the full shape for clarity and future assertions.
#[derive(Debug)]
#[allow(dead_code)]
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
/// timeout elapses. Asserts that every event sequence is strictly contiguous
/// starting from `expected_first_seq`, and returns the highest seq observed.
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
                    "output seq must be contiguous (gap detected)"
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
                panic!("unexpected re-snapshot while tailing a covered stream");
            }
            Ok(MirrorMessage::Closed { .. }) => panic!("stream closed before marker"),
            Ok(MirrorMessage::Other(v)) => panic!("unexpected server message variant {v}"),
            Err(_) => continue, // read timeout; keep polling until deadline
        }
    }
    panic!(
        "did not observe marker {:?} within timeout (last seq {last_seq})",
        String::from_utf8_lossy(marker)
    );
}

#[test]
fn mirror_streams_raw_output_with_contiguous_seqs() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let _server = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    let pane_id = create_root_pane(&api_socket, "mirror-basic");

    let mut stream = UnixStream::connect(&client_socket).expect("connect client socket");
    let (version, error) =
        client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none(), "handshake error: {error:?}");

    send_mirror_terminal(&mut stream, &pane_id, None);

    // First message must be the snapshot establishing base_seq.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let base_seq = match read_mirror_message(&mut stream).expect("read snapshot") {
        MirrorMessage::Snapshot {
            base_seq,
            cols,
            rows,
        } => {
            assert!(cols > 0 && rows > 0, "snapshot must carry a terminal size");
            base_seq
        }
        other => panic!("expected MirrorSnapshot first, got {other:?}"),
    };

    // Drive deterministic output and observe it byte-for-byte on the mirror.
    let marker = b"herdr_mirror_marker_alpha";
    pane_send_text(&api_socket, &pane_id, "echo herdr_mirror_marker_alpha");

    let last_seq = collect_until_marker(&mut stream, marker, base_seq + 1, Duration::from_secs(15));
    assert!(last_seq > base_seq, "should have advanced past base_seq");

    cleanup_test_base(&base);
}

#[test]
fn mirror_resume_continues_without_gap() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let _server = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    let pane_id = create_root_pane(&api_socket, "mirror-resume");

    // First mirror session: subscribe fresh, read up to a marker, then drop.
    let resume_point = {
        let mut stream = UnixStream::connect(&client_socket).expect("connect client socket");
        let (version, _) =
            client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
        assert_eq!(version, CURRENT_PROTOCOL);
        send_mirror_terminal(&mut stream, &pane_id, None);

        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let base_seq = match read_mirror_message(&mut stream).expect("snapshot") {
            MirrorMessage::Snapshot { base_seq, .. } => base_seq,
            other => panic!("expected snapshot, got {other:?}"),
        };
        pane_send_text(&api_socket, &pane_id, "echo herdr_resume_marker_one");
        collect_until_marker(
            &mut stream,
            b"herdr_resume_marker_one",
            base_seq + 1,
            Duration::from_secs(15),
        )
        // stream dropped here -> disconnect
    };

    // Produce more output while disconnected.
    pane_send_text(&api_socket, &pane_id, "echo herdr_resume_marker_two");

    // Reconnect and resume from the last delivered sequence.
    let mut stream = UnixStream::connect(&client_socket).expect("reconnect client socket");
    let (version, _) =
        client_handshake_direct(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake");
    assert_eq!(version, CURRENT_PROTOCOL);
    send_mirror_terminal(&mut stream, &pane_id, Some(resume_point));

    // A covered resume delivers events directly (no re-snapshot), contiguous
    // from resume_point + 1, and eventually carries the second marker.
    let last_seq = collect_until_marker(
        &mut stream,
        b"herdr_resume_marker_two",
        resume_point + 1,
        Duration::from_secs(15),
    );
    assert!(
        last_seq > resume_point,
        "resume should advance the sequence"
    );

    cleanup_test_base(&base);
}
