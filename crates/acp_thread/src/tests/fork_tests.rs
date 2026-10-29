//! The fork's own tests of `AcpThread`: command chips and their labels,
//! running work, session failures and the terminal changes behind them.

use super::*;

#[test]
fn quoted_commands_are_unquoted_so_they_highlight_as_bash() {
    assert_eq!(
        execute_command_label_source("\"cargo test --workspace\""),
        "```bash\ncargo test --workspace\n```"
    );
    assert_eq!(
        execute_command_label_source("'ls -la'"),
        "```bash\nls -la\n```"
    );
    assert_eq!(
        execute_command_label_source(r#""git commit -m \"fix: thing\"""#),
        "```bash\ngit commit -m \"fix: thing\"\n```"
    );

    for command in [
        "echo \"hi\"",
        "git commit -m \"wip\" && echo \"done\"",
        "\"my program\" --flag \"x\"",
        "echo 'a' && echo 'b'",
        "cargo test",
    ] {
        assert_eq!(
            execute_command_label_source(command),
            format!("```bash\n{command}\n```"),
            "{command} is not a quoted command"
        );
    }

    assert_eq!(
        execute_command_label_source("```sh\n\"cargo test\"\n```"),
        "```bash\ncargo test\n```"
    );
}

#[test]
fn command_display_prefixes() {
    assert_eq!(command_display_prefix("cargo build", 60), "cargo build");
    assert_eq!(
        command_display_prefix("echo abcdefghijklmnop", 9),
        "echo abcd…"
    );
    assert_eq!(
        command_display_prefix("cargo build\ncargo test", 60),
        "cargo build…"
    );
    assert_eq!(command_display_prefix("  spaced  ", 60), "spaced");
}

#[test]
fn a_long_command_loses_its_directories_before_its_file_names() {
    // The complaint's own line, which used to come out `wc flushcut-review.dif…`.
    assert_eq!(
        command_display_label("wc flushcut-review.diff", 100),
        "wc flushcut-review.diff"
    );
    assert_eq!(
        command_display_label("wc flushcut-review.diff", 40),
        "wc flushcut-review.diff"
    );

    // Directories go from the start, one at a time, until it fits.
    assert_eq!(
        command_display_label("wc crates/agent_ui/src/conversation_view/chips.rs", 40),
        "wc …/src/conversation_view/chips.rs"
    );
    assert_eq!(
        command_display_label("wc crates/agent_ui/src/conversation_view/chips.rs", 20),
        "wc …/chips.rs"
    );

    // The longest path first, so the room comes from where there is most of it.
    assert_eq!(
        command_display_label(
            "diff a/b.rs crates/agent_ui/src/conversation_view/chips.rs",
            40
        ),
        "diff a/b.rs …/conversation_view/chips.rs"
    );

    // Nothing left to shorten: fall back to cutting the end off.
    assert_eq!(
        command_display_label("echo abcdefghijklmnop", 9),
        "echo abcd…"
    );
    assert_eq!(
        command_display_label("wc …/anextremelylongfilenameindeed.diff", 12),
        "wc …/anextre…"
    );
    assert_eq!(
        command_display_label("cargo build\ncargo test", 60),
        "cargo build…"
    );
}

#[test]
fn json_error_payloads_are_parsed_into_message_and_code() {
    let raw = r#"Internal error: { "message": "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Jul 20th, 2026 11:23 PM.", "codexErrorInfo": "usageLimitExceeded" }"#;
    let payload = parse_agent_error_payload(raw).expect("the payload is JSON behind a prefix");
    assert_eq!(payload.code.as_deref(), Some("usageLimitExceeded"));
    assert!(payload.message.starts_with("You've hit your usage limit."));
    assert!(!payload.message.contains("codexErrorInfo"));

    assert_eq!(
        parse_agent_error_payload(r#"{"message": "boom", "code": 429}"#),
        Some(AgentErrorPayload {
            message: "boom".to_string(),
            code: Some("429".to_string()),
        })
    );

    assert_eq!(
        parse_agent_error_payload(r#"{"error": {"message": "nope", "type": "overloaded"}}"#),
        Some(AgentErrorPayload {
            message: "nope".to_string(),
            code: Some("overloaded".to_string()),
        })
    );

    assert_eq!(
        parse_agent_error_payload(r#"{"message": "{\"message\": \"inner\", \"code\": \"x\"}"}"#),
        Some(AgentErrorPayload {
            message: "inner".to_string(),
            code: Some("x".to_string()),
        })
    );

    for raw in [
        r#"{"code": "boom"}"#,
        "Something went wrong",
        r#"["a", "b"]"#,
        "Internal error: {oops",
        "",
    ] {
        assert_eq!(parse_agent_error_payload(raw), None, "{raw}");
    }
}

#[test]
fn urls_in_error_messages_become_links() {
    assert_eq!(
        linkify_urls(
            "Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://x.dev/u to buy."
        ),
        "Upgrade to Pro ([https://chatgpt.com/explore/pro](https://chatgpt.com/explore/pro)), \
         visit [https://x.dev/u](https://x.dev/u) to buy."
    );

    for unchanged in [
        "see [docs](https://zed.dev/docs) and <https://zed.dev>",
        "http is a protocol",
    ] {
        assert_eq!(linkify_urls(unchanged), unchanged);
    }
}

#[test]
fn generic_edit_labels_are_derived_from_the_edited_files() {
    let location = |path: &str| -> ToolCallLocation {
        acp_v1::ToolCallLocation::new(PathBuf::from(path)).into()
    };

    for title in ["editing files", "Editing Files.", ""] {
        assert_eq!(
            edit_label_source(title, &[location("/project/src/main.rs")]),
            "main.rs"
        );
    }
    assert_eq!(
        edit_label_source(
            "editing files",
            &[
                location("/project/a.rs"),
                location("/project/b.rs"),
                location("/project/c.rs"),
            ]
        ),
        "3 files"
    );
    assert_eq!(
        edit_label_source(
            "editing files",
            &[location("/project/a.rs"), location("/project/a.rs")]
        ),
        "a.rs"
    );

    assert_eq!(edit_label_source("editing files", &[]), "editing files");
    // Labels that name the file are kept, escaped.
    assert_eq!(
        edit_label_source("Create foo_bar.rs", &[location("/project/foo_bar.rs")]),
        "Create foo\\_bar.rs"
    );
}

#[gpui::test]
async fn assistant_markdown_renders_diagrams(cx: &mut TestAppContext) {
    init_test(cx);
    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let markdown = cx.update(|cx| {
        let block = ContentBlock::new_output(
            acp_v2::ContentBlock::Text(acp_v2::TextContent::new(String::from("Some prose."))),
            &language_registry,
            cx,
        );
        block
            .plain_markdown()
            .cloned()
            .unwrap_or_else(|| panic!("expected markdown, got {block:?}"))
    });
    cx.run_until_parked();
    markdown.read_with(cx, |markdown, _| {
        assert!(markdown.renders_mermaid_diagrams());
    });
}

#[test]
fn wait_calls_are_recognized_by_name_then_title() {
    use acp_v2::ToolKind as Kind;
    for (name, title, kind, expected) in [
        (Some("wait"), "Polling", Kind::Other, true),
        (Some("wait_for_task"), "Polling", Kind::Other, true),
        (Some("read_file"), "Waiting", Kind::Read, false),
        (None, "Wait", Kind::Other, true),
        (None, "Waiting…", Kind::Other, true),
        (None, "Waiting for the build", Kind::Other, true),
        (None, "Waitlist the user", Kind::Other, false),
        (None, "Read foo.rs", Kind::Read, false),
        (Some("wait"), "wait 5", Kind::Execute, false),
        (Some("wait"), "wait", Kind::Edit, false),
    ] {
        assert_eq!(is_wait_call(name, title, &kind), expected, "{title}");
    }
}

#[test]
fn tool_lookups_are_not_actions() {
    assert!(is_tool_lookup_call(Some("ToolSearch"), "ToolSearch"));
    assert!(is_tool_lookup_call(Some("tool_search"), "tool_search"));
    assert!(is_tool_lookup_call(Some("search_tools"), "search_tools"));
    assert!(is_tool_lookup_call(None, "ToolSearch"));
    assert!(!is_tool_lookup_call(Some("grep"), "Search for `foo`"));
    assert!(!is_tool_lookup_call(None, "Searched the codebase"));
}

#[test]
fn empty_stdin_writes_are_not_actions() {
    let empty = json!({ "chars": "" });
    let keystrokes = json!({ "chars": "y\n" });
    let unreadable = json!({ "data": [3] });
    for (name, title, input, expected) in [
        (Some("write_stdin"), "write_stdin", Some(&empty), true),
        (Some("write_stdin"), "write_stdin", None, true),
        (None, "write_stdin", Some(&empty), true),
        (Some("write_stdin"), "write_stdin", Some(&keystrokes), false),
        (Some("write_stdin"), "write_stdin", Some(&unreadable), false),
        (Some("read_file"), "Read foo.rs", None, false),
    ] {
        assert_eq!(
            is_empty_stdin_write_call(name, title, input),
            expected,
            "{name:?} {input:?}"
        );
    }
}

fn create_display_terminal(
    thread: &mut AcpThread,
    terminal_id: &acp_v1::TerminalId,
    cx: &mut Context<AcpThread>,
) {
    let builder = ::terminal::TerminalBuilder::new_display_only(
        ::terminal::terminal_settings::CursorShape::default(),
        ::terminal::terminal_settings::AlternateScroll::On,
        None,
        0,
        cx.background_executor(),
        thread.project().read(cx).path_style(cx),
    );
    let lower = cx.new(|cx| builder.subscribe(cx));
    thread.on_terminal_provider_event(
        TerminalProviderEvent::Created {
            terminal_id: terminal_id.clone(),
            label: "cargo test".to_string(),
            cwd: None,
            output_byte_limit: None,
            terminal: lower,
        },
        cx,
    );
}

fn execute_call_with_terminal(
    id: &'static str,
    status: acp_v1::ToolCallStatus,
    terminal_id: &acp_v1::TerminalId,
) -> acp_v1::SessionUpdate {
    acp_v1::SessionUpdate::ToolCall(
        acp_v1::ToolCall::new(id, "cargo test")
            .kind(acp_v1::ToolKind::Execute)
            .status(status)
            .content(vec![acp_v1::ToolCallContent::Terminal(
                acp_v1::Terminal::new(terminal_id.clone()),
            )]),
    )
}

fn claude_subagent_meta(session_id: Option<&str>) -> acp_v1::Meta {
    let mut meta = acp_v1::Meta::from_iter([(
        CLAUDE_CODE_META_KEY.into(),
        serde_json::json!({ "toolName": "Agent" }),
    )]);
    if let Some(session_id) = session_id {
        meta.insert(
            SUBAGENT_SESSION_INFO_META_KEY.into(),
            serde_json::json!({ "session_id": session_id, "message_start_index": 0 }),
        );
    }
    meta
}

#[gpui::test]
async fn test_display_only_terminal_reports_its_own_exit(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let terminal_id = acp_v2::TerminalId::new("display-only");
    thread.update(cx, |thread, cx| {
        create_display_terminal(thread, &acp_v1::TerminalId::new(terminal_id.0.clone()), cx);
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.terminals[&terminal_id].read(cx).output().is_none());
    });

    let still_running_at = Instant::now();
    thread.update(cx, |thread, cx| {
        thread.on_terminal_provider_event(
            TerminalProviderEvent::Exit {
                terminal_id: acp_v1::TerminalId::new(terminal_id.0.clone()),
                status: acp_v1::TerminalExitStatus::new().exit_code(1),
            },
            cx,
        );
    });
    cx.run_until_parked();

    thread.read_with(cx, |thread, cx| {
        let terminal = thread.terminals[&terminal_id].read(cx);
        let output = terminal
            .output()
            .expect("the reported exit ends the command");
        assert_eq!(output.exit_status.exit_code, Some(1));
        assert!(output.ended_at >= still_running_at);
    });
}

fn test_async_task(id: &str, tool_call_id: Option<acp_v2::ToolCallId>) -> AsyncTask {
    AsyncTask {
        id: id.to_string().into(),
        name: "pnpm dev".into(),
        description: None,
        kind: AsyncTaskKind::Command,
        state: AsyncTaskState::Running,
        summary: None,
        tool_call_id,
        output_file_path: None,
        can_stop: true,
        started_at: Instant::now(),
        service_port: None,
    }
}

#[gpui::test]
async fn test_a_fast_backgrounded_command_leaves_the_thread_idle(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    let tool_call_id = acp_v2::ToolCallId::new("fast-call");

    // The adapter publishes the task's whole life before the update saying
    // its call was backgrounded.
    thread.update(cx, |thread, cx| {
        thread.async_task_spawned(test_async_task("fast", Some(tool_call_id.clone())), cx);
        thread.async_task_state_updated("fast", AsyncTaskState::Completed, None, None, cx);
        thread.mark_tool_call_backgrounded(tool_call_id.clone(), cx);
    });
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
        assert!(!thread.tool_call_is_backgrounded(&tool_call_id));
    });
}

#[gpui::test]
async fn test_a_service_is_not_work(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    thread.update(cx, |thread, cx| {
        thread.async_task_spawned(test_async_task("dev", None), cx);
        thread.async_task_spawned(test_async_task("tests", None), cx);
    });
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).async_tasks, 2);
    });

    thread.update(cx, |thread, cx| {
        thread.set_async_task_service_port("dev", 3000, cx)
    });
    thread.read_with(cx, |thread, cx| {
        let work = thread.running_work(cx);
        assert_eq!(work.async_tasks, 1, "the tests still run");
        assert_eq!(work.services, 1);
        assert_eq!(thread.service_ports(), vec![3000]);
    });

    thread.update(cx, |thread, cx| {
        thread.async_task_state_updated("tests", AsyncTaskState::Completed, None, None, cx)
    });
    thread.read_with(cx, |thread, cx| {
        assert!(
            thread.running_work(cx).is_empty(),
            "a server alone is no work"
        );
        assert_eq!(thread.service_ports(), vec![3000]);
    });
}

