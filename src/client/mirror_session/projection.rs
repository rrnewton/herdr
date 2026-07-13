//! Projection of a [`SessionReplica`] into a real [`AppState`].
//!
//! The mirror session renders the existing pure TUI render path over
//! `(&AppState, &TerminalRuntimeRegistry)` (`design-mirror-tui.md` §3). That
//! render path needs a genuine [`AppState`] with `Workspace`/`Tab`/`PaneState`
//! and a BSP [`TileLayout`] per tab — but the control plane only gives us pure
//! JSON metadata (`SessionReplica`). This module bridges the two: it rebuilds the
//! app's workspaces/tabs/panes/terminals from the replica plus each tab's
//! exported layout tree, allocating local `PaneId`s and reconstructing the
//! server's `TerminalId`s so the render registry keys line up (§1.3 caveat).
//!
//! No PTYs are spawned — panes carry only `PaneState`; live content is rendered
//! from the mirror registry the data plane feeds. The server stays authoritative:
//! this is a pure re-projection of the current replica, called at startup and
//! after any structural change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};
use tracing::warn;

use crate::api::schema::{
    AgentStatus, LayoutNode, PaneInfo, SplitDirection, TabInfo, WorkspaceInfo,
};
use crate::app::{AppState, Mode};
use crate::detect::AgentState;
use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::PaneState;
use crate::terminal::{AgentMetadataReport, TerminalId, TerminalState};
use crate::workspace::{Tab, Workspace, WorktreeSpaceMembership};

use super::control::JsonApiClient;
use super::replica::SessionReplica;

/// Shared, unused event/notify handles a projected `Tab` needs to satisfy its
/// struct shape. The mirror has no server services to notify, so these are inert.
#[derive(Clone)]
pub(crate) struct MirrorTabChannels {
    pub events: mpsc::Sender<AppEvent>,
    pub render_notify: Arc<Notify>,
    pub render_dirty: Arc<AtomicBool>,
}

/// Rebuilds `state.workspaces`/`terminals`/focus from the current `replica`,
/// pulling each tab's layout tree from the server via `control.layout_export`.
///
/// Presentation-only fields already on `state` (theme, keybinds, sidebar, config)
/// are preserved; only the session structure is replaced. Returns the mapping of
/// server `terminal_id` → local [`TerminalId`] so the caller can key the render
/// registry with the exact ids the panes reference.
pub(crate) fn rebuild_app_state(
    state: &mut AppState,
    control: &JsonApiClient,
    replica: &SessionReplica,
    channels: &MirrorTabChannels,
    preserve_overlay_mode: bool,
) -> HashMap<String, TerminalId> {
    let mut terminal_ids: HashMap<String, TerminalId> = HashMap::new();
    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut public_pane_aliases = HashMap::new();

    for ws_info in &replica.workspaces {
        let mut tabs: Vec<TabInfo> = replica
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == ws_info.workspace_id)
            .cloned()
            .collect();
        tabs.sort_by_key(|tab| tab.number);

        let mut projected_tabs = Vec::new();
        let mut public_pane_numbers = HashMap::new();
        let mut next_pane_number = 1usize;
        let mut identity_cwd: Option<PathBuf> = None;

        for tab_info in &tabs {
            let Some(projected) = project_tab(
                control,
                replica,
                tab_info,
                channels,
                &mut terminal_ids,
                &mut terminals,
                &mut public_pane_aliases,
                &mut public_pane_numbers,
                &mut next_pane_number,
                &mut identity_cwd,
            ) else {
                continue;
            };
            projected_tabs.push(projected);
        }

        if projected_tabs.is_empty() {
            // A workspace with no renderable tab would violate the "≥1 tab"
            // invariant; skip it rather than panic.
            warn!(
                workspace_id = ws_info.workspace_id,
                "mirror projection skipped workspace with no tabs"
            );
            continue;
        }

        let active_tab = tabs
            .iter()
            .position(|tab| Some(&tab.tab_id) == replica.focused_tab_id.as_ref())
            .filter(|idx| *idx < projected_tabs.len())
            .unwrap_or(0);

        let identity_cwd =
            identity_cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));

        let projected_ws = project_workspace_metadata(ws_info);

        let mut workspace = Workspace::from_mirror(
            ws_info.workspace_id.clone(),
            Some(ws_info.label.clone()),
            identity_cwd,
            projected_ws.branch,
            projected_tabs,
            active_tab,
            public_pane_numbers,
            next_pane_number,
            tabs.len() + 1,
        );
        // The server exports each workspace's worktree-space membership so the
        // mirror sidebar groups linked worktrees under one collapsible space,
        // exactly like the server TUI (`render_workspace_list`).
        workspace.worktree_space = projected_ws.worktree_space;
        workspaces.push(workspace);
    }

    let active = replica
        .focused_workspace_id
        .as_ref()
        .and_then(|id| {
            replica
                .workspaces
                .iter()
                .position(|ws| &ws.workspace_id == id)
        })
        .filter(|idx| *idx < workspaces.len())
        .or_else(|| (!workspaces.is_empty()).then_some(0));

    state.workspaces = workspaces;
    state.terminals = terminals;
    state.public_pane_id_aliases = public_pane_aliases;
    state.pane_id_aliases.clear();
    state.active = active;
    state.selected = active.unwrap_or(0);
    // On a re-projection, a view-local overlay (copy-mode, resize, a modal, …) is
    // client state and must survive a background event (e.g. an agent-status
    // change), so we don't yank the user out of it; only the two base modes track
    // the projected structure. On the initial build we always adopt a base mode
    // (the source `App` may start in Onboarding/ProductAnnouncement).
    if !preserve_overlay_mode || matches!(state.mode, Mode::Terminal | Mode::Navigate) {
        state.mode = if active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    terminal_ids
}

