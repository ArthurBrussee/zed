use crate::frame;
use crate::state::{
    AgentCommand, AgentSpec, DRAIN_QUIET, IDLE_TIMEOUT, LinkFrame, LinkId, Output, ProcessId,
    RelayState,
};
use collections::{BTreeMap, HashMap};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The relay with its outputs collected, standing in for the driver.
struct Harness {
    state: RelayState,
    spawned: Vec<(ProcessId, AgentSpec)>,
    killed: Vec<ProcessId>,
    to_agent: Vec<(ProcessId, Value)>,
    to_link: HashMap<LinkId, Vec<(u64, LinkFrame)>>,
    fs: Vec<(u64, String, Value)>,
    later: Vec<(ProcessId, String)>,
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
            later: Vec::new(),
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
                Output::SendLater { process, line, .. } => self.later.push((process, line)),
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

    // The turn ends; the old relay ends its agent, the new one resumes the
    // session in the agent it already runs.
    old.agent(process, response(&prompt_id, json!({ "stopReason": "end_turn" })));
    assert_eq!(old.killed, [process]);
    assert!(old.exited);
    new.state.agent_exited(adopted, None);
    new.collect();
    let resumed = home;
    assert_eq!(new.spawned.len(), 2);
    let sent = new.take_agent();
    assert_eq!(sent[0].0, resumed);
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
fn links_for_the_same_agent_share_one_process() {
    let mut relay = Harness::new(None);
    let (first, process, session) = relay.start_session();

    // A second link for the same agent is answered from the first's
    // initialize and runs its session in the same process.
    let second = relay.open();
    relay.client(second, request(0, "initialize", json!({ "protocolVersion": 1 })));
    assert_eq!(relay.take_link(second)[0].1["result"]["protocolVersion"], json!(1));
    relay.client(second, request(1, "session/new", json!({ "cwd": "/box/other" })));
    assert_eq!(relay.spawned.len(), 1, "one adapter for both links");
    let new = relay.take_agent();
    assert_eq!(new[0].0, process);
    relay.agent(process, response(&new[0].1["id"], json!({ "sessionId": "s2" })));
    let replies = relay.take_link(second);
    assert_eq!(replies[0].1["id"], json!(1));
    assert_eq!(replies[0].1["result"]["sessionId"], json!("s2"));
    assert!(relay.take_link(first).is_empty());

    // Both links number their requests from 1; each answer finds its own.
    relay.client(
        first,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    relay.client(
        second,
        request(2, "session/prompt", json!({ "sessionId": "s2", "prompt": [] })),
    );
    let prompts = relay.take_agent();
    assert_eq!(prompts.len(), 2);
    assert_ne!(prompts[0].1["id"], prompts[1].1["id"]);
    relay.agent(process, update("s2", "for the second"));
    relay.agent(process, response(&prompts[1].1["id"], json!({ "stopReason": "end_turn" })));
    let to_second = relay.take_link(second);
    assert_eq!(texts(&to_second), ["for the second"]);
    assert_eq!(to_second.last().unwrap().1["id"], json!(2));
    assert!(relay.take_link(first).is_empty());
    relay.agent(process, response(&prompts[0].1["id"], json!({ "stopReason": "end_turn" })));
    assert_eq!(relay.take_link(first)[0].1["id"], json!(2));

    // Closing one link leaves the other's agent running.
    relay.state.close_link(first);
    relay.collect();
    assert!(relay.killed.is_empty());

    // A different agent gets a process of its own.
    let other = relay.state.open_link(AgentCommand {
        name: "codex-acp".into(),
        path: "/usr/bin/codex-acp".into(),
        ..command()
    });
    let other = other.unwrap();
    relay.client(other, request(0, "initialize", json!({ "protocolVersion": 1 })));
    assert_eq!(relay.spawned.len(), 2);
    assert_ne!(relay.spawned[1].0, process);
}

#[test]
fn a_shared_agent_that_exits_ends_every_session_and_tells_every_link() {
    let mut relay = Harness::new(None);
    let (first, process, _) = relay.start_session();
    let second = relay.open();
    relay.client(second, request(0, "initialize", json!({ "protocolVersion": 1 })));
    relay.client(second, request(1, "session/new", json!({ "cwd": "/box/other" })));
    let new = relay.take_agent();
    relay.agent(process, response(&new[0].1["id"], json!({ "sessionId": "s2" })));
    relay.take_link(second);
    relay.client(
        second,
        request(2, "session/prompt", json!({ "sessionId": "s2", "prompt": [] })),
    );
    relay.take_agent();

    relay.state.agent_exited(process, Some(1));
    relay.collect();
    assert!(relay.state.sessions().is_empty());
    for link in [first, second] {
        assert!(
            relay.to_link[&link]
                .iter()
                .any(|(_, frame)| *frame == LinkFrame::Exited(Some(1)))
        );
    }
    let to_second = relay.take_link(second);
    assert!(
        to_second
            .iter()
            .any(|(_, frame)| frame["id"] == json!(2) && frame["error"].is_object())
    );

    // The next session starts a fresh process.
    relay.client(second, request(3, "session/new", json!({ "cwd": "/box/other" })));
    assert_eq!(relay.spawned.len(), 2);
    let (fresh, _) = relay.spawned[1].clone();
    let sent = relay.take_agent();
    assert_eq!(sent[0].0, fresh);
    assert_eq!(sent[0].1["method"], json!("initialize"));
}

#[test]
fn an_idle_session_is_closed_after_the_timeout_and_not_before() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.agent(process, update(&session, "history"));

    // The sweep that sees the update starts nothing; the next one starts
    // the idle clock.
    let start = Instant::now();
    relay.state.close_idle(start);
    relay.state.close_idle(start + Duration::from_secs(60));
    let quiet_from = start + Duration::from_secs(60);
    relay.state.close_idle(quiet_from + IDLE_TIMEOUT - Duration::from_secs(1));
    relay.collect();
    assert!(relay.take_agent().is_empty(), "not before the timeout");

    relay.state.close_idle(quiet_from + IDLE_TIMEOUT);
    relay.collect();
    let close = relay.take_agent();
    assert_eq!(close.len(), 1);
    assert_eq!(close[0].0, process);
    assert_eq!(close[0].1["method"], json!("session/close"));
    assert_eq!(close[0].1["params"]["sessionId"], json!(session));
    relay.agent(process, response(&close[0].1["id"], json!({})));
    assert!(relay.take_link(link).iter().all(|(_, frame)| frame["id"].is_null()));
    assert_eq!(relay.state.sessions().len(), 1, "the session and its log stay");

    // With no session open and no link left, the adapter goes too.
    relay.state.close_link(link);
    relay.collect();
    assert_eq!(relay.killed, [process]);
}

#[test]
fn a_prompt_to_a_closed_session_resumes_it_first() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.state.set_idle_timeout(Duration::ZERO);
    let now = Instant::now();
    relay.state.close_idle(now);
    relay.state.close_idle(now);
    relay.collect();
    let close = relay.take_agent();
    assert_eq!(close[0].1["method"], json!("session/close"));

