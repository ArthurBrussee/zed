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
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub type LinkId = u64;
pub type ProcessId = u64;

/// How long a session stays open at the agent with nothing happening in it.
/// Arthur's pick: shorter would close sessions he comes back to through the
/// day.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// How long a prompt that lost the OAuth refresh race waits before its one
/// retry, long enough for the CLI that won it to finish.
pub const OAUTH_RETRY_DELAY: Duration = Duration::from_secs(2);

/// How long a draining relay waits on a session nothing happens in: a
/// permission nobody answers, or a turn that stopped streaming. Past it the
/// session is let go and the newer relay resumes it; on 10-21 one relay
/// drained for twelve hours on an unanswered request.
pub const DRAIN_QUIET: Duration = Duration::from_secs(10 * 60);

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

type AgentKey = (String, Vec<String>);

impl AgentCommand {
    fn key(&self) -> AgentKey {
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
    /// A line for the agent once `after` has passed; the driver calls
    /// [`RelayState::send_later_due`] then.
    SendLater {
        process: ProcessId,
        line: String,
        after: Duration,
    },
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
    /// The load or resume in flight. Each one starts a `claude` CLI, and
    /// several starting at once race on refreshing the OAuth token, so they
    /// go to the agent one at a time.
    opening: Option<u64>,
    open_queue: VecDeque<(u64, String)>,
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

/// Whether the agent holds the session open. One left idle is closed there,
/// which ends its `claude` CLI, and resumed when a client next asks for it.
/// Client requests for a session the agent is still opening, closing or
/// resuming wait for that to finish.
enum Life {
    Opening { waiting: Vec<Waiting> },
    Open,
    Closing { waiting: Vec<Waiting> },
    Closed,
    Resuming { waiting: Vec<Waiting> },
}

struct Waiting {
    link: LinkId,
    line: String,
}

struct Session {
    process: ProcessId,
    /// Every link the session was opened or loaded on; its updates go to
    /// all of them.
    links: Vec<LinkId>,
    /// The link whose prompt started the latest turn.
    driver: Option<LinkId>,
    log: SessionLog,
    open_result: Option<Value>,
    open_params: Value,
    command: AgentCommand,
    turn: Turn,
    /// Requests from the agent still waiting for an answer, with the log
    /// frame that carries each.
    pending: Vec<(u64, u64)>,
    title: Option<String>,
    life: Life,
    /// Something happened in the session since the last idle sweep.
    touched: bool,
    idle_since: Option<Instant>,
    /// Async tasks and subagents the agent reported and has not finished.
    background: HashSet<String>,
}

#[derive(PartialEq)]
enum Purpose {
    Client,
    Initialize,
    IdleClose,
    Resume,
    /// Setting a session's config again after a resume.
    Restore,
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
    init_cache: HashMap<AgentKey, InitCache>,
    /// The one process each agent runs in, whichever link its sessions came
    /// from: claude-agent-acp and codex-acp serve many sessions on one
    /// connection.
    agents: HashMap<AgentKey, ProcessId>,
    adoptable: HashMap<String, (PathBuf, AgentCommand)>,
    /// Prompts already sent again after losing the OAuth refresh race.
    retried_prompts: HashSet<u64>,
    draining: bool,
    /// The latest sweep while draining, which [`DRAIN_QUIET`] is measured
    /// against.
    drain_swept_at: Option<Instant>,
    idle_timeout: Duration,
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
            agents: HashMap::default(),
            adoptable: HashMap::default(),
            retried_prompts: HashSet::default(),
            draining: false,
            drain_swept_at: None,
            idle_timeout: IDLE_TIMEOUT,
            outputs: Vec::new(),
        }
    }

    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.idle_timeout = timeout;
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
        let resent = pending.len();
        let mut skipped = 0;
        for (lseq, frame) in pending {
            // The client releases frames strictly in order, so a number with
            // nothing left to send still gets a frame, one it only logs.
            let frame = frame.unwrap_or_else(|| {
                skipped += 1;
                LinkFrame::Stderr(format!("quiet-ui relay: frame {lseq} is no longer current"))
            });
            self.outputs.push(Output::ToLink { link, lseq, frame });
        }
        log::info!(
            "quiet-ui relay: link {link} reattached after frame {last_lseq}, resending {resent}, \
             {skipped} of them no longer current"
        );
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
            session.links.retain(|other| *other != link);
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

