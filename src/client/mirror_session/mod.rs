//! Client-rendered mirror session.
//!
//! A mirror session reproduces a running herdr session **locally**: it renders the
//! full multi-pane TUI on the client from replicated terminal content, so
//! scrollback, search, and navigation are latency-immune (`design-mirror-tui.md`
//! §2). It uses two transports — a JSON API **control plane** for session
//! structure/events and a binary **data plane** for per-terminal content.
//!
//! This module provides:
//!
//! * [`JsonApiClient`] — typed JSON API calls + the structural event subscription
//!   (control plane, Phase 2).
//! * [`SessionReplica`] — a pure-data replica of the server's session structure,
//!   reconciled from those events (control plane, Phase 2).
//! * [`MirrorConnectionManager`] — one per-terminal wire connection feeding each
//!   terminal's local emulator, with writable-on-focus and resume/reconnect
//!   (data plane, Phase 3).
//!
//! The app driver that ties these together lands in a later phase.

// The pieces here are consumed by the mirror connection manager (Phase 3) and the
// mirror app driver (Phase 4); until those land, the public surface (and these
// re-exports) is not yet called from a shipping code path. Allow dead/unused
// module-wide rather than sprinkling per-item attributes.
#![allow(dead_code, unused_imports)]

mod action;
mod app;
mod connection;
mod control;
#[cfg(test)]
mod e2e_tests;
mod projection;
mod replica;

pub use app::run_mirror_session;
pub use connection::{MirrorApply, MirrorConnectionManager};
pub use control::{JsonApiClient, JsonApiError, StructuralEventStream};
pub use replica::{ReplicaChange, SessionReplica};
