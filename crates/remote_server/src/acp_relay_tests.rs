//! quiet-ui: the relay end to end — a laptop project over the fake remote
//! transport, the server, the relay and a fake ACP agent — through a server
//! being replaced mid-turn, a permission question asked while nobody was
//! attached, and a fresh client attaching to a live session.
//!
//! This is the test that keeps the relay from silently breaking on a rebase:
//! the agent launch path upstream is being reworked for ACP v2.

use crate::{HeadlessAppState, HeadlessProject, acp_relay_host::RelayConnector};
use acp_relay::{AgentIo, AgentSpawner, AgentSpec, Relay, TaskSpawner, daemon};
use acp_thread::{AcpThread, AgentConnection, AgentThreadEntry, SelectedPermissionOutcome};
use agent_client_protocol::schema::v2 as acp;
use agent_settings::AgentSettings;
use anyhow::Result;
use collections::HashMap;
use extension::ExtensionHostProxy;
use fs::FakeFs;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use http_client::BlockedHttpClient;
use language::LanguageRegistry;
use node_runtime::NodeRuntime;
use project::{AgentId, Project, agent_server_store::AgentServerCommand};
use remote::RemoteClient;
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    sync::Arc,
};
use util::{path, path_list::PathList};

/// A scripted ACP agent. A prompt streams "one", waits for the test, streams
/// "two", asks a permission question, streams "three" once it is answered,
/// and ends the turn.
#[derive(Default)]
struct Script {
    gate: RefCell<Option<oneshot::Receiver<()>>>,
    answer: RefCell<Option<oneshot::Sender<Value>>>,
    received: RefCell<Vec<Value>>,
    spawned: Cell<usize>,
    long_turn: Cell<usize>,
}

struct FakeAgents {
    script: Rc<Script>,
    spawn_task: TaskSpawner,
}

fn chunk(session: &str, text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session,
            "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } },
        },
    })
    .to_string()
}

fn reply(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

impl AgentSpawner for FakeAgents {
    fn spawn(&self, spec: &AgentSpec) -> Result<AgentIo> {
        assert!(matches!(spec, AgentSpec::Command(_)));
        let script = self.script.clone();
        script.spawned.set(script.spawned.get() + 1);
        let (stdout, stdout_rx) = mpsc::unbounded::<String>();
        let (stdin, mut stdin_rx) = mpsc::unbounded::<String>();
        let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();
        let spawn_task = self.spawn_task.clone();
        (self.spawn_task)(Box::pin(async move {
            while let Some(line) = stdin_rx.next().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                script.received.borrow_mut().push(message.clone());
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                match message.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        stdout
                            .unbounded_send(reply(
                                &id,
                                json!({
                                    "protocolVersion": 1,
                                    "agentCapabilities": { "loadSession": true },
                                    "authMethods": [],
                                }),
                            ))
                            .ok();
                    }
                    Some("session/new") => {
                        stdout
                            .unbounded_send(reply(&id, json!({ "sessionId": "s1" })))
                            .ok();
                    }
                    Some("session/prompt") => {
                        let stdout = stdout.clone();
                        let script = script.clone();
                        spawn_task(Box::pin(async move {
                            stdout.unbounded_send(chunk("s1", "one")).ok();
                            let gate = script.gate.borrow_mut().take();
                            if let Some(gate) = gate {
                                gate.await.ok();
                            }
                            for _ in 0..script.long_turn.get() {
                                stdout.unbounded_send(chunk("s1", ".")).ok();
                            }
                            stdout.unbounded_send(chunk("s1", "two")).ok();
                            let (answer_tx, answer_rx) = oneshot::channel();
                            *script.answer.borrow_mut() = Some(answer_tx);
                            stdout
                                .unbounded_send(
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": 900,
                                        "method": "session/request_permission",
                                        "params": {
                                            "sessionId": "s1",
                                            "toolCall": { "toolCallId": "tc-1", "title": "Run a command", "kind": "execute", "status": "pending" },
                                            "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }],
                                        },
                                    })
                                    .to_string(),
                                )
                                .ok();
                            answer_rx.await.ok();
                            stdout.unbounded_send(chunk("s1", "three")).ok();
                            stdout
                                .unbounded_send(reply(&id, json!({ "stopReason": "end_turn" })))
                                .ok();
                        }));
                    }
                    Some(_) => {}
                    None => {
                        if id == json!(900)
                            && let Some(answer) = script.answer.borrow_mut().take()
                        {
                            answer.send(message).ok();
                        }
                    }
                }
            }
            exit_tx.send(Some(0)).ok();
        }));
        Ok(AgentIo {
            stdout: stdout_rx.boxed_local(),
            stderr: futures::stream::empty().boxed_local(),
            stdin,
            exited: exit_rx.map(|code| code.ok().flatten()).boxed_local(),
            kill: Box::new(|| {}),
        })
    }
}

struct RelayTest {
    project: Entity<Project>,
    headless: Entity<HeadlessProject>,
    script: Rc<Script>,
    relay: Rc<Relay>,
}

