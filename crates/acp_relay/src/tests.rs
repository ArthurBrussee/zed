use crate::frame;
use crate::state::{AgentCommand, AgentSpec, LinkFrame, LinkId, Output, ProcessId, RelayState};
use collections::{BTreeMap, HashMap};
use serde_json::{Value, json};

/// The relay with its outputs collected, standing in for the driver.
struct Harness {
    state: RelayState,
    spawned: Vec<(ProcessId, AgentSpec)>,
    killed: Vec<ProcessId>,
    to_agent: Vec<(ProcessId, Value)>,
    to_link: HashMap<LinkId, Vec<(u64, LinkFrame)>>,
    fs: Vec<(u64, String, Value)>,
    exited: bool,
    cseq: HashMap<LinkId, u64>,
}

impl Harness {
    fn new(dir: Option<&std::path::Path>) -> Self {
        Self {
            state: RelayState::new(dir.map(|dir| dir.to_path_buf())),
            spawned: Vec::new(),
            killed: Vec::new(),
            to_agent: Vec::new(),
            to_link: HashMap::default(),
            fs: Vec::new(),
            exited: false,
            cseq: HashMap::default(),
        }
    }

    fn collect(&mut self) {
        for output in self.state.take_outputs() {
            match output {
                Output::ToLink { link, lseq, frame } => {
                    self.to_link.entry(link).or_default().push((lseq, frame))
                }
                Output::ToAgent { process, line } => self
                    .to_agent
                    .push((process, serde_json::from_str(&line).unwrap())),
                Output::Spawn { process, spec } => self.spawned.push((process, spec)),
                Output::Kill { process } => self.killed.push(process),
                Output::ServeFs {
                    request,
                    method,
                    params,
                } => self.fs.push((request, method, params)),
                Output::Exit => self.exited = true,
            }
        }
    }

    fn open(&mut self) -> LinkId {
        let link = self.state.open_link(command()).unwrap();
        self.collect();
        link
    }

    fn client(&mut self, link: LinkId, message: Value) {
        let cseq = self.cseq.entry(link).or_insert(0);
        *cseq += 1;
        let cseq = *cseq;
        self.state.client_line(link, cseq, message.to_string());
        self.collect();
    }

    fn agent(&mut self, process: ProcessId, message: Value) {
        self.state.agent_line(process, message.to_string());
        self.collect();
    }

    /// What the agent was last sent, consumed.
    fn take_agent(&mut self) -> Vec<(ProcessId, Value)> {
        std::mem::take(&mut self.to_agent)
    }

    fn take_link(&mut self, link: LinkId) -> Vec<(u64, Value)> {
        self.to_link
            .remove(&link)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(lseq, frame)| match frame {
                LinkFrame::Line(line) => Some((lseq, serde_json::from_str(&line).unwrap())),
                _ => None,
            })
            .collect()
    }

    /// Opens a link, initializes, and creates a session, answering as the
    /// agent would. Returns the link, the agent process and the session id.
    fn start_session(&mut self) -> (LinkId, ProcessId, String) {
        let link = self.open();
        self.client(link, request(0, "initialize", json!({ "protocolVersion": 1 })));
        let (process, spec) = self.spawned.last().cloned().unwrap();
        assert_eq!(spec, AgentSpec::Command(command()));
        let init = self.take_agent();
        assert_eq!(init.len(), 1);
        self.agent(process, response(&init[0].1["id"], json!({ "protocolVersion": 1 })));
        self.client(link, request(1, "session/new", json!({ "cwd": "/box/project" })));
        let new = self.take_agent();
        self.agent(
            process,
            response(&new[0].1["id"], json!({ "sessionId": "s1", "modes": null })),
        );
        let replies = self.take_link(link);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1].1["id"], json!(1));
        assert_eq!(replies[1].1["result"]["sessionId"], json!("s1"));
        (link, process, "s1".into())
    }
}

fn command() -> AgentCommand {
    AgentCommand {
        name: "claude-acp".into(),
        path: "/usr/bin/agent".into(),
        args: vec!["--acp".into()],
        env: BTreeMap::new(),
        cwd: Some("/box/project".into()),
    }
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn update(session: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session,
            "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } },
        },
    })
}