    fn is_attached(&self, link: LinkId) -> bool {
        self.links.get(&link).is_some_and(|link| link.attached)
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
                    // Nothing runs in a session the agent does not hold open.
                    Some(session) if !matches!(session.life, Life::Open) => return,
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
            && let Some(state) = self.sessions.get_mut(session_id)
        {
            state.touched = true;
            match &mut state.life {
                Life::Open => {}
                Life::Opening { waiting }
                | Life::Closing { waiting }
                | Life::Resuming { waiting } => {
                    return waiting.push(Waiting {
                        link,
                        line: value.to_string(),
                    });
                }
                Life::Closed if method == frame::SESSION_CLOSE => {
                    self.send_line(link, frame::result(&id, serde_json::json!({})));
                    return self.end_session(session_id, None);
                }
                Life::Closed if self.draining => {
                    return self.send_line(
                        link,
                        frame::error(&id, "this relay is handing over to a newer one"),
                    );
                }
                Life::Closed => {
                    let waiting = vec![Waiting {
                        link,
                        line: value.to_string(),
                    }];
                    return self.resume(session_id.clone(), waiting);
                }
            }
            if opening {
                let replay = method == frame::SESSION_LOAD;
                return self.attach_session(link, session_id.clone(), id, replay);
            }
            let process = self.sessions[session_id].process;
            return self.forward(process, link, id, method, session, params, value);
        }

        if self.draining && (opening || method == frame::SESSION_NEW) {
            log::info!("quiet-ui relay: refusing {method} on link {link}: draining");
            return self.send_line(
                link,
                frame::error(&id, "this relay is handing over to a newer one"),
            );
        }

        if let Some(session_id) = &session
            && opening
        {
            let (process, command) = if let Some((socket, command)) =
                self.adoptable.remove(session_id)
            {
                let process = self.spawn(AgentSpec::Adopt { socket });
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
            if let Some(state) = self.sessions.get_mut(session_id) {
                state.life = Life::Opening {
                    waiting: Vec::new(),
                };
            }
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
            && let Some(session_id) = session.clone()
            && let Some(state) = self.sessions.get_mut(&session_id)
        {
            state.turn = Turn::Running { prompt: id };
            state.driver = Some(link);
            let running = state_frame(&session_id, &state.turn);
            let others = state
                .links
                .iter()
                .copied()
                .filter(|other| *other != link)
                .collect::<Vec<_>>();
            // The agent does not echo a prompt, so the log keeps it the way
            // the agent replays one, and the session's other links see it.
            let prompted = frame::session_id(&value).unwrap_or(&session_id).to_string();
            for chunk in frame::user_message_chunks(&prompted, &params) {
                self.session_frame(&session_id, chunk, |to| to != link);
            }
            for other in others {
                self.send_line(other, running.clone());
            }
        }
        let opening = method == frame::SESSION_LOAD || method == frame::SESSION_RESUME;
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
        let line = frame::with_id(value, Value::from(id));
        if opening {
            self.send_open(process, id, line);
        } else {
            self.send_to_process(process, line);
        }
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
        if self.draining {
            return self.send_line(
                link,
                frame::error(&id, "this relay is handing over to a newer one"),
            );
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
    /// Links already on the session keep getting its updates as well.
    fn attach_session(&mut self, link: LinkId, session_id: String, id: Value, replay: bool) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
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
            .filter(|(request, _)| self.shows_pending(session, *request, link))
            .filter_map(|(request, seq)| Some((*request, *seq, session.log.frame(*seq)?)))
            .collect::<Vec<_>>();
        let state = state_frame(&session_id, &session.turn);
        let others = session
            .links
            .iter()
            .copied()
            .filter(|other| *other != link)
            .collect::<Vec<_>>();
        log::info!(
            "quiet-ui relay: link {link} attached to {session_id}, replaying {} frames and {} \
             pending requests; it also streams to links {others:?}",
            frames.len(),
            pending.len(),
        );
        if let Some(session) = self.sessions.get_mut(&session_id)
            && !session.links.contains(&link)
        {
            session.links.push(link);
        }
        for (request, _, _) in &pending {
            if let Some(request) = self.agent_requests.get_mut(request)
                && request.method != frame::REQUEST_PERMISSION
            {
                request.delivered_to = Some(link);
            }
        }

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
        for (_, seq, line) in pending {
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

    /// Whether a request the agent still waits on is shown to a link
    /// attaching to its session: a permission question goes to every link,
    /// anything the client acts on (a file, a terminal) stays with the link it
    /// went to while that link is there.
    fn shows_pending(&self, session: &Session, request: u64, link: LinkId) -> bool {
        let Some(request) = self.agent_requests.get(&request) else {
            return true;
        };
        request.method == frame::REQUEST_PERMISSION
            || request.delivered_to.is_none_or(|to| {
                to == link || !session.links.contains(&to) || !self.is_attached(to)
            })
    }

    /// The link the agent's requests to act on the client's side go to: the
    /// one whose prompt started the turn while it is there, else the newest
    /// one attached.
    fn driver(&self, session: &Session) -> Option<LinkId> {
        let member = session.driver.filter(|link| session.links.contains(link));
        member
            .filter(|link| self.is_attached(*link))
            .or_else(|| {
                session
                    .links
                    .iter()
                    .rev()
                    .copied()
                    .find(|link| self.is_attached(*link))
            })
            .or(member)
            .or_else(|| session.links.last().copied())
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
                if let Some((work, running)) = frame::background_work(&value)
                    && let Some(state) = self.sessions.get_mut(&session)
                {
                    if running {
                        state.background.insert(work);
                    } else {
                        state.background.remove(&work);
                    }
                }
                if let Some((parent, key, config)) = frame::config_update(&value)
                    && let Some(state) = self.sessions.get_mut(&session)
                {
                    track_config(&mut state.open_result, parent, key, config);
                }
                // A turn an older relay runs is known here only by what it
                // reports, and this relay says when it ends if that one cannot.
                if let Some(running) = frame::relay_turn_running(&value)
                    && self
                        .processes
                        .get(&process)
                        .is_some_and(|process| process.command.is_none())
                    && let Some(state) = self.sessions.get_mut(&session)
                {
                    state.turn = if running {
                        Turn::Running { prompt: 0 }
                    } else {
                        Turn::Idle {
                            stop_reason: value["params"]["stopReason"].as_str().map(Into::into),
                            error: value["params"]["error"].as_str().map(Into::into),
                        }
                    };
                }
                self.session_frame(&session, line, |_| true);
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

    /// Logs a frame of the session's and sends it to those of its links `to`
    /// picks.
    fn session_frame(
        &mut self,
        root: &str,
        line: String,
        to: impl Fn(LinkId) -> bool,
    ) -> Option<u64> {
        let session = self.sessions.get_mut(root)?;
        session.touched = true;
        let seq = session.log.append(&line);
        let links = session
            .links
            .iter()
            .copied()
            .filter(|link| to(*link))
            .collect::<Vec<_>>();
        for link in links {
            self.push_out(
                link,
                Out::Session {
                    root: root.to_string(),
                    seq,
                },
                LinkFrame::Line(line.clone()),
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
        if let Some(id) = id.as_u64() {
            self.open_finished(process, id);
        }
        let result = value.get("result").cloned();
        let error = value.get("error").cloned();
        if request.method == frame::SESSION_PROMPT
            && request.purpose == Purpose::Client
            && let Some(id) = id.as_u64()
            && !self.retried_prompts.remove(&id)
            && error.as_ref().is_some_and(lost_the_oauth_refresh_race)
        {
            return self.retry_prompt(process, id, request);
        }
        match request.purpose {
            Purpose::Initialize => return self.initialized(process, request, result, value),
            Purpose::IdleClose => return self.idle_closed(request, result.is_some()),
            Purpose::Resume => return self.resumed(request, result),
            Purpose::Restore => return,
            Purpose::Client => {}
        }

        let mut opened = None;
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
                        self.record_open_result(session_id, result.clone(), request.link);
                        opened = Some(session_id.clone());
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
                if let Some(id) = id.as_u64() {
                    self.retried_prompts.remove(&id);
                }
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
                        let state = state_frame(session_id, &session.turn);
                        let others = session
                            .links
                            .iter()
                            .copied()
                            .filter(|link| Some(*link) != request.link)
                            .collect::<Vec<_>>();
                        for link in others {
                            self.send_line(link, state.clone());
                        }
                    }
                }
            }
            frame::SESSION_SET_MODE
            | frame::SESSION_SET_MODEL
            | frame::SESSION_SET_CONFIG_OPTION => {
                if let Some(result) = &result
                    && let Some(session) = request
                        .session
                        .as_ref()
                        .and_then(|s| self.sessions.get_mut(s))
                {
                    let change = match request.method.as_str() {
                        frame::SESSION_SET_MODE => request
                            .params
                            .get("modeId")
                            .map(|mode| (Some("modes"), "currentModeId", mode.clone())),
                        frame::SESSION_SET_MODEL => request
                            .params
                            .get("modelId")
                            .map(|model| (Some("models"), "currentModelId", model.clone())),
                        _ => result
                            .get("configOptions")
                            .map(|options| (None, "configOptions", options.clone())),
                    };
                    if let Some((parent, key, config)) = change {
                        track_config(&mut session.open_result, parent, key, config);
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
        if let Some(session_id) = opened {
            self.open_again(&session_id);
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
            // The next link starts a fresh one rather than joining this.
            self.agents.retain(|_, id| *id != process);
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
        let link = match session.as_ref().and_then(|s| self.sessions.get(s)) {
            Some(session) => self.driver(session),
            None => self.processes.get(&process).and_then(|process| {
                process
                    .home_of
                    .iter()
                    .copied()
                    .find(|link| self.is_attached(*link))
            }),
        };
        let attached = link.is_some_and(|link| self.is_attached(link));
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
                let everyone = method == frame::REQUEST_PERMISSION;
                if let Some(seq) =
                    self.session_frame(&session, line, |to| everyone || Some(to) == link)
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
        let Some(state) = self.remove_process(process) else {
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
        let mut closed = Vec::new();
        for id in orphaned {
            let Some(request) = self.client_requests.remove(&id) else {
                continue;
            };
            if request.purpose == Purpose::IdleClose {
                closed.push(request);
            } else if adopted && is_open(&request) {
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
                .chain(&closed)
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
        // An agent that exits while closing a session has closed it.
        for request in closed {
            self.idle_closed(request, true);
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
        self.forget_unanswerable();
        self.check_drain();
    }

    /// Drops what sessions still wait on from an agent that has gone: nobody
    /// can answer it, and a reload would show it as pending again.
    fn forget_unanswerable(&mut self) {
        for session in self.sessions.values_mut() {
            let gone = session
                .pending
                .iter()
                .map(|(request, _)| *request)
                .filter(|request| !self.agent_requests.contains_key(request))
                .collect::<Vec<_>>();
            for request in gone {
                session.log.mark_answered(request);
                session.pending.retain(|(pending, _)| *pending != request);
            }
        }
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
        if let Some(session) = self.sessions.get_mut(&session_id) {
            let was_running = matches!(session.turn, Turn::Running { .. });
            session.turn = Turn::Idle {
                stop_reason: None,
                error: None,
            };
            if was_running {
                let state = state_frame(&session_id, &session.turn);
                for link in session.links.clone() {
                    self.send_line(link, state.clone());
                }
            }
        }
        self.resume(session_id, Vec::new());
    }

    fn home(&mut self, link: LinkId) -> Option<ProcessId> {
        let state = self.links.get(&link)?;
        if let Some(home) = state.home
            && self.processes.contains_key(&home)
        {
            return Some(home);
        }
        let command = state.command.clone()?;
        let process = self.agent_process(command);
        if let Some(state) = self.links.get_mut(&link) {
            state.home = Some(process);
        }
        if let Some(state) = self.processes.get_mut(&process) {
            state.home_of.insert(link);
        }
        log::info!("quiet-ui relay: link {link} runs in agent {process}");
        Some(process)
    }

    /// The process `command`'s agent runs in, started if it is not running.
    fn agent_process(&mut self, command: AgentCommand) -> ProcessId {
        if let Some(&process) = self.agents.get(&command.key())
            && self.processes.contains_key(&process)
        {
            return process;
        }
        let key = command.key();
        let process = self.spawn(AgentSpec::Command(command));
        self.agents.insert(key, process);
        process
    }

    fn remove_process(&mut self, process: ProcessId) -> Option<Process> {
        self.agents.retain(|_, id| *id != process);
        self.processes.remove(&process)
    }

    fn spawn(&mut self, spec: AgentSpec) -> ProcessId {
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
                home_of: HashSet::default(),
                init: None,
                init_waiters: Vec::new(),
                opening: None,
                open_queue: VecDeque::new(),
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

    fn send_open(&mut self, process: ProcessId, request: u64, line: String) {
        let Some(state) = self.processes.get_mut(&process) else {
            return;
        };
        if state.opening.is_some() {
            state.open_queue.push_back((request, line));
            return;
        }
        state.opening = Some(request);
        self.send_to_process(process, line);
    }

    fn open_finished(&mut self, process: ProcessId, request: u64) {
        let Some(state) = self.processes.get_mut(&process) else {
            return;
        };
        if state.opening != Some(request) {
            return;
        }
        state.opening = None;
        if let Some((next, line)) = state.open_queue.pop_front() {
            self.send_open(process, next, line);
        }
    }

    /// Sends a prompt that lost the OAuth refresh race once more, a little
    /// later, as the same turn.
    fn retry_prompt(&mut self, process: ProcessId, failed: u64, request: ClientRequest) {
        let id = self.next_id();
        log::info!(
            "quiet-ui relay: a prompt lost the OAuth refresh race to another CLI; retrying it"
        );
        if let Some(session) = request
            .session
            .as_ref()
            .and_then(|session| self.sessions.get_mut(session))
            && matches!(session.turn, Turn::Running { prompt } if prompt == failed)
        {
            session.turn = Turn::Running { prompt: id };
        }
        let line = frame::request(id, frame::SESSION_PROMPT, request.params.clone());
        self.client_requests.insert(id, request);
        self.retried_prompts.insert(id);
        self.outputs.push(Output::SendLater {
            process,
            line,
            after: OAUTH_RETRY_DELAY,
        });
    }

    pub fn send_later_due(&mut self, process: ProcessId, line: String) {
        self.send_to_process(process, line);
    }

    fn reap(&mut self, process: ProcessId) {
        let Some(state) = self.processes.get(&process) else {
            return;
        };
        if state.sessions.is_empty() && state.home_of.is_empty() {
            self.remove_process(process);
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
                links: vec![link],
                driver: Some(link),
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
                life: Life::Open,
                touched: true,
                idle_since: None,
                background: HashSet::default(),
            },
        );
    }

    fn end_session(&mut self, session_id: &str, error: Option<String>) {
        let Some(session) = self.sessions.remove(session_id) else {
            return;
        };
        self.children.retain(|_, root| root != session_id);
        if let Life::Opening { waiting } | Life::Closing { waiting } | Life::Resuming { waiting } =
            session.life
        {
            self.fail_waiting(waiting, error.as_deref().unwrap_or("the session ended"));
        }
        if let Some(error) = error {
            let turn = Turn::Idle {
                stop_reason: None,
                error: Some(error),
            };
            let state = state_frame(session_id, &turn);
            for link in &session.links {
                self.send_line(*link, state.clone());
            }
        }
        session.log.retire();
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
                attached: session.links.iter().any(|link| self.is_attached(*link)),
                command: session.command.clone(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        sessions
    }

    // --- Closing idle sessions ---

    /// Closes, at the agent, every session nothing has happened in for the
    /// idle timeout. Called periodically with the time; a session counts as
    /// idle from the first sweep that finds nothing new in it.
    pub fn close_idle(&mut self, now: Instant) {
        if self.draining {
            return self.drain_sweep(now);
        }
        let in_flight = self
            .client_requests
            .values()
            .filter_map(|request| request.session.clone())
            .collect::<HashSet<_>>();
        let mut expired = Vec::new();
        for (session_id, session) in &mut self.sessions {
            let adopted = self
                .processes
                .get(&session.process)
                .is_some_and(|process| process.command.is_none());
            let busy = session.touched
                || adopted
                || !matches!(session.life, Life::Open)
                || matches!(session.turn, Turn::Running { .. })
                || !session.pending.is_empty()
                || !session.background.is_empty()
                || in_flight.contains(session_id);
            session.touched = false;
            if busy {
                session.idle_since = None;
                continue;
            }
            let idle_since = *session.idle_since.get_or_insert(now);
            if now.saturating_duration_since(idle_since) >= self.idle_timeout {
                expired.push(session_id.clone());
            }
        }
        for session_id in expired {
            self.close_idle_session(session_id);
        }
    }

    /// Marks when each session last had something happen in it, so draining
    /// stops waiting on the ones quiet for [`DRAIN_QUIET`].
    fn drain_sweep(&mut self, now: Instant) {
        for session in self.sessions.values_mut() {
            if std::mem::take(&mut session.touched) {
                session.idle_since = None;
            } else {
                session.idle_since.get_or_insert(now);
            }
        }
        self.drain_swept_at = Some(now);
        self.check_drain();
    }

    fn close_idle_session(&mut self, session_id: String) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        log::info!(
            "quiet-ui relay: closing {session_id}, idle for {} minutes",
            self.idle_timeout.as_secs() / 60
        );
        session.life = Life::Closing {
            waiting: Vec::new(),
        };
        let process = session.process;
        let params = serde_json::json!({ "sessionId": session_id });
        let request = self.next_id();
        self.client_requests.insert(
            request,
            ClientRequest {
                process,
                link: None,
                client_id: Value::Null,
                method: frame::SESSION_CLOSE.into(),
                session: Some(session_id),
                params: params.clone(),
                purpose: Purpose::IdleClose,
            },
        );
        self.send_to_process(
            process,
            frame::request(request, frame::SESSION_CLOSE, params),
        );
    }

    fn idle_closed(&mut self, request: ClientRequest, closed: bool) {
        let Some(session_id) = request.session else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Life::Closing { waiting } = std::mem::replace(&mut session.life, Life::Open) else {
            return;
        };
        if !closed {
            log::warn!("quiet-ui relay: the agent would not close {session_id}; keeping it open");
            for waiting in waiting {
                self.handle_client_line(waiting.link, waiting.line);
            }
            return;
        }
        session.life = Life::Closed;
        let process = session.process;
        if let Some(state) = self.processes.get_mut(&process) {
            state.sessions.remove(&session_id);
        }
        if !waiting.is_empty() {
            self.resume(session_id, waiting);
        }
        self.reap(process);
    }

    /// Opens a session the agent no longer holds (closed while idle, or let
    /// go by an older relay), then serves what was waiting for it.
    fn resume(&mut self, session_id: String, waiting: Vec<Waiting>) {
        let Some(command) = self.sessions.get(&session_id).map(|s| s.command.clone()) else {
            return;
        };
        if !self.init_cache.contains_key(&command.key()) {
            let error = "no agent to resume the session in";
            self.fail_waiting(waiting, error);
            return self.end_session(&session_id, Some(error.into()));
        }
        let process = self.agent_process(command);
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        log::info!("quiet-ui relay: resuming {session_id} in agent {process}");
        session.process = process;
        session.life = Life::Resuming { waiting };
        let mut params = match &session.open_params {
            Value::Object(params) => params.clone(),
            _ => Default::default(),
        };
        params.insert("sessionId".into(), Value::from(session_id.clone()));
        let params = Value::Object(params);
        if let Some(state) = self.processes.get_mut(&process) {
            state.sessions.insert(session_id.clone());
        }
        let request = self.next_id();
        self.client_requests.insert(
            request,
            ClientRequest {
                process,
                link: None,
                client_id: Value::Null,
                method: frame::SESSION_RESUME.into(),
                session: Some(session_id),
                params: params.clone(),
                purpose: Purpose::Resume,
            },
        );
        self.send_open(
            process,
            request,
            frame::request(request, frame::SESSION_RESUME, params),
        );
    }

    fn resumed(&mut self, request: ClientRequest, result: Option<Value>) {
        let Some(session_id) = request.session else {
            return;
        };
        let Some(result) = result else {
            log::warn!("quiet-ui relay: the agent could not resume {session_id}");
            return self.end_session(&session_id, Some("the session could not be resumed".into()));
        };
        let restore = self
            .sessions
            .get(&session_id)
            .map(|session| config_to_restore(&session_id, session.open_result.as_ref(), &result))
            .unwrap_or_default();
        // What the session has once the config below is set again, so a load
        // of a session this relay resumed by itself is answered in full.
        let mut recorded = Some(result);
        for (method, params) in &restore {
            match *method {
                frame::SESSION_SET_MODE => track_config(
                    &mut recorded,
                    Some("modes"),
                    "currentModeId",
                    params["modeId"].clone(),
                ),
                frame::SESSION_SET_MODEL => track_config(
                    &mut recorded,
                    Some("models"),
                    "currentModelId",
                    params["modelId"].clone(),
                ),
                _ => set_option_value(&mut recorded, &params["configId"], params["value"].clone()),
            }
        }
        if let Some(recorded) = recorded {
            self.record_open_result(&session_id, recorded, None);
        }
        for (method, params) in restore {
            log::info!("quiet-ui relay: {method} on {session_id} again after resuming it");
            let id = self.next_id();
            self.client_requests.insert(
                id,
                ClientRequest {
                    process: request.process,
                    link: None,
                    client_id: Value::Null,
                    method: method.into(),
                    session: Some(session_id.clone()),
                    params: params.clone(),
                    purpose: Purpose::Restore,
                },
            );
            self.send_to_process(request.process, frame::request(id, method, params));
        }
        self.open_again(&session_id);
    }

    /// Keeps what a load of the session is answered with. The links already
    /// on it are sent the config when it was not known before, except
    /// `answered`, which has it in the response.
    fn record_open_result(&mut self, session_id: &str, result: Value, answered: Option<LinkId>) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        let known = session.open_result.as_ref().is_some_and(has_config);
        session.open_result = Some(result);
        let Some(result) = session
            .open_result
            .as_ref()
            .filter(|result| has_config(result))
        else {
            return;
        };
        if known {
            return;
        }
        let mut params = result.clone();
        params["sessionId"] = Value::from(session_id);
        let line = frame::notification(frame::SESSION_CONFIG, params);
        let links = session
            .links
            .iter()
            .copied()
            .filter(|link| Some(*link) != answered)
            .collect::<Vec<_>>();
        if !links.is_empty() {
            log::info!("quiet-ui relay: sending {session_id}'s config to links {links:?}");
        }
        for link in links {
            self.send_line(link, line.clone());
        }
    }

    /// The agent holds the session open: serve what waited for that.
    fn open_again(&mut self, session_id: &str) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        let waiting = match std::mem::replace(&mut session.life, Life::Open) {
            Life::Opening { waiting } | Life::Resuming { waiting } => waiting,
            life => {
                session.life = life;
                return;
            }
        };
        for waiting in waiting {
            self.handle_client_line(waiting.link, waiting.line);
        }
    }

    fn fail_waiting(&mut self, waiting: Vec<Waiting>, message: &str) {
        for waiting in waiting {
            if let Some((_, Kind::Request { id, .. })) = frame::parse(&waiting.line) {
                self.send_line(waiting.link, frame::error(&id, message));
            }
        }
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
        let quiet = |session: &Session| {
            self.drain_swept_at
                .zip(session.idle_since)
                .is_some_and(|(now, since)| now.saturating_duration_since(since) >= DRAIN_QUIET)
        };
        let settled = self
            .processes
            .iter()
            .filter(|(id, process)| {
                process.sessions.iter().all(|session| {
                    self.sessions.get(session).is_none_or(|session| {
                        (matches!(session.turn, Turn::Idle { .. }) && session.pending.is_empty())
                            || quiet(session)
                    })
                }) && !self.client_requests.values().any(|request| {
                    request.process == **id
                        && !request
                            .session
                            .as_ref()
                            .and_then(|session| self.sessions.get(session))
                            .is_some_and(quiet)
                })
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for process in settled {
            if let Some(state) = self.processes.get(&process) {
                for session_id in &state.sessions {
                    if let Some(session) = self.sessions.get(session_id)
                        && (!matches!(session.turn, Turn::Idle { .. })
                            || !session.pending.is_empty())
                    {
                        log::info!(
                            "quiet-ui relay: letting {session_id} go with its turn or {} \
                             pending requests: nothing has happened in it for {} minutes",
                            session.pending.len(),
                            DRAIN_QUIET.as_secs() / 60
                        );
                    }
                }
            }
            self.agent_requests
                .retain(|_, request| request.process != process);
            self.client_requests
                .retain(|_, request| request.process != process);
            if let Some(state) = self.remove_process(process) {
                // The log stays on disk for whoever picks the session up.
                for session in state.sessions {
                    let links = self
                        .sessions
                        .remove(&session)
                        .map(|session| session.links)
                        .unwrap_or_default();
                    for link in links {
                        if self.links.get(&link).is_some_and(|l| l.command.is_none()) {
                            self.push_out(
                                link,
                                Out::Frame(LinkFrame::Exited(None)),
                                LinkFrame::Exited(None),
                            );
                        }
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

/// "Failed to refresh OAuth token: another Claude Code process is refreshing
/// it", from a CLI that started while another was refreshing.
fn lost_the_oauth_refresh_race(error: &Value) -> bool {
    error
        .to_string()
        .contains("another Claude Code process is refreshing")
}

fn is_open(request: &ClientRequest) -> bool {
    request.purpose == Purpose::Client
        && (request.method == frame::SESSION_LOAD || request.method == frame::SESSION_RESUME)
}

/// Keeps what a load answers with up to date as the config changes, so a
/// reload shows the mode, model and options the session has now rather than
/// those it opened with.
///
/// An agent that offers its mode or model both ways (claude-agent-acp lists
/// them among its config options too) reports a change through one of them,
/// and the other is kept in step: left stale, a resume would set the stale
/// one again after the current one, and a thread in `bypassPermissions` would
/// come back asking for permission.
fn track_config(open_result: &mut Option<Value>, parent: Option<&str>, key: &str, value: Value) {
    let Some(Value::Object(result)) = open_result else {
        return;
    };
    match (parent, key) {
        (Some("modes"), "currentModeId") => set_category_value(result, "mode", &value),
        (Some("models"), "currentModelId") => set_category_value(result, "model", &value),
        (None, "configOptions") => {
            for (category, parent, key) in [
                ("mode", "modes", "currentModeId"),
                ("model", "models", "currentModelId"),
            ] {
                let current = value.as_array().and_then(|options| {
                    options
                        .iter()
                        .find(|option| is_category(option, category))?
                        .get("currentValue")
                        .cloned()
                });
                if let Some(current) = current
                    && let Some(Value::Object(parent)) = result.get_mut(parent)
                    && parent.contains_key(key)
                {
                    parent.insert(key.into(), current);
                }
            }
        }
        _ => {}
    }
    let target = match parent {
        Some(parent) => match result.get_mut(parent) {
            Some(Value::Object(parent)) => parent,
            _ => return,
        },
        None => result,
    };
    target.insert(key.into(), value);
}

fn is_category(option: &Value, category: &str) -> bool {
    option.get("category").and_then(Value::as_str) == Some(category)
        || option.get("id").and_then(Value::as_str) == Some(category)
}

fn set_category_value(result: &mut serde_json::Map<String, Value>, category: &str, value: &Value) {
    if let Some(Value::Array(options)) = result.get_mut("configOptions") {
        for option in options
            .iter_mut()
            .filter(|option| is_category(option, category))
        {
            option["currentValue"] = value.clone();
        }
    }
}

fn set_option_value(open_result: &mut Option<Value>, id: &Value, value: Value) {
    let options = open_result
        .as_ref()
        .and_then(|result| result.get("configOptions"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .cloned()
                .map(|mut option| {
                    if option.get("id") == Some(id) {
                        option["currentValue"] = value.clone();
                    }
                    option
                })
                .collect::<Vec<_>>()
        });
    if let Some(options) = options {
        track_config(open_result, None, "configOptions", Value::Array(options));
    }
}

/// Whether a load answered with `result` gives the client controls to show.
fn has_config(result: &Value) -> bool {
    result.get("modes").is_some_and(Value::is_object)
        || result.get("models").is_some_and(Value::is_object)
        || result
            .get("configOptions")
            .and_then(Value::as_array)
            .is_some_and(|options| !options.is_empty())
}

/// What to set again after a resume so the agent matches what the session's
/// clients were last told, should the resume have put the mode, model or a
/// config option back to its default.
fn config_to_restore(
    session_id: &str,
    tracked: Option<&Value>,
    resumed: &Value,
) -> Vec<(&'static str, Value)> {
    let Some(tracked) = tracked else {
        return Vec::new();
    };
    let mut restore = Vec::new();
    let mut compare = |parent: &str, key: &str, method: &'static str, param: &str| {
        let current = |value: &Value| value.get(parent)?.get(key).cloned();
        if let (Some(want), Some(have)) = (current(tracked), current(resumed))
            && want != have
        {
            let mut params = serde_json::json!({ "sessionId": session_id });
            params[param] = want;
            restore.push((method, params));
        }
    };
    compare("modes", "currentModeId", frame::SESSION_SET_MODE, "modeId");
    compare(
        "models",
        "currentModelId",
        frame::SESSION_SET_MODEL,
        "modelId",
    );
    let options = |value: &Value| {
        value
            .get("configOptions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let resumed_options = options(resumed);
    for option in options(tracked) {
        let (Some(id), Some(want)) = (option.get("id"), option.get("currentValue")) else {
            continue;
        };
        let have = resumed_options
            .iter()
            .find(|resumed| resumed.get("id") == Some(id))
            .and_then(|resumed| resumed.get("currentValue"));
        if have.is_some_and(|have| have != want) {
            restore.push((
                frame::SESSION_SET_CONFIG_OPTION,
                serde_json::json!({ "sessionId": session_id, "configId": id, "value": want }),
            ));
        }
    }
    restore
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