    // A prompt that arrives while the close is in flight waits for it.
    let prompt = json!([{ "type": "text", "text": "hello" }]);
    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": prompt })),
    );
    assert!(relay.take_agent().is_empty());
    relay.agent(process, response(&close[0].1["id"], json!({})));

    let resume = relay.take_agent();
    assert_eq!(resume.len(), 1, "the prompt waits for the resume");
    assert_eq!(resume[0].0, process);
    assert_eq!(resume[0].1["method"], json!("session/resume"));
    assert_eq!(resume[0].1["params"]["sessionId"], json!(session));
    assert_eq!(resume[0].1["params"]["cwd"], json!("/box/project"));
    relay.agent(process, response(&resume[0].1["id"], json!({})));

    let forwarded = relay.take_agent();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].1["method"], json!("session/prompt"));
    assert_eq!(forwarded[0].1["params"]["prompt"], prompt);
    relay.agent(process, response(&forwarded[0].1["id"], json!({ "stopReason": "end_turn" })));
    let done = relay.take_link(link);
    assert_eq!(done.last().unwrap().1["id"], json!(2));
    assert_eq!(done.last().unwrap().1["result"]["stopReason"], json!("end_turn"));
}

#[test]
fn a_load_of_a_closed_session_resumes_it_in_a_fresh_adapter() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.agent(process, update(&session, "history"));
    relay.state.close_link(link);
    relay.state.set_idle_timeout(Duration::ZERO);
    let now = Instant::now();
    relay.state.close_idle(now);
    relay.state.close_idle(now);
    relay.collect();
    let close = relay.take_agent();
    relay.agent(process, response(&close[0].1["id"], json!({})));
    assert_eq!(relay.killed, [process]);

    let link = relay.open();
    relay.client(link, request(0, "initialize", json!({ "protocolVersion": 1 })));
    relay.client(
        link,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    let (fresh, _) = relay.spawned.last().cloned().unwrap();
    assert_ne!(fresh, process);
    let init = relay.take_agent();
    assert_eq!(init[0].1["method"], json!("initialize"));
    relay.agent(fresh, response(&init[0].1["id"], json!({ "protocolVersion": 1 })));
    let resume = relay.take_agent();
    assert_eq!(resume[0].1["method"], json!("session/resume"));
    relay.agent(fresh, response(&resume[0].1["id"], json!({})));
    assert!(relay.take_agent().is_empty(), "the load is served from the log");
    let frames = relay.take_link(link);
    assert_eq!(texts(&frames), ["history"]);
    assert!(frames.iter().any(|(_, frame)| frame["id"] == json!(1)));
}