fn texts(frames: &[(u64, Value)]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|(_, frame)| {
            frame["params"]["update"]["content"]["text"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect()
}

#[test]
fn a_turn_finishes_while_detached_and_reattaching_catches_up_exactly_once() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();

    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    let prompt = relay.take_agent();
    let prompt_id = prompt[0].1["id"].clone();
    assert_ne!(prompt_id, json!(2), "the relay owns the agent's id space");

    relay.agent(process, update(&session, "one"));
    let before = relay.take_link(link);
    assert_eq!(texts(&before), ["one"]);
    let last_seen = before.last().unwrap().0;

    // The connection drops mid-turn. Nothing is cancelled.
    relay.state.detach_link(link);
    relay.collect();
    assert!(relay.take_agent().is_empty());

    relay.agent(process, update(&session, "two"));
    relay.agent(
        process,
        request(
            40,
            "session/request_permission",
            json!({ "sessionId": session, "options": [] }),
        ),
    );
    relay.agent(process, update(&session, "three"));
    assert!(relay.take_link(link).is_empty(), "nothing goes out while detached");

    let next_cseq = relay.state.attach_link(link, last_seen).unwrap();
    relay.collect();
    assert_eq!(next_cseq, 4, "initialize, session/new and the prompt arrived");
    let caught_up = relay.take_link(link);
    assert_eq!(texts(&caught_up), ["two", "three"]);
    let permission = caught_up
        .iter()
        .find(|(_, frame)| frame["method"] == "session/request_permission")
        .expect("the permission prompt reaches the client")
        .1
        .clone();
    let lseqs = caught_up.iter().map(|(lseq, _)| *lseq).collect::<Vec<_>>();
    assert!(lseqs.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert_eq!(lseqs[0], last_seen + 1);

    // The answer reaches the agent under the agent's own id.
    relay.client(
        link,
        response(&permission["id"], json!({ "outcome": { "outcome": "selected" } })),
    );
    let answer = relay.take_agent();
    assert_eq!(answer, [(process, response(&json!(40), json!({ "outcome": { "outcome": "selected" } })))]);

    relay.agent(process, response(&prompt_id, json!({ "stopReason": "end_turn" })));
    let done = relay.take_link(link);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].1["id"], json!(2));
    assert_eq!(done[0].1["result"]["stopReason"], json!("end_turn"));

    // A second attach from the same place sends nothing twice.
    let last_seen = done[0].0;
    relay.state.detach_link(link);
    relay.state.attach_link(link, last_seen).unwrap();
    relay.collect();
    assert!(relay.take_link(link).is_empty());
}

#[test]
fn a_response_that_arrives_while_detached_is_kept() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    let prompt_id = relay.take_agent()[0].1["id"].clone();
    relay.state.detach_link(link);
    relay.agent(process, response(&prompt_id, json!({ "stopReason": "end_turn" })));
    relay.state.attach_link(link, 2).unwrap();
    relay.collect();
    let frames = relay.take_link(link);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1["id"], json!(2));
}

