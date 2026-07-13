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

use crate::api::schema::{LayoutNode, SplitDirection, TabInfo};
use crate::app::{AppState, Mode};
use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::PaneState;
use crate::terminal::{TerminalId, TerminalState};
use crate::workspace::{Tab, Workspace};

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

        workspaces.push(Workspace::from_mirror(
            ws_info.workspace_id.clone(),
            Some(ws_info.label.clone()),
            identity_cwd,
            ws_info.branch.clone(),
            projected_tabs,
            active_tab,
            public_pane_numbers,
            next_pane_number,
            tabs.len() + 1,
        ));
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

    Some(Tab::from_mirror(
        Some(tab_info.label.clone()),
        tab_info.number,
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
            if let Some(label) = info.and_then(|info| info.label.clone()) {
                terminal_state.set_manual_label(label);
            }
            if let Some(agent) = info.and_then(|info| info.agent.clone()) {
                terminal_state.set_agent_name(agent);
            }
            terminals.insert(terminal_id.clone(), terminal_state);
            panes.insert(pane_id, PaneState::new(terminal_id));

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