#[test]
fn a_session_with_a_turn_or_background_work_is_not_closed() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.state.set_idle_timeout(Duration::from_secs(60));
    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    let prompt = relay.take_agent();
    let start = Instant::now();
    for minutes in [0, 1, 180] {
        relay.state.close_idle(start + Duration::from_secs(minutes * 60));
    }
    relay.collect();
    assert!(relay.take_agent().is_empty(), "a running turn is never closed");

    // The turn ends with a background task still going.
    let task = |state: &str| {
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session,
                "update": { "sessionUpdate": "async_task_state_update", "asyncTaskId": "t1", "state": state },
            },
        })
    };
    relay.agent(process, task("running"));
    relay.agent(process, response(&prompt[0].1["id"], json!({ "stopReason": "end_turn" })));
    for minutes in [181, 182, 400] {
        relay.state.close_idle(start + Duration::from_secs(minutes * 60));
    }
    relay.collect();
    assert!(relay.take_agent().is_empty(), "nor is one with background work");

    relay.agent(process, task("completed"));
    for minutes in [401, 402, 403] {
        relay.state.close_idle(start + Duration::from_secs(minutes * 60));
    }
    relay.collect();
    assert_eq!(relay.take_agent()[0].1["method"], json!("session/close"));
}

fn kinds(frames: &[(u64, Value)]) -> Vec<String> {
    frames
        .iter()
        .map(|(_, frame)| {
            let kind = frame["params"]["update"]["sessionUpdate"]
                .as_str()
                .or(frame["method"].as_str())
                .unwrap_or("response");
            kind.to_string()
        })
        .collect()
}

#[test]
fn a_prompt_is_in_the_replay_another_link_loads() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    let prompt = json!([{ "type": "text", "text": "fix the bug" }]);
    relay.client(
        link,
        request(2, "session/prompt", json!({ "sessionId": session, "prompt": prompt })),
    );
    let forwarded = relay.take_agent();
    relay.agent(process, update(&session, "on it"));
    relay.agent(process, response(&forwarded[0].1["id"], json!({ "stopReason": "end_turn" })));
    let to_sender = relay.take_link(link);
    assert_eq!(texts(&to_sender), ["on it"], "the sender already shows its prompt");

    let other = relay.open();
    relay.client(other, request(0, "initialize", json!({ "protocolVersion": 1 })));
    relay.client(
        other,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    let replay = relay.take_link(other);
    assert_eq!(texts(&replay), ["fix the bug", "on it"]);
    assert_eq!(kinds(&replay[1..3]), ["user_message_chunk", "agent_message_chunk"]);
    assert_eq!(replay[1].1["params"]["update"]["content"], prompt[0]);
}