#[gpui::test]
async fn test_a_session_failure_stops_what_the_agent_was_running(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    thread.update(cx, |thread, cx| {
        thread.async_task_spawned(test_async_task("dev", None), cx);
        thread.report_session_failure(
            SessionFailure {
                id: "failure".into(),
                revision: 1,
                category: SessionFailureCategory::Connection,
                title: "Claude exited.".into(),
                details: None,
            },
            cx,
        );
    });
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
        assert_eq!(thread.async_tasks()[0].state, AsyncTaskState::Stopped);
    });
}

#[gpui::test]
async fn test_detached_work_keeps_a_thread_and_its_card_working(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let tool_call_id = acp_v2::ToolCallId::new("bash-call");
    thread.update(cx, |thread, cx| {
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new(
                        acp_v1::ToolCallId::new(tool_call_id.0.clone()),
                        "cargo test",
                    )
                    .kind(acp_v1::ToolKind::Execute)
                    .status(acp_v1::ToolCallStatus::Completed),
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
        assert!(!thread.tool_call_is_backgrounded(&tool_call_id));
    });

    // The marker can arrive before the task that names the call.
    thread.update(cx, |thread, cx| {
        thread.mark_tool_call_backgrounded(tool_call_id.clone(), cx);
    });
    thread.read_with(cx, |thread, cx| {
        assert!(thread.tool_call_is_backgrounded(&tool_call_id));
        assert!(thread.running_work(cx).is_empty());
    });

    thread.update(cx, |thread, cx| {
        thread.async_task_spawned(
            AsyncTask {
                id: "task-1".into(),
                name: "cargo test".into(),
                description: Some("running the suite".into()),
                state: AsyncTaskState::Running,
                summary: None,
                tool_call_id: Some(tool_call_id.clone()),
                output_file_path: Some("/tmp/task-1.log".into()),
                can_stop: true,
                kind: AsyncTaskKind::Command,
                started_at: Instant::now(),
                service_port: None,
            },
            cx,
        );
    });
    thread.read_with(cx, |thread, cx| {
        assert_eq!(
            thread.running_work(cx),
            RunningWork {
                async_tasks: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            thread
                .async_task_for_tool_call(&tool_call_id)
                .map(|task| task.id.clone()),
            Some("task-1".into()),
        );
    });

    thread.update(cx, |thread, cx| {
        thread.async_task_progress(
            "task-1",
            None,
            Some("42 passed".into()),
            None,
            Some("/tmp/task-1-final.log".into()),
            cx,
        );
    });
    thread.read_with(cx, |thread, _| {
        let task = &thread.async_tasks()[0];
        assert_eq!(task.summary.as_deref(), Some("42 passed"));
        assert_eq!(
            task.output_file_path.as_deref(),
            Some("/tmp/task-1-final.log")
        );
        assert_eq!(task.description.as_deref(), Some("running the suite"));
    });

    thread.update(cx, |thread, cx| {
        thread.async_task_state_updated(
            "task-1",
            AsyncTaskState::Completed,
            Some("done".into()),
            None,
            cx,
        );
    });
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
        assert!(!thread.tool_call_is_backgrounded(&tool_call_id));
    });
}

