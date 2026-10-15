//! The relay itself, as a state machine with no I/O of its own: frames go in,
//! and what to send, spawn or kill comes out as [`Output`]s for a driver to
//! carry out. That keeps every decision testable without processes or sockets.
//!
//! The relay owns the JSON-RPC id space on both sides. A client's request is
//! renumbered before it reaches the agent and numbered back on the way out, so
//! a response that arrives while no client is attached is kept rather than
//! lost; an agent's request is renumbered too, so requests from two agents can
//! share one client link and a request can be answered from a different link
//! than the one it was first shown on.

use crate::frame::{self, Kind};
use crate::log::SessionLog;
use collections::{BTreeMap, HashMap, HashSet, VecDeque};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub type LinkId = u64;
pub type ProcessId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    pub name: String,
    pub path: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl AgentCommand {
    fn key(&self) -> (String, Vec<String>) {
        (self.path.clone(), self.args.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentSpec {
    Command(AgentCommand),
    /// A session a relay from an older build still runs, reached through a
    /// link on that relay's socket while it drains.
    Adopt { socket: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkFrame {
    Line(String),
    Stderr(String),
    Exited(Option<i32>),
}

#[derive(Debug, PartialEq)]
pub enum Output {
    ToLink {
        link: LinkId,
        lseq: u64,
        frame: LinkFrame,
    },
    ToAgent {
        process: ProcessId,
        line: String,
    },
    Spawn {
        process: ProcessId,
        spec: AgentSpec,
    },
    Kill {
        process: ProcessId,
    },
    /// An `fs/*` request the relay answers itself because no client is there
    /// to; the driver calls [`RelayState::fs_served`] with the outcome.
    ServeFs {
        request: u64,
        method: String,
        params: Value,
    },
    /// A draining relay with nothing left to run.
    Exit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub agent_name: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub running: bool,
    pub attached: bool,
    pub command: AgentCommand,
}

enum Out {
    Frame(LinkFrame),
    Session { root: String, seq: u64 },
}

struct Link {
    command: Option<AgentCommand>,
    home: Option<ProcessId>,
    attached: bool,
    next_lseq: u64,
    outbox: VecDeque<(u64, Out)>,
    next_cseq: u64,
    held: BTreeMap<u64, String>,
}

struct Process {
    command: Option<AgentCommand>,
    ready: bool,
    queued: Vec<String>,
    sessions: HashSet<String>,
    home_of: HashSet<LinkId>,
    init: Option<u64>,
    init_waiters: Vec<(LinkId, Value)>,
}

enum Turn {
    Idle {
        stop_reason: Option<String>,
        error: Option<String>,
    },
    Running {
        prompt: u64,
    },
}

struct Session {
    process: ProcessId,
    link: Option<LinkId>,
    log: SessionLog,
    open_result: Option<Value>,
    open_params: Value,
    command: AgentCommand,
    turn: Turn,
    /// Requests from the agent still waiting for an answer, with the log
    /// frame that carries each.
    pending: Vec<(u64, u64)>,
    title: Option<String>,
}

#[derive(PartialEq)]
enum Purpose {
    Client,
    Initialize,
    HandoverResume,
}

struct ClientRequest {
    process: ProcessId,
    link: Option<LinkId>,
    client_id: Value,
    method: String,
    session: Option<String>,
    params: Value,
    purpose: Purpose,
}

struct AgentRequest {
    process: ProcessId,
    original_id: Value,
    session: Option<String>,
    method: String,
    delivered_to: Option<LinkId>,
}

struct InitCache {
    params: Value,
    result: Value,
}

pub struct RelayState {
    log_dir: Option<PathBuf>,
    next_id: u64,
    links: HashMap<LinkId, Link>,
    processes: HashMap<ProcessId, Process>,
    sessions: HashMap<String, Session>,
    children: HashMap<String, String>,
    client_requests: HashMap<u64, ClientRequest>,
    agent_requests: HashMap<u64, AgentRequest>,
    init_cache: HashMap<(String, Vec<String>), InitCache>,
    adoptable: HashMap<String, (PathBuf, AgentCommand)>,
    draining: bool,
    outputs: Vec<Output>,
}

impl RelayState {
    pub fn new(log_dir: Option<PathBuf>) -> Self {
        Self {
            log_dir,
            next_id: 1,
            links: HashMap::default(),
            processes: HashMap::default(),
            sessions: HashMap::default(),
            children: HashMap::default(),
            client_requests: HashMap::default(),
            agent_requests: HashMap::default(),
            init_cache: HashMap::default(),
            adoptable: HashMap::default(),
            draining: false,
            outputs: Vec::new(),
        }
    }

    pub fn take_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outputs)
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // --- Links ---

    pub fn open_link(&mut self, command: AgentCommand) -> anyhow::Result<LinkId> {
        anyhow::ensure!(!self.draining, "this relay is handing over to a newer one");
        Ok(self.insert_link(Some(command)))
    }

    /// A link from a newer relay that takes over this one's live sessions.
    pub fn open_adopting_link(&mut self) -> LinkId {
        self.insert_link(None)
    }

    fn insert_link(&mut self, command: Option<AgentCommand>) -> LinkId {
        let link = self.next_id();
        self.links.insert(
            link,
            Link {
                command,
                home: None,
                attached: true,
                next_lseq: 1,
                outbox: VecDeque::new(),
                next_cseq: 1,
                held: BTreeMap::new(),
            },
        );
        link
    }

    /// Resumes a link after its client lost the connection: everything sent
    /// after `last_lseq` goes out again, and the answer says which client
    /// frame the relay expects next, so the client resends from there.
    pub fn attach_link(&mut self, link: LinkId, last_lseq: u64) -> anyhow::Result<u64> {
        let Some(state) = self.links.get_mut(&link) else {
            anyhow::bail!("the relay has no link {link}");
        };
        state.attached = true;
        state.outbox.retain(|(lseq, _)| *lseq > last_lseq);
        let next_cseq = state.next_cseq;
        let state = &self.links[&link];
        let pending = state
            .outbox
            .iter()
            .map(|(lseq, out)| (*lseq, self.resolve(out)))
            .collect::<Vec<_>>();
        for (lseq, frame) in pending {
            if let Some(frame) = frame {
                self.outputs.push(Output::ToLink { link, lseq, frame });
            }
        }
        Ok(next_cseq)
    }

    pub fn detach_link(&mut self, link: LinkId) {
        let Some(state) = self.links.get_mut(&link) else {
            return;
        };
        state.attached = false;
        // Nobody will answer a file read while the client is away.
        let stranded = self
            .agent_requests
            .iter()
            .filter(|(_, request)| {
                request.delivered_to == Some(link) && request.method.starts_with("fs/")
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for request in stranded {
            self.serve_detached(request);
        }
    }

    pub fn ack(&mut self, link: LinkId, lseq: u64) {
        if let Some(state) = self.links.get_mut(&link) {
            state.outbox.retain(|(frame, _)| *frame > lseq);
        }
    }

    /// The client is gone for good. Its sessions keep running and can be
    /// attached again from another link.
    pub fn close_link(&mut self, link: LinkId) {
        let Some(state) = self.links.remove(&link) else {
            return;
        };
        for session in self.sessions.values_mut() {
            if session.link == Some(link) {
                session.link = None;
            }
        }
        if let Some(home) = state.home
            && let Some(process) = self.processes.get_mut(&home)
        {
            process.home_of.remove(&link);
            self.reap(home);
        }
        self.check_drain();
    }

    fn resolve(&self, out: &Out) -> Option<LinkFrame> {
        match out {
            Out::Frame(frame) => Some(frame.clone()),
            Out::Session { root, seq } => {
                let log = &self.sessions.get(root)?.log;
                let line = log.frame(*seq)?;
                // A request answered while the client was away is not asked again.
                if let Some((_, Kind::Request { id, .. })) = frame::parse(&line)
                    && id.as_u64().is_some_and(|id| log.is_answered(id))
                {
                    return None;
                }
                Some(LinkFrame::Line(line.to_string()))
            }
        }
    }

    fn push_out(&mut self, link: LinkId, out: Out, frame: LinkFrame) {
        let Some(state) = self.links.get_mut(&link) else {
            return;
        };
        let lseq = state.next_lseq;
        state.next_lseq += 1;
        state.outbox.push_back((lseq, out));
        if state.attached {
            self.outputs.push(Output::ToLink { link, lseq, frame });
        }
    }

    fn send_line(&mut self, link: LinkId, line: String) {
        self.push_out(
            link,
            Out::Frame(LinkFrame::Line(line.clone())),
            LinkFrame::Line(line),
        );
    }

    // --- Frames from the client ---

    pub fn client_line(&mut self, link: LinkId, cseq: u64, line: String) {
        let Some(state) = self.links.get_mut(&link) else {
            return;
        };
        if cseq < state.next_cseq {
            return;
        }
        state.held.insert(cseq, line);
        let mut ready = Vec::new();
        while let Some(line) = state.held.remove(&state.next_cseq) {
            state.next_cseq += 1;
            ready.push(line);
        }
        for line in ready {
            self.handle_client_line(link, line);
        }
    }

    fn handle_client_line(&mut self, link: LinkId, line: String) {
        let Some((value, kind)) = frame::parse(&line) else {
            log::warn!("quiet-ui relay: dropping an unreadable client frame");
            return;
        };
        match kind {
            Kind::Response { id } => self.client_response(id, value),
            Kind::Notification { method } => {
                let session = frame::session_id(&value).map(|id| self.root(id));
                let process = match session.as_ref().and_then(|s| self.sessions.get(s)) {
                    Some(session) => Some(session.process),
                    None => self.home(link),
                };
                if let Some(process) = process {
                    self.send_to_process(process, line);
                } else {
                    log::warn!("quiet-ui relay: no agent for {method}");
                }
            }
            Kind::Request { id, method } => self.client_request(link, id, method, value),
        }
    }

    fn client_request(&mut self, link: LinkId, id: Value, method: String, value: Value) {
        if method == frame::INITIALIZE {
            return self.client_initialize(link, id, value);
        }
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let session = frame::session_id(&value).map(|id| self.root(id));
        let opening = method == frame::SESSION_LOAD || method == frame::SESSION_RESUME;

        if let Some(session_id) = &session
            && self.sessions.contains_key(session_id)
        {
            if opening {
                let replay = method == frame::SESSION_LOAD;
                return self.attach_session(link, session_id.clone(), id, replay);
            }
            let process = self.sessions[session_id].process;
            return self.forward(process, link, id, method, session, params, value);
        }

        if let Some(session_id) = &session
            && opening
        {
            let (process, command) = if let Some((socket, command)) =
                self.adoptable.remove(session_id)
            {
                let process = self.spawn(AgentSpec::Adopt { socket }, None);
                (process, command)
            } else {
                let Some(process) = self.home(link) else {
                    return self.send_line(link, frame::error(&id, "no agent on this link"));
                };
                let Some(command) = self.links[&link].command.clone() else {
                    return self.send_line(link, frame::error(&id, "no agent on this link"));
                };
                (process, command)
            };
            self.register_session(session_id.clone(), process, link, params.clone(), command);
            return self.forward(process, link, id, method, session, params, value);
        }

        let process = match session.as_ref().and_then(|s| self.children_process(s)) {
            Some(process) => Some(process),
            None => self.home(link),
        };
        let Some(process) = process else {
            return self.send_line(link, frame::error(&id, "no agent on this link"));
        };
        self.forward(process, link, id, method, session, params, value);
    }

    fn children_process(&self, session: &str) -> Option<ProcessId> {
        self.sessions.get(session).map(|session| session.process)
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &mut self,
        process: ProcessId,
        link: LinkId,
        client_id: Value,
        method: String,
        session: Option<String>,
        params: Value,
        value: Value,
    ) {
        let id = self.next_id();
        if method == frame::SESSION_PROMPT
            && let Some(session) = session.as_ref().and_then(|s| self.sessions.get_mut(s))
        {
            session.turn = Turn::Running { prompt: id };
        }
        self.client_requests.insert(
            id,
            ClientRequest {
                process,
                link: Some(link),
                client_id,
                method,
                session,
                params,
                purpose: Purpose::Client,
            },
        );
        self.send_to_process(process, frame::with_id(value, Value::from(id)));
    }

    fn client_initialize(&mut self, link: LinkId, id: Value, value: Value) {
        let Some(command) = self.links.get(&link).map(|link| link.command.clone()) else {
            return;
        };
        let Some(command) = command else {
            // An adopting link speaks to sessions that are already set up.
            let result = self.init_cache.values().next().map(|cache| cache.result.clone());
            let line = match result {
                Some(result) => frame::result(&id, result),
                None => frame::error(&id, "this relay has no initialized agent"),
            };
            return self.send_line(link, line);
        };
        if let Some(cache) = self.init_cache.get(&command.key()) {
            let line = frame::result(&id, cache.result.clone());
            return self.send_line(link, line);
        }
        let Some(process) = self.home(link) else {
            return;
        };
        let Some(state) = self.processes.get_mut(&process) else {
            return;
        };
        if state.init.is_some() {
            state.init_waiters.push((link, id));
            return;
        }
        let request = self.next_id();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        if let Some(state) = self.processes.get_mut(&process) {
            state.init = Some(request);
        }
        self.client_requests.insert(
            request,
            ClientRequest {
                process,
                link: Some(link),
                client_id: id,
                method: frame::INITIALIZE.into(),
                session: None,
                params,
                purpose: Purpose::Initialize,
            },
        );
        self.outputs.push(Output::ToAgent {
            process,
            line: frame::with_id(value, Value::from(request)),
        });
    }

    fn client_response(&mut self, id: Value, value: Value) {
        let Some(request) = id.as_u64().and_then(|id| {
            let request = self.agent_requests.remove(&id)?;
            Some((id, request))
        }) else {
            return;
        };
        let (id, request) = request;
        self.answered(id, &request);
        self.send_to_process(request.process, frame::with_id(value, request.original_id));
    }

    fn answered(&mut self, id: u64, request: &AgentRequest) {
        if let Some(session) = request
            .session
            .as_ref()
            .and_then(|s| self.sessions.get_mut(s))
        {
            session.log.mark_answered(id);
            session.pending.retain(|(pending, _)| *pending != id);
        }
        self.check_drain();
    }

    /// Hands a live session to `link` instead of asking the agent to load it
    /// again, which would start a second copy of it (claude-agent-acp#1011).
    fn attach_session(&mut self, link: LinkId, session_id: String, id: Value, replay: bool) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.link = Some(link);
        let frames = if replay {
            session
                .log
                .frames_after(0)
                .into_iter()
                .filter(|(_, line)| {
                    !matches!(frame::parse(line), Some((_, Kind::Request { .. })))
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut result = session
            .open_result
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));
        if let Some(object) = result.as_object_mut() {
            object.remove("sessionId");
        }
        let pending = session
            .pending
            .iter()
            .filter_map(|(_, seq)| Some((*seq, session.log.frame(*seq)?)))
            .collect::<Vec<_>>();
        let state = state_frame(&session_id, &session.turn);

        for (seq, line) in frames {
            self.push_out(
                link,
                Out::Session {
                    root: session_id.clone(),
                    seq,
                },
                LinkFrame::Line(line.to_string()),
            );
        }
        self.send_line(link, frame::result(&id, result));
        for (seq, line) in pending {
            self.push_out(
                link,
                Out::Session {
                    root: session_id.clone(),
                    seq,
                },
                LinkFrame::Line(line.to_string()),
            );
        }
        self.send_line(link, state);
    }

    // --- Frames from an agent ---

    pub fn agent_line(&mut self, process: ProcessId, line: String) {
        let Some((value, kind)) = frame::parse(&line) else {
            log::warn!("quiet-ui relay: dropping an unreadable agent frame");
            return;
        };
        match kind {
            Kind::Response { id } => self.agent_response(process, id, value),
            Kind::Request { id, method } => self.agent_request(process, id, method, value),
            Kind::Notification { .. } => {
                let Some(session) = frame::session_id(&value).map(|id| self.root(id)) else {
                    return self.to_home_links(process, line);
                };
                if let Some(child) = frame::announced_child_session(&value)
                    && child != session
                {
                    self.children.insert(child.to_string(), session.clone());
                }
                if let Some(title) = frame::session_title(&value)
                    && let Some(state) = self.sessions.get_mut(&session)
                {
                    state.title = Some(title.to_string());
                }
                self.session_frame(&session, line);
            }
        }
    }

    fn to_home_links(&mut self, process: ProcessId, line: String) {
        let links = self
            .processes
            .get(&process)
            .map(|process| process.home_of.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for link in links {
            self.send_line(link, line.clone());
        }
    }

    fn session_frame(&mut self, root: &str, line: String) -> Option<u64> {
        let session = self.sessions.get_mut(root)?;
        let seq = session.log.append(&line);
        if let Some(link) = session.link {
            self.push_out(
                link,
                Out::Session {
                    root: root.to_string(),
                    seq,
                },
                LinkFrame::Line(line),
            );
        }
        Some(seq)
    }

    fn agent_response(&mut self, process: ProcessId, id: Value, value: Value) {
        let Some(request) = id
            .as_u64()
            .and_then(|id| self.client_requests.remove(&id))
        else {
            return;
        };
        let result = value.get("result").cloned();
        let error = value.get("error").cloned();
        match request.purpose {
            Purpose::Initialize => return self.initialized(process, request, result, value),
            Purpose::HandoverResume => {
                if result.is_none()
                    && let Some(session) = request.session
                {
                    log::warn!("quiet-ui relay: resuming {session} after the handover failed");
                    self.end_session(&session, Some("the session did not survive the update".into()));
                }
                return;
            }
            Purpose::Client => {}
        }

        match request.method.as_str() {
            frame::SESSION_NEW => {
                if let Some(session_id) = result
                    .as_ref()
                    .and_then(|result| result.get("sessionId"))
                    .and_then(Value::as_str)
                    && let Some(link) = request.link
                    && let Some(command) =
                        self.links.get(&link).and_then(|link| link.command.clone())
                {
                    let session_id = session_id.to_string();
                    self.register_session(
                        session_id.clone(),
                        process,
                        link,
                        request.params.clone(),
                        command,
                    );
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.open_result = result.clone();
                    }
                }
            }
            frame::SESSION_LOAD | frame::SESSION_RESUME => {
                if let Some(session_id) = &request.session {
                    if let Some(result) = &result {
                        if let Some(session) = self.sessions.get_mut(session_id) {
                            session.open_result = Some(result.clone());
                        }
                    } else if self
                        .processes
                        .get(&process)
                        .is_some_and(|process| process.command.is_none())
                    {
                        return self.reopen_here(request);
                    } else {
                        self.end_session(session_id, None);
                    }
                }
            }
            frame::SESSION_PROMPT => {
                if let Some(session_id) = &request.session
                    && let Some(session) = self.sessions.get_mut(session_id)
                {
                    let ended_ours = matches!(
                        session.turn,
                        Turn::Running { prompt, .. } if prompt == id.as_u64().unwrap_or_default()
                    );
                    if ended_ours {
                        session.turn = Turn::Idle {
                            stop_reason: result
                                .as_ref()
                                .and_then(|result| result.get("stopReason"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            error: error
                                .as_ref()
                                .and_then(|error| error.get("message"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        };
                        if session.link.is_some() && session.link != request.link {
                            let state = state_frame(session_id, &session.turn);
                            let link = session.link;
                            if let Some(link) = link {
                                self.send_line(link, state);
                            }
                        }
                    }
                }
            }
            frame::SESSION_CLOSE => {
                if result.is_some()
                    && let Some(session_id) = &request.session
                {
                    self.end_session(session_id, None);
                }
            }
            _ => {}
        }

        if let Some(link) = request.link {
            self.send_line(link, frame::with_id(value, request.client_id));
        }
        self.check_drain();
    }

    fn initialized(
        &mut self,
        process: ProcessId,
        request: ClientRequest,
        result: Option<Value>,
        value: Value,
    ) {
        let Some(state) = self.processes.get_mut(&process) else {
            return;
        };
        state.init = None;
        let waiters = std::mem::take(&mut state.init_waiters);
        let Some(result) = result else {
            log::warn!("quiet-ui relay: agent {process} refused to initialize");
            if let Some(link) = request.link {
                self.send_line(link, frame::with_id(value.clone(), request.client_id));
            }
            for (link, id) in waiters {
                self.send_line(link, frame::with_id(value.clone(), id));
            }
            self.outputs.push(Output::Kill { process });
            return;
        };
        state.ready = true;
        let queued = std::mem::take(&mut state.queued);
        if let Some(command) = &state.command {
            self.init_cache.insert(
                command.key(),
                InitCache {
                    params: request.params.clone(),
                    result: result.clone(),
                },
            );
        }
        if let Some(link) = request.link {
            self.send_line(link, frame::result(&request.client_id, result.clone()));
        }
        for (link, id) in waiters {
            self.send_line(link, frame::result(&id, result.clone()));
        }
        for line in queued {
            self.outputs.push(Output::ToAgent { process, line });
        }
    }

    fn agent_request(&mut self, process: ProcessId, id: Value, method: String, value: Value) {
        let session = frame::session_id(&value).map(|id| self.root(id));
        let request = self.next_id();
        let line = frame::with_id(value, Value::from(request));
        let link = session
            .as_ref()
            .and_then(|s| self.sessions.get(s))
            .and_then(|session| session.link)
            .or_else(|| {
                self.processes
                    .get(&process)?
                    .home_of
                    .iter()
                    .copied()
                    .find(|link| self.links.get(link).is_some_and(|link| link.attached))
            });
        let attached = link.is_some_and(|link| self.links.get(&link).is_some_and(|l| l.attached));
        self.agent_requests.insert(
            request,
            AgentRequest {
                process,
                original_id: id.clone(),
                session: session.clone(),
                method: method.clone(),
                delivered_to: link,
            },
        );

        if !attached && method.starts_with(frame::TERMINAL_PREFIX) {
            self.agent_requests.remove(&request);
            return self.outputs.push(Output::ToAgent {
                process,
                line: frame::error(&id, "Zed is not attached to run a terminal"),
            });
        }

        match session {
            Some(session) if self.sessions.contains_key(&session) => {
                if let Some(seq) = self.session_frame(&session, line)
                    && let Some(state) = self.sessions.get_mut(&session)
                {
                    state.pending.push((request, seq));
                }
            }
            _ => match link {
                Some(link) => self.send_line(link, line),
                None => {
                    self.agent_requests.remove(&request);
                    self.outputs.push(Output::ToAgent {
                        process,
                        line: frame::error(&id, "no client is attached"),
                    });
                    return;
                }
            },
        }
        if !attached && method.starts_with("fs/") {
            self.serve_detached(request);
        }
    }

    fn serve_detached(&mut self, request: u64) {
        let Some(state) = self.agent_requests.get(&request) else {
            return;
        };
        let params = self
            .sessions
            .get(state.session.as_deref().unwrap_or_default())
            .and_then(|session| {
                session
                    .pending
                    .iter()
                    .find(|(id, _)| *id == request)
                    .and_then(|(_, seq)| session.log.frame(*seq))
            })
            .and_then(|line| frame::parse(&line))
            .and_then(|(value, _)| value.get("params").cloned())
            .unwrap_or(Value::Null);
        self.outputs.push(Output::ServeFs {
            request,
            method: state.method.clone(),
            params,
        });
    }

    /// The outcome of an `fs/*` request the relay served itself.
    pub fn fs_served(&mut self, request: u64, outcome: Result<Value, String>) {
        let Some(state) = self.agent_requests.remove(&request) else {
            return;
        };
        let line = match outcome {
            Ok(result) => frame::result(&state.original_id, result),
            Err(message) => frame::error(&state.original_id, &message),
        };
        self.answered(request, &state);
        self.send_to_process(state.process, line);
    }

    // --- Processes ---

    pub fn agent_stderr(&mut self, process: ProcessId, line: String) {
        let links = self
            .processes
            .get(&process)
            .map(|process| process.home_of.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for link in links {
            self.push_out(
                link,
                Out::Frame(LinkFrame::Stderr(line.clone())),
                LinkFrame::Stderr(line.clone()),
            );
        }
    }

    pub fn agent_exited(&mut self, process: ProcessId, code: Option<i32>) {
        let Some(state) = self.processes.remove(&process) else {
            return;
        };
        let adopted = state.command.is_none();
        let orphaned = self
            .client_requests
            .iter()
            .filter(|(_, request)| request.process == process)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut reopening = Vec::new();
        for id in orphaned {
            let Some(request) = self.client_requests.remove(&id) else {
                continue;
            };
            if adopted && is_open(&request) {
                reopening.push(request);
            } else if let Some(link) = request.link
                && request.purpose == Purpose::Client
            {
                self.send_line(link, frame::error(&request.client_id, "the agent exited"));
            }
        }
        for session_id in state.sessions {
            if reopening
                .iter()
                .any(|request| request.session.as_ref() == Some(&session_id))
            {
                continue;
            }
            if adopted {
                self.hand_over(session_id);
            } else {
                self.end_session(&session_id, Some(format!("the agent exited ({code:?})")));
            }
        }
        for request in reopening {
            self.reopen_here(request);
        }
        for link in state.home_of {
            if let Some(link_state) = self.links.get_mut(&link) {
                link_state.home = None;
            }
            self.push_out(
                link,
                Out::Frame(LinkFrame::Exited(code)),
                LinkFrame::Exited(code),
            );
        }
        self.agent_requests
            .retain(|_, request| request.process != process);
        self.check_drain();
    }

    /// An older relay was asked for a session it no longer runs: load it here
    /// the ordinary way, from the agent's own record.
    fn reopen_here(&mut self, request: ClientRequest) {
        let (Some(link), Some(session_id)) = (request.link, request.session.clone()) else {
            return;
        };
        let Some(process) = self.home(link) else {
            self.end_session(&session_id, None);
            return self.send_line(link, frame::error(&request.client_id, "no agent on this link"));
        };
        let previous = self.sessions.get(&session_id).map(|session| session.process);
        if let Some(previous) = previous
            && let Some(state) = self.processes.get_mut(&previous)
        {
            state.sessions.remove(&session_id);
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.process = process;
        }
        if let Some(state) = self.processes.get_mut(&process) {
            state.sessions.insert(session_id.clone());
        }
        log::info!("quiet-ui relay: {session_id} is no longer live elsewhere; loading it here");
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request.client_id,
            "method": request.method,
            "params": request.params,
        });
        self.forward(
            process,
            link,
            request.client_id,
            request.method,
            request.session,
            request.params,
            value,
        );
    }

    /// The older relay that ran this session let it go: run it here instead,
    /// picking it up from the agent's own record with `session/resume`.
    fn hand_over(&mut self, session_id: String) {
        let Some(command) = self.sessions.get(&session_id).map(|s| s.command.clone()) else {
            return;
        };
        if !self.init_cache.contains_key(&command.key()) {
            return self.end_session(&session_id, Some("no agent to resume it in".into()));
        }
        let process = self.spawn(AgentSpec::Command(command.clone()), None);
        let request = self.next_id();
        let params = serde_json::json!({
            "sessionId": session_id,
            "cwd": command.cwd.unwrap_or_default(),
            "mcpServers": [],
        });
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.process = process;
            session.turn = Turn::Idle {
                stop_reason: None,
                error: None,
            };
        }
        if let Some(state) = self.processes.get_mut(&process) {
            state.sessions.insert(session_id.clone());
        }
        self.client_requests.insert(
            request,
            ClientRequest {
                process,
                link: None,
                client_id: Value::Null,
                method: frame::SESSION_RESUME.into(),
                session: Some(session_id),
                params: params.clone(),
                purpose: Purpose::HandoverResume,
            },
        );
        self.send_to_process(process, frame::request(request, frame::SESSION_RESUME, params));
    }

    fn home(&mut self, link: LinkId) -> Option<ProcessId> {
        let state = self.links.get(&link)?;
        if let Some(home) = state.home
            && self.processes.contains_key(&home)
        {
            return Some(home);
        }
        let command = state.command.clone()?;
        let process = self.spawn(AgentSpec::Command(command), Some(link));
        if let Some(state) = self.links.get_mut(&link) {
            state.home = Some(process);
        }
        Some(process)
    }

    fn spawn(&mut self, spec: AgentSpec, home_of: Option<LinkId>) -> ProcessId {
        let process = self.next_id();
        let command = match &spec {
            AgentSpec::Command(command) => Some(command.clone()),
            AgentSpec::Adopt { .. } => None,
        };
        let cached_init = command
            .as_ref()
            .and_then(|command| self.init_cache.get(&command.key()))
            .map(|cache| cache.params.clone());
        self.processes.insert(
            process,
            Process {
                // An adopted session's relay answers as an initialized agent.
                ready: command.is_none(),
                command,
                queued: Vec::new(),
                sessions: HashSet::default(),
                home_of: home_of.into_iter().collect(),
                init: None,
                init_waiters: Vec::new(),
            },
        );
        self.outputs.push(Output::Spawn { process, spec });
        if let Some(params) = cached_init {
            let request = self.next_id();
            if let Some(state) = self.processes.get_mut(&process) {
                state.init = Some(request);
            }
            self.client_requests.insert(
                request,
                ClientRequest {
                    process,
                    link: None,
                    client_id: Value::Null,
                    method: frame::INITIALIZE.into(),
                    session: None,
                    params: params.clone(),
                    purpose: Purpose::Initialize,
                },
            );
            self.outputs.push(Output::ToAgent {
                process,
                line: frame::request(request, frame::INITIALIZE, params),
            });
        }
        process
    }

    fn send_to_process(&mut self, process: ProcessId, line: String) {
        let Some(state) = self.processes.get_mut(&process) else {
            return;
        };
        if state.ready {
            self.outputs.push(Output::ToAgent { process, line });
        } else {
            state.queued.push(line);
        }
    }

    fn reap(&mut self, process: ProcessId) {
        let Some(state) = self.processes.get(&process) else {
            return;
        };
        if state.sessions.is_empty() && state.home_of.is_empty() {
            self.processes.remove(&process);
            self.outputs.push(Output::Kill { process });
        }
    }

    // --- Sessions ---

    fn root(&self, session: &str) -> String {
        self.children
            .get(session)
            .cloned()
            .unwrap_or_else(|| session.to_string())
    }

    fn register_session(
        &mut self,
        session_id: String,
        process: ProcessId,
        link: LinkId,
        params: Value,
        mut command: AgentCommand,
    ) {
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            command.cwd = Some(cwd.to_string());
        }
        if let Some(state) = self.processes.get_mut(&process) {
            state.sessions.insert(session_id.clone());
        }
        let log = SessionLog::create(self.log_dir.as_deref(), &session_id);
        self.sessions.insert(
            session_id,
            Session {
                process,
                link: Some(link),
                log,
                open_result: None,
                open_params: params,
                command,
                turn: Turn::Idle {
                    stop_reason: None,
                    error: None,
                },
                pending: Vec::new(),
                title: None,
            },
        );
    }

    fn end_session(&mut self, session_id: &str, error: Option<String>) {
        let Some(session) = self.sessions.remove(session_id) else {
            return;
        };
        self.children.retain(|_, root| root != session_id);
        if let Some(error) = error
            && let Some(link) = session.link
        {
            let turn = Turn::Idle {
                stop_reason: None,
                error: Some(error),
            };
            self.send_line(link, state_frame(session_id, &turn));
        }
        session.log.delete();
        if let Some(process) = self.processes.get_mut(&session.process) {
            process.sessions.remove(session_id);
            let process = session.process;
            self.reap(process);
        }
    }

    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut sessions = self
            .sessions
            .iter()
            .map(|(id, session)| SessionSummary {
                session_id: id.clone(),
                agent_name: session.command.name.clone(),
                cwd: session
                    .open_params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| session.command.cwd.clone()),
                title: session.title.clone(),
                running: matches!(session.turn, Turn::Running { .. }),
                attached: session
                    .link
                    .and_then(|link| self.links.get(&link))
                    .is_some_and(|link| link.attached),
                command: session.command.clone(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        sessions
    }

    // --- Handing over to a newer relay ---

    /// Sessions an older relay still runs, to be taken over through it when a
    /// client asks for them.
    pub fn add_adoptable(&mut self, socket: PathBuf, sessions: Vec<SessionSummary>) {
        for session in sessions {
            self.adoptable
                .insert(session.session_id, (socket.clone(), session.command));
        }
    }

    /// A newer relay has started: take no new sessions, let every turn in
    /// flight finish (its permission prompts stay answerable), then end each
    /// agent so the newer relay can resume its sessions, and exit.
    pub fn drain(&mut self) {
        self.draining = true;
        self.check_drain();
    }

    fn check_drain(&mut self) {
        if !self.draining {
            return;
        }
        let settled = self
            .processes
            .iter()
            .filter(|(id, process)| {
                process.sessions.iter().all(|session| {
                    self.sessions.get(session).is_none_or(|session| {
                        matches!(session.turn, Turn::Idle { .. }) && session.pending.is_empty()
                    })
                }) && !self
                    .client_requests
                    .values()
                    .any(|request| request.process == **id)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for process in settled {
            if let Some(state) = self.processes.remove(&process) {
                // The log stays on disk for whoever picks the session up.
                for session in state.sessions {
                    let link = self.sessions.remove(&session).and_then(|s| s.link);
                    if let Some(link) = link
                        && self.links.get(&link).is_some_and(|l| l.command.is_none())
                    {
                        self.push_out(
                            link,
                            Out::Frame(LinkFrame::Exited(None)),
                            LinkFrame::Exited(None),
                        );
                    }
                }
                for link in state.home_of {
                    self.push_out(
                        link,
                        Out::Frame(LinkFrame::Exited(None)),
                        LinkFrame::Exited(None),
                    );
                }
                self.outputs.push(Output::Kill { process });
            }
        }
        // An adopting link sees its sessions end the same way.
        let adopting = self
            .links
            .iter()
            .filter(|(_, link)| link.command.is_none())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if self.processes.is_empty() {
            for link in adopting {
                self.push_out(
                    link,
                    Out::Frame(LinkFrame::Exited(None)),
                    LinkFrame::Exited(None),
                );
            }
            self.outputs.push(Output::Exit);
        }
    }
}

fn is_open(request: &ClientRequest) -> bool {
    request.purpose == Purpose::Client
        && (request.method == frame::SESSION_LOAD || request.method == frame::SESSION_RESUME)
}

fn state_frame(session_id: &str, turn: &Turn) -> String {
    let params = match turn {
        Turn::Running { .. } => serde_json::json!({
            "sessionId": session_id,
            "state": "running",
        }),
        Turn::Idle { stop_reason, error } => serde_json::json!({
            "sessionId": session_id,
            "state": "idle",
            "stopReason": stop_reason,
            "error": error,
        }),
    };
    frame::notification(frame::STATE_UPDATE, params)
}
