//! Shared client IO event pump.
//!
//! Both client event loops — the round-trip attach/app loop
//! ([`super::run_client_loop`]) and the local mirror loop
//! ([`super::run_connected_mirror_session`]) — share the same plumbing: an mpsc
//! channel of [`ClientLoopEvent`], background threads for stdin, terminal
//! resize, and reading server messages, and a `select!` over that channel plus a
//! periodic timer tick.
//!
//! [`ClientEventPump`] owns that plumbing once so the two loops can't drift, and
//! [`ClientEventPump::run`] is the single event-loop implementation; the
//! per-mode dispatch lives behind [`ClientLoopHandler`] (code-review Finding 2).

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{input, resize_poll_loop, server_reader_thread, ClientError, ClientLoopEvent};
use crate::ipc::LocalStream;

/// Why the event loop stopped.
pub(crate) enum LoopEnd {
    /// `should_quit` was set (e.g. Ctrl+C); the caller performs its clean-exit.
    Quit,
    /// The handler asked to stop (returned `ControlFlow::Break`).
    Handler,
}

/// Per-mode event processing.
///
/// `on_event` is deliberately synchronous: every client dispatch body is sync
/// (all server writes and local rendering are blocking); only the pump's
/// `select!` is async.
pub(crate) trait ClientLoopHandler {
    /// Handle one event. `Continue` keeps looping; `Break` stops the loop with
    /// [`LoopEnd::Handler`]. Errors abort the loop and propagate to the caller.
    fn on_event(
        &mut self,
        event: ClientLoopEvent,
        pump: &mut ClientEventPump,
    ) -> Result<ControlFlow<()>, ClientError>;
}

/// Owns the client event channel, its feeder threads, and the select/timer.
pub(crate) struct ClientEventPump {
    event_tx: mpsc::Sender<ClientLoopEvent>,
    event_rx: mpsc::Receiver<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    /// Events pushed back by a handler after over-reading a burst; drained
    /// before the channel so arrival order is preserved.
    pending: VecDeque<ClientLoopEvent>,
}

impl ClientEventPump {
    pub(crate) fn new(should_quit: Arc<AtomicBool>) -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        Self {
            event_tx,
            event_rx,
            should_quit,
            pending: VecDeque::new(),
        }
    }

    /// Spawn the stdin reader thread (lives until `should_quit`).
    pub(crate) fn spawn_stdin(
        &self,
        host_color_query_sent: bool,
        mouse_capture_active: Arc<AtomicBool>,
    ) {
        let tx = self.event_tx.clone();
        let quit = self.should_quit.clone();
        std::thread::spawn(move || {
            input::stdin_reader_loop(tx, &quit, host_color_query_sent, mouse_capture_active);
        });
    }

    /// Spawn the terminal resize poll thread.
    pub(crate) fn spawn_resize(&self, cols: u16, rows: u16, kitty_graphics_enabled: bool) {
        let tx = self.event_tx.clone();
        let quit = self.should_quit.clone();
        std::thread::spawn(move || {
            resize_poll_loop(tx, cols, rows, kitty_graphics_enabled, &quit);
        });
    }

    /// Spawn a server reader thread feeding this pump's channel. Returns its join
    /// handle so callers that reconnect (the mirror) can join the previous
    /// reader once it has exited.
    pub(crate) fn spawn_server_reader(
        &self,
        stream: LocalStream,
        max_frame_size: usize,
    ) -> JoinHandle<()> {
        let tx = self.event_tx.clone();
        let quit = self.should_quit.clone();
        std::thread::spawn(move || {
            server_reader_thread(stream, tx, &quit, max_frame_size);
        })
    }

    /// Spawn a per-terminal mirror reader feeding this pump's channel, tagging
    /// each message with `terminal_id` so the full mirror session can route
    /// several data connections through one pump (`design-mirror-tui.md` §2.1,
    /// Phase 3). Returns its join handle for cleanup on close/reconnect.
    #[cfg(unix)]
    pub(crate) fn spawn_mirror_reader(
        &self,
        stream: LocalStream,
        max_frame_size: usize,
        terminal_id: String,
    ) -> JoinHandle<()> {
        let tx = self.event_tx.clone();
        let quit = self.should_quit.clone();
        std::thread::spawn(move || {
            super::mirror_server_reader_thread(stream, tx, &quit, max_frame_size, terminal_id);
        })
    }

    /// A clone of the event sender, for feeder threads the pump doesn't own
    /// directly (e.g. the mirror session's JSON API control-event reader).
    #[cfg(unix)]
    pub(crate) fn event_sender(&self) -> mpsc::Sender<ClientLoopEvent> {
        self.event_tx.clone()
    }

    /// The shared quit flag, so feeder threads stop with the pump.
    #[cfg(unix)]
    pub(crate) fn quit_flag(&self) -> Arc<AtomicBool> {
        self.should_quit.clone()
    }

    /// Await the next event: pending buffer first, then a `select!` over the
    /// channel and a 100ms timer tick.
    pub(crate) async fn next_event(&mut self) -> ClientLoopEvent {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        tokio::select! {
            ev = self.event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
            _ = tokio::time::sleep(Duration::from_millis(100)) => ClientLoopEvent::Timer,
        }
    }

    /// Non-blocking pop used to coalesce a burst: pending buffer first, then the
    /// channel. Returns `None` when nothing is immediately available.
    pub(crate) fn try_next(&mut self) -> Option<ClientLoopEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        self.event_rx.try_recv().ok()
    }

    /// Return an event to the front of the queue so the next `next_event` /
    /// `try_next` yields it again (used when a burst drain over-reads a
    /// non-burst event).
    pub(crate) fn push_front(&mut self, event: ClientLoopEvent) {
        self.pending.push_front(event);
    }

    /// The single client event loop: quit-check, await an event, dispatch to the
    /// handler until it breaks or `should_quit` is set.
    pub(crate) async fn run<H: ClientLoopHandler>(
        &mut self,
        handler: &mut H,
    ) -> Result<LoopEnd, ClientError> {
        loop {
            if self.should_quit.load(Ordering::Acquire) {
                return Ok(LoopEnd::Quit);
            }
            let event = self.next_event().await;
            match handler.on_event(event, self)? {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => return Ok(LoopEnd::Handler),
            }
        }
    }
}