#[gpui::test]
async fn test_a_detached_task_announced_late_keeps_the_state_it_reached(
    cx: &mut gpui::TestAppContext,
) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let task = AsyncTask {
        id: "task-1".into(),
        name: "sleep 300".into(),
        description: None,
        state: AsyncTaskState::Running,
        summary: None,
        tool_call_id: None,
        output_file_path: None,
        can_stop: true,
        kind: AsyncTaskKind::Command,
        started_at: Instant::now(),
        service_port: None,
    };
    thread.update(cx, |thread, cx| {
        thread.async_task_spawned(task.clone(), cx);
        thread.async_task_state_updated("task-1", AsyncTaskState::Stopped, None, None, cx);
        thread.async_task_spawned(task, cx);
    });
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
        assert_eq!(thread.async_tasks().len(), 1);
    });
}

/// Terminals with no output are left behind by replayed history and by
/// cancelled turns; counting them marked idle sidebar rows as busy.
#[gpui::test]
async fn test_running_work_counts_only_what_is_still_in_flight(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });

    // A replayed call whose terminal never reported an exit.
    let replayed = acp_v1::TerminalId::new("replayed-command");
    thread.update(cx, |thread, cx| {
        create_display_terminal(thread, &replayed, cx);
        thread
            .handle_session_update(
                execute_call_with_terminal(
                    "replayed-call",
                    acp_v1::ToolCallStatus::Completed,
                    &replayed,
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });

    let in_flight = |thread: &mut AcpThread,
                     call: &'static str,
                     subagent: &'static str,
                     terminal_id: &acp_v1::TerminalId,
                     cx: &mut Context<AcpThread>| {
        create_display_terminal(thread, terminal_id, cx);
        thread
            .handle_session_update(
                execute_call_with_terminal(call, acp_v1::ToolCallStatus::InProgress, terminal_id),
                cx,
            )
            .unwrap();
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new(subagent, "Investigate the failure")
                        .status(acp_v1::ToolCallStatus::InProgress)
                        .meta(meta_with_tool_name("spawn_agent")),
                ),
                cx,
            )
            .unwrap();
    };
    let busy = RunningWork {
        terminals: 1,
        subagents: 1,
        ..Default::default()
    };

    // Work that finishes on its own.
    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();
    let finished = acp_v1::TerminalId::new("finished-command");
    thread.update(cx, |thread, cx| {
        in_flight(thread, "terminal-call", "subagent-call", &finished, cx);
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| assert_eq!(thread.running_work(cx), busy));
    thread.update(cx, |thread, cx| {
        thread.on_terminal_provider_event(
            TerminalProviderEvent::Exit {
                terminal_id: finished.clone(),
                status: acp_v1::TerminalExitStatus::new().exit_code(0),
            },
            cx,
        );
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::ToolCallUpdate(acp_v1::ToolCallUpdate::new(
                    acp_v1::ToolCallId::new("subagent-call"),
                    acp_v1::ToolCallUpdateFields::new().status(acp_v1::ToolCallStatus::Completed),
                )),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });
    complete
        .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
        .expect("turn should still be running");
    request.await.expect("turn should complete");

    // Work a cancelled turn leaves behind.
    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();
    let cancelled = acp_v1::TerminalId::new("cancelled-command");
    thread.update(cx, |thread, cx| {
        in_flight(
            thread,
            "cancelled-call",
            "cancelled-subagent",
            &cancelled,
            cx,
        );
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| assert_eq!(thread.running_work(cx), busy));

    // `cancel` waits for the turn's send task, which this test holds.
    let cancelled_turn = thread.update(cx, |thread, cx| thread.cancel(cx));
    drop(complete);
    cancelled_turn.await.ok();
    request.await.ok();
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });
}

