//! Input action classification for the mirror session (`design-mirror-tui.md`
//! §3.4).
//!
//! The mirror keeps the server app's key→[`NavigateAction`] mapping (so user
//! keybindings behave identically) but replaces the **effect** layer. Each action
//! is one of three classes:
//!
//! * **View-local** — copy-mode, scroll, selection, sidebar, navigator, modals,
//!   resize-mode entry: mutate the local replica `AppState` (zero round-trips).
//! * **Structural** — new/close/move/focus pane, tab, or workspace, split, zoom,
//!   reload-config: issued to the server as a JSON API [`Method`]; the replica is
//!   updated only when the resulting event arrives (server stays authoritative).
//! * **Unsupported** — actions the mirror does not yet drive locally.
//!
//! The classification and the action→[`Method`] translation are pure functions so
//! they are unit-testable without a live server; the [`MirrorActionSink`] trait
//! lets tests observe dispatched mutations with a recording double.

use crate::api::schema::{
    Method, PaneDirection, PaneFocusDirectionParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneZoomMode, PaneZoomParams, SplitDirection, TabCreateParams, TabTarget,
    WorkspaceCreateParams, WorkspaceTarget,
};
use crate::app::NavigateAction;

use super::control::{JsonApiClient, JsonApiError};
use super::replica::SessionReplica;

/// How a [`NavigateAction`] is dispatched by the mirror driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionClass {
    /// Mutate the local replica `AppState` directly (no network).
    ViewLocal,
    /// Send to the server as a JSON API mutation (authoritative).
    Structural,
    /// Not yet driven by the mirror.
    Unsupported,
}

/// Classifies a [`NavigateAction`] into its dispatch class (pure, testable).
pub(crate) fn classify(action: NavigateAction) -> ActionClass {
    use NavigateAction::*;
    match action {
        // Presentation-only: local to the client per the runtime/client boundary.
        EnterResizeMode | ToggleSidebar | WorkspacePicker => ActionClass::ViewLocal,

        // Authoritative session/layout mutations → JSON API. The server owns
        // session/layout/focus, so all pane/tab/workspace navigation and
        // structural edits round-trip; the replica updates from the resulting
        // events (matching the server app, which also routes these through the
        // runtime API — see `src/app/input/navigate.rs`).
        NewWorkspace | CloseWorkspace | SwitchWorkspace(_) | PreviousWorkspace | NextWorkspace
        | NewTab | CloseTab | SwitchTab(_) | PreviousTab | NextTab | SplitVertical
        | SplitHorizontal | ClosePane | Zoom | FocusPaneLeft | FocusPaneDown | FocusPaneUp
        | FocusPaneRight | CyclePaneNext | CyclePanePrevious | SwapPaneLeft | SwapPaneDown
        | SwapPaneUp | SwapPaneRight | ReloadConfig => ActionClass::Structural,

        // Not yet driven by the mirror: copy-mode/search/selection, help,
        // settings, navigator, rename modals, worktrees, agent-panel focus,
        // last-pane, scrollback editor, and notification targets. These need
        // interactive overlay/modal input wiring, which is a later phase (see
        // `MirrorApp::apply_view_local`).
        _ => ActionClass::Unsupported,
    }
}

