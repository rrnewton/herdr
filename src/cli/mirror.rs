//! `herdr mirror <host>` — one-command remote mirror over SSH.
//!
//! Connecting a mirror to a *remote* Herdr server used to mean setting up two
//! SSH Unix-socket tunnels by hand and exporting `HERDR_SOCKET_PATH`. This
//! subcommand automates the whole dance:
//!
//! 1. discover the remote server's API socket path (`ssh <host> herdr status
//!    server --json`);
//! 2. forward both the control-plane and data-plane sockets over one background
//!    SSH connection (`ssh -N -L …`);
//! 3. launch the local mirror (`herdr --mirror`) pointed at the forwarded
//!    sockets via `HERDR_SOCKET_PATH`;
//! 4. tear the tunnel down and remove the temporary sockets on exit.
//!
//! Only `HERDR_SOCKET_PATH` needs to be set on the child: the client derives the
//! data-plane socket from it (`server::socket_paths::client_socket_path`), so a
//! single forwarded pair is enough.
//!
//! The discovery and tunnel steps are two separate `ssh` invocations. By default
//! they authenticate independently — on a 2FA host that means up to two prompts,
//! but both happen *before* the mirror TUI launches, so they can never corrupt
//! the running UI.
//!
//! `--controlmaster` opts into SSH connection multiplexing so the two steps share
//! a single authenticated connection (one 2FA prompt). This is **off by default**
//! because a persisted master can later need to re-authenticate, and that prompt
//! would render straight into the mirror TUI — which shares the terminal with the
//! SSH children — corrupting the screen. When enabled we mitigate that by keeping
//! the master alive for the whole session (`ControlPersist=yes`, no expiry timer)
//! on a private control path, and tearing it down explicitly on exit, so no
//! re-auth is triggered mid-session.
//!
//! This is purely a client-side launcher — it adds no server state or protocol.
//! It relies on OpenSSH ≥ 6.7 Unix-domain-socket forwarding, so it is gated to
//! Unix; Windows gets a stub that explains the limitation.

/// Usage string shared by the help paths.
pub(super) const MIRROR_USAGE: &str = "\
usage: herdr mirror <host> [ssh args ...]

Connect a mirror to a Herdr server running on a remote host over SSH, in one
command. Discovers the remote server socket, forwards it over SSH, and starts
`herdr --mirror` against it. The tunnel is removed on exit.

options (before <host>):
  --remote-herdr <path>   herdr executable to run on the remote (default: herdr)
  --remote-socket <path>  remote API socket path, skipping auto-discovery
  --controlmaster         share one SSH connection for discovery + tunnel so a
                          2FA host prompts once (off by default; see below)
  --no-controlmaster      disable SSH connection sharing (the default)
  -h, --help              show this help

Any arguments after <host> are passed through to ssh, e.g.:
  herdr mirror devbox
  herdr mirror devbox -p 2222 -i ~/.ssh/id_ed25519
  herdr mirror --controlmaster devbox      # one 2FA prompt, not two

ControlMaster is off by default: sharing a persisted SSH connection can make a
later re-authentication prompt appear inside the mirror TUI and corrupt it. Use
--controlmaster only if a second auth prompt at startup is more annoying than
that risk.

Requires OpenSSH >= 6.7 (Unix-domain-socket forwarding) on both ends.";

/// Parsed `herdr mirror` invocation.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct MirrorArgs {
    /// SSH destination (`user@host`, an ssh_config alias, …).
    pub host: String,
    /// Extra arguments forwarded verbatim to every `ssh` invocation.
    pub ssh_args: Vec<String>,
    /// Remote herdr executable name/path (default `herdr`).
    pub remote_herdr: String,
    /// Explicit remote API socket path; when set, discovery is skipped.
    pub remote_socket: Option<String>,
    /// Opt into SSH `ControlMaster` multiplexing (single auth). Off by default
    /// because a mid-session re-auth prompt would corrupt the mirror TUI.
    pub control_master: bool,
}

/// Outcome of parsing that is not a ready-to-run [`MirrorArgs`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParseOutcome {
    /// Print help and exit with this code (0 for `--help`, 2 for a usage error).
    Usage(i32),
}

