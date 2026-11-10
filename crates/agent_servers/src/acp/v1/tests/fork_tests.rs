//! The fork's own tests of the v1 connection: the AIR async-task and
//! subagent extensions, and the relay's session config.

use super::*;

#[test]
fn client_capabilities_advertise_the_air_async_tasks_extension() {
    // The adapter checks this exact shape.
    let capabilities = client_capabilities_for_agent(&AgentId::new("claude-code"), false);
    let meta = capabilities
        .meta
        .expect("expected client capabilities meta");
    let air = meta
        .get(AIR_META_KEY)
        .and_then(|jetbrains| jetbrains.get(AIR_EXTENSION_KEY))
        .expect("the AIR extension should be advertised");
    assert_eq!(
        air.get("version").and_then(|version| version.as_u64()),
        Some(AIR_EXTENSION_VERSION)
    );
    assert_eq!(
        air.get("capabilities"),
        Some(&serde_json::json!([
            AIR_ASYNC_TASKS_CAPABILITY,
            AIR_NATIVE_SUBAGENT_SESSIONS_CAPABILITY,
            AIR_SESSION_FAILURE_CAPABILITY
        ])),
    );
}

#[test]
fn an_extension_session_update_survives_being_parsed() {
    let params = serde_json::json!({
        "sessionId": "session-1",
        "update": {
            "sessionUpdate": "async_task_spawned",
            "asyncTaskId": "task-1",
            "name": "cargo test",
            "canStop": true,
        },
    });
    assert!(serde_json::from_value::<acp::SessionNotification>(params.clone()).is_err());
    let raw = RawSessionNotification::parse_message(SESSION_UPDATE_NOTIFICATION, &params)
        .expect("the raw shape keeps it");
    assert_eq!(raw.session_id, acp::SessionId::new("session-1"));
    assert_eq!(
        raw.update.get("sessionUpdate").and_then(|k| k.as_str()),
        Some("async_task_spawned")
    );

    let params = serde_json::json!({
        "sessionId": "session-1",
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" },
        },
    });
    let raw = RawSessionNotification::parse_message(SESSION_UPDATE_NOTIFICATION, &params)
        .expect("a standard update parses raw too");
    assert!(matches!(
        serde_json::from_value::<acp::SessionUpdate>(raw.update).expect("and then typed"),
        acp::SessionUpdate::AgentMessageChunk(_)
    ));
}

#[test]
fn the_backgrounded_marker_is_read_off_the_air_namespace() {
    let meta: acp::Meta = serde_json::from_value(serde_json::json!({
        "claudeCode": { "toolName": "Bash" },
        "jetbrains": { "air": { "version": 1, "asyncTasks": { "backgrounded": true } } },
    }))
    .unwrap();
    assert_eq!(
        air_async_tasks_meta(Some(&meta))
            .and_then(|async_tasks| async_tasks.get("backgrounded"))
            .and_then(|backgrounded| backgrounded.as_bool()),
        Some(true)
    );
    let meta: acp::Meta =
        serde_json::from_value(serde_json::json!({ "claudeCode": { "toolName": "Bash" } }))
            .unwrap();
    assert!(air_async_tasks_meta(Some(&meta)).is_none());
}

/// The shape the adapter's `sessionFailureMeta` builds, which carries the
/// category behind the failure rather than its own finer kind.
#[test]
fn the_session_failure_is_read_off_the_air_namespace() {
    let update = serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "jetbrains": {
                "air": {
                    "version": 1,
                    "sessionFailure": {
                        "id": "session-1:session-error:epoch:1",
                        "revision": 2,
                        "category": "limit",
                        "severity": "error",
                        "title": "Claude is temporarily rate limited.",
                        "details": "Try again in a few minutes.",
                        "actions": ["retry"],
                    }
                }
            }
        },
    });
    let Some(AirSessionReport::Failure(failure)) = air_session_report(&update) else {
        panic!("a published failure");
    };
    assert_eq!(failure.id.as_ref(), "session-1:session-error:epoch:1");
    assert_eq!(failure.revision, 2);
    assert_eq!(failure.category, acp_thread::SessionFailureCategory::Limit);
    assert_eq!(
        failure.title.as_ref(),
        "Claude is temporarily rate limited."
    );
    assert_eq!(
        failure.details.as_deref(),
        Some("Try again in a few minutes.")
    );

    // An `api_retry` notice is a warning about a retry in progress, not a
    // reason the thread stopped.
    let mut warning = update;
    warning["_meta"]["jetbrains"]["air"]["sessionFailure"]["severity"] =
        serde_json::json!("warning");
    warning["_meta"]["jetbrains"]["air"]["sessionFailure"]["title"] =
        serde_json::json!("Reconnecting to Claude, attempt 3 of 10");
    let Some(AirSessionReport::Warning(title)) = air_session_report(&warning) else {
        panic!("a retry notice");
    };
    assert_eq!(title.as_ref(), "Reconnecting to Claude, attempt 3 of 10");

    // An ordinary session info update carries no such thing.
    assert!(
        air_session_report(&serde_json::json!({
            "sessionUpdate": "session_info_update",
            "_meta": { "claudeCode": { "whatever": true } },
        }))
        .is_none()
    );
    assert!(
        air_session_report(&serde_json::json!({ "sessionUpdate": "session_info_update" }))
            .is_none()
    );
}