/// The row rebuilds on the thread's events, and before this the whole
/// running set could move with nothing but a `cx.notify()` behind it, which
/// reaches the thread's own views and nothing watching from outside.
#[gpui::test]
async fn test_running_work_says_so_with_an_event(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        cx.subscribe(&thread, {
            let events = events.clone();
            move |_, event, _| match event {
                AcpThreadEvent::RunningWorkChanged => events.borrow_mut().push("running work"),
                AcpThreadEvent::NewEntry => events.borrow_mut().push("new entry"),
                AcpThreadEvent::EntryUpdated(_) => events.borrow_mut().push("entry updated"),
                _ => {}
            }
        })
    });

    let tool_call_id = acp_v2::ToolCallId::new("bash-call");
    thread.update(cx, |thread, cx| {
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new(
                        acp_v1::ToolCallId::new(tool_call_id.0.clone()),
                        "cargo test",
                    )
                    .kind(acp_v1::ToolKind::Execute)
                    .status(acp_v1::ToolCallStatus::Completed),
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    events.borrow_mut().clear();

    // The turn has ended and the agent reports the command detached, which
    // changes no entry at all.
    thread.update(cx, |thread, cx| {
        thread.mark_tool_call_backgrounded(tool_call_id.clone(), cx);
        thread.async_task_spawned(
            AsyncTask {
                id: "task-1".into(),
                name: "cargo test".into(),
                description: None,
                state: AsyncTaskState::Running,
                summary: None,
                tool_call_id: Some(tool_call_id.clone()),
                output_file_path: None,
                can_stop: true,
                kind: AsyncTaskKind::Command,
                started_at: Instant::now(),
                service_port: None,
            },
            cx,
        );
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).async_tasks, 1);
    });
    assert_eq!(
        *events.borrow(),
        vec!["running work", "running work"],
        "a detached command has to say so, or the row keeps what it last drew"
    );

    events.borrow_mut().clear();
    thread.update(cx, |thread, cx| {
        thread.async_task_state_updated("task-1", AsyncTaskState::Completed, None, None, cx);
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });
    assert_eq!(
        *events.borrow(),
        vec!["running work"],
        "the row goes idle on this event alone; no other event arrives"
    );
}

