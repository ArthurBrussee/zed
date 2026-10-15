//! Drives [`RelayState`]: carries out what it decides and feeds it what the
//! agents and clients send. Executor-agnostic, so the daemon runs it on smol
//! and tests run it on gpui's deterministic executor.

use crate::state::{
    AgentCommand, AgentSpec, LinkFrame, LinkId, Output, ProcessId, RelayState, SessionSummary,
};
use anyhow::Result;
use collections::HashMap;
use futures::{
    StreamExt as _,
    channel::mpsc,
    future::LocalBoxFuture,
    stream::LocalBoxStream,
};
use serde_json::Value;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::{Rc, Weak},
};

pub type TaskSpawner = Rc<dyn Fn(LocalBoxFuture<'static, ()>)>;
pub type LinkSink = Rc<dyn Fn(u64, LinkFrame)>;

/// A running agent as the relay sees it: lines in, lines out, and an exit.
pub struct AgentIo {
    pub stdout: LocalBoxStream<'static, String>,
    pub stderr: LocalBoxStream<'static, String>,
    pub stdin: mpsc::UnboundedSender<String>,
    pub exited: LocalBoxFuture<'static, Option<i32>>,
    pub kill: Box<dyn FnOnce()>,
}

pub trait AgentSpawner {
    fn spawn(&self, spec: &AgentSpec) -> Result<AgentIo>;
}

struct ProcessIo {
    stdin: mpsc::UnboundedSender<String>,
    kill: Option<Box<dyn FnOnce()>>,
}

pub struct Relay {
    state: RefCell<RelayState>,
    agents: Rc<dyn AgentSpawner>,
    spawn_task: TaskSpawner,
    sinks: RefCell<HashMap<LinkId, LinkSink>>,
    processes: RefCell<HashMap<ProcessId, ProcessIo>>,
    on_exit: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Relay {
    pub fn new(
        log_dir: Option<PathBuf>,
        agents: Rc<dyn AgentSpawner>,
        spawn_task: TaskSpawner,
    ) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(RelayState::new(log_dir)),
            agents,
            spawn_task,
            sinks: RefCell::default(),
            processes: RefCell::default(),
            on_exit: RefCell::default(),
        })
    }

    pub fn on_exit(&self, callback: impl FnOnce() + 'static) {
        *self.on_exit.borrow_mut() = Some(Box::new(callback));
    }

    fn update<R>(self: &Rc<Self>, f: impl FnOnce(&mut RelayState) -> R) -> R {
        let result = f(&mut self.state.borrow_mut());
        self.flush();
        result
    }

    fn flush(self: &Rc<Self>) {
        loop {
            let outputs = self.state.borrow_mut().take_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                self.perform(output);
            }
        }
    }

    fn perform(self: &Rc<Self>, output: Output) {
        match output {
            Output::ToLink { link, lseq, frame } => {
                let sink = self.sinks.borrow().get(&link).cloned();
                if let Some(sink) = sink {
                    sink(lseq, frame);
                }
            }
            Output::ToAgent { process, line } => {
                if let Some(io) = self.processes.borrow().get(&process) {
                    io.stdin.unbounded_send(line).ok();
                }
            }
            Output::Spawn { process, spec } => self.start(process, spec),
            Output::Kill { process } => {
                let io = self.processes.borrow_mut().remove(&process);
                if let Some(kill) = io.and_then(|mut io| io.kill.take()) {
                    kill();
                }
            }
            Output::ServeFs {
                request,
                method,
                params,
            } => {
                let this = Rc::downgrade(self);
                (self.spawn_task)(Box::pin(async move {
                    let outcome = serve_fs(&method, &params);
                    if let Some(this) = this.upgrade() {
                        this.update(|state| state.fs_served(request, outcome));
                    }
                }));
            }
            Output::Exit => {
                if let Some(on_exit) = self.on_exit.borrow_mut().take() {
                    on_exit();
                }
            }
        }
    }

    fn start(self: &Rc<Self>, process: ProcessId, spec: AgentSpec) {
        let io = match self.agents.spawn(&spec) {
            Ok(io) => io,
            Err(error) => {
                log::error!("quiet-ui relay: could not start {spec:?}: {error:#}");
                let this = Rc::downgrade(self);
                (self.spawn_task)(Box::pin(async move {
                    if let Some(this) = this.upgrade() {
                        this.update(|state| state.agent_exited(process, None));
                    }
                }));
                return;
            }
        };
        log::info!("quiet-ui relay: started agent {process}");
        self.processes.borrow_mut().insert(
            process,
            ProcessIo {
                stdin: io.stdin,
                kill: Some(io.kill),
            },
        );
        let this = Rc::downgrade(self);
        (self.spawn_task)(Box::pin(pump(this, process, io.stdout, io.stderr, io.exited)));
    }

    pub fn open(self: &Rc<Self>, command: AgentCommand, sink: LinkSink) -> Result<LinkId> {
        let link = self.update(|state| state.open_link(command))?;
        self.sinks.borrow_mut().insert(link, sink);
        Ok(link)
    }

    pub fn open_adopting(self: &Rc<Self>, sink: LinkSink) -> LinkId {
        let link = self.update(|state| state.open_adopting_link());
        self.sinks.borrow_mut().insert(link, sink);
        link
    }

    pub fn attach(self: &Rc<Self>, link: LinkId, last_lseq: u64, sink: LinkSink) -> Result<u64> {
        let previous = self.sinks.borrow_mut().insert(link, sink);
        let result = self.update(|state| state.attach_link(link, last_lseq));
        if result.is_err() {
            let mut sinks = self.sinks.borrow_mut();
            match previous {
                Some(previous) => sinks.insert(link, previous),
                None => sinks.remove(&link),
            };
        }
        result
    }

    pub fn send(self: &Rc<Self>, link: LinkId, cseq: u64, line: String) {
        self.update(|state| state.client_line(link, cseq, line));
    }

    pub fn ack(self: &Rc<Self>, link: LinkId, lseq: u64) {
        self.update(|state| state.ack(link, lseq));
    }

    pub fn detach(self: &Rc<Self>, link: LinkId) {
        self.sinks.borrow_mut().remove(&link);
        self.update(|state| state.detach_link(link));
    }

    pub fn close(self: &Rc<Self>, link: LinkId) {
        self.sinks.borrow_mut().remove(&link);
        self.update(|state| state.close_link(link));
    }

    pub fn sessions(&self) -> Vec<SessionSummary> {
        self.state.borrow().sessions()
    }

    pub fn drain(self: &Rc<Self>) {
        log::info!("quiet-ui relay: a newer relay started; draining");
        self.update(|state| state.drain());
    }

    pub fn add_adoptable(self: &Rc<Self>, socket: PathBuf, sessions: Vec<SessionSummary>) {
        self.update(|state| state.add_adoptable(socket, sessions));
    }
}