/// Translates a structural [`NavigateAction`] into the JSON API [`Method`] the
/// server app would issue for it, using the replica's server-authoritative focus
/// ids for targets. Returns `None` for actions with no direct method (or when a
/// required focus target is missing).
pub(crate) fn structural_method(
    action: NavigateAction,
    replica: &SessionReplica,
) -> Option<Method> {
    use NavigateAction::*;
    match action {
        SplitVertical => Some(Method::PaneSplit(split_params(SplitDirection::Right))),
        SplitHorizontal => Some(Method::PaneSplit(split_params(SplitDirection::Down))),
        ClosePane => Some(Method::PaneClose(PaneTarget {
            pane_id: replica.focused_pane_id.clone()?,
        })),
        FocusPaneLeft => Some(focus_direction(PaneDirection::Left)),
        FocusPaneRight => Some(focus_direction(PaneDirection::Right)),
        FocusPaneUp => Some(focus_direction(PaneDirection::Up)),
        FocusPaneDown => Some(focus_direction(PaneDirection::Down)),
        SwapPaneLeft => Some(swap_direction(PaneDirection::Left)),
        SwapPaneRight => Some(swap_direction(PaneDirection::Right)),
        SwapPaneUp => Some(swap_direction(PaneDirection::Up)),
        SwapPaneDown => Some(swap_direction(PaneDirection::Down)),
        CyclePaneNext => {
            cycle_pane_target(replica, 1).map(|pane_id| Method::PaneFocus(PaneTarget { pane_id }))
        }
        CyclePanePrevious => {
            cycle_pane_target(replica, -1).map(|pane_id| Method::PaneFocus(PaneTarget { pane_id }))
        }
        Zoom => Some(Method::PaneZoom(PaneZoomParams {
            pane_id: None,
            mode: PaneZoomMode::Toggle,
        })),
        NewTab => Some(Method::TabCreate(TabCreateParams {
            workspace_id: None,
            cwd: None,
            focus: true,
            label: None,
            env: Default::default(),
        })),
        CloseTab => Some(Method::TabClose(TabTarget {
            tab_id: replica.focused_tab_id.clone()?,
        })),
        SwitchTab(idx) => tab_target_at(replica, idx).map(Method::TabFocus),
        PreviousTab => relative_tab(replica, -1).map(Method::TabFocus),
        NextTab => relative_tab(replica, 1).map(Method::TabFocus),
        NewWorkspace => Some(Method::WorkspaceCreate(WorkspaceCreateParams {
            cwd: None,
            focus: true,
            label: None,
            env: Default::default(),
        })),
        CloseWorkspace => Some(Method::WorkspaceClose(WorkspaceTarget {
            workspace_id: replica.focused_workspace_id.clone()?,
        })),
        SwitchWorkspace(idx) => workspace_target_at(replica, idx).map(Method::WorkspaceFocus),
        PreviousWorkspace => relative_workspace(replica, -1).map(Method::WorkspaceFocus),
        NextWorkspace => relative_workspace(replica, 1).map(Method::WorkspaceFocus),
        ReloadConfig => Some(Method::ServerReloadConfig(Default::default())),
        _ => None,
    }
}

fn split_params(direction: SplitDirection) -> PaneSplitParams {
    PaneSplitParams {
        workspace_id: None,
        target_pane_id: None,
        direction,
        ratio: None,
        cwd: None,
        focus: true,
        env: Default::default(),
    }
}

fn focus_direction(direction: PaneDirection) -> Method {
    Method::PaneFocusDirection(PaneFocusDirectionParams {
        pane_id: None,
        direction,
    })
}

fn swap_direction(direction: PaneDirection) -> Method {
    Method::PaneSwap(PaneSwapParams {
        pane_id: None,
        direction: Some(direction),
        source_pane_id: None,
        target_pane_id: None,
    })
}

/// The pane to focus when cycling `delta` steps (±1) through the focused tab's
/// panes, in the server's layout order. Returns `None` when the focused tab has
/// no exported layout yet (the resulting event will still keep focus in sync).
fn cycle_pane_target(replica: &SessionReplica, delta: isize) -> Option<String> {
    let tab_id = replica.focused_tab_id.as_ref()?;
    let panes = &replica.layouts.get(tab_id)?.panes;
    let current = panes
        .iter()
        .position(|pane| pane.focused || Some(&pane.pane_id) == replica.focused_pane_id.as_ref())
        .unwrap_or(0);
    let next = wrap_index(current, delta, panes.len())?;
    panes.get(next).map(|pane| pane.pane_id.clone())
}

/// Tabs of the focused workspace, in display order (by `number`).
fn focused_workspace_tabs(replica: &SessionReplica) -> Vec<&crate::api::schema::TabInfo> {
    let Some(ws_id) = replica.focused_workspace_id.as_ref() else {
        return Vec::new();
    };
    let mut tabs: Vec<_> = replica
        .tabs
        .iter()
        .filter(|tab| &tab.workspace_id == ws_id)
        .collect();
    tabs.sort_by_key(|tab| tab.number);
    tabs
}

fn tab_target_at(replica: &SessionReplica, idx: usize) -> Option<TabTarget> {
    focused_workspace_tabs(replica)
        .get(idx)
        .map(|tab| TabTarget {
            tab_id: tab.tab_id.clone(),
        })
}

