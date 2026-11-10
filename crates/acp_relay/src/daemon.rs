//! The relay as a process of its own on the box, and the client a
//! `remote_server` uses to reach it.
//!
//! `remote_server` runs one process per Zed window and is replaced whenever
//! that window reconnects from scratch, so an agent it owned would die with it.
//! The daemon outlives them all: it is started by the first server that needs
//! it, listens on a socket named for its build, and keeps running for as long
//! as the box does. A daemon from a newer build asks older ones to drain.
//!
//! The wire is one JSON object per line in each direction.

use crate::relay::{AgentIo, AgentSpawner, LinkSink, Relay, TaskSpawner};
use crate::state::{AgentCommand, AgentSpec, LinkFrame, LinkId, SessionSummary};
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    io::BufReader,
    stream::LocalBoxStream,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

/// How often the daemon looks for sessions left idle long enough to close.
const IDLE_SWEEP: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToDaemon {
    Open { req: u64, command: AgentCommand },
    Adopt { req: u64 },
    Attach { req: u64, link: LinkId, last_lseq: u64 },
    Send { link: LinkId, cseq: u64, line: String },
    Ack { link: LinkId, lseq: u64 },
    Detach { link: LinkId },
    Close { link: LinkId },
    List { req: u64 },
    Drain { req: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FromDaemon {
    Reply {
        req: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        err: Option<String>,
    },
    Frame {
        link: LinkId,
        lseq: u64,
        frame: LinkFrame,
    },
}

pub fn socket_path(dir: &Path, version: &str) -> PathBuf {
    let version = version
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(64)
        .collect::<String>();
    dir.join(format!("relay-{version}.sock"))
}

/// Serves one client connection until it closes. A closed connection
/// detaches its links rather than closing them: the client is a server that
/// went away, and the next one picks them up.
pub async fn serve_connection(
    relay: Rc<Relay>,
    mut incoming: LocalBoxStream<'static, String>,
    outgoing: mpsc::UnboundedSender<String>,
) {
    let links = Rc::new(RefCell::new(HashSet::<LinkId>::default()));
    let send = {
        let outgoing = outgoing.clone();
        move |message: FromDaemon| {
            if let Ok(line) = serde_json::to_string(&message) {
                outgoing.unbounded_send(line).ok();
            }
        }
    };
    let sink = |link: LinkId| -> LinkSink {
        let send = send.clone();
        Rc::new(move |lseq, frame| send(FromDaemon::Frame { link, lseq, frame }))
    };
    let reply = |req: u64, result: Result<Value>| {
        send(match result {
            Ok(ok) => FromDaemon::Reply {
                req,
                ok: Some(ok),
                err: None,
            },
            Err(error) => FromDaemon::Reply {
                req,
                ok: None,
                err: Some(format!("{error:#}")),
            },
        })
    };

    while let Some(line) = incoming.next().await {
        let message = match serde_json::from_str::<ToDaemon>(&line) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("quiet-ui relay: unreadable request: {error}");
                continue;
            }
        };
        match message {
            ToDaemon::Open { req, command } => {
                // The sink needs the link id, which the relay picks.
                let link_cell = Rc::new(Cell::new(0));
                let deferred: LinkSink = {
                    let link_cell = link_cell.clone();
                    let send = send.clone();
                    Rc::new(move |lseq, frame| {
                        send(FromDaemon::Frame {
                            link: link_cell.get(),
                            lseq,
                            frame,
                        })
                    })
                };
                let result = relay.open(command, deferred).map(|link| {
                    link_cell.set(link);
                    links.borrow_mut().insert(link);
                    serde_json::json!({ "link": link })
                });
                reply(req, result);
            }
            ToDaemon::Adopt { req } => {
                let link_cell = Rc::new(Cell::new(0));
                let deferred: LinkSink = {
                    let link_cell = link_cell.clone();
                    let send = send.clone();
                    Rc::new(move |lseq, frame| {
                        send(FromDaemon::Frame {
                            link: link_cell.get(),
                            lseq,
                            frame,
                        })
                    })
                };
                let link = relay.open_adopting(deferred);
                link_cell.set(link);
                links.borrow_mut().insert(link);
                reply(req, Ok(serde_json::json!({ "link": link })));
            }
            ToDaemon::Attach {
                req,
                link,
                last_lseq,
            } => {
                let result = relay.attach(link, last_lseq, sink(link)).map(|next_cseq| {
                    links.borrow_mut().insert(link);
                    serde_json::json!({ "next_cseq": next_cseq })
                });
                reply(req, result);
            }
            ToDaemon::Send { link, cseq, line } => relay.send(link, cseq, line),
            ToDaemon::Ack { link, lseq } => relay.ack(link, lseq),
            ToDaemon::Detach { link } => {
                links.borrow_mut().remove(&link);
                relay.detach(link);
            }
            ToDaemon::Close { link } => {
                links.borrow_mut().remove(&link);
                relay.close(link);
            }
            ToDaemon::List { req } => {
                reply(
                    req,
                    serde_json::to_value(relay.sessions()).map_err(anyhow::Error::from),
                );
            }
            ToDaemon::Drain { req } => {
                relay.drain();
                reply(req, Ok(Value::Null));
            }
        }
    }
    let links = std::mem::take(&mut *links.borrow_mut());
    for link in links {
        relay.detach(link);
    }
}