#[test]
fn a_session_loaded_on_two_links_streams_to_both() {
    let mut relay = Harness::new(None);
    let (first, process, session) = relay.start_session();
    let second = relay.open();
    relay.client(second, request(0, "initialize", json!({ "protocolVersion": 1 })));
    relay.client(
        second,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    relay.take_link(second);

    // A prompt from the second link shows on the first as it happens.
    relay.client(
        second,
        request(
            2,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "go" }] }),
        ),
    );
    let prompt = relay.take_agent();
    let to_first = relay.take_link(first);
    assert_eq!(kinds(&to_first), ["user_message_chunk", frame::STATE_UPDATE]);
    assert_eq!(to_first[1].1["params"]["state"], json!("running"));
    assert!(relay.take_link(second).is_empty());

    relay.agent(process, update(&session, "working"));
    assert_eq!(texts(&relay.take_link(first)), ["working"]);
    assert_eq!(texts(&relay.take_link(second)), ["working"]);

    // A permission question goes to both; the first answer wins.
    relay.agent(
        process,
        request(70, "session/request_permission", json!({ "sessionId": session })),
    );
    let asked_first = relay.take_link(first);
    let asked_second = relay.take_link(second);
    assert_eq!(asked_first[0].1["id"], asked_second[0].1["id"]);
    relay.client(first, response(&asked_first[0].1["id"], json!({ "outcome": "a" })));
    relay.client(second, response(&asked_second[0].1["id"], json!({ "outcome": "b" })));
    assert_eq!(
        relay.take_agent(),
        [(process, response(&json!(70), json!({ "outcome": "a" })))]
    );

    // A file read goes to the link whose prompt is running, only.
    relay.agent(
        process,
        request(71, "fs/read_text_file", json!({ "sessionId": session, "path": "/a" })),
    );
    assert!(relay.take_link(first).is_empty());
    assert_eq!(relay.take_link(second)[0].1["method"], json!("fs/read_text_file"));

    // The answer to the prompt goes to the link that sent it; the other one
    // hears the turn end as state.
    relay.agent(process, response(&prompt[0].1["id"], json!({ "stopReason": "end_turn" })));
    let to_second = relay.take_link(second);
    assert_eq!(to_second.len(), 1);
    assert_eq!(to_second[0].1["id"], json!(2));
    let to_first = relay.take_link(first);
    assert_eq!(kinds(&to_first), [frame::STATE_UPDATE]);
    assert_eq!(to_first[0].1["params"]["state"], json!("idle"));
}

#[test]
fn a_reattach_fills_the_numbers_it_cannot_send_again() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();
    relay.state.detach_link(link);
    relay.agent(
        process,
        request(80, "session/request_permission", json!({ "sessionId": session })),
    );
    relay.agent(process, update(&session, "after"));

    // The question is answered from another link while the first is away.
    let other = relay.open();
    relay.client(
        other,
        request(1, "session/load", json!({ "sessionId": session, "cwd": "/box/project" })),
    );
    let asked = relay
        .take_link(other)
        .into_iter()
        .find(|(_, frame)| frame["method"] == json!("session/request_permission"))
        .unwrap();
    relay.client(other, response(&asked.1["id"], json!({ "outcome": "a" })));

    relay.state.attach_link(link, 2).unwrap();
    relay.collect();
    let frames = relay.to_link.remove(&link).unwrap();
    assert_eq!(frames.iter().map(|(lseq, _)| *lseq).collect::<Vec<_>>(), [3, 4]);
    assert!(matches!(frames[0].1, LinkFrame::Stderr(_)), "a placeholder holds the gap");
    assert!(matches!(&frames[1].1, LinkFrame::Line(line) if line.contains("after")));
}