fn relative_tab(replica: &SessionReplica, delta: isize) -> Option<TabTarget> {
    let tabs = focused_workspace_tabs(replica);
    let current = tabs
        .iter()
        .position(|tab| Some(&tab.tab_id) == replica.focused_tab_id.as_ref())?;
    let next = wrap_index(current, delta, tabs.len())?;
    tabs.get(next).map(|tab| TabTarget {
        tab_id: tab.tab_id.clone(),
    })
}

fn workspace_target_at(replica: &SessionReplica, idx: usize) -> Option<WorkspaceTarget> {
    replica.workspaces.get(idx).map(|ws| WorkspaceTarget {
        workspace_id: ws.workspace_id.clone(),
    })
}

fn relative_workspace(replica: &SessionReplica, delta: isize) -> Option<WorkspaceTarget> {
    let current = replica
        .workspaces
        .iter()
        .position(|ws| Some(&ws.workspace_id) == replica.focused_workspace_id.as_ref())?;
    let next = wrap_index(current, delta, replica.workspaces.len())?;
    replica.workspaces.get(next).map(|ws| WorkspaceTarget {
        workspace_id: ws.workspace_id.clone(),
    })
}

fn wrap_index(current: usize, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let len_i = len as isize;
    Some((((current as isize + delta) % len_i + len_i) % len_i) as usize)
}

/// The effect layer for structural mirror actions. Real impl issues JSON API
/// calls; a test double records them.
pub(crate) trait MirrorActionSink {
    /// Dispatch an authoritative mutation to the server.
    fn dispatch(&self, verb: &'static str, method: Method) -> Result<(), JsonApiError>;
}

/// Production sink: forwards mutations over the JSON API control plane.
pub(crate) struct JsonApiSink<'a> {
    pub control: &'a JsonApiClient,
}

