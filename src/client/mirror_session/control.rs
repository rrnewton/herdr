//! Control-plane client for the mirror session.
//!
//! The mirror session's *structure* (workspaces, tabs, panes, layout, focus, and
//! agent status) is authoritative on the server and travels over the **JSON API
//! socket**, never the private per-terminal wire socket (see the runtime/client
//! boundary guardrail in the project `CLAUDE.md`, and `design-mirror-tui.md` §2.1).
//!
//! [`JsonApiClient`] wraps the generic [`crate::api::client::ApiClient`] with the
//! small set of typed calls the mirror needs to (1) enumerate the session and
//! build a replica ([`super::SessionReplica`]) and (2) subscribe to the structural
//! event feed that keeps that replica current. It is deliberately read-and-observe
//! only here; the structural *mutations* a mirror client issues (create/close/move
//! pane, switch tab, …) are ordinary JSON API calls added alongside these in a
//! later phase.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget, EventStream};
use crate::api::schema::{
    EmptyParams, EventData, EventEnvelope, EventKind, EventsSubscribeParams, LayoutDescription,
    LayoutExportParams, Method, PaneInfo, PaneListParams, Request, ResponseResult, SessionSnapshot,
    Subscription, SubscriptionEventData, SubscriptionEventEnvelope, SubscriptionEventKind, TabInfo,
    TabListParams, WorkspaceInfo,
};

/// The structural/event subscriptions the mirror control plane needs to keep its
/// replica in sync with the server (`design-mirror-tui.md` §2.4).
///
fn structural_subscriptions() -> Vec<Subscription> {
    vec![
        Subscription::WorkspaceCreated {},
        Subscription::WorkspaceUpdated {},
        Subscription::WorkspaceRenamed {},
        Subscription::WorkspaceMoved {},
        Subscription::WorkspaceClosed {},
        Subscription::WorkspaceFocused {},
        Subscription::WorktreeCreated {},
        Subscription::WorktreeOpened {},
        Subscription::WorktreeRemoved {},
        Subscription::TabCreated {},
        Subscription::TabClosed {},
        Subscription::TabFocused {},
        Subscription::TabRenamed {},
        Subscription::TabMoved {},
        Subscription::PaneCreated {},
        Subscription::PaneClosed {},
        Subscription::PaneFocused {},
        Subscription::PaneMoved {},
        Subscription::PaneExited {},
        Subscription::PaneAgentDetected {},
        Subscription::PaneAgentStatusChanged {
            pane_id: None,
            agent_status: None,
        },
        Subscription::LayoutUpdated {},
        Subscription::NotificationShown {},
    ]
}

/// Errors from a control-plane call.
#[derive(Debug)]
pub enum JsonApiError {
    /// The underlying JSON API transport or the server returned an error.
    Client(ApiClientError),
    /// The server replied successfully but with a result variant we did not ask
    /// for (indicates a protocol/version mismatch).
    UnexpectedResult(String),
}

impl std::fmt::Display for JsonApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(err) => write!(f, "{err}"),
            Self::UnexpectedResult(result) => {
                write!(f, "unexpected control-plane result: {result}")
            }
        }
    }
}

impl std::error::Error for JsonApiError {}

impl From<ApiClientError> for JsonApiError {
    fn from(err: ApiClientError) -> Self {
        Self::Client(err)
    }
}

/// Typed control-plane client over the JSON API socket.
///
/// Cheap to clone/construct; each call opens a short-lived request/response
/// connection, exactly like [`crate::api::client::ApiClient`]. The one exception
/// is [`Self::subscribe`], which holds a connection open for the streamed event
/// feed.
pub struct JsonApiClient {
    api: ApiClient,
    next_request_id: AtomicU64,
}

impl JsonApiClient {
    /// Connects to the given target (a local session, optionally named, or an
    /// explicit socket path).
    pub fn new(target: ConnectionTarget) -> Self {
        Self {
            api: ApiClient::for_target(target),
            next_request_id: AtomicU64::new(0),
        }
    }

    /// Connects to the local session (`None`) or a specific named session.
    pub fn local(session: Option<String>) -> Self {
        Self::new(ConnectionTarget::LocalSession(session))
    }

    /// The JSON API socket this client talks to.
    pub fn socket_path(&self) -> std::path::PathBuf {
        self.api.socket_path()
    }

    fn next_id(&self, verb: &str) -> String {
        let n = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        format!("mirror-control:{verb}:{n}")
    }

    fn call(&self, verb: &str, method: Method) -> Result<ResponseResult, JsonApiError> {
        let request = Request {
            id: self.next_id(verb),
            method,
        };
        Ok(self.api.request(request)?.result)
    }

    /// One-shot snapshot of the whole session: workspaces, tabs, panes, layouts,
    /// agents, and current focus. This alone is enough to build a replica; the
    /// narrower list/export calls below exist for targeted refreshes.
    pub fn session_snapshot(&self) -> Result<SessionSnapshot, JsonApiError> {
        match self.call("session.snapshot", Method::SessionSnapshot(EmptyParams {}))? {
            ResponseResult::SessionSnapshot { snapshot } => Ok(*snapshot),
            other => Err(unexpected(&other)),
        }
    }