async fn init_relay_test(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
    log_dir: Option<&Path>,
) -> RelayTest {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/project"), json!({ "a.rs": "fn a() {}" }))
        .await;
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    zlog::init_test();

    let script = Rc::new(Script::default());
    let relay = server_cx.update(|cx| {
        let spawn_task = crate::acp_relay_host::task_spawner(cx);
        Relay::new(
            log_dir.map(Path::to_path_buf),
            Rc::new(FakeAgents {
                script: script.clone(),
                spawn_task: spawn_task.clone(),
            }),
            spawn_task,
        )
    });
    // Each connect is a fresh server's connection to the same relay.
    server_cx.update(|cx| {
        let relay = relay.clone();
        cx.set_global(RelayConnector(Rc::new(move |on_frame, cx| {
            let spawn_task = crate::acp_relay_host::task_spawner(cx);
            let (to_relay, from_server) = mpsc::unbounded::<String>();
            let (to_server, from_relay) = mpsc::unbounded::<String>();
            spawn_task(Box::pin(daemon::serve_connection(
                relay.clone(),
                from_server.boxed_local(),
                to_server,
            )));
            gpui::Task::ready(Ok(daemon::DaemonClient::new(
                from_relay.boxed_local(),
                to_relay,
                &spawn_task,
                on_frame,
            )))
        })));
    });

    let (opts, server_session, _) = RemoteClient::fake_server(cx, server_cx);
    server_cx.update(HeadlessProject::init);
    let headless = server_cx.new(|cx| {
        HeadlessProject::new(
            HeadlessAppState {
                session: server_session,
                fs: fs.clone(),
                http_client: Arc::new(BlockedHttpClient),
                node_runtime: NodeRuntime::unavailable(),
                languages: Arc::new(LanguageRegistry::new(cx.background_executor().clone())),
                extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    let remote = RemoteClient::connect_mock(opts, cx).await;
    let project = crate::remote_editing_tests::build_project(remote, cx);
    cx.update(|cx| {
        AgentSettings::register(cx);
    });
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    RelayTest {
        project,
        headless,
        script,
        relay,
    }
}

async fn connect(test: &RelayTest, cx: &mut TestAppContext) -> Rc<dyn AgentConnection> {
    let store = test
        .project
        .read_with(cx, |project, _| project.agent_server_store().clone());
    let connection = agent_servers::AcpConnection::stdio(
        AgentId::new("fake"),
        test.project.clone(),
        AgentServerCommand {
            path: "fake-agent".into(),
            args: Vec::new(),
            env: None,
        },
        store.downgrade(),
        None,
        HashMap::default(),
        &mut cx.to_async(),
    )
    .await
    .expect("the agent connects through the relay");
    Rc::new(connection)
}

fn assistant_text(thread: &Entity<AcpThread>, cx: &TestAppContext) -> String {
    thread.read_with(cx, |thread, cx| {
        thread
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                AgentThreadEntry::AssistantMessage(message) => Some(message.to_markdown(cx)),
                _ => None,
            })
            .collect::<String>()
            .replace("## Assistant", "")
            .replace('\n', "")
    })
}

fn has_pending_permission(thread: &Entity<AcpThread>, cx: &TestAppContext) -> bool {
    thread.read_with(cx, |thread, _| {
        thread
            .entries()
            .iter()
            .any(|entry| matches!(entry, AgentThreadEntry::ToolCall(call) if call.authorization_id().is_some()))
    })
}

fn run(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    for _ in 0..4 {
        server_cx.run_until_parked();
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_a_turn_survives_the_server_being_replaced(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let test = init_relay_test(cx, server_cx, None).await;
    let connection = connect(&test, cx).await;
    let thread = cx
        .update(|cx| {
            connection.clone().new_session(
                test.project.clone(),
                PathList::new(&[Path::new(path!("/project"))]),
                cx,
            )
        })
        .await
        .unwrap();

    let (gate_tx, gate_rx) = oneshot::channel();
    *test.script.gate.borrow_mut() = Some(gate_rx);
    let turn = thread.update(cx, |thread, cx| thread.send_raw("go", cx));
    let turn = cx.background_spawn(turn);
    run(cx, server_cx);
    assert_eq!(assistant_text(&thread, cx), "one");

    // The server on the box dies: the relay detaches the link and the agent
    // carries on alone, asking its permission question with nobody there.
    test.headless.update(server_cx, |headless, cx| {
        headless._acp_relay.update(cx, |host, _| host.forget_daemon_for_test())
    });
    gate_tx.send(()).unwrap();
    run(cx, server_cx);
    assert_eq!(assistant_text(&thread, cx), "one");
    assert!(!has_pending_permission(&thread, cx));
    assert_eq!(
        thread.read_with(cx, |thread, _| thread.status()),
        acp_thread::ThreadStatus::Generating,
        "a dropped connection neither cancels nor fails the turn"
    );

    // Zed reconnects to what is now a fresh server, which reattaches.
    let remote = test
        .project
        .read_with(cx, |project, _| project.remote_client().unwrap());
    remote
        .update(cx, |remote, cx| remote.simulate_disconnect(cx))
        .detach();
    run(cx, server_cx);
    assert_eq!(assistant_text(&thread, cx), "onetwo");
    assert!(has_pending_permission(&thread, cx), "the question is shown");

    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(
            acp::ToolCallId::new("tc-1"),
            SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ),
            cx,
        )
    });
    run(cx, server_cx);
    let answer = test
        .script
        .received
        .borrow()
        .iter()
        .find(|message| message["id"] == json!(900))
        .cloned()
        .expect("the answer reached the agent");
    assert_eq!(answer["result"]["outcome"]["optionId"], json!("allow"));

    let response = turn.await.unwrap().unwrap();
    assert_eq!(
        response.stop_reason,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    );
    assert_eq!(assistant_text(&thread, cx), "onetwothree", "each update exactly once");
    assert_eq!(
        thread.read_with(cx, |thread, _| thread.status()),
        acp_thread::ThreadStatus::Idle
    );
    assert_eq!(test.script.spawned.get(), 1, "one agent throughout");
    assert!(
        !test
            .script
            .received
            .borrow()
            .iter()
            .any(|message| message["method"] == json!("session/cancel")),
        "nothing cancelled the turn"
    );

    // Two more reconnects, to the same server, change nothing.
    for _ in 0..2 {
        remote
            .update(cx, |remote, cx| remote.simulate_disconnect(cx))
            .detach();
        run(cx, server_cx);
    }
    assert_eq!(assistant_text(&thread, cx), "onetwothree");
}

#[gpui::test]
async fn test_a_fresh_client_attaches_to_a_running_turn(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let test = init_relay_test(cx, server_cx, Some(dir.path())).await;
    // Long enough that catching up has to read the log on disk.
    test.script.long_turn.set(5000);
    let connection = connect(&test, cx).await;
    let thread = cx
        .update(|cx| {
            connection.clone().new_session(
                test.project.clone(),
                PathList::new(&[Path::new(path!("/project"))]),
                cx,
            )
        })
        .await
        .unwrap();
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
    let (gate_tx, gate_rx) = oneshot::channel();
    *test.script.gate.borrow_mut() = Some(gate_rx);
    let turn = thread.update(cx, |thread, cx| thread.send_raw("go", cx));
    let _turn = cx.background_spawn(turn);
    run(cx, server_cx);

    // Zed restarts: the old connection and its thread are gone. The agent
    // keeps working on the box and asks its question while nobody is there.
    drop(thread);
    drop(connection);
    run(cx, server_cx);
    gate_tx.send(()).unwrap();
    run(cx, server_cx);
    assert_eq!(test.relay.sessions().len(), 1, "the session outlived its client");
    assert!(test.relay.sessions()[0].running);

    // Reopening the thread attaches rather than loading it again.
    let connection = connect(&test, cx).await;
    let thread = cx
        .update(|cx| {
            connection.clone().load_session(
                session_id,
                test.project.clone(),
                PathList::new(&[Path::new(path!("/project"))]),
                None,
                cx,
            )
        })
        .await
        .unwrap();
    run(cx, server_cx);
    let text = assistant_text(&thread, cx);
    assert!(text.starts_with("one....."), "the history is there");
    assert!(text.ends_with("two"));
    assert_eq!(text.matches('.').count(), 5000, "every update exactly once");
    assert!(has_pending_permission(&thread, cx));
    assert_eq!(
        thread.read_with(cx, |thread, _| thread.status()),
        acp_thread::ThreadStatus::Generating,
        "the running turn shows as running"
    );
    assert!(
        !test
            .script
            .received
            .borrow()
            .iter()
            .any(|message| message["method"] == json!("session/load")),
        "a live session is never loaded twice"
    );

    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(
            acp::ToolCallId::new("tc-1"),
            SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ),
            cx,
        )
    });
    run(cx, server_cx);
    assert!(assistant_text(&thread, cx).ends_with("twothree"));
    assert_eq!(
        thread.read_with(cx, |thread, _| thread.status()),
        acp_thread::ThreadStatus::Idle,
        "the turn this client did not start ends here too"
    );
    assert_eq!(test.script.spawned.get(), 1);
}

#[gpui::test]
async fn test_the_relay_setting_falls_back_to_stdio(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let test = init_relay_test(cx, server_cx, None).await;
    cx.executor().allow_parking();
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.agent.get_or_insert_default().remote_relay = Some(false);
            });
        });
        assert!(!AgentSettings::get_global(cx).remote_relay);
    });
    let store = test
        .project
        .read_with(cx, |project, _| project.agent_server_store().clone());
    // The stdio path tries to run the command through the remote transport;
    // the relay is never asked.
    agent_servers::AcpConnection::stdio(
        AgentId::new("fake"),
        test.project.clone(),
        AgentServerCommand {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            env: None,
        },
        store.downgrade(),
        None,
        HashMap::default(),
        &mut cx.to_async(),
    )
    .await
    .ok();
    run(cx, server_cx);
    assert_eq!(test.script.spawned.get(), 0, "the relay was never asked");
}
