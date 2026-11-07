//! A pane's shell is reaped when it exits, however it ends, without a blocking thread per pane.

use super::harness::*;

/// The pids of the server's direct children: the panes' shells.
fn server_children(server: u32) -> Vec<u32> {
    let mut children: Vec<u32> = fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            // Field 4 of /proc/PID/stat, after the parenthesised command name, is the parent.
            fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
                    == Some(server)
            })
        })
        .collect();
    children.sort_unstable();
    children
}

/// Threads of the server blocked in wait4: the old per-pane child watchers.
fn blocking_waiters(server: u32) -> usize {
    const WAIT4: &str = "61";
    fs::read_dir(format!("/proc/{server}/task"))
        .unwrap()
        .flatten()
        .filter(|task| {
            fs::read_to_string(task.path().join("syscall"))
                .is_ok_and(|syscall| syscall.split_whitespace().next() == Some(WAIT4))
        })
        .count()
}

fn create_pane(socket_path: &Path, label: &str) -> String {
    let created = run_cli_json(socket_path, &["workspace", "create", "--label", label]);
    created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace create should return its root pane")
        .to_owned()
}

/// The pane's shell: the one server child that was not there before it was created.
fn new_shell(server: u32, before: &[u32]) -> u32 {
    let mut shell = None;
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            shell = server_children(server)
                .into_iter()
                .find(|pid| !before.contains(pid));
            shell.is_some()
        }),
        "the pane's shell never appeared"
    );
    shell.unwrap()
}

/// The pane is gone and its shell reaped: no process, not even a zombie, has that pid.
fn assert_closed_and_reaped(socket_path: &Path, pane_id: &str, shell: u32, how: &str) {
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(25), || {
            !run_cli(socket_path, &["pane", "get", pane_id])
                .status
                .success()
        }),
        "the pane stayed open after its shell {how}"
    );
    assert!(
        wait_for_pid_exit(shell, Duration::from_secs(5)),
        "the shell that {how} was left unreaped (pid {shell})"
    );
}

#[test]
fn a_pane_whose_shell_exits_closes_and_is_reaped() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let server = herdr.child.process_id().expect("server pid");

    let before = server_children(server);
    let pane_id = create_pane(&socket_path, "reap-exit");
    let shell = new_shell(server, &before);
    assert_eq!(
        blocking_waiters(server),
        0,
        "a live pane holds a blocking wait4 thread"
    );

    let sent = run_cli(&socket_path, &["pane", "send-text", &pane_id, "exit 7\n"]);
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert_closed_and_reaped(&socket_path, &pane_id, shell, "exited");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn a_pane_whose_shell_is_killed_closes_and_is_reaped() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let server = herdr.child.process_id().expect("server pid");

    let before = server_children(server);
    let pane_id = create_pane(&socket_path, "reap-kill");
    let shell = new_shell(server, &before);
    unsafe { libc::kill(shell as libc::pid_t, libc::SIGKILL) };
    assert_closed_and_reaped(&socket_path, &pane_id, shell, "was killed");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn a_pane_restored_after_a_server_restart_is_reaped() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let mut herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    create_pane(&socket_path, "reap-restart");

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let pid = herdr.child.process_id();
    assert!(
        herdr.child.wait().unwrap().success(),
        "server stop should exit cleanly"
    );
    unregister_spawned_herdr_pid(pid);
    drop(herdr);

    let restarted = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let server = restarted.child.process_id().expect("server pid");
    let workspaces = run_cli_json(&socket_path, &["workspace", "list"]);
    let workspace_id = workspaces["result"]["workspaces"]
        .as_array()
        .expect("workspace.list should return workspaces")
        .iter()
        .find(|workspace| workspace["label"] == "reap-restart")
        .and_then(|workspace| workspace["workspace_id"].as_str())
        .expect("the workspace should be restored")
        .to_owned();
    let panes = run_cli_json(
        &socket_path,
        &["pane", "list", "--workspace", &workspace_id],
    );
    let pane_id = panes["result"]["panes"][0]["pane_id"]
        .as_str()
        .expect("the restored workspace should have its pane")
        .to_owned();
    let shell = new_shell(server, &[]);
    assert_eq!(
        blocking_waiters(server),
        0,
        "a restored pane holds a blocking wait4 thread"
    );

    unsafe { libc::kill(shell as libc::pid_t, libc::SIGKILL) };
    assert_closed_and_reaped(
        &socket_path,
        &pane_id,
        shell,
        "was killed after the restart",
    );

    cleanup_spawned_herdr(restarted, base);
}
