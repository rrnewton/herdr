//! One thread reaps every pane's child process.
//!
//! A pane used to wait for its shell on a tokio blocking-pool thread
//! (`spawn_blocking(child.wait())`), so every live pane held one of the pool's
//! 512 threads for its whole life. Here each child is registered as a pidfd
//! (which becomes readable when the process exits, zombie or not) with one
//! epoll set, and a single `herdr-reaper` thread waits on it, reaps whichever
//! child exited, and hands the status to the pane's async task.
//!
//! It reaps only the pids registered with it (`waitpid(pid)`, never `waitpid(-1)`)
//! and installs no SIGCHLD handler, so it cannot take another child's exit, such
//! as one `tokio::process` is waiting for.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use tokio::sync::oneshot;
use tracing::{error, warn};

/// What the reaper reports for a child: its exit status, or why it could not be read.
pub(crate) type ChildExit = io::Result<ExitStatus>;

/// A child the reaper should watch: its pid, its pidfd, and where to send its exit.
struct Watch {
    pid: u32,
    pidfd: OwnedFd,
    exited: oneshot::Sender<ChildExit>,
}

/// The running reaper: the queue of new watches and the eventfd that wakes it for them.
struct Reaper {
    queue: Mutex<mpsc::Sender<Watch>>,
    wake: OwnedFd,
}

/// The epoll token of the wake eventfd; every other token is a pid.
const WAKE_TOKEN: u64 = u64::MAX;

static REAPER: OnceLock<Option<Reaper>> = OnceLock::new();

/// Watch `pid`, a child of this process that nothing else reaps, and receive its exit status
/// once it has exited and been reaped. `None` when this kernel cannot watch it (no pidfds, or the
/// reaper could not start); the caller then waits for the child itself.
pub(crate) fn watch_child_exit(pid: u32) -> Option<oneshot::Receiver<ChildExit>> {
    let reaper = REAPER.get_or_init(start).as_ref()?;
    let pidfd = match pidfd_open(pid) {
        Ok(pidfd) => pidfd,
        Err(err) => {
            warn!(pid, err = %err, "cannot watch pane child with a pidfd; waiting on a thread");
            return None;
        }
    };
    let (exited, receiver) = oneshot::channel();
    reaper
        .queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(Watch { pid, pidfd, exited })
        .ok()?;
    wake(reaper.wake.as_raw_fd());
    Some(receiver)
}

fn start() -> Option<Reaper> {
    let started = (|| -> io::Result<Reaper> {
        let epoll = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
        let wake = cvt(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
        let wake = unsafe { OwnedFd::from_raw_fd(wake) };
        add(epoll.as_raw_fd(), wake.as_raw_fd(), WAKE_TOKEN)?;
        let (queue, watches) = mpsc::channel();
        let wake_fd = wake.as_raw_fd();
        std::thread::Builder::new()
            .name("herdr-reaper".into())
            .spawn(move || run(epoll, wake_fd, watches))?;
        Ok(Reaper {
            queue: Mutex::new(queue),
            wake,
        })
    })();
    match started {
        Ok(reaper) => Some(reaper),
        Err(err) => {
            error!(err = %err, "cannot start the pane child reaper; panes wait on threads");
            None
        }
    }
}

fn run(epoll: OwnedFd, wake_fd: RawFd, watches: mpsc::Receiver<Watch>) {
    let mut watched: HashMap<u64, Watch> = HashMap::new();
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
    loop {
        let ready = unsafe {
            libc::epoll_wait(
                epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                -1,
            )
        };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            error!(err = %err, "pane child reaper stopped");
            return;
        }
        for event in &events[..ready as usize] {
            let token = event.u64;
            if token == WAKE_TOKEN {
                drain(wake_fd);
                while let Ok(watch) = watches.try_recv() {
                    let token = u64::from(watch.pid);
                    match add(epoll.as_raw_fd(), watch.pidfd.as_raw_fd(), token) {
                        Ok(()) => {
                            watched.insert(token, watch);
                        }
                        Err(err) => {
                            warn!(pid = watch.pid, err = %err, "cannot watch pane child");
                            let _ = watch.exited.send(reap(watch.pid));
                        }
                    }
                }
            } else if let Some(watch) = watched.remove(&token) {
                // Readable pidfd: the process has exited, so this wait does not block.
                let exit = reap(watch.pid);
                drop(watch.pidfd);
                let _ = watch.exited.send(exit);
            }
        }
    }
}

