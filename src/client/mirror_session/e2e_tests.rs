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
use crate::detect::{Agent, AgentState};
use crate::terminal::{AgentMetadataReport, TerminalId, TerminalState};
use crate::workspace::Workspace;

use super::projection::project_pane_metadata;
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
fn projected_pane_reproduces_server_agent_state_over_the_wire() {
    let (mut app, _hub) = app_with_two_tabs();

    // Drive one real server terminal into a rich agent state: Working, with a
    // manual label and a custom status. This is exactly the state the sidebar
    // agent panel, workspace dots, and border labels read from.
    let terminal_id = app
        .state
        .terminals
        .keys()
        .next()
        .expect("a terminal exists")
        .clone();
    {
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal present");
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_metadata(AgentMetadataReport {
            source: "test".into(),
            agent_label: None,
            applies_to_source: None,
            title: Some("running tests".into()),
            display_agent: None,
            custom_status: Some("87%".into()),
            state_labels: Default::default(),
            clear_title: false,
            clear_display_agent: false,
            clear_custom_status: false,
            clear_state_labels: false,
            ttl: None,
            seq: None,
        });
        terminal.set_manual_label("worker".into());
    }

    let server_terminal = app.state.terminals.get(&terminal_id).unwrap();
    let server_state = server_terminal.state;
    let server_presentation = server_terminal.effective_presentation();
    let server_label = server_terminal.manual_label.clone();
    assert_eq!(server_state, AgentState::Working, "server drove Working");

    // Take a real snapshot and reconcile it into a replica, exactly as the mirror
    // client does — the agent state now travels the wire as `PaneInfo.agent_status`
    // plus the presentation fields.
    let replica = replica_from_response(&app.handle_api_request(Request {
        id: "snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    }));

    let pane_info = replica
        .pane_for_terminal(&terminal_id.to_string())
        .expect("pane for the driven terminal");

    // Project the wire pane back into a TerminalState, as the mirror render path
    // does, and assert it reproduces what the server renderer would read.
    let mut projected = TerminalState::new(TerminalId::alloc(), "/".into());
    let seen = project_pane_metadata(&mut projected, pane_info);

    assert!(seen, "Working projects as seen");
    assert_eq!(
        projected.state, server_state,
        "projected agent state matches the server's"
    );
    assert_eq!(
        projected.manual_label, server_label,
        "projected manual label matches the server's"
    );
    assert_eq!(
        projected.effective_presentation(),
        server_presentation,
        "projected effective presentation matches the server's"
    );
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