impl MirrorActionSink for JsonApiSink<'_> {
    fn dispatch(&self, verb: &'static str, method: Method) -> Result<(), JsonApiError> {
        self.control.mutate(verb, method)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;
    use crate::api::schema::{
        PaneInfo, PaneLayoutPane, PaneLayoutRect, PaneLayoutSnapshot, SessionSnapshot, TabInfo,
        WorkspaceInfo,
    };
    use std::collections::HashMap;

    fn tab(id: &str, ws: &str, number: usize) -> TabInfo {
        TabInfo {
            tab_id: id.into(),
            workspace_id: ws.into(),
            number,
            label: id.into(),
            focused: false,
            pane_count: 1,
            agent_status: AgentStatus::Unknown,
        }
    }

    fn workspace(id: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.into(),
            number: 1,
            label: id.into(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "t1".into(),
            agent_status: AgentStatus::Unknown,
            branch: None,
            worktree: None,
        }
    }

    fn pane(pane_id: &str, tab: &str, ws: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: format!("term_{pane_id}"),
            workspace_id: ws.into(),
            tab_id: tab.into(),
            focused: true,
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

    fn replica() -> SessionReplica {
        SessionReplica::from_snapshot(SessionSnapshot {
            version: "t".into(),
            protocol: 1,
            focused_workspace_id: Some("w1".into()),
            focused_tab_id: Some("t1".into()),
            focused_pane_id: Some("p1".into()),
            workspaces: vec![workspace("w1"), workspace("w2")],
            tabs: vec![tab("t1", "w1", 1), tab("t2", "w1", 2)],
            panes: vec![pane("p1", "t1", "w1")],
            layouts: Vec::new(),
            agents: Vec::new(),
        })
    }

    fn layout_pane(pane_id: &str, focused: bool) -> PaneLayoutPane {
        PaneLayoutPane {
            pane_id: pane_id.into(),
            focused,
            rect: PaneLayoutRect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
        }
    }

    /// A replica whose focused tab `t1` has a two-pane layout (`p1` focused, then
    /// `p2`), so pane cycling has an order to walk.
    fn replica_with_layout() -> SessionReplica {
        SessionReplica::from_snapshot(SessionSnapshot {
            version: "t".into(),
            protocol: 1,
            focused_workspace_id: Some("w1".into()),
            focused_tab_id: Some("t1".into()),
            focused_pane_id: Some("p1".into()),
            workspaces: vec![workspace("w1")],
            tabs: vec![tab("t1", "w1", 1)],
            panes: vec![pane("p1", "t1", "w1"), pane("p2", "t1", "w1")],
            layouts: vec![PaneLayoutSnapshot {
                workspace_id: "w1".into(),
                tab_id: "t1".into(),
                zoomed: false,
                area: PaneLayoutRect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 10,
                },
                focused_pane_id: "p1".into(),
                panes: vec![layout_pane("p1", true), layout_pane("p2", false)],
                splits: Vec::new(),
            }],
            agents: Vec::new(),
        })
    }

    #[test]
    fn implemented_presentation_actions_are_view_local() {
        assert_eq!(
            classify(NavigateAction::ToggleSidebar),
            ActionClass::ViewLocal
        );
        assert_eq!(
            classify(NavigateAction::EnterResizeMode),
            ActionClass::ViewLocal
        );
        assert_eq!(
            classify(NavigateAction::WorkspacePicker),
            ActionClass::ViewLocal
        );
        assert_eq!(classify(NavigateAction::CopyMode), ActionClass::Unsupported);
        assert_eq!(classify(NavigateAction::Help), ActionClass::Unsupported);
    }

    #[test]
    fn split_and_focus_are_structural() {
        assert_eq!(
            classify(NavigateAction::SplitVertical),
            ActionClass::Structural
        );
        assert_eq!(classify(NavigateAction::ClosePane), ActionClass::Structural);
        assert_eq!(
            classify(NavigateAction::FocusPaneLeft),
            ActionClass::Structural
        );
    }

    #[test]
    fn cycle_and_swap_pane_are_structural() {
        for action in [
            NavigateAction::CyclePaneNext,
            NavigateAction::CyclePanePrevious,
            NavigateAction::SwapPaneLeft,
            NavigateAction::SwapPaneRight,
            NavigateAction::SwapPaneUp,
            NavigateAction::SwapPaneDown,
        ] {
            assert_eq!(classify(action), ActionClass::Structural, "{action:?}");
        }
    }

    #[test]
    fn swap_pane_left_maps_to_pane_swap_left() {
        let method = structural_method(NavigateAction::SwapPaneLeft, &replica()).unwrap();
        assert!(matches!(
            method,
            Method::PaneSwap(PaneSwapParams {
                direction: Some(PaneDirection::Left),
                pane_id: None,
                source_pane_id: None,
                target_pane_id: None,
            })
        ));
    }

    #[test]
    fn cycle_pane_next_focuses_the_following_pane_in_layout_order() {
        let method = structural_method(NavigateAction::CyclePaneNext, &replica_with_layout());
        assert!(matches!(
            method,
            Some(Method::PaneFocus(PaneTarget { pane_id })) if pane_id == "p2"
        ));
    }

    #[test]
    fn cycle_pane_without_layout_yields_no_method() {
        // The base `replica()` has no exported layouts; cycling is a no-op until
        // one arrives (focus stays server-authoritative via events).
        assert!(structural_method(NavigateAction::CyclePaneNext, &replica()).is_none());
    }

    #[test]
    fn split_vertical_maps_to_pane_split_right() {
        let method = structural_method(NavigateAction::SplitVertical, &replica()).unwrap();
        assert!(matches!(
            method,
            Method::PaneSplit(PaneSplitParams {
                direction: SplitDirection::Right,
                ..
            })
        ));
    }

    #[test]
    fn close_pane_targets_focused_pane() {
        let method = structural_method(NavigateAction::ClosePane, &replica()).unwrap();
        assert!(matches!(
            method,
            Method::PaneClose(PaneTarget { pane_id }) if pane_id == "p1"
        ));
    }

    #[test]
    fn next_tab_wraps_within_focused_workspace() {
        let method = structural_method(NavigateAction::NextTab, &replica()).unwrap();
        assert!(matches!(
            method,
            Method::TabFocus(TabTarget { tab_id }) if tab_id == "t2"
        ));
    }

    #[test]
    fn next_workspace_targets_the_following_workspace() {
        let method = structural_method(NavigateAction::NextWorkspace, &replica()).unwrap();
        assert!(matches!(
            method,
            Method::WorkspaceFocus(WorkspaceTarget { workspace_id }) if workspace_id == "w2"
        ));
    }
}