/// Parse `herdr mirror` arguments.
///
/// Grammar: `[--remote-herdr X] [--remote-socket Y] <host> [ssh args ...]`.
/// The first bare token is the host; everything after it is passed to ssh, so
/// ssh flags belong after the host (`herdr mirror host -p 2222`).
pub(super) fn parse_mirror_args(args: &[String]) -> Result<MirrorArgs, ParseOutcome> {
    let mut host: Option<String> = None;
    let mut ssh_args: Vec<String> = Vec::new();
    let mut remote_herdr = "herdr".to_string();
    let mut remote_socket: Option<String> = None;
    let mut control_master = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if host.is_some() {
            // Everything after the host is ssh passthrough.
            ssh_args.push(args[index].clone());
            index += 1;
            continue;
        }
        match arg {
            "-h" | "--help" | "help" => return Err(ParseOutcome::Usage(0)),
            "--remote-herdr" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --remote-herdr");
                    return Err(ParseOutcome::Usage(2));
                };
                remote_herdr = value.clone();
                index += 2;
            }
            "--remote-socket" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --remote-socket");
                    return Err(ParseOutcome::Usage(2));
                };
                remote_socket = Some(value.clone());
                index += 2;
            }
            "--controlmaster" => {
                control_master = true;
                index += 1;
            }
            "--no-controlmaster" => {
                control_master = false;
                index += 1;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                return Err(ParseOutcome::Usage(2));
            }
            other => {
                host = Some(other.to_string());
                index += 1;
            }
        }
    }

    match host {
        Some(host) => Ok(MirrorArgs {
            host,
            ssh_args,
            remote_herdr,
            remote_socket,
            control_master,
        }),
        None => {
            eprintln!("missing <host>");
            Err(ParseOutcome::Usage(2))
        }
    }
}

/// The fields we care about from `herdr status server --json`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RemoteServerStatus {
    pub socket: String,
    pub running: bool,
    /// `Some(false)` means the remote protocol version differs from ours.
    pub compatible: Option<bool>,
}

/// Extract the server socket path and health from `herdr status server --json`.
pub(super) fn parse_status_json(stdout: &str) -> Result<RemoteServerStatus, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|err| format!("invalid status JSON: {err}"))?;
    let socket = value
        .get("socket")
        .and_then(|socket| socket.as_str())
        .ok_or_else(|| "status JSON missing `socket`".to_string())?
        .to_string();
    if socket.is_empty() {
        return Err("remote reported an empty socket path".to_string());
    }
    let running = value
        .get("running")
        .and_then(|running| running.as_bool())
        .unwrap_or(false);
    let compatible = value.get("compatible").and_then(|value| value.as_bool());
    Ok(RemoteServerStatus {
        socket,
        running,
        compatible,
    })
}

#[cfg(not(unix))]
pub(super) fn run_mirror_command(args: &[String]) -> std::io::Result<i32> {
    // Unix-domain-socket forwarding (`ssh -L unixsock:unixsock`) and the mirror
    // client are Unix-only, so there is nothing to orchestrate here on Windows.
    if matches!(parse_mirror_args(args), Err(ParseOutcome::Usage(0))) {
        eprintln!("{MIRROR_USAGE}");
        return Ok(0);
    }
    eprintln!("herdr mirror <host> (remote mirror over SSH) is not supported on this platform yet");
    Ok(1)
}

#[cfg(unix)]
pub(super) use unix_impl::run_mirror_command;

#[cfg(unix)]
mod unix_impl {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{parse_mirror_args, parse_status_json, MirrorArgs, ParseOutcome, MIRROR_USAGE};

    /// How long to wait for a forwarded socket to become connectable.
    const FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(15);

    /// Opt-in SSH connection multiplexing (`--controlmaster`). When present, the
    /// discovery SSH opens a master connection at [`ControlMasterConfig::path`]
    /// and the tunnel SSH reuses it, so a 2FA host authenticates once rather than
    /// per connection.
    ///
    /// This is deliberately *not* the default: a persisted master that later has
    /// to re-authenticate would print its prompt into the mirror TUI (SSH shares
    /// the terminal) and corrupt the screen. We keep the opt-in path safe by
    /// persisting the master for the whole session (`ControlPersist=yes`, no
    /// expiry timer) on a private per-process path, and closing it explicitly on
    /// exit — so no mid-session re-auth is ever triggered.
    struct ControlMasterConfig {
        /// SSH `ControlPath`. `%h` expands to the remote host so distinct hosts
        /// get distinct sockets; the pid keeps concurrent launchers isolated and
        /// avoids reusing a stale master left by an earlier run.
        path: String,
    }