/// What arrives for any of a connection's links.
pub type FrameHandler = Rc<dyn Fn(LinkId, u64, LinkFrame)>;

/// A connection to a relay daemon.
pub struct DaemonClient {
    outgoing: mpsc::UnboundedSender<String>,
    next_req: Cell<u64>,
    replies: RefCell<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    on_frame: FrameHandler,
    connected: Cell<bool>,
    on_disconnect: RefCell<Vec<Box<dyn FnOnce()>>>,
}

impl DaemonClient {
    /// Frames are handed to `on_frame` in the order they arrive, which puts
    /// a link's first frames after the reply that named it.
    pub fn new(
        mut incoming: LocalBoxStream<'static, String>,
        outgoing: mpsc::UnboundedSender<String>,
        spawn_task: &TaskSpawner,
        on_frame: FrameHandler,
    ) -> Rc<Self> {
        let client = Rc::new(Self {
            outgoing,
            next_req: Cell::new(1),
            replies: RefCell::default(),
            on_frame,
            connected: Cell::new(true),
            on_disconnect: RefCell::default(),
        });
        let weak = Rc::downgrade(&client);
        spawn_task(Box::pin(async move {
            while let Some(line) = incoming.next().await {
                let Some(client) = weak.upgrade() else {
                    return;
                };
                match serde_json::from_str::<FromDaemon>(&line) {
                    Ok(FromDaemon::Reply { req, ok, err }) => {
                        if let Some(reply) = client.replies.borrow_mut().remove(&req) {
                            reply
                                .send(match (ok, err) {
                                    (_, Some(err)) => Err(anyhow!(err)),
                                    (ok, None) => Ok(ok.unwrap_or(Value::Null)),
                                })
                                .ok();
                        }
                    }
                    Ok(FromDaemon::Frame { link, lseq, frame }) => {
                        (client.on_frame)(link, lseq, frame);
                    }
                    Err(error) => log::warn!("quiet-ui relay: unreadable reply: {error}"),
                }
            }
            if let Some(client) = weak.upgrade() {
                client.connected.set(false);
                client.replies.borrow_mut().clear();
                let callbacks = std::mem::take(&mut *client.on_disconnect.borrow_mut());
                for callback in callbacks {
                    callback();
                }
            }
        }));
        client
    }

    pub fn is_connected(&self) -> bool {
        self.connected.get()
    }

    pub fn on_disconnect(&self, callback: impl FnOnce() + 'static) {
        if self.connected.get() {
            self.on_disconnect.borrow_mut().push(Box::new(callback));
        } else {
            callback();
        }
    }