#[test]
fn a_fresh_client_attaches_to_a_live_session_without_loading_it_again() {
    let mut relay = Harness::new(None);
    let (old_link, process, session) = relay.start_session();
    relay.client(
        old_link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    relay.take_agent();
    relay.agent(process, update(&session, "history"));
    relay.agent(
        process,
        request(41, "session/request_permission", json!({ "sessionId": session })),
    );
    relay.state.close_link(old_link);
    relay.collect();
    assert!(relay.killed.is_empty(), "the agent outlives the client");

    let link = relay.open();
    relay.client(link, request(0, "initialize", json!({ "protocolVersion": 1 })));
    assert_eq!(relay.spawned.len(), 1, "initialize is answered from the cache");
    relay.client(
        link,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    assert!(
        relay.take_agent().is_empty(),
        "a live session is never loaded twice"
    );
    let frames = relay.take_link(link);
    assert_eq!(frames[0].1["id"], json!(0));
    assert_eq!(texts(&frames[1..2]), ["history"]);
    assert_eq!(frames[2].1["id"], json!(1));
    assert!(frames[2].1["result"].get("sessionId").is_none());
    assert_eq!(frames[3].1["method"], json!("session/request_permission"));
    assert_eq!(frames[4].1["method"], json!(frame::STATE_UPDATE));
    assert_eq!(frames[4].1["params"]["state"], json!("running"));
    assert_eq!(frames.len(), 5);

    // The turn the old client started ends; this client hears it as state.
    relay.client(
        link,
        response(&frames[3].1["id"], json!({ "outcome": { "outcome": "cancelled" } })),
    );
    let prompt = relay.take_agent();
    assert_eq!(prompt[0].1["id"], json!(41));
    let summary = relay.state.sessions();
    assert!(summary[0].running);
}

#[test]
fn replay_past_the_memory_tail_reads_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut relay = Harness::new(Some(dir.path()));
    let (old_link, process, session) = relay.start_session();
    for index in 0..5000 {
        relay.agent(process, update(&session, &index.to_string()));
    }
    relay.state.close_link(old_link);
    relay.collect();
    let link = relay.open();
    relay.client(
        link,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    let frames = relay.take_link(link);
    let replayed = texts(&frames);
    assert_eq!(replayed.len(), 5000);
    assert_eq!(replayed[0], "0");
    assert_eq!(replayed[4999], "4999");
}

#[test]
fn file_requests_are_answered_from_the_box_while_detached() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.state.detach_link(link);
    relay.agent(
        process,
        request(
            50,
            "fs/read_text_file",
            json!({ "sessionId": session, "path": "/box/project/a.rs" }),
        ),
    );
    assert_eq!(relay.fs.len(), 1);
    let (request_id, method, params) = relay.fs.pop().unwrap();
    assert_eq!(method, "fs/read_text_file");
    assert_eq!(params["path"], json!("/box/project/a.rs"));
    relay.state.fs_served(request_id, Ok(json!({ "content": "fn a() {}" })));
    relay.collect();
    assert_eq!(
        relay.take_agent(),
        [(process, response(&json!(50), json!({ "content": "fn a() {}" })))]
    );

    // On reattach the request is not shown again as something to answer.
    relay.state.attach_link(link, 2).unwrap();
    relay.collect();
    assert!(relay.take_link(link).is_empty());
    let link_2 = relay.open();
    relay.client(
        link_2,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/" })),
    );
    let frames = relay.take_link(link_2);
    assert!(
        frames
            .iter()
            .all(|(_, frame)| frame["method"] != "fs/read_text_file")
    );
}

#[test]
fn subagent_frames_follow_the_session_that_launched_them() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.agent(
        process,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session,
                "update": { "sessionUpdate": "subagent_update", "sessionId": "child", "state": { "state": "running" } },
            },
        }),
    );
    relay.agent(process, update("child", "from the child"));
    relay.agent(
        process,
        request(60, "session/request_permission", json!({ "sessionId": "child" })),
    );
    let frames = relay.take_link(link);
    assert_eq!(texts(&frames), ["from the child"]);
    assert_eq!(frames.len(), 3);
    relay.client(link, response(&frames[2].1["id"], json!({})));
    assert_eq!(relay.take_agent()[0].1["id"], json!(60));
}

#[test]
fn closing_the_last_session_ends_its_agent() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.state.close_link(link);
    relay.collect();
    assert!(relay.killed.is_empty());

    let link = relay.open();
    relay.client(
        link,
        request(1, "session/close", json!({ "sessionId": session })),
    );
    let close = relay.take_agent();
    assert_eq!(close[0].0, process);
    relay.agent(process, response(&close[0].1["id"], json!({})));
    relay.state.close_link(link);
    relay.collect();
    assert_eq!(relay.killed, [process]);
}