    impl ControlMasterConfig {
        /// A fresh, private control path for this launcher process.
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("herdr-mirror-cm-{}-%h", std::process::id()))
                .to_string_lossy()
                .into_owned();
            Self { path }
        }

        /// Add the options that *create or reuse* the master (discovery step).
        fn apply_master(&self, command: &mut Command) {
            command
                .arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg(format!("ControlPath={}", self.path))
                // `yes`, not a timeout: the master lives for the whole session
                // and is torn down explicitly, so it never expires and re-auths
                // mid-session (which would corrupt the TUI).
                .arg("-o")
                .arg("ControlPersist=yes");
        }

        /// Add the options that *attach to* the existing master (tunnel step).
        fn apply_client(&self, command: &mut Command) {
            command.arg("-o").arg(format!("ControlPath={}", self.path));
        }
    }

    pub(crate) fn run_mirror_command(args: &[String]) -> io::Result<i32> {
        let parsed = match parse_mirror_args(args) {
            Ok(parsed) => parsed,
            Err(ParseOutcome::Usage(code)) => {
                eprintln!("{MIRROR_USAGE}");
                return Ok(code);
            }
        };

        match connect(parsed) {
            Ok(code) => Ok(code),
            Err(err) => {
                eprintln!("herdr mirror: {err}");
                Ok(1)
            }
        }
    }

    fn connect(parsed: MirrorArgs) -> io::Result<i32> {
        let MirrorArgs {
            host,
            ssh_args,
            remote_herdr,
            remote_socket,
            control_master,
        } = parsed;

        // Opt-in SSH multiplexing so discovery + tunnel share one auth. Off by
        // default (see `ControlMasterConfig`); when off, each `ssh` authenticates
        // on its own, but always before the TUI starts.
        let control = control_master.then(ControlMasterConfig::new);

        // 1. Resolve the remote API socket (explicit override or discovery).
        let remote_api_socket = match remote_socket {
            Some(socket) => socket,
            None => discover_remote_socket(&host, &ssh_args, &remote_herdr, control.as_ref())?,
        };
        let remote_client_socket =
            crate::server::socket_paths::derive_client_socket_from_api_socket(Path::new(
                &remote_api_socket,
            ));

        // 2. Pick local temp socket paths. The client derives the data-plane
        //    socket from the API socket by inserting `-client`, so we mirror that
        //    naming for the local pair too.
        let local_api_socket = default_local_socket_path();
        let local_client_socket =
            crate::server::socket_paths::derive_client_socket_from_api_socket(&local_api_socket);
        remove_socket_file(&local_api_socket);
        remove_socket_file(&local_client_socket);

        // 3. Forward both sockets over one background SSH connection. The guard
        //    tears everything down on any exit path.
        eprintln!("herdr mirror: forwarding {host} sockets over SSH…");
        let ssh_child = spawn_forward(
            &host,
            &ssh_args,
            &local_api_socket,
            &remote_api_socket,
            &local_client_socket,
            &remote_client_socket.to_string_lossy(),
            control.as_ref(),
        )?;
        let mut guard = TunnelGuard {
            ssh: ssh_child,
            sockets: vec![local_api_socket.clone(), local_client_socket.clone()],
            // On exit, close the shared master (if any) so no ssh process is left
            // holding an authenticated connection.
            control_master: control.map(|control| ControlMasterTeardown {
                control_path: control.path,
                host: host.clone(),
                ssh_args: ssh_args.clone(),
            }),
        };

        // 4. Wait for the forwarded sockets to come up (or SSH to fail).
        wait_for_socket(&mut guard.ssh, &local_api_socket)?;
        wait_for_socket(&mut guard.ssh, &local_client_socket)?;

        // 5. Launch the local mirror against the forwarded sockets. Only
        //    HERDR_SOCKET_PATH is needed; scrub anything that would override it.
        eprintln!("herdr mirror: connected — starting mirror…");
        let exe = std::env::current_exe()?;
        let status = Command::new(exe)
            .arg("--mirror")
            .env(crate::api::SOCKET_PATH_ENV_VAR, &local_api_socket)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR)
            .env_remove(crate::session::SESSION_ENV_VAR)
            .status()?;

        // 6. Guard drops here: SSH is killed and temp sockets removed.
        Ok(status.code().unwrap_or(1))
    }

    /// Ask the remote herdr for its server socket path via `status server --json`.
    fn discover_remote_socket(
        host: &str,
        ssh_args: &[String],
        remote_herdr: &str,
        control: Option<&ControlMasterConfig>,
    ) -> io::Result<String> {
        eprintln!("herdr mirror: discovering herdr server on {host}…");
        let mut command = Command::new("ssh");
        command.args(ssh_args);
        // With `--controlmaster`, open a multiplexed master here so the tunnel
        // SSH can reuse it without a second authentication (single 2FA prompt).
        // The master persists for the whole session and is closed on exit.
        if let Some(control) = control {
            control.apply_master(&mut command);
        }
        let output = command
            .arg(host)
            .arg(remote_herdr)
            .arg("status")
            .arg("server")
            .arg("--json")
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "could not run `{remote_herdr} status server --json` on {host} (is herdr installed there?): {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let status = parse_status_json(&stdout).map_err(io::Error::other)?;
        if !status.running {
            return Err(io::Error::other(format!(
                "no herdr server is running on {host} (start one there first)"
            )));
        }
        if status.compatible == Some(false) {
            eprintln!(
                "herdr mirror: warning: remote server protocol differs from this client; the mirror may not work correctly"
            );
        }
        Ok(status.socket)
    }

    /// Spawn `ssh -N -L localApi:remoteApi -L localClient:remoteClient host`.
    fn spawn_forward(
        host: &str,
        ssh_args: &[String],
        local_api: &Path,
        remote_api: &str,
        local_client: &Path,
        remote_client: &str,
        control: Option<&ControlMasterConfig>,
    ) -> io::Result<Child> {
        let mut command = Command::new("ssh");
        command.args(ssh_args);
        // With `--controlmaster`, reuse the discovery SSH's master connection
        // (same ControlPath) so the tunnel does not trigger a second auth prompt.
        if let Some(control) = control {
            control.apply_client(&mut command);
        }
        command
            .arg("-N")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-L")
            .arg(forward_spec(local_api, remote_api))
            .arg("-L")
            .arg(forward_spec(local_client, remote_client))
            .arg(host)
            .spawn()
    }

    /// `local.sock:remote.sock` forwarding spec for `ssh -L`.
    fn forward_spec(local: &Path, remote: &str) -> String {
        format!("{}:{}", local.display(), remote)
    }

    /// Poll `socket` until it accepts a connection, giving up if SSH exits first
    /// or the timeout elapses.
    fn wait_for_socket(ssh: &mut Child, socket: &Path) -> io::Result<()> {
        let deadline = Instant::now() + FORWARD_READY_TIMEOUT;
        loop {
            if UnixStream::connect(socket).is_ok() {
                return Ok(());
            }
            if let Some(status) = ssh.try_wait()? {
                return Err(io::Error::other(format!(
                    "SSH tunnel exited before the socket was ready ({status}); check the host and your SSH access"
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "timed out waiting for the forwarded socket {} to become ready",
                    socket.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `/tmp/herdr-mirror-<pid>.sock`, unique to this launcher process.
    fn default_local_socket_path() -> PathBuf {
        std::env::temp_dir().join(format!("herdr-mirror-{}.sock", std::process::id()))
    }

    fn remove_socket_file(path: &Path) {
        // Best-effort: `ssh -L` refuses to bind a local socket that already
        // exists, so clear any stale file first. Ignore "not found".
        let _ = std::fs::remove_file(path);
    }

    /// What [`TunnelGuard`] needs to close a `--controlmaster` master on exit.
    struct ControlMasterTeardown {
        control_path: String,
        host: String,
        ssh_args: Vec<String>,
    }

    /// Kills the SSH tunnel, closes the shared master (if any), and removes the
    /// temporary local sockets on drop, so every exit path (success, error, or
    /// panic) cleans up.
    struct TunnelGuard {
        ssh: Child,
        sockets: Vec<PathBuf>,
        control_master: Option<ControlMasterTeardown>,
    }

    impl Drop for TunnelGuard {
        fn drop(&mut self) {
            let _ = self.ssh.kill();
            let _ = self.ssh.wait();
            // Explicitly close the shared master so no authenticated ssh process
            // lingers past this launcher (`ControlPersist=yes` would otherwise
            // keep it alive). Best-effort and silent.
            if let Some(control) = &self.control_master {
                let _ = Command::new("ssh")
                    .args(&control.ssh_args)
                    .arg("-o")
                    .arg(format!("ControlPath={}", control.control_path))
                    .arg("-O")
                    .arg("exit")
                    .arg(&control.host)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            for socket in &self.sockets {
                let _ = std::fs::remove_file(socket);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_host_only() {
        let parsed = parse_mirror_args(&args(&["devbox"])).unwrap();
        assert_eq!(parsed.host, "devbox");
        assert!(parsed.ssh_args.is_empty());
        assert_eq!(parsed.remote_herdr, "herdr");
        assert_eq!(parsed.remote_socket, None);
        // ControlMaster is opt-in, so it must default to off.
        assert!(!parsed.control_master);
    }

    #[test]
    fn controlmaster_flag_opts_in() {
        let parsed = parse_mirror_args(&args(&["--controlmaster", "devbox"])).unwrap();
        assert_eq!(parsed.host, "devbox");
        assert!(parsed.control_master);
        assert!(parsed.ssh_args.is_empty());
    }

    #[test]
    fn no_controlmaster_flag_keeps_it_off() {
        let parsed = parse_mirror_args(&args(&["--no-controlmaster", "devbox"])).unwrap();
        assert!(!parsed.control_master);
    }

    #[test]
    fn last_controlmaster_flag_wins() {
        let on =
            parse_mirror_args(&args(&["--no-controlmaster", "--controlmaster", "devbox"])).unwrap();
        assert!(on.control_master);
        let off =
            parse_mirror_args(&args(&["--controlmaster", "--no-controlmaster", "devbox"])).unwrap();
        assert!(!off.control_master);
    }

    #[test]
    fn controlmaster_after_host_is_ssh_passthrough() {
        // A `--controlmaster` after the host is an ssh arg, not a herdr flag.
        let parsed = parse_mirror_args(&args(&["devbox", "--controlmaster"])).unwrap();
        assert_eq!(parsed.host, "devbox");
        assert!(!parsed.control_master);
        assert_eq!(parsed.ssh_args, args(&["--controlmaster"]));
    }

    #[test]
    fn passes_ssh_args_after_host_through() {
        let parsed = parse_mirror_args(&args(&["devbox", "-p", "2222", "-i", "key"])).unwrap();
        assert_eq!(parsed.host, "devbox");
        assert_eq!(parsed.ssh_args, args(&["-p", "2222", "-i", "key"]));
    }

    #[test]
    fn parses_remote_herdr_and_socket_before_host() {
        let parsed = parse_mirror_args(&args(&[
            "--remote-herdr",
            "/opt/herdr",
            "--remote-socket",
            "/run/herdr.sock",
            "devbox",
        ]))
        .unwrap();
        assert_eq!(parsed.host, "devbox");
        assert_eq!(parsed.remote_herdr, "/opt/herdr");
        assert_eq!(parsed.remote_socket.as_deref(), Some("/run/herdr.sock"));
    }

    #[test]
    fn ssh_flags_after_host_are_not_treated_as_herdr_flags() {
        // `--remote-herdr` after the host is ssh passthrough, not a herdr flag.
        let parsed = parse_mirror_args(&args(&["devbox", "--remote-herdr", "x"])).unwrap();
        assert_eq!(parsed.host, "devbox");
        assert_eq!(parsed.ssh_args, args(&["--remote-herdr", "x"]));
        assert_eq!(parsed.remote_herdr, "herdr");
    }

    #[test]
    fn help_flag_requests_usage_zero() {
        assert_eq!(
            parse_mirror_args(&args(&["--help"])),
            Err(ParseOutcome::Usage(0))
        );
        assert_eq!(
            parse_mirror_args(&args(&["-h"])),
            Err(ParseOutcome::Usage(0))
        );
    }

    #[test]
    fn missing_host_is_usage_error() {
        assert_eq!(parse_mirror_args(&args(&[])), Err(ParseOutcome::Usage(2)));
    }

    #[test]
    fn unknown_leading_flag_is_usage_error() {
        assert_eq!(
            parse_mirror_args(&args(&["--bogus", "devbox"])),
            Err(ParseOutcome::Usage(2))
        );
    }

    #[test]
    fn missing_flag_value_is_usage_error() {
        assert_eq!(
            parse_mirror_args(&args(&["--remote-socket"])),
            Err(ParseOutcome::Usage(2))
        );
    }

    #[test]
    fn extracts_socket_from_running_status_json() {
        let json = r#"{"status":"running","running":true,"socket":"/home/u/.config/herdr/herdr.sock","compatible":true}"#;
        let status = parse_status_json(json).unwrap();
        assert_eq!(status.socket, "/home/u/.config/herdr/herdr.sock");
        assert!(status.running);
        assert_eq!(status.compatible, Some(true));
    }

    #[test]
    fn extracts_socket_when_server_not_running() {
        let json = r#"{"status":"not_running","running":false,"socket":"/home/u/.config/herdr/herdr.sock"}"#;
        let status = parse_status_json(json).unwrap();
        assert!(!status.running);
        assert_eq!(status.compatible, None);
    }

    #[test]
    fn rejects_status_json_without_socket() {
        assert!(parse_status_json(r#"{"running":true}"#).is_err());
    }

    #[test]
    fn rejects_non_json_status_output() {
        assert!(parse_status_json("herdr: command not found").is_err());
    }
}
