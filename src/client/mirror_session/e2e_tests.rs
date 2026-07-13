//! End-to-end control-plane tests against a real in-process server.
//!
//! These exercise the full data path — a genuine [`crate::app::App`] answering a
//! `session.snapshot` request and emitting real structural events into its
//! [`crate::api::EventHub`], then parsed exactly as [`super::JsonApiClient`] does
//! and reconciled into a [`super::SessionReplica`]. They cover everything except
//! the socket byte transport itself, which is owned and tested by
//! [`crate::api::client::ApiClient`].

use crate::api::schema::{
    EmptyParams, EventEnvelope, Method, Request, ResponseResult, SuccessResponse, TabRenameParams,
};
use crate::api::EventHub;
use crate::app::App;
use crate::config::Config;
use crate::workspace::Workspace;

use super::{ReplicaChange, SessionReplica};

/// A real app with one workspace, two tabs (one pane/terminal each) and runtime
/// resources bootstrapped, mirroring the app-layer snapshot tests.
fn app_with_two_tabs() -> (App, EventHub) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = EventHub::default();
    let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
    let mut workspace = Workspace::test_new("snapshot");
    workspace.test_add_tab(None);
    app.state.workspaces = vec![workspace];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    (app, event_hub)
}

/// Parses a `session.snapshot` response exactly as
/// [`super::JsonApiClient::session_snapshot`] does.
fn replica_from_response(response: &str) -> SessionReplica {
    let success: SuccessResponse = serde_json::from_str(response).expect("valid success response");
    let ResponseResult::SessionSnapshot { snapshot } = success.result else {
        panic!("expected session snapshot response");
    };
    SessionReplica::from_snapshot(*snapshot)
}

#[test]
fn replica_retrieves_pane_and_layout_state_from_server_snapshot() {
    let (mut app, _hub) = app_with_two_tabs();

    let response = app.handle_api_request(Request {
        id: "snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    });
    let replica = replica_from_response(&response);

    // The replica retrieved the full pane/terminal/layout/focus state.
    assert_eq!(replica.panes.len(), 2, "both panes present");
    assert_eq!(replica.terminal_ids().len(), 2, "two distinct terminals");
    assert_eq!(replica.layouts.len(), 2, "a layout per tab");
    assert!(replica.focused_terminal_id().is_some(), "focus resolved");
    // Every pane maps to a terminal we can look up (the data-plane key).
    for terminal_id in replica.terminal_ids() {
        assert!(replica.pane_for_terminal(&terminal_id).is_some());
    }
}

#[test]
fn replica_reconciles_a_real_structural_event_stream() {
    let (mut app, hub) = app_with_two_tabs();

    let mut replica = replica_from_response(&app.handle_api_request(Request {
        id: "snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    }));

    let tab_id = replica.tabs.first().expect("a tab").tab_id.clone();
    assert_ne!(replica.tabs[0].label, "renamed-by-test");

    // Rename the tab on the real server; this emits a genuine TabRenamed event
    // into the hub (no PTY spawn).
    let before_seq = hub.current_sequence();
    let response = app.handle_api_request(Request {
        id: "rename".into(),
        method: Method::TabRename(TabRenameParams {
            tab_id: tab_id.clone(),
            label: "renamed-by-test".into(),
        }),
    });
    assert!(
        !response.contains("\"error\""),
        "tab rename should succeed: {response}"
    );

    // Feed the server's events through the same JSON round-trip the live event
    // stream uses, then reconcile them.
    let mut changes = Vec::new();
    for (_seq, event) in hub.events_after(before_seq) {
        let wire = serde_json::to_string(&event).expect("serialize event");
        let parsed: EventEnvelope = serde_json::from_str(&wire).expect("parse event");
        changes.extend(replica.apply_event(parsed));
    }

    let renamed = replica
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .expect("tab still present");
    assert_eq!(
        renamed.label, "renamed-by-test",
        "the replica reflects the server rename"
    );
    assert!(
        changes.contains(&ReplicaChange::Structural),
        "a Structural change was produced: {changes:?}"
    );
}