    /// Lists the session's workspaces in display order.
    pub fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, JsonApiError> {
        match self.call("workspace.list", Method::WorkspaceList(EmptyParams {}))? {
            ResponseResult::WorkspaceList { workspaces } => Ok(workspaces),
            other => Err(unexpected(&other)),
        }
    }

    /// Lists tabs, optionally scoped to one workspace.
    pub fn tab_list(&self, workspace_id: Option<String>) -> Result<Vec<TabInfo>, JsonApiError> {
        match self.call("tab.list", Method::TabList(TabListParams { workspace_id }))? {
            ResponseResult::TabList { tabs } => Ok(tabs),
            other => Err(unexpected(&other)),
        }
    }

    /// Lists panes, optionally scoped to one workspace.
    pub fn pane_list(&self, workspace_id: Option<String>) -> Result<Vec<PaneInfo>, JsonApiError> {
        match self.call(
            "pane.list",
            Method::PaneList(PaneListParams { workspace_id }),
        )? {
            ResponseResult::PaneList { panes } => Ok(panes),
            other => Err(unexpected(&other)),
        }
    }

    /// Exports a tab's layout as a nested [`crate::api::schema::LayoutNode`] tree
    /// (the shape best suited to rebuilding a client-side tiling layout). Scopes
    /// to the focused tab when `tab_id` is `None`.
    pub fn layout_export(&self, tab_id: Option<String>) -> Result<LayoutDescription, JsonApiError> {
        let params = LayoutExportParams {
            tab_id,
            pane_id: None,
        };
        match self.call("layout.export", Method::LayoutExport(params))? {
            ResponseResult::LayoutExport { layout } => Ok(layout),
            other => Err(unexpected(&other)),
        }
    }

    /// Issues an arbitrary structural mutation (`pane.*`/`tab.*`/`workspace.*`/…)
    /// and returns its result. The mirror driver uses this to send authoritative
    /// changes to the server rather than mutating its local replica
    /// (`design-mirror-tui.md` §3.4); the resulting event feeds back through
    /// [`Self::subscribe`] and updates the replica.
    pub fn mutate(
        &self,
        verb: &'static str,
        method: Method,
    ) -> Result<ResponseResult, JsonApiError> {
        self.call(verb, method)
    }

    /// Builds a fresh replica from a single `session.snapshot`.
    pub fn build_replica(&self) -> Result<super::SessionReplica, JsonApiError> {
        Ok(super::SessionReplica::from_snapshot(
            self.session_snapshot()?,
        ))
    }

    /// Subscribes to the structural event feed and returns the live stream. The
    /// caller drives it (see [`StructuralEventStream::next_event`]) and feeds each
    /// event to [`super::SessionReplica::apply_event`].
    ///
    /// `read_timeout` bounds each blocking read; `None` blocks indefinitely.
    pub fn subscribe(
        &self,
        read_timeout: Option<Duration>,
    ) -> Result<StructuralEventStream, JsonApiError> {
        let params = EventsSubscribeParams {
            subscriptions: structural_subscriptions(),
        };
        let (_ack, stream) =
            self.api
                .subscribe(self.next_id("events.subscribe"), params, read_timeout)?;
        Ok(StructuralEventStream { stream })
    }
}

fn unexpected(result: &ResponseResult) -> JsonApiError {
    JsonApiError::UnexpectedResult(format!("{result:?}"))
}

/// A live stream of structural [`EventEnvelope`]s from `events.subscribe`.
///
/// Structural subscriptions emit an `EventEnvelope { event, data }` per line (see
/// `api::subscriptions::ActiveEventSubscription::poll`), distinct from the
/// poll-based `SubscriptionEventEnvelope` used by output/scroll waits — so this
/// deserializes the raw line to [`EventEnvelope`] itself.
pub struct StructuralEventStream {
    stream: EventStream,
}

impl StructuralEventStream {
    /// Blocks for the next structural event. Returns `Ok(None)` when the server
    /// closes the stream (EOF).
    pub fn next_event(&mut self) -> Result<Option<EventEnvelope>, JsonApiError> {
        let Some(value) = self.stream.next_value()? else {
            return Ok(None);
        };
        if let Ok(envelope) = serde_json::from_value::<EventEnvelope>(value.clone()) {
            return Ok(Some(envelope));
        }
        let subscription = serde_json::from_value::<SubscriptionEventEnvelope>(value)
            .map_err(|err| JsonApiError::Client(ApiClientError::Json(err)))?;
        Ok(subscription_event_to_event(subscription))
    }
}

fn subscription_event_to_event(envelope: SubscriptionEventEnvelope) -> Option<EventEnvelope> {
    match (envelope.event, envelope.data) {
        (
            SubscriptionEventKind::PaneAgentStatusChanged,
            SubscriptionEventData::PaneAgentStatusChanged(event),
        ) => Some(EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: event.pane_id,
                workspace_id: event.workspace_id,
                agent_status: event.agent_status,
                agent: event.agent,
                title: event.title,
                display_agent: event.display_agent,
                custom_status: event.custom_status,
                state_labels: event.state_labels,
            },
        }),
        _ => None,
    }
}