#[test]
fn a_load_answers_with_the_config_the_session_has_now() {
    let mut relay = Harness::new(None);
    let link = relay.open();
    relay.client(link, request(0, "initialize", json!({ "protocolVersion": 1 })));
    let (process, _) = relay.spawned[0].clone();
    let init = relay.take_agent();
    relay.agent(process, response(&init[0].1["id"], json!({ "protocolVersion": 1 })));
    relay.client(link, request(1, "session/new", json!({ "cwd": "/box/project" })));
    let new = relay.take_agent();
    relay.agent(
        process,
        response(
            &new[0].1["id"],
            json!({
                "sessionId": "s1",
                "modes": { "currentModeId": "default", "availableModes": [] },
                "models": { "currentModelId": "sonnet", "availableModels": [] },
                "configOptions": [{ "id": "effort", "currentValue": "low" }],
            }),
        ),
    );
    for (id, method, params, result) in [
        (2, "session/set_model", json!({ "modelId": "opus" }), json!({})),
        (
            3,
            "session/set_config_option",
            json!({ "configId": "effort", "value": "high" }),
            json!({ "configOptions": [{ "id": "effort", "currentValue": "high" }] }),
        ),
        (4, "session/set_mode", json!({ "modeId": "plan" }), json!({})),
    ] {
        let mut params = params;
        params["sessionId"] = json!("s1");
        relay.client(link, request(id, method, params));
        let sent = relay.take_agent();
        relay.agent(process, response(&sent[0].1["id"], result));
    }
    relay.agent(
        process,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": { "sessionUpdate": "current_mode_update", "currentModeId": "bypassPermissions" },
            },
        }),
    );

    let other = relay.open();
    relay.client(
        other,
        request(1, "session/load", json!({ "sessionId": "s1", "cwd": "/box/project" })),
    );
    let loaded = relay
        .take_link(other)
        .into_iter()
        .find(|(_, frame)| frame["id"] == json!(1))
        .unwrap()
        .1;
    assert_eq!(loaded["result"]["modes"]["currentModeId"], json!("bypassPermissions"));
    assert_eq!(loaded["result"]["models"]["currentModelId"], json!("opus"));
    assert_eq!(loaded["result"]["configOptions"][0]["currentValue"], json!("high"));

    // A resume that comes back with the defaults gets the config set again
    // before the prompt that asked for the resume.
    relay.state.set_idle_timeout(Duration::ZERO);
    let now = Instant::now();
    relay.state.close_idle(now);
    relay.state.close_idle(now);
    relay.collect();
    let close = relay.take_agent();
    relay.agent(process, response(&close[0].1["id"], json!({})));
    relay.client(
        link,
        request(5, "session/prompt", json!({ "sessionId": "s1", "prompt": [] })),
    );
    let resume = relay.take_agent();
    relay.agent(
        process,
        response(
            &resume[0].1["id"],
            json!({
                "modes": { "currentModeId": "default", "availableModes": [] },
                "models": { "currentModelId": "opus", "availableModels": [] },
                "configOptions": [{ "id": "effort", "currentValue": "low" }],
            }),
        ),
    );
    let sent = relay
        .take_agent()
        .into_iter()
        .map(|(_, message)| (message["method"].clone(), message["params"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        sent,
        [
            (
                json!("session/set_mode"),
                json!({ "sessionId": "s1", "modeId": "bypassPermissions" })
            ),
            (
                json!("session/set_config_option"),
                json!({ "sessionId": "s1", "configId": "effort", "value": "high" })
            ),
            (json!("session/prompt"), json!({ "sessionId": "s1", "prompt": [] })),
        ]
    );
}

#[test]
fn a_second_load_waits_for_the_agent_to_load_the_session() {
    let mut relay = Harness::new(None);
    let first = relay.open();
    relay.client(first, request(0, "initialize", json!({ "protocolVersion": 1 })));
    let (process, _) = relay.spawned[0].clone();
    let init = relay.take_agent();
    relay.agent(process, response(&init[0].1["id"], json!({ "protocolVersion": 1 })));
    relay.client(
        first,
        request(1, "session/load", json!({ "sessionId": "s9", "cwd": "/box/project" })),
    );
    let load = relay.take_agent();
    assert_eq!(load[0].1["method"], json!("session/load"));
    relay.agent(process, update("s9", "replayed"));

    let second = relay.open();
    relay.client(second, request(0, "initialize", json!({ "protocolVersion": 1 })));
    relay.take_link(second);
    relay.client(
        second,
        request(1, "session/load", json!({ "sessionId": "s9", "cwd": "/box/project" })),
    );
    relay.client(
        second,
        request(
            2,
            "session/set_config_option",
            json!({ "sessionId": "s9", "configId": "effort", "value": "high" }),
        ),
    );
    assert!(relay.take_agent().is_empty(), "the agent is still loading it");
    assert!(relay.take_link(second).is_empty());

    relay.agent(process, response(&load[0].1["id"], json!({ "modes": null })));
    let to_second = relay.take_link(second);
    assert_eq!(texts(&to_second), ["replayed"]);
    assert!(to_second.iter().any(|(_, frame)| frame["id"] == json!(1)));
    let sent = relay.take_agent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["method"], json!("session/set_config_option"));
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

        let listed = pool.run_until(async move { second.list().await.unwrap() });
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
        new.add_adoptable(old_socket, listing);
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
        pool.run_until(async move {
            while !old_exited.get() {
                smol::future::yield_now().await;
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

/// 2026-10-19: after a restart the relay loaded sessions in the same second,
/// each starting a CLI, and the next prompt failed with "another Claude Code
/// process is refreshing it".
#[test]
fn sessions_open_one_at_a_time_and_a_prompt_that_lost_the_oauth_race_is_retried_once() {
    let mut relay = Harness::new(None);
    let (link, process, session) = relay.start_session();

    relay.client(link, request(2, "session/load", json!({ "sessionId": "s2", "cwd": "/box" })));
    relay.client(link, request(3, "session/load", json!({ "sessionId": "s3", "cwd": "/box" })));
    let sent = relay.take_agent();
    assert_eq!(sent.len(), 1, "the second load waits for the first");
    assert_eq!(sent[0].1["params"]["sessionId"], json!("s2"));
    relay.agent(process, response(&sent[0].1["id"], json!({})));
    let sent = relay.take_agent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["params"]["sessionId"], json!("s3"));
    relay.agent(process, response(&sent[0].1["id"], json!({})));
    relay.take_link(link);

    let refresh_race = |id: &Value| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32603,
                "message": "Failed to refresh OAuth token: another Claude Code process is refreshing it",
            },
        })
    };
    relay.client(
        link,
        request(4, "session/prompt", json!({ "sessionId": session, "prompt": [] })),
    );
    let first = relay.take_agent()[0].1["id"].clone();
    relay.agent(process, refresh_race(&first));
    assert!(relay.take_link(link).is_empty(), "the client sees no error");
    assert!(relay.take_agent().is_empty(), "the retry waits");
    let (later_process, line) = relay.later.pop().expect("a retry is scheduled");
    relay.state.send_later_due(later_process, line);
    relay.collect();
    let retry = relay.take_agent();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].1["method"], json!("session/prompt"));
    assert_ne!(retry[0].1["id"], first);

    // A second loss is the client's to see.
    relay.agent(process, refresh_race(&retry[0].1["id"]));
    let answers = relay.take_link(link);
    let answer = answers
        .iter()
        .find(|(_, frame)| frame["id"] == json!(4))
        .expect("the prompt is answered");
    assert!(answer.1["error"]["message"].as_str().unwrap().contains("refreshing"));
    assert!(relay.later.is_empty());
}