/// A command the turn left running has a process of ours behind it, which
/// is the evidence that counts outside a turn. Its exit is a notify on the
/// terminal and nothing on the thread, so the thread watches for it.
#[gpui::test]
async fn test_a_command_exiting_after_the_turn_says_so(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    cx.executor().allow_parking();
    cx.update(|cx| cx.set_global(::terminal::HeadlessTerminal(true)));

    let terminal = thread
        .update(cx, |thread, cx| {
            thread.create_terminal(
                "sleep 60".into(),
                Vec::new(),
                Vec::new(),
                None,
                None,
                None,
                cx,
            )
        })
        .await
        .expect("a real client process");
    let terminal_id = terminal.read_with(cx, |terminal, _| terminal.id().clone());

    let events = Rc::new(RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        cx.subscribe(&thread, {
            let events = events.clone();
            move |_, event, _| {
                if matches!(event, AcpThreadEvent::RunningWorkChanged) {
                    *events.borrow_mut() += 1;
                }
            }
        })
    });

    // The call the turn left behind, with no turn running over it.
    thread.update(cx, |thread, cx| {
        thread
            .handle_session_update(
                execute_call_with_terminal(
                    "terminal-call",
                    acp_v1::ToolCallStatus::InProgress,
                    &acp_v1::TerminalId::new(terminal_id.0.clone()),
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(
            thread.running_work(cx).terminals,
            1,
            "a process of ours is still running under the call"
        );
    });

    *events.borrow_mut() = 0;
    thread
        .update(cx, |thread, cx| {
            thread.kill_terminal(terminal_id.clone(), cx)
        })
        .expect("the terminal is ours to kill");
    terminal
        .read_with(cx, |terminal, _| terminal.wait_for_exit())
        .expect("client process exit")
        .await;
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert!(thread.running_work(cx).is_empty());
    });
    assert!(
        *events.borrow() > 0,
        "the exit has to reach the row, which hears only the thread's events"
    );
}

/// Nothing on the wire takes a failure back, so a row would show the last
/// one forever if this side did not drop it when a turn starts.
/// 2026-10-19: a thread spun for eight minutes while the CLI retried
/// `ECONNRESET`, with nothing on screen saying so.
#[gpui::test]
async fn test_a_retry_warning_shows_until_the_next_update(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    thread.update(cx, |thread, cx| {
        thread.report_session_warning("Reconnecting to Claude, attempt 2 of 10".into(), cx);
        assert_eq!(
            thread.session_warning().map(|warning| warning.as_ref()),
            Some("Reconnecting to Claude, attempt 2 of 10")
        );
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::SessionInfoUpdate(acp_v1::SessionInfoUpdate::new()),
                cx,
            )
            .unwrap();
        assert!(
            thread.session_warning().is_some(),
            "session info is not progress"
        );
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new("back".into())),
                cx,
            )
            .unwrap();
        assert!(thread.session_warning().is_none());
    });
}

#[gpui::test]
async fn test_a_reported_failure_lasts_until_the_next_turn(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    let reports = Rc::new(RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        cx.subscribe(&thread, {
            let reports = reports.clone();
            move |_, event, _| {
                if matches!(event, AcpThreadEvent::StopReasonChanged) {
                    *reports.borrow_mut() += 1;
                }
            }
        })
    });

    let failure = |revision: u64, title: &str| SessionFailure {
        id: "session-1:session-error:epoch:1".into(),
        revision,
        category: SessionFailureCategory::Limit,
        title: title.to_string().into(),
        details: None,
    };

    thread.update(cx, |thread, cx| {
        thread.report_session_failure(failure(1, "Claude is temporarily rate limited."), cx);
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(
            thread
                .session_failure()
                .map(|failure| failure.title.clone()),
            Some("Claude is temporarily rate limited.".into())
        );
    });
    assert_eq!(*reports.borrow(), 1);

    // The adapter republishes the same failure as it is restored from the
    // transcript, with the revision it had.
    thread.update(cx, |thread, cx| {
        thread.report_session_failure(failure(1, "a replay of the same report"), cx);
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(
            thread
                .session_failure()
                .map(|failure| failure.title.clone()),
            Some("Claude is temporarily rate limited.".into()),
            "a report that is not newer leaves the one in hand alone"
        );
    });
    assert_eq!(*reports.borrow(), 1);

    thread.update(cx, |thread, cx| {
        thread.report_session_failure(failure(2, "and now it is worse"), cx);
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(
            thread
                .session_failure()
                .map(|failure| failure.title.clone()),
            Some("and now it is worse".into())
        );
    });
    assert_eq!(*reports.borrow(), 2);

    // A turn starting is what clears it; nothing on the wire does.
    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();
    thread.read_with(cx, |thread, _| {
        assert!(thread.session_failure().is_none());
    });
    assert_eq!(*reports.borrow(), 3);
    complete
        .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
        .expect("turn should still be running");
    request.await.expect("turn should complete");
}