/// Every frame for a subagent's own session was dropped as an unknown
/// session, so its tool calls never showed inside its card.
#[gpui::test]
async fn subagent_frames_land_in_the_subagents_thread(cx: &mut gpui::TestAppContext) {
    let (connection, project, _, _, _, _, _agent) = connect_fake_agent(None, cx).await;
    let work_dirs = PathList::new(&[std::path::Path::new("/a")]);
    let parent = cx
        .update(|cx| {
            connection.clone().load_session(
                acp_v2::SessionId::new("parent"),
                project.clone(),
                work_dirs.clone(),
                None,
                cx,
            )
        })
        .await
        .expect("the parent session");
    let context = ClientContext {
        sessions: connection.sessions.clone(),
        session_list: Rc::new(RefCell::new(None)),
        request_elicitations: cx.new(|_| ElicitationStore::default()),
        active_request_scopes: None,
        pending_session_updates: None,
        accepts_subagent_updates: false,
    };
    let raw = |session: &str, update: serde_json::Value| RawSessionNotification {
        session_id: acp::SessionId::new(session),
        update,
        meta: None,
    };
    let mut async_cx = cx.to_async();
    handle_session_notification(
        raw(
            "parent",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": "child",
                "name": "Explore",
            }),
        ),
        &mut async_cx,
        &context,
    );
    handle_session_notification(
        raw(
            "child",
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "from the subagent" },
            }),
        ),
        &mut async_cx,
        &context,
    );
    cx.run_until_parked();

    let child = cx
        .update(|cx| {
            connection.clone().load_session(
                acp_v2::SessionId::new("child"),
                project,
                work_dirs,
                None,
                cx,
            )
        })
        .await
        .expect("the subagent's own session, without asking the agent for it");
    child.read_with(cx, |child, cx| {
        assert!(child.to_markdown(cx).contains("from the subagent"));
        assert_eq!(
            child.parent_session_id(),
            Some(&acp_v2::SessionId::new("parent"))
        );
    });
    drop(parent);
}

/// quiet-ui: a session the relay answered a load for before it knew the
/// session's config gets its controls when the relay sends them.
#[gpui::test]
async fn test_a_session_loaded_without_config_gets_it_from_the_relay(
    cx: &mut gpui::TestAppContext,
) {
    let (connection, project, _, _, _, _, _keep_agent_alive) = connect_fake_agent(None, cx).await;
    let session_id = acp_v2::SessionId::new("relay-config");
    let thread = cx
        .update(|cx| {
            connection.clone().open_or_create_session(
                session_id.clone(),
                project,
                PathList::new(&[std::path::Path::new("/a")]),
                None,
                true,
                |_, _, _, _| {
                    async move {
                        Ok(SessionConfigResponse {
                            modes: None,
                            config_options: None,
                        })
                    }
                    .boxed_local()
                },
                cx,
            )
        })
        .await
        .expect("session should load");
    let updates = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let updates = updates.clone();
        cx.subscribe(&thread, move |_, event, _| {
            if let acp_thread::AcpThreadEvent::ConfigOptionsUpdated(options) = event {
                updates.borrow_mut().push(options.len());
            }
        })
    });
    cx.update(|cx| {
        assert!(connection.session_config_options(&session_id, cx).is_none());
    });

    // As the relay writes it: the session's open result, with its id.
    let config = RelaySessionConfig::parse_message(
        RELAY_SESSION_CONFIG,
        &serde_json::json!({
            "sessionId": "relay-config",
            "modes": null,
            "models": { "currentModelId": "opus", "availableModels": [] },
            "configOptions": [{
                "id": "effort",
                "name": "Effort",
                "type": "select",
                "currentValue": "high",
                "options": [{ "value": "high", "name": "High" }],
            }],
        }),
    )
    .expect("the relay's frame parses");
    enqueue_notification(
        &connection.dispatch_tx,
        config.clone(),
        handle_relay_session_config,
    );
    cx.run_until_parked();
    cx.update(|cx| {
        let options = connection
            .session_config_options(&session_id, cx)
            .expect("the controls arrived");
        assert_eq!(options.config_options().len(), 1);
    });
    assert_eq!(*updates.borrow(), [1]);

    // Controls the session has are not replaced.
    enqueue_notification(&connection.dispatch_tx, config, handle_relay_session_config);
    cx.run_until_parked();
    assert_eq!(*updates.borrow(), [1]);
}
