//! quiet-ui: the notifications v1's own types cannot carry. Session updates
//! kept raw so the AIR extension's async-task and subagent reports get
//! through, and the relay's turn-state and session-config notifications.

use super::*;

/// A `session/update` notification with its payload kept raw, so extension
/// kinds that v1's `SessionUpdate` cannot parse still get through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RawSessionNotification {
    #[serde(rename = "sessionId")]
    pub(super) session_id: acp::SessionId,
    pub(super) update: serde_json::Value,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub(super) meta: Option<acp::Meta>,
}

impl JsonRpcMessage for RawSessionNotification {
    fn matches_method(method: &str) -> bool {
        method == SESSION_UPDATE_NOTIFICATION
    }

    fn method(&self) -> &str {
        SESSION_UPDATE_NOTIFICATION
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(SESSION_UPDATE_NOTIFICATION, self)
    }

    fn parse_message(
        method: &str,
        params: &impl Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != SESSION_UPDATE_NOTIFICATION {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        serde_json::from_value(serde_json::to_value(params)?)
            .map_err(agent_client_protocol::Error::into_internal_error)
    }
}

impl JsonRpcNotification for RawSessionNotification {}

impl From<acp::SessionNotification> for RawSessionNotification {
    fn from(notification: acp::SessionNotification) -> Self {
        Self {
            session_id: notification.session_id,
            update: serde_json::to_value(notification.update).unwrap_or(serde_json::Value::Null),
            meta: notification.meta,
        }
    }
}

pub(super) const SESSION_UPDATE_NOTIFICATION: &str = "session/update";

/// `_meta.jetbrains.air`, the AIR extension's namespace.
pub(super) const AIR_META_KEY: &str = "jetbrains";
pub(super) const AIR_EXTENSION_KEY: &str = "air";
pub(super) const AIR_EXTENSION_VERSION: u64 = 1;
pub(super) const AIR_ASYNC_TASKS_CAPABILITY: &str = "asyncTasks";
pub(super) const AIR_NATIVE_SUBAGENT_SESSIONS_CAPABILITY: &str = "nativeSubagentSessions";
/// Without this the adapter never says why a session stopped.
pub(super) const AIR_SESSION_FAILURE_CAPABILITY: &str = "sessionFailure";
pub(super) const ASYNC_TASK_STOP_METHOD: &str = "_session/async_task/stop";
pub(super) const SESSION_INFO_UPDATE: &str = "session_info_update";

pub(super) fn air_async_tasks_meta(
    meta: Option<&acp::Meta>,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    meta?
        .get(AIR_META_KEY)?
        .get(AIR_EXTENSION_KEY)?
        .get(AIR_ASYNC_TASKS_CAPABILITY)?
        .as_object()
}

pub(super) enum AirSessionReport {
    Failure(acp_thread::SessionFailure),
    /// A retry in progress ("Reconnecting to Claude, attempt 3 of 10"), not a
    /// stop.
    Warning(SharedString),
}

/// A session failure the agent published on a `session_info_update`. The
/// payload carries no `kind`, only the category behind it, so that is what a
/// row can say.
pub(super) fn air_session_report(update: &serde_json::Value) -> Option<AirSessionReport> {
    let failure = update
        .get("_meta")?
        .get(AIR_META_KEY)?
        .get(AIR_EXTENSION_KEY)?
        .get(AIR_SESSION_FAILURE_CAPABILITY)?;
    let title = update_string_field(failure, "title")?;
    match failure.get("severity").and_then(|value| value.as_str()) {
        Some("error") => {}
        Some("warning") => return Some(AirSessionReport::Warning(title)),
        _ => return None,
    }
    Some(AirSessionReport::Failure(acp_thread::SessionFailure {
        id: update_string_field(failure, "id")?,
        revision: failure
            .get("revision")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        category: failure
            .get("category")
            .and_then(|value| value.as_str())
            .map(acp_thread::SessionFailureCategory::from_wire)
            .unwrap_or(acp_thread::SessionFailureCategory::Unknown),
        title,
        details: update_string_field(failure, "details"),
    }))
}

/// A non-empty string field of an extension update.
pub(super) fn update_string_field(update: &serde_json::Value, name: &str) -> Option<SharedString> {
    update
        .get(name)
        .and_then(|value| value.as_str())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(SharedString::from)
}

/// Every field but the task's id is optional on the wire: a progress report
/// carries whichever has just become known.
pub(super) fn apply_async_task_update(
    update: &serde_json::Value,
    thread: &WeakEntity<AcpThread>,
    cx: &mut AsyncApp,
) {
    let field = |name: &str| update_string_field(update, name);
    let Some(id) = field("asyncTaskId") else {
        return;
    };
    let tool_call_id = field("toolCallId").map(|id| acp_v2::ToolCallId::new(id.as_ref()));
    let output_file_path = field("outputFilePath");

    match update.get("sessionUpdate").and_then(|kind| kind.as_str()) {
        Some("async_task_spawned") => {
            let task = acp_thread::AsyncTask {
                name: field("name").unwrap_or_else(|| id.clone()),
                description: field("description"),
                id,
                state: acp_thread::AsyncTaskState::Running,
                summary: None,
                tool_call_id,
                output_file_path,
                can_stop: update
                    .get("canStop")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                kind: acp_thread::AsyncTaskKind::from_wire(field("taskType").as_deref()),
                started_at: std::time::Instant::now(),
                service_port: None,
            };
            thread
                .update(cx, |thread, cx| thread.async_task_spawned(task, cx))
                .log_err();
        }
        Some("async_task_progress") => {
            let description = field("description");
            let summary = field("summary");
            thread
                .update(cx, |thread, cx| {
                    thread.async_task_progress(
                        &id,
                        description,
                        summary,
                        tool_call_id,
                        output_file_path,
                        cx,
                    );
                })
                .log_err();
        }
        Some("async_task_state_update") => {
            let state = update
                .get("state")
                .and_then(|value| value.as_str())
                .map(acp_thread::AsyncTaskState::from_wire)
                .unwrap_or(acp_thread::AsyncTaskState::Running);
            let summary = field("summary");
            thread
                .update(cx, |thread, cx| {
                    thread.async_task_state_updated(&id, state, summary, tool_call_id, cx);
                })
                .log_err();
        }
        _ => {}
    }
}

pub(super) fn register_subagent_session(
    update: &serde_json::Value,
    parent: &WeakEntity<AcpThread>,
    cx: &mut AsyncApp,
) {
    if update.get("sessionUpdate").and_then(|kind| kind.as_str()) != Some("subagent_spawned") {
        return;
    }
    let Some(session_id) = update_string_field(update, "subagentSessionId") else {
        return;
    };
    let Some(parent) = parent.upgrade() else {
        return;
    };
    cx.update(|cx| {
        let connection = parent.read(cx).connection().clone().into_any();
        if let Ok(connection) = connection.downcast::<AcpConnection>() {
            connection.register_child_session(
                acp_v2::SessionId::new(session_id.as_ref()),
                &parent,
                cx,
            );
        }
    });
}

pub(super) fn apply_reported_subagent_update(
    update: &serde_json::Value,
    thread: &WeakEntity<AcpThread>,
    cx: &mut AsyncApp,
) {
    let field = |name: &str| update_string_field(update, name);
    let Some(session_id) = field("subagentSessionId") else {
        return;
    };
    let session_id = acp_v2::SessionId::new(session_id.as_ref());

    match update.get("sessionUpdate").and_then(|kind| kind.as_str()) {
        Some("subagent_spawned") => {
            let subagent = acp_thread::Subagent {
                session_id,
                name: field("name"),
                task: field("task").or_else(|| field("prompt")),
                state: acp_thread::AsyncTaskState::Running,
            };
            thread
                .update(cx, |thread, cx| thread.subagent_reported(subagent, cx))
                .log_err();
        }
        Some("subagent_state_update") => {
            let state = update
                .get("state")
                .and_then(|value| value.as_str())
                .map(acp_thread::AsyncTaskState::from_wire)
                .unwrap_or(acp_thread::AsyncTaskState::Running);
            thread
                .update(cx, |thread, cx| {
                    thread.subagent_state_updated(&session_id, state, cx);
                })
                .log_err();
        }
        _ => {}
    }
}

/// quiet-ui: while the connection to the box is down the agent keeps running
/// there, so its threads say they are reconnecting rather than failing.
pub(super) async fn watch_relay(
    reconnecting: async_channel::Receiver<bool>,
    sessions: Rc<RefCell<HashMap<acp_v2::SessionId, AcpSession>>>,
    cx: &mut AsyncApp,
) {
    while let Ok(reconnecting) = reconnecting.recv().await {
        let threads = sessions
            .borrow()
            .values()
            .map(|session| session.thread.clone())
            .collect::<Vec<_>>();
        for thread in threads {
            thread
                .update(cx, |thread, cx| {
                    thread.set_relay_reconnecting(reconnecting, cx)
                })
                .ok();
        }
    }
}

/// quiet-ui: the turn state the relay reports after a reattach, for a turn
/// this connection did not start. Named after ACP v2's `state_update`, which
/// replaces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RelayStateUpdate {
    #[serde(rename = "sessionId")]
    pub(super) session_id: acp::SessionId,
    pub(super) state: String,
    #[serde(default, rename = "stopReason")]
    pub(super) stop_reason: Option<String>,
    #[serde(default)]
    pub(super) error: Option<String>,
}

