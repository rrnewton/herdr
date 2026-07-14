//! The client-side session replica.
//!
//! [`SessionReplica`] is a **pure-data** mirror of the server's authoritative
//! session structure — workspaces, tabs, panes (each carrying its `terminal_id`),
//! per-tab layout, focus, and agent status. It is built from a `session.snapshot`
//! and continuously reconciled from the structural event feed
//! ([`super::JsonApiClient::subscribe`]), per `design-mirror-tui.md` §2.2/§2.4.
//!
//! The server stays the single source of truth: nothing here mutates authoritative
//! state on its own; every change is the projection of a server response or event.
//! Because it is plain data (no PTYs, channels, or config), it is fully unit
//! testable — the same "pure data" property the app's `AppState::test_new()` has.
//!
//! Projection into the app's real `AppState` (which embeds live event channels,
//! locally-allocated `PaneId`s, and a BSP `TileLayout`) is deliberately **not**
//! done here; it belongs to the mirror app driver (Phase 4), which owns those
//! channels. This replica exposes exactly the `pane_id ⇄ terminal_id`, layout, and
//! focus facts the data-plane connection manager (Phase 3) needs.

use std::collections::{BTreeMap, BTreeSet};

use crate::api::schema::{
    EventData, EventEnvelope, PaneInfo, PaneLayoutSnapshot, SessionSnapshot, TabInfo, WorkspaceInfo,
};

/// A structurally-relevant change produced by reconciling one event, used by the
/// data-plane connection manager to open/close/promote per-terminal connections
/// (`design-mirror-tui.md` §2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaChange {
    /// A pane (and thus a new terminal) appeared; open a data connection.
    PaneAdded {
        pane_id: String,
        terminal_id: String,
    },
    /// A pane went away; close its data connection.
    PaneRemoved {
        pane_id: String,
        terminal_id: String,
    },
    /// Focus moved; promote the newly-focused terminal's connection to writable
    /// and demote the previous one. `terminal_id` is `None` if nothing is focused.
    FocusChanged {
        pane_id: Option<String>,
        terminal_id: Option<String>,
    },
    /// A tab's layout tree changed; the affected tab must be re-tiled.
    LayoutChanged { tab_id: String },
    /// Workspace/tab tree metadata changed (labels, ordering, focus) with no
    /// direct data-plane impact.
    Structural,
    /// The server raised a notification/toast (e.g. an agent finished). The mirror
    /// driver projects this into `AppState.toast` and arms a local expiry timer;
    /// it has no data-plane impact and does not persist in the replica structure.
    NotificationShown {
        kind: crate::api::schema::NotificationKind,
        title: String,
        context: String,
        workspace_id: Option<String>,
        pane_id: Option<String>,
    },
}

/// A pure-data replica of the server's session structure.
#[derive(Debug, Clone, Default)]
pub struct SessionReplica {
    /// Server version string reported by the last snapshot.
    pub version: String,
    /// Wire protocol version reported by the last snapshot.
    pub protocol: u32,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    /// Workspaces in display order.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Tabs across all workspaces (scope by `TabInfo::workspace_id`).
    pub tabs: Vec<TabInfo>,
    /// Panes keyed by public `pane_id`. Each carries its `terminal_id`.
    pub panes: BTreeMap<String, PaneInfo>,
    /// Latest layout geometry per `tab_id`.
    pub layouts: BTreeMap<String, PaneLayoutSnapshot>,
}