/// Projects one tab: converts its exported layout tree into a [`TileLayout`],
/// builds `PaneState`s + `TerminalState`s, and records the pane-id/number maps.
#[allow(clippy::too_many_arguments)]
fn project_tab(
    control: &JsonApiClient,
    replica: &SessionReplica,
    tab_info: &TabInfo,
    channels: &MirrorTabChannels,
    terminal_ids: &mut HashMap<String, TerminalId>,
    terminals: &mut HashMap<TerminalId, TerminalState>,
    public_pane_aliases: &mut HashMap<String, PaneId>,
    public_pane_numbers: &mut HashMap<PaneId, usize>,
    next_pane_number: &mut usize,
    identity_cwd: &mut Option<PathBuf>,
) -> Option<Tab> {
    let layout = match control.layout_export(Some(tab_info.tab_id.clone())) {
        Ok(layout) => layout,
        Err(err) => {
            warn!(tab_id = tab_info.tab_id, err = %err, "mirror layout export failed");
            return None;
        }
    };

    let mut panes = HashMap::new();
    let mut pane_string_to_id: HashMap<String, PaneId> = HashMap::new();
    let mut first_pane: Option<PaneId> = None;

    let node = convert_layout_node(
        &layout.root,
        replica,
        terminal_ids,
        terminals,
        &mut panes,
        &mut pane_string_to_id,
        public_pane_aliases,
        public_pane_numbers,
        next_pane_number,
        identity_cwd,
        &mut first_pane,
    );

    let root_pane = first_pane?;
    let focus_pane = pane_string_to_id
        .get(&layout.focused_pane_id)
        .copied()
        .unwrap_or(root_pane);

    let tile = TileLayout::from_saved(node, focus_pane);

    let (label, number) = project_tab_metadata(tab_info);

    Some(Tab::from_mirror(
        Some(label),
        number,
        root_pane,
        tile,
        panes,
        layout.zoomed,
        channels.events.clone(),
        channels.render_notify.clone(),
        channels.render_dirty.clone(),
    ))
}