pub(super) const RELAY_STATE_UPDATE: &str = "_zed/relay/state_update";

impl JsonRpcMessage for RelayStateUpdate {
    fn matches_method(method: &str) -> bool {
        method == RELAY_STATE_UPDATE
    }

    fn method(&self) -> &str {
        RELAY_STATE_UPDATE
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(RELAY_STATE_UPDATE, self)
    }

    fn parse_message(
        method: &str,
        params: &impl Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != RELAY_STATE_UPDATE {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        serde_json::from_value(serde_json::to_value(params)?)
            .map_err(agent_client_protocol::Error::into_internal_error)
    }
}

impl JsonRpcNotification for RelayStateUpdate {}

impl RelayStateUpdate {
    fn to_state(&self) -> acp_v2::StateUpdate {
        if self.state == "running" {
            return acp_v2::StateUpdate::Running(acp_v2::RunningStateUpdate::new());
        }
        let stop_reason = match (self.stop_reason.as_deref(), &self.error) {
            (_, Some(error)) => {
                log::warn!("quiet-ui relay: the turn ended in error: {error}");
                Some(acp_v2::StopReason::Error(acp_v2::ErrorStopReason::new()))
            }
            (Some("end_turn"), _) => Some(acp_v2::StopReason::EndTurn),
            (Some("max_tokens"), _) => Some(acp_v2::StopReason::MaxTokens),
            (Some("max_turn_requests"), _) => Some(acp_v2::StopReason::MaxTurnRequests),
            (Some("refusal"), _) => Some(acp_v2::StopReason::Refusal),
            (Some("cancelled"), _) => Some(acp_v2::StopReason::Cancelled),
            _ => None,
        };
        acp_v2::StateUpdate::Idle(acp_v2::IdleStateUpdate::new().stop_reason(stop_reason))
    }
}

pub(super) fn handle_relay_state_update(
    update: RelayStateUpdate,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let session_id = acp_v2::SessionId::new(update.session_id.0.clone());
    let thread = ctx
        .sessions
        .borrow()
        .get(&session_id)
        .map(|session| session.thread.clone());
    if let Some(thread) = thread {
        thread
            .update(cx, |thread, cx| {
                thread.apply_relay_state(update.to_state(), cx)
            })
            .log_err();
    }
}

/// quiet-ui: a session's modes and config options, sent by the relay when it
/// learns them after this connection loaded the session without them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RelaySessionConfig {
    #[serde(rename = "sessionId")]
    pub(super) session_id: acp::SessionId,
    #[serde(default)]
    pub(super) modes: Option<acp::SessionModeState>,
    #[serde(default, rename = "configOptions")]
    pub(super) config_options: Option<Vec<acp::SessionConfigOption>>,
}