impl SessionReplica {
    /// Builds a replica from a full `session.snapshot`.
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        let panes = snapshot
            .panes
            .into_iter()
            .map(|pane| (pane.pane_id.clone(), pane))
            .collect();
        let layouts = snapshot
            .layouts
            .into_iter()
            .map(|layout| (layout.tab_id.clone(), layout))
            .collect();
        Self {
            version: snapshot.version,
            protocol: snapshot.protocol,
            focused_workspace_id: snapshot.focused_workspace_id,
            focused_tab_id: snapshot.focused_tab_id,
            focused_pane_id: snapshot.focused_pane_id,
            workspaces: snapshot.workspaces,
            tabs: snapshot.tabs,
            panes,
            layouts,
        }
    }

    // --- queries used by the data plane / render ---

    /// The pane backing `pane_id`, if present.
    pub fn pane(&self, pane_id: &str) -> Option<&PaneInfo> {
        self.panes.get(pane_id)
    }

    /// The pane bound to `terminal_id`, if present. A terminal backs exactly one
    /// pane within a session.
    pub fn pane_for_terminal(&self, terminal_id: &str) -> Option<&PaneInfo> {
        self.panes
            .values()
            .find(|pane| pane.terminal_id == terminal_id)
    }

    /// All distinct terminal ids currently in the session, in stable order — one
    /// data connection is opened per entry.
    pub fn terminal_ids(&self) -> Vec<String> {
        self.panes
            .values()
            .map(|pane| pane.terminal_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The terminal id of the currently-focused pane, if any. The connection for
    /// this terminal is the writable one.
    pub fn focused_terminal_id(&self) -> Option<&str> {
        self.focused_pane_id
            .as_deref()
            .and_then(|pane_id| self.panes.get(pane_id))
            .map(|pane| pane.terminal_id.as_str())
    }

    /// Panes belonging to `tab_id`.
    pub fn panes_in_tab(&self, tab_id: &str) -> Vec<&PaneInfo> {
        self.panes
            .values()
            .filter(|pane| pane.tab_id == tab_id)
            .collect()
    }

    // --- reconciliation ---

    /// Applies one structural event, returning the data-plane-relevant changes it
    /// produced (empty for events with no structural effect, e.g. output changes).
    pub fn apply_event(&mut self, envelope: EventEnvelope) -> Vec<ReplicaChange> {
        match envelope.data {
            EventData::WorkspaceCreated { workspace }
            | EventData::WorkspaceUpdated { workspace } => {
                self.upsert_workspace(workspace);
                vec![ReplicaChange::Structural]
            }
            EventData::WorkspaceClosed { workspace_id, .. } => self.remove_workspace(&workspace_id),
            EventData::WorkspaceRenamed {
                workspace_id,
                label,
            } => {
                if let Some(ws) = self.workspace_mut(&workspace_id) {
                    ws.label = label;
                }
                vec![ReplicaChange::Structural]
            }
            EventData::WorkspaceMoved { workspaces, .. } => {
                self.workspaces = workspaces;
                vec![ReplicaChange::Structural]
            }
            EventData::WorkspaceFocused { workspace_id } => {
                self.focused_workspace_id = Some(workspace_id);
                vec![ReplicaChange::Structural]
            }
            EventData::WorktreeCreated { workspace, .. }
            | EventData::WorktreeOpened { workspace, .. } => {
                self.upsert_workspace(workspace);
                vec![ReplicaChange::Structural]
            }
            EventData::WorktreeRemoved { workspace, .. } => {
                if let Some(workspace) = workspace {
                    self.upsert_workspace(workspace);
                }
                vec![ReplicaChange::Structural]
            }
            EventData::TabCreated { tab } => {
                self.upsert_tab(tab);
                vec![ReplicaChange::Structural]
            }
            EventData::TabClosed { tab_id, .. } => self.remove_tab(&tab_id),
            EventData::TabRenamed { tab_id, label, .. } => {
                if let Some(tab) = self.tab_mut(&tab_id) {
                    tab.label = label;
                }
                vec![ReplicaChange::Structural]
            }
            EventData::TabMoved {
                workspace_id, tabs, ..
            } => {
                // `tabs` is the new order for this workspace only; keep other
                // workspaces' tabs untouched.
                self.tabs.retain(|tab| tab.workspace_id != workspace_id);
                self.tabs.extend(tabs);
                vec![ReplicaChange::Structural]
            }
            EventData::TabFocused {
                tab_id,
                workspace_id,
            } => {
                self.focused_tab_id = Some(tab_id);
                self.focused_workspace_id = Some(workspace_id);
                vec![ReplicaChange::Structural]
            }
            EventData::PaneCreated { pane } => {
                let change = ReplicaChange::PaneAdded {
                    pane_id: pane.pane_id.clone(),
                    terminal_id: pane.terminal_id.clone(),
                };
                self.panes.insert(pane.pane_id.clone(), pane);
                vec![change]
            }
            EventData::PaneClosed { pane_id, .. } | EventData::PaneExited { pane_id, .. } => {
                let was_focused = self.focused_pane_id.as_deref() == Some(pane_id.as_str());
                match self.panes.remove(&pane_id) {
                    Some(pane) => {
                        let mut changes = vec![ReplicaChange::PaneRemoved {
                            pane_id,
                            terminal_id: pane.terminal_id,
                        }];
                        if was_focused {
                            self.focused_pane_id = None;
                            changes.push(ReplicaChange::FocusChanged {
                                pane_id: None,
                                terminal_id: None,
                            });
                        }
                        changes
                    }
                    None => Vec::new(),
                }
            }
            EventData::PaneFocused { pane_id, .. } => self.set_focused_pane(Some(pane_id)),
            EventData::PaneMoved {
                previous_pane_id,
                previous_tab_id,
                pane,
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id,
                ..
            } => self.apply_pane_moved(
                previous_pane_id,
                previous_tab_id,
                *pane,
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id,
            ),
            EventData::PaneAgentDetected { pane_id, agent, .. } => {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.agent = agent;
                }
                vec![ReplicaChange::Structural]
            }
            EventData::PaneAgentStatusChanged {
                pane_id,
                agent_status,
                agent,
                title,
                display_agent,
                custom_status,
                state_labels,
                ..
            } => {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.agent_status = agent_status;
                    pane.agent = agent;
                    pane.title = title;
                    pane.display_agent = display_agent;
                    pane.custom_status = custom_status;
                    pane.state_labels = state_labels;
                }
                vec![ReplicaChange::Structural]
            }
            EventData::LayoutUpdated { layout } => {
                let tab_id = layout.tab_id.clone();
                self.layouts.insert(tab_id.clone(), layout);
                vec![ReplicaChange::LayoutChanged { tab_id }]
            }
            EventData::NotificationShown {
                kind,
                title,
                context,
                workspace_id,
                pane_id,
            } => vec![ReplicaChange::NotificationShown {
                kind,
                title,
                context,
                workspace_id,
                pane_id,
            }],
            // Output revisions carry no structural change; the data plane feeds
            // content directly into each terminal's mirror runtime.
            EventData::PaneOutputChanged { .. } => Vec::new(),
        }
    }

    // --- reconciliation helpers ---

    fn workspace_mut(&mut self, workspace_id: &str) -> Option<&mut WorkspaceInfo> {
        self.workspaces
            .iter_mut()
            .find(|ws| ws.workspace_id == workspace_id)
    }

    fn tab_mut(&mut self, tab_id: &str) -> Option<&mut TabInfo> {
        self.tabs.iter_mut().find(|tab| tab.tab_id == tab_id)
    }

    fn upsert_workspace(&mut self, workspace: WorkspaceInfo) {
        match self.workspace_mut(&workspace.workspace_id) {
            Some(existing) => *existing = workspace,
            None => self.workspaces.push(workspace),
        }
    }

    fn upsert_tab(&mut self, tab: TabInfo) {
        match self.tab_mut(&tab.tab_id) {
            Some(existing) => *existing = tab,
            None => self.tabs.push(tab),
        }
    }

    fn remove_workspace(&mut self, workspace_id: &str) -> Vec<ReplicaChange> {
        self.workspaces.retain(|ws| ws.workspace_id != workspace_id);
        let closed_tabs: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
            .map(|tab| tab.tab_id.clone())
            .collect();
        self.tabs.retain(|tab| tab.workspace_id != workspace_id);
        for tab_id in &closed_tabs {
            self.layouts.remove(tab_id);
        }
        let mut changes = self.drop_panes(|pane| pane.workspace_id == workspace_id);
        if self.focused_workspace_id.as_deref() == Some(workspace_id) {
            self.focused_workspace_id = None;
        }
        changes.push(ReplicaChange::Structural);
        changes
    }

    fn remove_tab(&mut self, tab_id: &str) -> Vec<ReplicaChange> {
        self.tabs.retain(|tab| tab.tab_id != tab_id);
        self.layouts.remove(tab_id);
        let mut changes = self.drop_panes(|pane| pane.tab_id == tab_id);
        if self.focused_tab_id.as_deref() == Some(tab_id) {
            self.focused_tab_id = None;
        }
        changes.push(ReplicaChange::Structural);
        changes
    }

    /// Removes every pane matching `pred`, emitting a `PaneRemoved` per pane and
    /// clearing focus if the focused pane was among them.
    fn drop_panes(&mut self, pred: impl Fn(&PaneInfo) -> bool) -> Vec<ReplicaChange> {
        let removed: Vec<PaneInfo> = self
            .panes
            .values()
            .filter(|pane| pred(pane))
            .cloned()
            .collect();
        for pane in &removed {
            self.panes.remove(&pane.pane_id);
        }
        if self
            .focused_pane_id
            .as_deref()
            .is_some_and(|focused| removed.iter().any(|pane| pane.pane_id == focused))
        {
            self.focused_pane_id = None;
        }
        removed
            .into_iter()
            .map(|pane| ReplicaChange::PaneRemoved {
                pane_id: pane.pane_id,
                terminal_id: pane.terminal_id,
            })
            .collect()
    }

    fn set_focused_pane(&mut self, pane_id: Option<String>) -> Vec<ReplicaChange> {
        if self.focused_pane_id == pane_id {
            return Vec::new();
        }
        // Maintain per-pane `focused` flags so a projection can rely on them.
        if let Some(previous) = self
            .focused_pane_id
            .as_deref()
            .and_then(|id| self.panes.get_mut(id))
        {
            previous.focused = false;
        }
        self.focused_pane_id = pane_id.clone();
        let terminal_id = pane_id.as_deref().and_then(|id| {
            self.panes.get_mut(id).map(|pane| {
                pane.focused = true;
                pane.terminal_id.clone()
            })
        });
        vec![ReplicaChange::FocusChanged {
            pane_id,
            terminal_id,
        }]
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_pane_moved(
        &mut self,
        previous_pane_id: String,
        previous_tab_id: String,
        pane: PaneInfo,
        created_workspace: Option<WorkspaceInfo>,
        created_tab: Option<TabInfo>,
        closed_workspace_id: Option<String>,
        closed_tab_id: Option<String>,
    ) -> Vec<ReplicaChange> {
        let mut changes = Vec::new();
        if let Some(workspace) = created_workspace {
            self.upsert_workspace(workspace);
        }
        if let Some(tab) = created_tab {
            self.upsert_tab(tab);
        }
        // The pane keeps its terminal across a move, so no data connection opens or
        // closes — only the tiling changes. Re-key if the public id changed.
        let moved_focused = self.focused_pane_id.as_deref() == Some(previous_pane_id.as_str());
        let new_pane_id = pane.pane_id.clone();
        if previous_pane_id != new_pane_id {
            self.panes.remove(&previous_pane_id);
        }
        let new_tab_id = pane.tab_id.clone();
        self.panes.insert(new_pane_id.clone(), pane);

        if moved_focused {
            changes.extend(self.set_focused_pane(Some(new_pane_id)));
        }

        // A move can empty (and thus close) the source tab/workspace.
        if let Some(tab_id) = closed_tab_id {
            self.tabs.retain(|tab| tab.tab_id != tab_id);
            self.layouts.remove(&tab_id);
        }
        if let Some(workspace_id) = closed_workspace_id {
            self.workspaces.retain(|ws| ws.workspace_id != workspace_id);
        }

        // Re-tile the source tab, and the destination too if it differs.
        if new_tab_id != previous_tab_id {
            changes.push(ReplicaChange::LayoutChanged { tab_id: new_tab_id });
        }
        changes.push(ReplicaChange::LayoutChanged {
            tab_id: previous_tab_id,
        });
        changes.push(ReplicaChange::Structural);
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        AgentStatus, EventKind, PaneLayoutRect, PaneLayoutSnapshot, TabInfo, WorkspaceInfo,
    };
    use std::collections::HashMap;

    fn pane(pane_id: &str, terminal_id: &str, tab_id: &str, workspace_id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: terminal_id.into(),
            workspace_id: workspace_id.into(),
            tab_id: tab_id.into(),
            focused: false,
            cols: None,
            rows: None,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            display_agent: None,
            agent_status: AgentStatus::Unknown,
            custom_status: None,
            state_labels: HashMap::new(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    fn workspace(id: &str, active_tab_id: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.into(),
            number: 1,
            label: id.into(),
            focused: true,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: active_tab_id.into(),
            agent_status: AgentStatus::Unknown,
            branch: None,
            git_ahead: None,
            git_behind: None,
            worktree: None,
        }
    }

    fn tab(tab_id: &str, workspace_id: &str) -> TabInfo {
        TabInfo {
            tab_id: tab_id.into(),
            workspace_id: workspace_id.into(),
            number: 1,
            label: tab_id.into(),
            focused: true,
            pane_count: 1,
            agent_status: AgentStatus::Unknown,
        }
    }

    fn layout(tab_id: &str, focused_pane_id: &str) -> PaneLayoutSnapshot {
        PaneLayoutSnapshot {
            workspace_id: "ws1".into(),
            tab_id: tab_id.into(),
            zoomed: false,
            area: PaneLayoutRect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            focused_pane_id: focused_pane_id.into(),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: "test".into(),
            protocol: 1,
            focused_workspace_id: Some("ws1".into()),
            focused_tab_id: Some("tab1".into()),
            focused_pane_id: Some("pane1".into()),
            workspaces: vec![workspace("ws1", "tab1")],
            tabs: vec![tab("tab1", "ws1")],
            panes: vec![pane("pane1", "term1", "tab1", "ws1")],
            layouts: vec![layout("tab1", "pane1")],
            agents: Vec::new(),
        }
    }

    fn envelope(event: EventKind, data: EventData) -> EventEnvelope {
        EventEnvelope { event, data }
    }

    #[test]
    fn builds_replica_from_snapshot() {
        let replica = SessionReplica::from_snapshot(snapshot());
        assert_eq!(replica.terminal_ids(), vec!["term1".to_string()]);
        assert_eq!(replica.focused_terminal_id(), Some("term1"));
        assert_eq!(replica.pane_for_terminal("term1").unwrap().pane_id, "pane1");
        assert!(replica.layouts.contains_key("tab1"));
    }

    #[test]
    fn pane_created_and_closed_emit_data_plane_changes() {
        let mut replica = SessionReplica::from_snapshot(snapshot());

        let changes = replica.apply_event(envelope(
            EventKind::PaneCreated,
            EventData::PaneCreated {
                pane: pane("pane2", "term2", "tab1", "ws1"),
            },
        ));
        assert_eq!(
            changes,
            vec![ReplicaChange::PaneAdded {
                pane_id: "pane2".into(),
                terminal_id: "term2".into(),
            }]
        );
        assert_eq!(replica.terminal_ids(), vec!["term1", "term2"]);

        let changes = replica.apply_event(envelope(
            EventKind::PaneClosed,
            EventData::PaneClosed {
                pane_id: "pane2".into(),
                workspace_id: "ws1".into(),
            },
        ));
        assert_eq!(
            changes,
            vec![ReplicaChange::PaneRemoved {
                pane_id: "pane2".into(),
                terminal_id: "term2".into(),
            }]
        );
        assert_eq!(replica.terminal_ids(), vec!["term1"]);
    }

    #[test]
    fn pane_focused_reports_new_terminal_and_updates_flags() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        replica.apply_event(envelope(
            EventKind::PaneCreated,
            EventData::PaneCreated {
                pane: pane("pane2", "term2", "tab1", "ws1"),
            },
        ));

        let changes = replica.apply_event(envelope(
            EventKind::PaneFocused,
            EventData::PaneFocused {
                pane_id: "pane2".into(),
                workspace_id: "ws1".into(),
            },
        ));
        assert_eq!(
            changes,
            vec![ReplicaChange::FocusChanged {
                pane_id: Some("pane2".into()),
                terminal_id: Some("term2".into()),
            }]
        );
        assert_eq!(replica.focused_terminal_id(), Some("term2"));
        assert!(replica.pane("pane2").unwrap().focused);
        assert!(!replica.pane("pane1").unwrap().focused);
    }

    #[test]
    fn layout_updated_replaces_tab_layout() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        let changes = replica.apply_event(envelope(
            EventKind::LayoutUpdated,
            EventData::LayoutUpdated {
                layout: layout("tab1", "pane1"),
            },
        ));
        assert_eq!(
            changes,
            vec![ReplicaChange::LayoutChanged {
                tab_id: "tab1".into()
            }]
        );
    }

    #[test]
    fn workspace_closed_cascades_to_tabs_and_panes() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        let changes = replica.apply_event(envelope(
            EventKind::WorkspaceClosed,
            EventData::WorkspaceClosed {
                workspace_id: "ws1".into(),
                workspace: None,
            },
        ));
        assert!(changes.contains(&ReplicaChange::PaneRemoved {
            pane_id: "pane1".into(),
            terminal_id: "term1".into(),
        }));
        assert!(replica.workspaces.is_empty());
        assert!(replica.tabs.is_empty());
        assert!(replica.panes.is_empty());
        assert!(replica.layouts.is_empty());
        assert_eq!(replica.focused_pane_id, None);
    }

    #[test]
    fn tab_created_is_tracked() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        replica.apply_event(envelope(
            EventKind::TabCreated,
            EventData::TabCreated {
                tab: tab("tab2", "ws1"),
            },
        ));
        assert_eq!(replica.tabs.len(), 2);
    }

    #[test]
    fn agent_status_changed_updates_pane_presentation() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        replica.apply_event(envelope(
            EventKind::PaneAgentStatusChanged,
            EventData::PaneAgentStatusChanged {
                pane_id: "pane1".into(),
                workspace_id: "ws1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("building".into()),
                display_agent: None,
                custom_status: None,
                state_labels: HashMap::new(),
            },
        ));
        let pane = replica.pane("pane1").unwrap();
        assert_eq!(pane.agent_status, AgentStatus::Working);
        assert_eq!(pane.agent.as_deref(), Some("pi"));
        assert_eq!(pane.title.as_deref(), Some("building"));
    }

    #[test]
    fn notification_shown_yields_notification_change() {
        use crate::api::schema::NotificationKind;
        let mut replica = SessionReplica::from_snapshot(snapshot());
        let changes = replica.apply_event(envelope(
            EventKind::NotificationShown,
            EventData::NotificationShown {
                kind: NotificationKind::Finished,
                title: "pi finished".into(),
                context: "workspace api".into(),
                workspace_id: Some("ws1".into()),
                pane_id: Some("pane1".into()),
            },
        ));
        assert_eq!(
            changes,
            vec![ReplicaChange::NotificationShown {
                kind: NotificationKind::Finished,
                title: "pi finished".into(),
                context: "workspace api".into(),
                workspace_id: Some("ws1".into()),
                pane_id: Some("pane1".into()),
            }]
        );
        // Notifications are ephemeral; they don't mutate the replica structure.
        assert_eq!(replica.panes.len(), 1);
    }

    #[test]
    fn output_changed_is_not_structural() {
        let mut replica = SessionReplica::from_snapshot(snapshot());
        let changes = replica.apply_event(envelope(
            EventKind::PaneOutputChanged,
            EventData::PaneOutputChanged {
                pane_id: "pane1".into(),
                workspace_id: "ws1".into(),
                revision: 7,
            },
        ));
        assert!(changes.is_empty());
    }
}