/// Recursively converts an API [`LayoutNode`] tree into a [`Node`] tree,
/// allocating a fresh [`PaneId`] per leaf and building that pane's state.
#[allow(clippy::too_many_arguments)]
fn convert_layout_node(
    node: &LayoutNode,
    replica: &SessionReplica,
    terminal_ids: &mut HashMap<String, TerminalId>,
    terminals: &mut HashMap<TerminalId, TerminalState>,
    panes: &mut HashMap<PaneId, PaneState>,
    pane_string_to_id: &mut HashMap<String, PaneId>,
    public_pane_aliases: &mut HashMap<String, PaneId>,
    public_pane_numbers: &mut HashMap<PaneId, usize>,
    next_pane_number: &mut usize,
    identity_cwd: &mut Option<PathBuf>,
    first_pane: &mut Option<PaneId>,
) -> Node {
    match node {
        LayoutNode::Pane { pane } => {
            let pane_id = PaneId::alloc();
            if first_pane.is_none() {
                *first_pane = Some(pane_id);
            }

            // Resolve this leaf's server metadata (terminal id, cwd, label).
            let info = pane.pane_id.as_ref().and_then(|id| replica.pane(id));

            let terminal_id = match info {
                Some(info) => terminal_ids
                    .entry(info.terminal_id.clone())
                    .or_insert_with(|| TerminalId::from_string(info.terminal_id.clone()))
                    .clone(),
                None => TerminalId::alloc(),
            };

            let cwd = info
                .and_then(|info| info.cwd.clone())
                .or_else(|| pane.cwd.clone())
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
            if identity_cwd.is_none() {
                *identity_cwd = Some(cwd.clone());
            }

            let mut terminal_state = TerminalState::new(terminal_id.clone(), cwd);
            // Reconstruct the server's pane presentation (labels, agent state,
            // and effective metadata) so the shared renderer draws the mirror's
            // sidebar dots, agent panel, and border labels identically. Defaults
            // to `seen = true` when the server sent no metadata for this pane.
            let seen = info
                .map(|info| project_pane_metadata(&mut terminal_state, info))
                .unwrap_or(true);
            terminals.insert(terminal_id.clone(), terminal_state);
            let mut pane_state = PaneState::new(terminal_id);
            pane_state.seen = seen;
            panes.insert(pane_id, pane_state);

            if let Some(public_id) = pane.pane_id.clone() {
                pane_string_to_id.insert(public_id.clone(), pane_id);
                public_pane_aliases.insert(public_id, pane_id);
            }
            public_pane_numbers.insert(pane_id, *next_pane_number);
            *next_pane_number += 1;

            Node::Pane(pane_id)
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => Node::Split {
            direction: match direction {
                SplitDirection::Right => Direction::Horizontal,
                SplitDirection::Down => Direction::Vertical,
            },
            ratio: *ratio,
            first: Box::new(convert_layout_node(
                first,
                replica,
                terminal_ids,
                terminals,
                panes,
                pane_string_to_id,
                public_pane_aliases,
                public_pane_numbers,
                next_pane_number,
                identity_cwd,
                first_pane,
            )),
            second: Box::new(convert_layout_node(
                second,
                replica,
                terminal_ids,
                terminals,
                panes,
                pane_string_to_id,
                public_pane_aliases,
                public_pane_numbers,
                next_pane_number,
                identity_cwd,
                first_pane,
            )),
        },
    }
}

/// Projects one server [`TabInfo`] into the render inputs the mirror rebuilds
/// (label + stable number). Destructured exhaustively (no `..`) so a new
/// server-provided tab field fails the build here until the mirror handles it.
fn project_tab_metadata(info: &TabInfo) -> (String, usize) {
    let TabInfo {
        // Used by the caller to fetch this tab's exported layout.
        tab_id: _,
        // Tabs are already scoped to their workspace by the caller's filter.
        workspace_id: _,
        // Derived by the projection from focus/panes, not carried into `Tab`.
        focused: _,
        pane_count: _,
        agent_status: _,
        label,
        number,
    } = info;
    (label.clone(), *number)
}

/// Metadata source key for presentation the mirror reconstructs from the server's
/// `PaneInfo`. A single stable source keeps `effective_presentation()` returning
/// exactly the fields the server computed (it aggregates by source).
const MIRROR_METADATA_SOURCE: &str = "mirror";

/// Reverses [`crate::app::api_helpers::pane_agent_status`]: recovers the
/// `(AgentState, seen)` pair the server collapsed into a single [`AgentStatus`].
/// `Done` is the only status that encodes "unseen"; the working/blocked seen bit
/// is not carried on the wire but does not affect attention priority, so `true`
/// is the faithful default there.
fn agent_status_to_state_and_seen(status: AgentStatus) -> (AgentState, bool) {
    match status {
        AgentStatus::Idle => (AgentState::Idle, true),
        AgentStatus::Working => (AgentState::Working, true),
        AgentStatus::Blocked => (AgentState::Blocked, true),
        AgentStatus::Done => (AgentState::Idle, false),
        AgentStatus::Unknown => (AgentState::Unknown, true),
    }
}

/// Projects one server [`PaneInfo`] onto the reconstructed [`TerminalState`],
/// returning the pane's `seen` flag for its [`PaneState`].
///
/// The `PaneInfo` is destructured exhaustively (no `..`): when the server grows a
/// new pane-presentation field, this fails to compile until the mirror decides
/// whether and how to render it — the render-layer analogue of the
/// compile-enforced input pipeline. Fields that are structural ids, live
/// geometry, or runtime facts (not render inputs the projection reconstructs) are
/// bound to `_` with a note explaining why.
pub(super) fn project_pane_metadata(terminal: &mut TerminalState, info: &PaneInfo) -> bool {
    let PaneInfo {
        // Structural ids: consumed by the caller / replica keying, not TerminalState.
        pane_id: _,
        terminal_id: _,
        workspace_id: _,
        tab_id: _,
        // Focus is projected via the tab's `TileLayout` focus + `AppState::active`.
        focused: _,
        // Live geometry follows the mirror runtime's replicated stream (§3.3).
        cols: _,
        rows: _,
        // cwd is resolved by the caller (with the layout node's cwd as fallback).
        cwd: _,
        foreground_cwd: _,
        label,
        agent,
        title,
        display_agent,
        agent_status,
        custom_status,
        state_labels,
        // Runtime facts, not render inputs the projection reconstructs.
        agent_session: _,
        scroll: _,
        revision: _,
    } = info;

    if let Some(label) = label {
        terminal.set_manual_label(label.clone());
    }
    if let Some(agent) = agent {
        terminal.set_agent_name(agent.clone());
    }

    // Reconstruct the effective presentation (title / display agent / custom
    // status / state labels) the server computed so the sidebar agent panel and
    // pane border labels match. `set_agent_metadata` recomputes `state` from
    // `fallback_state`, so it must run before we pin the agent state below.
    if title.is_some()
        || display_agent.is_some()
        || custom_status.is_some()
        || !state_labels.is_empty()
    {
        terminal.set_agent_metadata(AgentMetadataReport {
            source: MIRROR_METADATA_SOURCE.to_string(),
            agent_label: None,
            applies_to_source: None,
            title: title.clone(),
            display_agent: display_agent.clone(),
            custom_status: custom_status.clone(),
            state_labels: state_labels.clone(),
            clear_title: false,
            clear_display_agent: false,
            clear_custom_status: false,
            clear_state_labels: false,
            ttl: None,
            seq: None,
        });
    }

    let (state, seen) = agent_status_to_state_and_seen(*agent_status);
    terminal.fallback_state = state;
    terminal.state = state;
    seen
}

/// The render inputs the mirror reconstructs from a server [`WorkspaceInfo`].
pub(super) struct ProjectedWorkspace {
    pub(super) branch: Option<String>,
    pub(super) worktree_space: Option<WorktreeSpaceMembership>,
}

/// Projects one server [`WorkspaceInfo`] into the render inputs the mirror rebuilds
/// (branch subtitle + worktree-space grouping). Like [`project_pane_metadata`],
/// the struct is destructured exhaustively so a new server-provided workspace
/// field fails the build here until the mirror handles it.
pub(super) fn project_workspace_metadata(info: &WorkspaceInfo) -> ProjectedWorkspace {
    let WorkspaceInfo {
        // Public id + label are passed to `Workspace::from_mirror` directly.
        workspace_id: _,
        label: _,
        // Derived by the projection from the rebuilt tabs/panes, not the wire.
        number: _,
        focused: _,
        pane_count: _,
        tab_count: _,
        active_tab_id: _,
        // Aggregated by the renderer from each pane's projected agent state.
        agent_status: _,
        branch,
        worktree,
    } = info;

    let worktree_space = worktree.as_ref().map(|worktree| WorktreeSpaceMembership {
        key: worktree.repo_key.clone(),
        label: worktree.repo_name.clone(),
        repo_root: PathBuf::from(&worktree.repo_root),
        checkout_path: PathBuf::from(&worktree.checkout_path),
        is_linked_worktree: worktree.is_linked_worktree,
    });

    ProjectedWorkspace {
        branch: branch.clone(),
        worktree_space,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::api::schema::{AgentStatus, PaneInfo, WorkspaceInfo, WorkspaceWorktreeInfo};
    use crate::detect::AgentState;
    use crate::terminal::{TerminalId, TerminalState};

    use super::{project_pane_metadata, project_workspace_metadata};

    fn pane_info(agent_status: AgentStatus) -> PaneInfo {
        PaneInfo {
            pane_id: "p1".into(),
            terminal_id: "t1".into(),
            workspace_id: "w1".into(),
            tab_id: "tab1".into(),
            focused: false,
            cols: None,
            rows: None,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            display_agent: None,
            agent_status,
            custom_status: None,
            state_labels: HashMap::new(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    fn terminal() -> TerminalState {
        TerminalState::new(TerminalId::alloc(), "/".into())
    }

    #[test]
    fn projects_agent_state_labels_and_effective_presentation() {
        let mut state_labels = HashMap::new();
        state_labels.insert("working".to_string(), "cooking".to_string());
        let info = PaneInfo {
            label: Some("build".into()),
            agent: Some("pi".into()),
            title: Some("running tests".into()),
            display_agent: Some("Pi".into()),
            custom_status: Some("87%".into()),
            state_labels: state_labels.clone(),
            ..pane_info(AgentStatus::Working)
        };

        let mut terminal = terminal();
        let seen = project_pane_metadata(&mut terminal, &info);

        assert!(seen, "Working is a seen state");
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.fallback_state, AgentState::Working);
        assert_eq!(terminal.manual_label.as_deref(), Some("build"));
        assert_eq!(terminal.agent_name.as_deref(), Some("pi"));
        let presentation = terminal.effective_presentation();
        assert_eq!(presentation.title.as_deref(), Some("running tests"));
        assert_eq!(presentation.display_agent.as_deref(), Some("Pi"));
        assert_eq!(presentation.custom_status.as_deref(), Some("87%"));
        assert_eq!(presentation.state_labels, state_labels);
    }

    #[test]
    fn done_status_maps_to_unseen_idle() {
        let mut terminal = terminal();
        let seen = project_pane_metadata(&mut terminal, &pane_info(AgentStatus::Done));
        assert!(!seen, "Done encodes an unseen finished agent");
        assert_eq!(terminal.state, AgentState::Idle);
    }

    #[test]
    fn idle_status_maps_to_seen_idle() {
        let mut terminal = terminal();
        let seen = project_pane_metadata(&mut terminal, &pane_info(AgentStatus::Idle));
        assert!(seen);
        assert_eq!(terminal.state, AgentState::Idle);
    }

    #[test]
    fn pane_without_metadata_leaves_presentation_empty() {
        let mut terminal = terminal();
        let seen = project_pane_metadata(&mut terminal, &pane_info(AgentStatus::Unknown));
        assert!(seen);
        assert_eq!(terminal.state, AgentState::Unknown);
        let presentation = terminal.effective_presentation();
        assert_eq!(presentation.title, None);
        assert_eq!(presentation.custom_status, None);
        assert!(presentation.state_labels.is_empty());
    }

    fn workspace_info(worktree: Option<WorkspaceWorktreeInfo>) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: "w1".into(),
            number: 1,
            label: "main".into(),
            focused: true,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "tab1".into(),
            agent_status: AgentStatus::Unknown,
            branch: Some("feat/x".into()),
            worktree,
        }
    }

    #[test]
    fn projects_branch_and_worktree_space() {
        let info = workspace_info(Some(WorkspaceWorktreeInfo {
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        }));

        let projected = project_workspace_metadata(&info);

        assert_eq!(projected.branch.as_deref(), Some("feat/x"));
        let space = projected.worktree_space.expect("worktree space projected");
        assert_eq!(space.key, "repo-key");
        assert_eq!(space.label, "herdr");
        assert_eq!(space.repo_root, std::path::PathBuf::from("/repo/herdr"));
        assert_eq!(
            space.checkout_path,
            std::path::PathBuf::from("/repo/herdr-issue")
        );
        assert!(space.is_linked_worktree);
    }

    #[test]
    fn workspace_without_worktree_has_no_space() {
        let projected = project_workspace_metadata(&workspace_info(None));
        assert!(projected.worktree_space.is_none());
        assert_eq!(projected.branch.as_deref(), Some("feat/x"));
    }
}