#[test]
fn a_closed_sessions_log_is_kept_for_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut relay = Harness::new(Some(&sessions));
    let (link, process, session) = relay.start_session();
    relay.agent(process, update(&session, "hello"));
    relay.client(link, request(2, "session/close", json!({ "sessionId": session })));
    let close = relay.take_agent();
    relay.agent(process, response(&close[0].1["id"], json!({})));

    let kept = std::fs::read_dir(sessions.join("closed"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(kept.len(), 1, "the log moved aside rather than going away");
    assert!(std::fs::read_to_string(&kept[0]).unwrap().contains("hello"));

    crate::log::prune_retired(&sessions, crate::log::RETIRED_LOG_RETENTION);
    assert!(kept[0].exists(), "a day has not passed");
    crate::log::prune_retired(&sessions, Duration::ZERO);
    assert!(!kept[0].exists());
}

/// 2026-10-22: a thread whose session the relay took over from an older one
/// opened with no mode, model or effort controls, because the older relay
/// answered its load with an empty result and the resume this relay did by
/// itself was never recorded.
#[test]
fn a_session_this_relay_resumed_by_itself_loads_with_its_config() {
    let mut old = Harness::new(None);
    old.start_session();
    let mut new = Harness::new(None);
    new.state
        .add_adoptable("/relay-old.sock".into(), old.state.sessions());
    let link = new.open();
    new.client(
        link,
        request(0, "initialize", json!({ "protocolVersion": 1 })),
    );
    let (home, _) = new.spawned[0].clone();
    let init = new.take_agent();
    new.agent(
        home,
        response(&init[0].1["id"], json!({ "protocolVersion": 1 })),
    );
    new.client(
        link,
        request(
            1,
            "session/load",
            json!({ "sessionId": "s1", "cwd": "/box/project" }),
        ),
    );
    let (adopted, _) = new.spawned.last().cloned().unwrap();
    let load = new.take_agent();
    new.agent(adopted, response(&load[0].1["id"], json!({})));
    new.take_link(link);

    // The older relay lets the session go; this one resumes it.
    new.state.agent_exited(adopted, None);
    new.collect();
    let resume = new.take_agent();
    assert_eq!(resume[0].1["method"], json!("session/resume"));
    let config = json!({
        "modes": { "currentModeId": "bypassPermissions", "availableModes": [] },
        "models": { "currentModelId": "opus", "availableModels": [] },
        "configOptions": [{ "id": "effort", "currentValue": "high" }],
    });
    new.agent(home, response(&resume[0].1["id"], config.clone()));
    let pushed = new
        .take_link(link)
        .into_iter()
        .find(|(_, frame)| frame["method"] == json!(frame::SESSION_CONFIG))
        .expect("the thread already open gets its controls")
        .1;
    assert_eq!(pushed["params"]["sessionId"], json!("s1"));
    assert_eq!(pushed["params"]["modes"], config["modes"]);
    assert_eq!(pushed["params"]["configOptions"], config["configOptions"]);

    let other = new.open();
    new.client(
        other,
        request(
            1,
            "session/load",
            json!({ "sessionId": "s1", "cwd": "/box/project" }),
        ),
    );
    let loaded = new
        .take_link(other)
        .into_iter()
        .find(|(_, frame)| frame["id"] == json!(1))
        .unwrap()
        .1;
    assert_eq!(loaded["result"]["modes"], config["modes"]);
    assert_eq!(loaded["result"]["models"], config["models"]);
    assert_eq!(loaded["result"]["configOptions"], config["configOptions"]);
    assert!(
        new.take_link(link).is_empty(),
        "a config already sent is not sent again"
    );
}

/// claude-agent-acp offers its mode both as a mode and as a config option.
/// A change reported through one of them must not leave the other stale, or a
/// resume sets the stale one last and the thread loses `bypassPermissions`.
#[test]
fn a_resume_never_sets_a_mode_the_session_has_left() {
    let mut relay = Harness::new(None);
    let link = relay.open();
    relay.client(
        link,
        request(0, "initialize", json!({ "protocolVersion": 1 })),
    );
    let (process, _) = relay.spawned[0].clone();
    let init = relay.take_agent();
    relay.agent(
        process,
        response(&init[0].1["id"], json!({ "protocolVersion": 1 })),
    );
    relay.client(
        link,
        request(1, "session/new", json!({ "cwd": "/box/project" })),
    );
    let new = relay.take_agent();
    let defaults = json!({
        "modes": { "currentModeId": "default", "availableModes": [] },
        "configOptions": [
            { "id": "mode", "category": "mode", "currentValue": "default" },
            { "id": "effort", "currentValue": "low" },
        ],
    });
    let mut created = defaults.clone();
    created["sessionId"] = json!("s1");
    relay.agent(process, response(&new[0].1["id"], created));
    // Plan mode picked from the config options, then left for bypass by the
    // agent, which reports it as a mode.
    relay.client(
        link,
        request(
            2,
            "session/set_config_option",
            json!({ "sessionId": "s1", "configId": "mode", "value": "plan" }),
        ),
    );
    let set = relay.take_agent();
    relay.agent(
        process,
        response(
            &set[0].1["id"],
            json!({ "configOptions": [
                { "id": "mode", "category": "mode", "currentValue": "plan" },
                { "id": "effort", "currentValue": "low" },
            ] }),
        ),
    );
    relay.agent(
        process,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": { "sessionUpdate": "current_mode_update", "currentModeId": "bypassPermissions" },
            },
        }),
    );

    relay.state.set_idle_timeout(Duration::ZERO);
    let now = Instant::now();
    relay.state.close_idle(now);
    relay.state.close_idle(now);
    relay.collect();
    let close = relay.take_agent();
    relay.agent(process, response(&close[0].1["id"], json!({})));
    relay.client(
        link,
        request(
            3,
            "session/prompt",
            json!({ "sessionId": "s1", "prompt": [] }),
        ),
    );
    let resume = relay.take_agent();
    relay.agent(process, response(&resume[0].1["id"], defaults));
    let modes_set = relay
        .take_agent()
        .into_iter()
        .filter_map(|(_, message)| match message["method"].as_str()? {
            "session/set_mode" => Some(message["params"]["modeId"].clone()),
            "session/set_config_option" if message["params"]["configId"] == json!("mode") => {
                Some(message["params"]["value"].clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!modes_set.is_empty());
    assert!(
        modes_set
            .iter()
            .all(|mode| *mode == json!("bypassPermissions")),
        "{modes_set:?}"
    );
}

/// 2026-10-21: a draining relay stayed up for twelve hours because one of its
/// sessions waited on a permission nobody answered. Past `DRAIN_QUIET` it lets
/// the session go, the newer relay resumes it, and a reload no longer shows
/// the request as pending.
#[test]
fn a_draining_relay_lets_a_session_waiting_on_nobody_go() {
    let mut old = Harness::new(None);
    let (link, process, session) = old.start_session();
    old.client(
        link,
        request(
            2,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [] }),
        ),
    );
    old.take_agent();
    old.agent(
        process,
        request(
            40,
            "session/request_permission",
            json!({ "sessionId": session, "toolCall": { "toolCallId": "t1" }, "options": [] }),
        ),
    );
    old.state.detach_link(link);

    // The newer relay has taken the session over through an adopting link.
    let mut new = Harness::new(None);
    new.state
        .add_adoptable("/relay-old.sock".into(), old.state.sessions());
    let new_link = new.open();
    new.client(
        new_link,
        request(0, "initialize", json!({ "protocolVersion": 1 })),
    );
    let (home, _) = new.spawned[0].clone();
    let init = new.take_agent();
    new.agent(
        home,
        response(&init[0].1["id"], json!({ "protocolVersion": 1 })),
    );
    new.client(
        new_link,
        request(
            1,
            "session/load",
            json!({ "sessionId": session, "cwd": "/box/project" }),
        ),
    );
    let (adopted, _) = new.spawned.last().cloned().unwrap();
    let load = new.take_agent();
    let adopting = old.state.open_adopting_link();
    old.state.client_line(adopting, 1, load[0].1.to_string());
    old.collect();
    for (_, frame) in old.take_link(adopting) {
        new.agent(adopted, frame);
    }
    new.take_link(new_link);

    old.state.drain();
    old.collect();
    assert!(
        old.state.open_link(command()).is_err(),
        "a draining relay takes no new links"
    );
    old.state.attach_link(link, 0).unwrap();
    old.client(
        link,
        request(3, "session/new", json!({ "cwd": "/box/project" })),
    );
    let refused = old.take_link(link);
    assert!(
        refused
            .iter()
            .any(|(_, frame)| frame["id"] == json!(3) && frame["error"].is_object()),
        "nor new sessions on the links it has"
    );
    old.state.detach_link(link);

    let start = Instant::now();
    old.state.close_idle(start);
    old.state.close_idle(start + DRAIN_QUIET / 2);
    old.collect();
    assert!(
        old.killed.is_empty(),
        "a turn waiting on the user is left a while"
    );
    old.state.close_idle(start + DRAIN_QUIET * 2);
    old.collect();
    assert_eq!(old.killed, [process]);
    assert!(old.exited);

    // The adopting link sees the session end, and the newer relay resumes it.
    assert!(
        old.to_link
            .remove(&adopting)
            .unwrap_or_default()
            .iter()
            .any(|(_, frame)| matches!(frame, LinkFrame::Exited(_)))
    );
    new.state.agent_exited(adopted, None);
    new.collect();
    let resume = new.take_agent();
    assert_eq!(resume[0].0, home);
    assert_eq!(resume[0].1["method"], json!("session/resume"));
    new.agent(home, response(&resume[0].1["id"], json!({})));
    let ended = new.take_link(new_link);
    assert!(
        ended
            .iter()
            .any(|(_, frame)| frame["params"]["state"] == json!("idle")),
        "the thread hears its turn is over"
    );

    let other = new.open();
    new.client(
        other,
        request(
            1,
            "session/load",
            json!({ "sessionId": session, "cwd": "/box/project" }),
        ),
    );
    assert!(
        new.take_link(other)
            .iter()
            .all(|(_, frame)| frame["method"] != json!("session/request_permission")),
        "the request nobody can answer any more is not shown again"
    );
}