/// The chip's spinner and the row's working state read the same calls
/// through the same predicate, so one cannot say running while the other
/// says idle.
#[gpui::test]
async fn test_a_spinning_chip_always_has_a_working_row(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let running_calls = |thread: &AcpThread, cx: &App| {
        thread
            .entries()
            .iter()
            .filter(|entry| match entry {
                AgentThreadEntry::ToolCall(call) => thread.tool_call_is_running(call, cx),
                _ => false,
            })
            .count()
    };

    let terminal_id = acp_v1::TerminalId::new("long-command");
    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();
    thread.update(cx, |thread, cx| {
        create_display_terminal(thread, &terminal_id, cx);
        thread
            .handle_session_update(
                execute_call_with_terminal(
                    "terminal-call",
                    acp_v1::ToolCallStatus::InProgress,
                    &terminal_id,
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(running_calls(thread, cx), 1);
        assert!(!thread.running_work(cx).is_empty());
    });

    // The turn ends with the call still `InProgress`. Nobody is left to
    // finish it, so neither the chip nor the row claims it is running.
    complete
        .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
        .expect("turn should still be running");
    request.await.expect("turn should complete");
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(running_calls(thread, cx), 0);
        assert!(thread.running_work(cx).is_empty());
    });

    // Until the agent says it detached it, when both say so again.
    thread.update(cx, |thread, cx| {
        thread.mark_tool_call_backgrounded(acp_v2::ToolCallId::new("terminal-call"), cx);
    });
    thread.read_with(cx, |thread, cx| {
        assert_eq!(running_calls(thread, cx), 1);
        assert!(!thread.running_work(cx).is_empty());
    });
}

/// Claude names its tools only in its own meta namespace.
#[gpui::test]
async fn test_claude_subagents_are_counted_while_they_work(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let claude_tool = |name: &str| {
        acp_v1::Meta::from_iter([(
            CLAUDE_CODE_META_KEY.into(),
            serde_json::json!({ "toolName": name }),
        )])
    };
    let call = |id: &'static str, name: &str, status| {
        acp_v1::SessionUpdate::ToolCall(
            acp_v1::ToolCall::new(id, id)
                .status(status)
                .meta(claude_tool(name)),
        )
    };

    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();

    thread.update(cx, |thread, cx| {
        for (id, name) in [
            ("agent-call", "Agent"),
            ("task-call", "Task"),
            ("bash-call", "Bash"),
        ] {
            thread
                .handle_session_update(call(id, name, acp_v1::ToolCallStatus::InProgress), cx)
                .unwrap();
        }
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).subagents, 2);
    });

    thread.update(cx, |thread, cx| {
        thread
            .handle_session_update(
                call("agent-call", "Agent", acp_v1::ToolCallStatus::Completed),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).subagents, 1);
    });

    // Nobody is left to finish a call once the turn ends.
    complete
        .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
        .expect("turn should still be running");
    request.await.expect("turn should complete");
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).subagents, 0);
    });
}

/// Claude's `Agent` call completes when the subagent is handed off, so only
/// the adapter's lifecycle reports say it is still running.
#[gpui::test]
async fn test_reported_subagents_outlive_the_turn_that_launched_them(
    cx: &mut gpui::TestAppContext,
) {
    init_test(cx);
    let thread = new_test_thread(cx).await;

    let reviews = [
        ("subagent-1", "Reuse review of flush cuts"),
        ("subagent-2", "Simplification and altitude review"),
        ("subagent-3", "Efficiency review of flush cuts"),
    ];

    let (complete, request) = start_test_turn(&thread, cx);
    cx.run_until_parked();

    thread.update(cx, |thread, cx| {
        for (session, task) in reviews {
            thread.subagent_reported(
                Subagent {
                    session_id: acp_v2::SessionId::new(session),
                    name: Some("code-reviewer".into()),
                    task: Some(task.into()),
                    state: AsyncTaskState::Running,
                },
                cx,
            );
            thread
                .handle_session_update(
                    acp_v1::SessionUpdate::ToolCall(
                        acp_v1::ToolCall::new(session, task)
                            // Reported and still in flight: counted once.
                            .status(if session == "subagent-1" {
                                acp_v1::ToolCallStatus::InProgress
                            } else {
                                acp_v1::ToolCallStatus::Completed
                            })
                            .meta(claude_subagent_meta(Some(session))),
                    ),
                    cx,
                )
                .unwrap();
        }
        // One nobody reports on, which only its call speaks for.
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new("unreported", "Summarise the diff")
                        .status(acp_v1::ToolCallStatus::InProgress)
                        .meta(claude_subagent_meta(None)),
                ),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).subagents, 4);
        for (session, _) in reviews {
            let call = thread
                .tool_call(&acp_v2::ToolCallId::new(session))
                .expect("subagent call")
                .1;
            assert_eq!(
                thread.subagent_state_for_tool_call(call),
                Some(AsyncTaskState::Running),
            );
        }
    });

    complete
        .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
        .expect("turn should still be running");
    request.await.expect("turn should complete");
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.running_work(cx).subagents, 3);
    });

    for (session, state, remaining) in [
        ("subagent-1", AsyncTaskState::Completed, 2),
        ("subagent-2", AsyncTaskState::Failed, 1),
        ("subagent-3", AsyncTaskState::Stopped, 0),
    ] {
        thread.update(cx, |thread, cx| {
            thread.subagent_state_updated(&acp_v2::SessionId::new(session), state, cx);
        });
        cx.run_until_parked();
        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.running_work(cx).subagents, remaining, "{session}");
        });
    }
}