pub(super) const RELAY_SESSION_CONFIG: &str = "_zed/relay/session_config";

impl JsonRpcMessage for RelaySessionConfig {
    fn matches_method(method: &str) -> bool {
        method == RELAY_SESSION_CONFIG
    }

    fn method(&self) -> &str {
        RELAY_SESSION_CONFIG
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(RELAY_SESSION_CONFIG, self)
    }

    fn parse_message(
        method: &str,
        params: &impl Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != RELAY_SESSION_CONFIG {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        serde_json::from_value(serde_json::to_value(params)?)
            .map_err(agent_client_protocol::Error::into_internal_error)
    }
}

impl JsonRpcNotification for RelaySessionConfig {}

/// Gives a session loaded without controls the ones the relay now knows; a
/// session that has its own already keeps them.
pub(super) fn handle_relay_session_config(
    update: RelaySessionConfig,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let session_id = acp_v2::SessionId::new(update.session_id.0.clone());
    let Some((modes, config_options)) = config_state(update.modes, update.config_options).log_err()
    else {
        return;
    };
    let event = {
        let mut sessions = ctx.sessions.borrow_mut();
        let Some(session) = sessions.get_mut(&session_id) else {
            return;
        };
        if session.session_modes.is_some() || session.config_options.is_some() {
            return;
        }
        let event = if let Some(options) = &config_options {
            acp_thread::AcpThreadEvent::ConfigOptionsUpdated(
                options.config_options.borrow().clone(),
            )
        } else if let Some(modes) = &modes {
            acp_thread::AcpThreadEvent::ModeUpdated(acp_v2::SessionConfigValueId::new(
                modes.borrow().current_mode_id.0.clone(),
            ))
        } else {
            return;
        };
        session.session_modes = modes;
        session.config_options = config_options;
        (session.thread.clone(), event)
    };
    log::info!("quiet-ui relay: {session_id} has its controls again");
    let (thread, event) = event;
    thread.update(cx, |_, cx| cx.emit(event)).ok();
}