#[test]
fn a_draining_relay_waits_for_the_turn_then_hands_over() {
    let mut old = Harness::new(None);
    let (link, process, session) = old.start_session();
    old.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    let prompt_id = old.take_agent()[0].1["id"].clone();
    old.state.detach_link(link);

    old.state.drain();
    old.collect();
    assert!(old.state.open_link(command()).is_err(), "no new sessions");
    assert!(old.killed.is_empty(), "the running turn is left to finish");

    // The newer relay adopts the session through an adopting link.
    let mut new = Harness::new(None);
    new.state
        .add_adoptable("/relay-old.sock".into(), old.state.sessions());
    let new_link = new.open();
    new.client(new_link, request(0, "initialize", json!({ "protocolVersion": 1 })));
    let (home, _) = new.spawned[0].clone();
    let init = new.take_agent();
    new.agent(home, response(&init[0].1["id"], json!({ "protocolVersion": 1 })));
    new.take_link(new_link);
    new.client(
        new_link,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    let (adopted, spec) = new.spawned.last().cloned().unwrap();
    assert_eq!(
        spec,
        AgentSpec::Adopt {
            socket: "/relay-old.sock".into()
        }
    );
    let load = new.take_agent();
    assert_eq!(load[0].0, adopted);

    let adopting = old.state.open_adopting_link();
    old.state
        .client_line(adopting, 1, load[0].1.to_string());
    old.collect();
    let replay = old.take_link(adopting);
    assert!(old.take_agent().is_empty());
    for (_, frame) in &replay {
        new.agent(adopted, frame.clone());
    }
    let loaded = new.take_link(new_link);
    assert_eq!(loaded.last().unwrap().1["params"]["state"], json!("running"));

    // The turn ends; the old relay ends its agent, the new one resumes.
    old.agent(process, response(&prompt_id, json!({ "stopReason": "end_turn" })));
    assert_eq!(old.killed, [process]);
    assert!(old.exited);
    new.state.agent_exited(adopted, None);
    new.collect();
    let (resumed, spec) = new.spawned.last().cloned().unwrap();
    assert!(matches!(spec, AgentSpec::Command(_)));
    let sent = new.take_agent();
    assert_eq!(sent[0].1["method"], json!("initialize"));
    new.agent(resumed, response(&sent[0].1["id"], json!({ "protocolVersion": 1 })));
    let sent = new.take_agent();
    assert_eq!(sent[0].1["method"], json!("session/resume"));
    assert_eq!(sent[0].1["params"]["sessionId"], json!(session));
    new.agent(resumed, response(&sent[0].1["id"], json!({})));

    // Prompts for the session now reach the resumed agent.
    new.client(
        new_link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    assert_eq!(new.take_agent()[0].0, resumed);
}

#[test]
fn an_agent_that_exits_ends_its_sessions_with_a_reason() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    relay.take_agent();
    relay.state.agent_exited(process, Some(1));
    relay.collect();
    let frames = relay.take_link(link);
    let state = frames
        .iter()
        .find(|(_, frame)| frame["method"] == json!(frame::STATE_UPDATE))
        .unwrap();
    assert_eq!(state.1["params"]["state"], json!("idle"));
    assert!(state.1["params"]["error"].is_string());
    assert!(frames.iter().any(|(_, frame)| frame["id"] == json!(2)));
    assert!(relay.to_link.is_empty() || relay.take_link(link).is_empty());
    assert!(
        relay.state.sessions().is_empty(),
        "a dead agent's sessions are gone"
    );
}

mod daemon {
    use crate::daemon::{DaemonClient, FrameHandler, ProcessSpawner, serve_connection};
    use crate::relay::{Relay, TaskSpawner};
    use crate::state::{AgentCommand, LinkFrame};
    use collections::BTreeMap;
    use futures::{StreamExt as _, channel::mpsc, executor::LocalPool, task::LocalSpawnExt as _};
    use serde_json::{Value, json};
    use std::{cell::RefCell, rc::Rc};

    /// A server's connection to the relay, as a pair of in-memory pipes.
    fn connect(
        relay: &Rc<Relay>,
        spawn: &TaskSpawner,
        frames: Rc<RefCell<Vec<(u64, u64, LinkFrame)>>>,
    ) -> Rc<DaemonClient> {
        let (to_daemon, from_client) = mpsc::unbounded::<String>();
        let (to_client, from_daemon) = mpsc::unbounded::<String>();
        spawn(Box::pin(serve_connection(
            relay.clone(),
            from_client.boxed_local(),
            to_client,
        )));
        let on_frame: FrameHandler = Rc::new(move |link, lseq, frame| {
            frames.borrow_mut().push((link, lseq, frame));
        });
        DaemonClient::new(from_daemon.boxed_local(), to_daemon, spawn, on_frame)
    }

    fn lines(frames: &RefCell<Vec<(u64, u64, LinkFrame)>>) -> Vec<(u64, Value)> {
        frames
            .borrow_mut()
            .drain(..)
            .filter_map(|(_, lseq, frame)| match frame {
                LinkFrame::Line(line) => Some((lseq, serde_json::from_str(&line).unwrap())),
                _ => None,
            })
            .collect()
    }

    /// An agent that answers every request with its own params, so what
    /// reaches it can be read back.
    #[cfg(unix)]
    fn echo_agent() -> AgentCommand {
        AgentCommand {
            name: "echo".into(),
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"while IFS= read -r line; do id=$(printf '%s' "$line" | sed -nE 's/.*"id":([0-9]+).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":true,"sessionId":"s-%s"}}\n' "$id" "$id"; done"#.into(),
            ],
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_survives_its_server_being_replaced() {
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let spawn: TaskSpawner = Rc::new(move |future| {
            spawner.spawn_local(future).ok();
        });
        let relay = Relay::new(
            None,
            Rc::new(ProcessSpawner {
                spawn_task: spawn.clone(),
            }),
            spawn.clone(),
        );
        let frames = Rc::new(RefCell::new(Vec::new()));

        // The first server opens a link and gets an answer from a real process.
        let first = connect(&relay, &spawn, frames.clone());
        let link = pool.run_until({
            let first = first.clone();
            async move { first.open(echo_agent()).await.unwrap() }
        });
        first.send(link, 1, json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}).to_string());
        let answered = pool.run_until(async {
            loop {
                let got = lines(&frames);
                if !got.is_empty() {
                    break got;
                }
                smol::future::yield_now().await;
            }
        });
        assert_eq!(answered[0].1["id"], json!(7));
        let last = answered[0].0;

        // That server goes away; the link is detached, not closed.
        drop(first);
        pool.run_until_stalled();

        // A new server attaches where the client left off.
        let second = connect(&relay, &spawn, frames.clone());
        let next_cseq = pool.run_until({
            let second = second.clone();
            async move { second.attach(link, last).await.unwrap() }
        });
        assert_eq!(next_cseq, 2);
        second.send(link, 2, json!({"jsonrpc":"2.0","id":8,"method":"session/new","params":{}}).to_string());
        let answered = pool.run_until(async {
            loop {
                let got = lines(&frames);
                if !got.is_empty() {
                    break got;
                }
                smol::future::yield_now().await;
            }
        });
        assert_eq!(answered[0].0, last + 1, "numbering carries on across servers");
        assert_eq!(answered[0].1["id"], json!(8));
        assert_eq!(relay.sessions().len(), 1, "the session the agent named is live");

        let listed = pool.run_until({
            let second = second.clone();
            async move { second.list().await.unwrap() }
        });
        assert_eq!(listed.len(), 1);
        assert!(listed[0].attached);
    }

    /// An agent that answers at once, except that it holds a prompt until a
    /// `_test/release` notification arrives.
    #[cfg(unix)]
    fn holding_agent() -> AgentCommand {
        AgentCommand {
            name: "holding".into(),
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"held=; while IFS= read -r line; do id=$(printf '%s' "$line" | sed -nE 's/.*"id":([0-9]+).*/\1/p'); case "$line" in *'"session/prompt"'*) held=$id ;; *'"_test/release"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$held" ;; *) [ -n "$id" ] && printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s-%s"}}\n' "$id" "$id" ;; esac; done"#.into(),
            ],
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[cfg(unix)]
    fn serve_socket(relay: Rc<Relay>, socket: &std::path::Path, spawn: &TaskSpawner) {
        let listener = smol::net::unix::UnixListener::bind(socket).unwrap();
        let tasks = spawn.clone();
        spawn(Box::pin(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let reader = stream.clone();
                let mut writer = stream;
                let (outgoing, mut outgoing_rx) = mpsc::unbounded::<String>();
                tasks(Box::pin(async move {
                    use futures::AsyncWriteExt as _;
                    while let Some(mut line) = outgoing_rx.next().await {
                        line.push('\n');
                        if writer.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }));
                use futures::AsyncBufReadExt as _;
                let incoming = futures::io::BufReader::new(reader)
                    .lines()
                    .filter_map(|line| futures::future::ready(line.ok()))
                    .boxed_local();
                tasks(Box::pin(serve_connection(relay.clone(), incoming, outgoing)));
            }
        }));
    }

    #[cfg(unix)]
    fn wait_for(
        pool: &mut LocalPool,
        frames: &RefCell<Vec<(u64, u64, LinkFrame)>>,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        pool.run_until(async {
            for _ in 0..200_000 {
                let found = frames.borrow().iter().find_map(|(_, _, frame)| match frame {
                    LinkFrame::Line(line) => {
                        let value: Value = serde_json::from_str(line).unwrap();
                        predicate(&value).then_some(value)
                    }
                    _ => None,
                });
                if let Some(found) = found {
                    frames.borrow_mut().clear();
                    return found;
                }
                smol::future::yield_now().await;
            }
            panic!("timed out; saw {:?}", frames.borrow());
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_newer_relay_takes_over_a_running_session_from_an_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let old_socket = dir.path().join("relay-old.sock");
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let spawn: TaskSpawner = Rc::new(move |future| {
            spawner.spawn_local(future).ok();
        });
        let relay = |spawn: &TaskSpawner| {
            Relay::new(
                None,
                Rc::new(ProcessSpawner {
                    spawn_task: spawn.clone(),
                }),
                spawn.clone(),
            )
        };

        // The older build's relay runs a session with a turn in flight.
        let old = relay(&spawn);
        let old_exited = Rc::new(std::cell::Cell::new(false));
        old.on_exit({
            let old_exited = old_exited.clone();
            move || old_exited.set(true)
        });
        serve_socket(old.clone(), &old_socket, &spawn);
        let old_frames = Rc::new(RefCell::new(Vec::new()));
        let old_server = connect(&old, &spawn, old_frames.clone());
        let old_link = pool.run_until({
            let old_server = old_server.clone();
            async move { old_server.open(holding_agent()).await.unwrap() }
        });
        old_server.send(old_link, 1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).to_string());
        wait_for(&mut pool, &old_frames, |frame| frame["id"] == json!(1));
        old_server.send(old_link, 2, json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/"}}).to_string());
        let session = wait_for(&mut pool, &old_frames, |frame| frame["id"] == json!(2))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        old_server.send(old_link, 3, json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[]}}).to_string());
        drop(old_server);
        pool.run_until_stalled();

        // The newer relay starts: it learns the older one's sessions and asks
        // it to drain. The turn in flight keeps the older one going.
        let new = relay(&spawn);
        let listing = pool.run_until({
            let socket = old_socket.clone();
            let spawn = spawn.clone();
            async move {
                let client = crate::daemon::connect(&socket, &spawn, Rc::new(|_, _, _| {}))
                    .await
                    .unwrap();
                let sessions = client.list().await.unwrap();
                client.drain().await.unwrap();
                sessions
            }
        });
        assert_eq!(listing.len(), 1);
        assert!(listing[0].running);
        new.add_adoptable(old_socket.clone(), listing);
        pool.run_until_stalled();
        assert!(!old_exited.get(), "a running turn is left to finish");

        // A client of the newer relay opens the session: it is reached
        // through the older relay, and shows as running.
        let frames = Rc::new(RefCell::new(Vec::new()));
        let server = connect(&new, &spawn, frames.clone());
        let link = pool.run_until({
            let server = server.clone();
            async move { server.open(holding_agent()).await.unwrap() }
        });
        server.send(link, 1, json!({"jsonrpc":"2.0","id":10,"method":"initialize","params":{}}).to_string());
        wait_for(&mut pool, &frames, |frame| frame["id"] == json!(10));
        server.send(link, 2, json!({"jsonrpc":"2.0","id":11,"method":"session/load","params":{"sessionId":session,"cwd":"/"}}).to_string());
        let state = wait_for(&mut pool, &frames, |frame| {
            frame["method"] == json!(crate::frame::STATE_UPDATE)
        });
        assert_eq!(state["params"]["state"], json!("running"));

        // The turn finishes; the older relay ends its agent and exits, and
        // the newer one resumes the session in an agent of its own.
        server.send(link, 3, json!({"jsonrpc":"2.0","method":"_test/release","params":{"sessionId":session}}).to_string());
        pool.run_until({
            let old_exited = old_exited.clone();
            async move {
                while !old_exited.get() {
                    smol::future::yield_now().await;
                }
            }
        });
        pool.run_until(async {
            for _ in 0..50_000 {
                smol::future::yield_now().await;
            }
        });
        server.send(link, 4, json!({"jsonrpc":"2.0","id":12,"method":"session/prompt","params":{"sessionId":session,"prompt":[]}}).to_string());
        server.send(link, 5, json!({"jsonrpc":"2.0","method":"_test/release","params":{"sessionId":session}}).to_string());
        let done = wait_for(&mut pool, &frames, |frame| frame["id"] == json!(12));
        assert_eq!(done["result"]["stopReason"], json!("end_turn"), "{done}");
        assert_eq!(new.sessions().len(), 1, "the session lives on in the newer relay");
    }
}