#[gpui::test]
async fn test_terminal_output_records_how_long_the_command_ran(cx: &mut gpui::TestAppContext) {
    use std::collections::HashMap;
    use task::Shell;
    use util::shell_builder::ShellBuilder;

    init_test(cx);
    cx.executor().allow_parking();
    let thread = new_test_thread(cx).await;

    let terminal_id = acp_v2::TerminalId::new(uuid::Uuid::new_v4().to_string());
    let (program, args) =
        ShellBuilder::new(&Shell::System, false).build(Some("sleep 1".to_owned()), &[]);
    let terminal_mode = ::terminal::TerminalMode::task(task::SpawnInTerminal {
        command: Some(program.clone()),
        args: args.clone(),
        ..Default::default()
    });
    let builder = cx
        .update(|cx| {
            ::terminal::TerminalBuilder::new(
                None,
                terminal_mode,
                task::Shell::WithArguments {
                    program,
                    args,
                    title_override: None,
                },
                HashMap::default(),
                ::terminal::terminal_settings::CursorShape::default(),
                ::terminal::terminal_settings::AlternateScroll::On,
                None,
                vec![],
                Duration::ZERO,
                false,
                0,
                cx,
                vec![],
                PathStyle::local(),
            )
        })
        .await
        .unwrap();
    let lower_terminal = cx.new(|cx| builder.subscribe(cx));

    thread.update(cx, |thread, cx| {
        thread.on_terminal_provider_event(
            TerminalProviderEvent::Created {
                terminal_id: acp_v1::TerminalId::new(terminal_id.0.clone()),
                label: "sleep 1".to_string(),
                cwd: None,
                output_byte_limit: None,
                terminal: lower_terminal.clone(),
            },
            cx,
        );
    });

    // Wait for the real process first, so the duration is the command's.
    let ran = cx.update(|cx| lower_terminal.read(cx).wait_for_completed_task(cx));
    ran.await;
    thread.update(cx, |thread, cx| {
        thread.on_terminal_provider_event(
            TerminalProviderEvent::Exit {
                terminal_id: acp_v1::TerminalId::new(terminal_id.0.clone()),
                status: acp_v1::TerminalExitStatus::new().exit_code(Some(0)),
            },
            cx,
        );
    });
    cx.run_until_parked();

    let (started_at, ended_at) = thread.read_with(cx, |thread, cx| {
        let terminal = thread.terminals[&terminal_id].read(cx);
        let output = terminal.output().expect("the command finished");
        (terminal.started_at(), output.ended_at)
    });
    let ran_for = ended_at.duration_since(started_at);
    assert!(
        ran_for >= Duration::from_millis(500),
        "a command that slept for a second reported {ran_for:?}"
    );
}

#[gpui::test]
async fn test_compaction_notice_becomes_a_compaction_entry(cx: &mut gpui::TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection = Rc::new(FakeAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection.new_session(project, PathList::new(&[Path::new(path!("/test"))]), cx)
        })
        .await
        .unwrap();

    thread.update(cx, |thread, cx| {
        thread.push_assistant_content_block(
            "Context compacted to fit the model's context window.".into(),
            false,
            cx,
        );
    });
    thread.update(cx, |thread, _cx| {
        assert!(matches!(
            thread.entries.as_slice(),
            [AgentThreadEntry::ContextCompaction(compaction)]
                if compaction.status == ContextCompactionStatus::Completed
        ));
    });

    thread.update(cx, |thread, cx| {
        thread.push_assistant_content_block(
            "I could compact the context window here, but let me first explain what \
             compaction does and why the model's context window fills up: every tool \
             call, every file I read, and every message we exchange accumulates in it."
                .into(),
            false,
            cx,
        );
    });
    thread.update(cx, |thread, _cx| {
        assert_eq!(thread.entries.len(), 2);
        assert!(matches!(
            thread.entries[1],
            AgentThreadEntry::AssistantMessage(_)
        ));
    });
}

/// 2026-10-19: worktree slots went from 23 to 43 in 45 minutes, one per
/// file an agent read outside the project.
#[gpui::test]
async fn test_a_location_outside_the_project_makes_no_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({"file.rs": "fn a() {}\n", "plot.png": "png"}),
    )
    .await;
    fs.insert_tree(path!("/home/memory"), json!({"notes.md": "remember\n"}))
        .await;
    let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
    let connection = Rc::new(FakeAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection.new_session(
                project.clone(),
                PathList::new(&[Path::new(path!("/project"))]),
                cx,
            )
        })
        .await
        .expect("session");
    thread.update(cx, |thread, cx| {
        for (id, path) in [
            ("outside", path!("/home/memory/notes.md")),
            ("image", path!("/project/plot.png")),
            ("inside", path!("/project/file.rs")),
        ] {
            thread
                .upsert_tool_call(
                    acp_v1::ToolCall::new(id, "Read")
                        .locations(vec![acp_v1::ToolCallLocation::new(PathBuf::from(path))]),
                    cx,
                )
                .expect("tool call");
        }
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        assert_eq!(
            project.worktrees(cx).count(),
            1,
            "no worktree for the outside file"
        );
    });
    thread.read_with(cx, |thread, _| {
        let resolved = |id: &str| {
            let (index, call) = thread.tool_call(&acp_v2::ToolCallId::new(id)).unwrap();
            assert_eq!(call.locations.len(), 1, "the path is kept");
            thread.entries[index].location(0).is_some()
        };
        assert!(!resolved("outside"));
        assert!(!resolved("image"));
        assert!(resolved("inside"));
    });
}