    fn post(&self, message: &ToDaemon) {
        if let Ok(line) = serde_json::to_string(message) {
            self.outgoing.unbounded_send(line).ok();
        }
    }

    async fn request(&self, make: impl FnOnce(u64) -> ToDaemon) -> Result<Value> {
        anyhow::ensure!(self.connected.get(), "the relay connection is closed");
        let req = self.next_req.get();
        self.next_req.set(req + 1);
        let (tx, rx) = oneshot::channel();
        self.replies.borrow_mut().insert(req, tx);
        self.post(&make(req));
        rx.await.map_err(|_| anyhow!("the relay connection closed"))?
    }

    fn link_from(value: Value) -> Result<LinkId> {
        value
            .get("link")
            .and_then(Value::as_u64)
            .context("the relay did not name the link")
    }

    pub async fn open(&self, command: AgentCommand) -> Result<LinkId> {
        Self::link_from(self.request(|req| ToDaemon::Open { req, command }).await?)
    }

    pub async fn adopt(&self) -> Result<LinkId> {
        Self::link_from(self.request(|req| ToDaemon::Adopt { req }).await?)
    }

    pub async fn attach(&self, link: LinkId, last_lseq: u64) -> Result<u64> {
        self.request(|req| ToDaemon::Attach {
            req,
            link,
            last_lseq,
        })
        .await?
        .get("next_cseq")
        .and_then(Value::as_u64)
        .context("the relay did not say where to resume")
    }

    pub fn send(&self, link: LinkId, cseq: u64, line: String) {
        self.post(&ToDaemon::Send { link, cseq, line });
    }

    pub fn ack(&self, link: LinkId, lseq: u64) {
        self.post(&ToDaemon::Ack { link, lseq });
    }

    pub fn detach(&self, link: LinkId) {
        self.post(&ToDaemon::Detach { link });
    }

    pub fn close(&self, link: LinkId) {
        self.post(&ToDaemon::Close { link });
    }

    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let value = self.request(|req| ToDaemon::List { req }).await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn drain(&self) -> Result<()> {
        self.request(|req| ToDaemon::Drain { req }).await?;
        Ok(())
    }
}

/// Lines in and out of a Unix socket.
pub async fn connect_socket(
    socket: &Path,
    spawn_task: &TaskSpawner,
) -> Result<(LocalBoxStream<'static, String>, mpsc::UnboundedSender<String>)> {
    let stream = smol::net::unix::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    Ok(split_stream(stream, spawn_task))
}

fn split_stream(
    stream: smol::net::unix::UnixStream,
    spawn_task: &TaskSpawner,
) -> (LocalBoxStream<'static, String>, mpsc::UnboundedSender<String>) {
    let reader = stream.clone();
    let mut writer = stream;
    let (outgoing, mut outgoing_rx) = mpsc::unbounded::<String>();
    spawn_task(Box::pin(async move {
        while let Some(mut line) = outgoing_rx.next().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    }));
    let incoming = BufReader::new(reader)
        .lines()
        .take_while(|line| futures::future::ready(line.is_ok()))
        .filter_map(|line| futures::future::ready(line.ok()))
        .boxed_local();
    (incoming, outgoing)
}

pub async fn connect(
    socket: &Path,
    spawn_task: &TaskSpawner,
    on_frame: FrameHandler,
) -> Result<Rc<DaemonClient>> {
    let (incoming, outgoing) = connect_socket(socket, spawn_task).await?;
    Ok(DaemonClient::new(incoming, outgoing, spawn_task, on_frame))
}

/// Starts agents as child processes of the daemon, and reaches sessions an
/// older daemon still runs through a link on its socket.
pub struct ProcessSpawner {
    pub spawn_task: TaskSpawner,
}

impl AgentSpawner for ProcessSpawner {
    fn spawn(&self, spec: &AgentSpec) -> Result<AgentIo> {
        match spec {
            AgentSpec::Command(command) => spawn_process(command, &self.spawn_task),
            AgentSpec::Adopt { socket } => Ok(adopt_through(socket.clone(), &self.spawn_task)),
        }
    }
}