/// Reap `pid` (blocking until it exits) and return its status, as `Child::wait` would.
fn reap(pid: u32) -> ChildExit {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        if result == pid as libc::pid_t {
            return Ok(ExitStatus::from_raw(status));
        }
        let err = io::Error::last_os_error();
        if result < 0 && err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // ECHILD: something else reaped it; the process is gone, its status unknown.
        return Err(err);
    }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn add(epoll: RawFd, fd: RawFd, token: u64) -> io::Result<()> {
    let mut event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: token,
    };
    cvt(unsafe { libc::epoll_ctl(epoll, libc::EPOLL_CTL_ADD, fd, &mut event) }).map(drop)
}

fn wake(eventfd: RawFd) {
    let one: u64 = 1;
    unsafe { libc::write(eventfd, (&one as *const u64).cast(), 8) };
}

fn drain(eventfd: RawFd) {
    let mut count: u64 = 0;
    unsafe { libc::read(eventfd, (&mut count as *mut u64).cast(), 8) };
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
// The reaper under test is what waits for these children.
#[allow(clippy::zombie_processes)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn exit_of(child: &std::process::Child) -> ExitStatus {
        let receiver = watch_child_exit(child.id()).expect("pidfds are available");
        receiver
            .blocking_recv()
            .expect("the reaper reports every exit")
            .expect("the reaper reads the status")
    }

    fn reaped(pid: u32) -> bool {
        // Reaped: the pid no longer names a zombie (or anything) of ours.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => !stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    #[test]
    fn reports_a_shell_exit_code_and_reaps_it() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(exit_of(&child).code(), Some(3));
        assert!(reaped(pid));
    }

    #[test]
    fn reports_a_kill_by_signal() {
        let child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let receiver = watch_child_exit(pid).expect("pidfds are available");
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let status = receiver.blocking_recv().unwrap().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(reaped(pid));
    }

    #[test]
    fn reaps_a_child_that_was_already_a_zombie() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while reaped(pid) {
            assert!(Instant::now() < deadline, "the child never became a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(exit_of(&child).code(), Some(0));
        assert!(reaped(pid));
    }

    #[test]
    fn leaves_other_children_to_their_own_waiters() {
        // A child the reaper was not asked about is still there for its own wait.
        let mut other = Command::new("/bin/sh")
            .args(["-c", "exit 5"])
            .spawn()
            .unwrap();
        let watched = Command::new("/bin/sh")
            .args(["-c", "exit 6"])
            .spawn()
            .unwrap();
        assert_eq!(exit_of(&watched).code(), Some(6));
        assert_eq!(other.wait().unwrap().code(), Some(5));
    }

    #[test]
    fn watches_many_children_on_one_thread() {
        let children: Vec<_> = (0..20)
            .map(|code| {
                Command::new("/bin/sh")
                    .args(["-c", &format!("sleep 0.2; exit {code}")])
                    .spawn()
                    .unwrap()
            })
            .collect();
        let receivers: Vec<_> = children
            .iter()
            .map(|child| watch_child_exit(child.id()).unwrap())
            .collect();
        for (code, receiver) in receivers.into_iter().enumerate() {
            let status = receiver.blocking_recv().unwrap().unwrap();
            assert_eq!(status.code(), Some(code as i32));
        }
        let reapers = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .flatten()
            .filter(|task| {
                std::fs::read_to_string(task.path().join("comm"))
                    .is_ok_and(|name| name.trim() == "herdr-reaper")
            })
            .count();
        assert_eq!(reapers, 1);
    }
}