#[gpui::test]
async fn test_cancel_stops_the_in_progress_plan_entry(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection = Rc::new(FakeAgentConnection::new().on_user_message(
        move |_, thread, mut cx| {
            async move {
                thread
                    .update(&mut cx, |thread, cx| {
                        thread.handle_session_update(
                            acp_v1::SessionUpdate::Plan(acp_v1::Plan::new(vec![
                                acp_v1::PlanEntry::new(
                                    "First",
                                    acp_v1::PlanEntryPriority::Medium,
                                    acp_v1::PlanEntryStatus::InProgress,
                                ),
                                acp_v1::PlanEntry::new(
                                    "Second",
                                    acp_v1::PlanEntryPriority::Medium,
                                    acp_v1::PlanEntryStatus::Pending,
                                ),
                            ])),
                            cx,
                        )
                    })
                    .unwrap()
                    .unwrap();
                Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::Cancelled))
            }
            .boxed_local()
        },
    ));

    let thread = cx
        .update(|cx| {
            connection.new_session(project, PathList::new(&[Path::new(path!("/test"))]), cx)
        })
        .await
        .unwrap();

    thread
        .update(cx, |thread, cx| thread.send_raw("Do the thing", cx))
        .await
        .unwrap();
    cx.run_until_parked();

    thread.read_with(cx, |thread, _| {
        let plan = thread.plan().expect("the turn reported a plan");
        assert!(
            plan.stats().in_progress_entry.is_none(),
            "a cancelled turn leaves nothing in progress"
        );
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(
            plan.entries[0].source.status,
            acp_v2::PlanEntryStatus::Pending
        );
    });
}

/// An off-screen thread gives up its Markdown parses and its finished
/// terminals' grids, and output arriving after that lands on the text it had
/// rather than on an empty grid.
#[gpui::test]
async fn test_a_released_thread_keeps_what_its_terminals_printed(cx: &mut TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    let terminal_id = acp_v2::TerminalId::new("released-terminal");
    thread.update(cx, |thread, cx| {
        thread.push_assistant_content_block("Some **markdown**".into(), false, cx);
        thread
            .upsert_wire_tool_call(
                acp_v2::ToolCallUpdate::new("terminal-tool")
                    .title("ls")
                    .kind(acp_v2::ToolKind::Execute)
                    .status(acp_v2::ToolCallStatus::Completed)
                    .content(vec![acp_v2::ToolCallContent::Terminal(
                        acp_v2::Terminal::new(terminal_id.clone()),
                    )]),
                cx,
            )
            .unwrap();
        thread
            .upsert_display_terminal(
                terminal_id.clone(),
                DisplayTerminalPatch {
                    command: MaybeUndefined::Value("ls".into()),
                    output: MaybeUndefined::Value(DisplayTerminalOutput {
                        data: b"one\r\ntwo\r\n".to_vec(),
                        meta: None,
                    }),
                    exit_status: MaybeUndefined::Value(
                        acp_v2::TerminalExitStatus::new().exit_code(0),
                    ),
                    ..Default::default()
                },
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();

    let terminal = thread.read_with(cx, |thread, _| {
        thread.terminal(terminal_id.clone()).unwrap()
    });
    let renderer = terminal.read_with(cx, |terminal, _| terminal.inner().clone());
    thread.read_with(cx, |thread, cx| {
        let held = thread.held_render_state(cx);
        assert!(
            held.markdowns >= 2,
            "the message and the tool label: {held:?}"
        );
        assert_eq!(held.terminal_grids, 1);
    });

    let released = thread.update(cx, |thread, cx| thread.release_render_state(cx));
    assert!(
        released.markdowns >= 2 && released.terminal_grids == 1,
        "{released:?}"
    );
    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.held_render_state(cx), ReleasedRenderState::default());
        assert!(thread.to_markdown(cx).contains("one\ntwo"));
        assert!(thread.to_markdown(cx).contains("Some **markdown**"));
    });
    assert_eq!(
        thread.update(cx, |thread, cx| thread.release_render_state(cx)),
        ReleasedRenderState::default(),
        "nothing is left to release twice"
    );
    renderer.read_with(cx, |renderer, _| {
        assert!(renderer.get_content().trim().is_empty(), "the grid is gone");
    });

    thread
        .update(cx, |thread, cx| {
            thread.append_display_terminal_output(terminal_id.clone(), b"three\r\n", cx)
        })
        .unwrap();
    terminal.read_with(cx, |terminal, cx| {
        assert!(!terminal.is_grid_released());
        let content = renderer.read(cx).get_content();
        assert!(content.contains("one\ntwo\nthree"), "{content:?}");
        assert_eq!(terminal.output().unwrap().content, content);
    });

    terminal.update(cx, |terminal, cx| assert!(terminal.release_grid(cx)));
    terminal.update(cx, |terminal, cx| terminal.restore_grid(cx));
    renderer.read_with(cx, |renderer, _| {
        assert!(renderer.get_content().contains("one\ntwo\nthree"));
    });
}