async fn pump(
    relay: Weak<Relay>,
    process: ProcessId,
    stdout: LocalBoxStream<'static, String>,
    stderr: LocalBoxStream<'static, String>,
    exited: LocalBoxFuture<'static, Option<i32>>,
) {
    enum Event {
        Line(String),
        Stderr(String),
    }
    let mut events = futures::stream::select(
        stdout.map(Event::Line),
        stderr.map(Event::Stderr),
    );
    while let Some(event) = events.next().await {
        let Some(relay) = relay.upgrade() else {
            return;
        };
        match event {
            Event::Line(line) => relay.update(|state| state.agent_line(process, line)),
            Event::Stderr(line) => relay.update(|state| state.agent_stderr(process, line)),
        }
    }
    let code = exited.await;
    if let Some(relay) = relay.upgrade() {
        log::info!("quiet-ui relay: agent {process} exited ({code:?})");
        relay.processes.borrow_mut().remove(&process);
        relay.update(|state| state.agent_exited(process, code));
    }
}

/// Answers the agent's file requests from this machine's disk, as the
/// project would, while no client is attached to answer them.
fn serve_fs(method: &str, params: &Value) -> Result<Value, String> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing path".to_string())?;
    match method {
        crate::frame::FS_READ => {
            let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
            let start = params
                .get("line")
                .and_then(Value::as_u64)
                .map_or(0, |line| line.saturating_sub(1) as usize);
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|limit| limit as usize);
            let content = if start == 0 && limit.is_none() {
                content
            } else {
                let lines = content.split_inclusive('\n').skip(start);
                match limit {
                    Some(limit) => lines.take(limit).collect(),
                    None => lines.collect(),
                }
            };
            Ok(serde_json::json!({ "content": content }))
        }
        crate::frame::FS_WRITE => {
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            std::fs::write(path, content).map_err(|error| error.to_string())?;
            Ok(Value::Null)
        }
        _ => Err(format!("{method} is not served while Zed is away")),
    }
}