fn spawn_process(command: &AgentCommand, spawn_task: &TaskSpawner) -> Result<AgentIo> {
    let mut process = smol::process::Command::new(&command.path);
    process
        .args(&command.args)
        .envs(&command.env)
        .stdin(smol::process::Stdio::piped())
        .stdout(smol::process::Stdio::piped())
        .stderr(smol::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &command.cwd
        && Path::new(cwd).is_dir()
    {
        process.current_dir(cwd);
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("spawning {}", command.path))?;
    log::info!(
        "quiet-ui relay: spawned {} (pid {}) {:?}",
        command.path,
        child.id(),
        command.args
    );
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded::<String>();
    spawn_task(Box::pin(async move {
        while let Some(mut line) = stdin_rx.next().await {
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    }));
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let exited = async move {
        let status = futures::select! {
            status = child.status().fuse() => status,
            _ = kill_rx.fuse() => {
                child.kill().ok();
                child.status().await
            }
        };
        status.ok().and_then(|status| status.code())
    };
    Ok(AgentIo {
        stdout: lines(BufReader::new(stdout)),
        stderr: lines(BufReader::new(stderr)),
        stdin: stdin_tx,
        exited: Box::pin(exited),
        kill: Box::new(move || {
            kill_tx.send(()).ok();
        }),
    })
}

fn lines(reader: impl futures::AsyncBufRead + Unpin + 'static) -> LocalBoxStream<'static, String> {
    reader
        .lines()
        .take_while(|line| futures::future::ready(line.is_ok()))
        .filter_map(|line| futures::future::ready(line.ok()))
        .boxed_local()
}

/// An older relay's session, as if it were an agent of this one. When that
/// relay lets the session go, the pipes close the way a process's would.
fn adopt_through(socket: PathBuf, spawn_task: &TaskSpawner) -> AgentIo {
    struct Pipes {
        stdout: Option<mpsc::UnboundedSender<String>>,
        stderr: Option<mpsc::UnboundedSender<String>>,
        exit: Option<oneshot::Sender<Option<i32>>>,
    }
    let (stdout_tx, stdout_rx) = mpsc::unbounded::<String>();
    let (stderr_tx, stderr_rx) = mpsc::unbounded::<String>();
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded::<String>();
    let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let pipes = Rc::new(RefCell::new(Pipes {
        stdout: Some(stdout_tx),
        stderr: Some(stderr_tx),
        exit: Some(exit_tx),
    }));
    let finish = {
        let pipes = pipes.clone();
        move |code: Option<i32>| {
            let mut pipes = pipes.borrow_mut();
            pipes.stdout.take();
            pipes.stderr.take();
            if let Some(exit) = pipes.exit.take() {
                exit.send(code).ok();
            }
        }
    };
    let tasks = spawn_task.clone();
    spawn_task(Box::pin(async move {
        let on_frame: FrameHandler = {
            let finish = finish.clone();
            Rc::new(move |_, _, frame| match frame {
                LinkFrame::Line(line) => {
                    if let Some(stdout) = &pipes.borrow().stdout {
                        stdout.unbounded_send(line).ok();
                    }
                }
                LinkFrame::Stderr(line) => {
                    if let Some(stderr) = &pipes.borrow().stderr {
                        stderr.unbounded_send(line).ok();
                    }
                }
                LinkFrame::Exited(code) => finish(code),
            })
        };
        let client = match connect(&socket, &tasks, on_frame).await {
            Ok(client) => client,
            Err(error) => {
                log::warn!("quiet-ui relay: the older relay is gone: {error:#}");
                return finish(None);
            }
        };
        client.on_disconnect({
            let finish = finish.clone();
            move || finish(None)
        });
        let link = match client.adopt().await {
            Ok(link) => link,
            Err(error) => {
                log::warn!("quiet-ui relay: the older relay refused: {error:#}");
                return finish(None);
            }
        };
        let mut cseq = 0;
        let mut kill_rx = kill_rx.fuse();
        loop {
            futures::select! {
                line = stdin_rx.next() => {
                    let Some(line) = line else { break };
                    cseq += 1;
                    client.send(link, cseq, line);
                }
                _ = kill_rx => break,
            }
        }
        client.close(link);
        finish(None);
    }));
    AgentIo {
        stdout: stdout_rx.boxed_local(),
        stderr: stderr_rx.boxed_local(),
        stdin: stdin_tx,
        exited: Box::pin(exit_rx.map(|code| code.ok().flatten())),
        kill: Box::new(move || {
            kill_tx.send(()).ok();
        }),
    }
}

/// The daemon's main loop: listen on this build's socket, ask older builds'
/// relays to hand over, and serve until there is nothing left to run.
pub fn run(dir: PathBuf, version: &str) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    let socket = socket_path(&dir, version);
    let executor = Rc::new(smol::LocalExecutor::new());
    let spawn_task: TaskSpawner = {
        let executor = Rc::downgrade(&executor);
        Rc::new(move |future| {
            if let Some(executor) = executor.upgrade() {
                executor.spawn(future).detach();
            }
        })
    };
    smol::block_on(executor.run(async {
        if smol::net::unix::UnixStream::connect(&socket).await.is_ok() {
            log::info!("quiet-ui relay: one is already listening on {}", socket.display());
            return Ok(());
        }
        std::fs::remove_file(&socket).ok();
        let listener = smol::net::unix::UnixListener::bind(&socket)
            .with_context(|| format!("binding {}", socket.display()))?;
        log::info!("quiet-ui relay: listening on {}", socket.display());

        let relay = Relay::new(
            Some(dir.join("sessions")),
            Rc::new(ProcessSpawner {
                spawn_task: spawn_task.clone(),
            }),
            spawn_task.clone(),
        );
        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        relay.on_exit(move || {
            exit_tx.send(()).ok();
        });

        hand_over_older_relays(&dir, &socket, &relay, &spawn_task).await;

        spawn_task(Box::pin({
            let relay = Rc::downgrade(&relay);
            let sessions = dir.join("sessions");
            let mut sweeps = smol::Timer::interval(IDLE_SWEEP);
            async move {
                while sweeps.next().await.is_some() {
                    let Some(relay) = relay.upgrade() else {
                        return;
                    };
                    relay.close_idle(Instant::now());
                    crate::log::prune_retired(&sessions, crate::log::RETIRED_LOG_RETENTION);
                }
            }
        }));

        let mut exit_rx = exit_rx.fuse();
        loop {
            futures::select! {
                accepted = listener.accept().fuse() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (incoming, outgoing) = split_stream(stream, &spawn_task);
                    spawn_task(Box::pin(serve_connection(relay.clone(), incoming, outgoing)));
                }
                _ = exit_rx => break,
            }
        }
        std::fs::remove_file(&socket).ok();
        log::info!("quiet-ui relay: drained; exiting");
        Ok(())
    }))
}

async fn hand_over_older_relays(
    dir: &Path,
    own_socket: &Path,
    relay: &Rc<Relay>,
    spawn_task: &TaskSpawner,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_relay_socket = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("relay-") && name.ends_with(".sock"));
        if !is_relay_socket || path == own_socket {
            continue;
        }
        let Ok(client) = connect(&path, spawn_task, Rc::new(|_, _, _| {})).await else {
            std::fs::remove_file(&path).ok();
            continue;
        };
        match client.list().await {
            Ok(sessions) => {
                log::info!(
                    "quiet-ui relay: taking over {} session(s) from {}",
                    sessions.len(),
                    path.display()
                );
                relay.add_adoptable(path.clone(), sessions);
            }
            Err(error) => log::warn!("quiet-ui relay: listing {}: {error:#}", path.display()),
        }
        client.drain().await.ok();
    }
}
