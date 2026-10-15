//! quiet-ui: hands a remote project's agent connections to the relay daemon
//! on this box (`acp_relay::daemon`), which keeps the agents running after
//! this server is gone. The server is a pass-through: it forwards a link's
//! lines both ways and remembers which links it carries.

use acp_relay::{
    AgentCommand, LinkFrame, LinkId, TaskSpawner,
    daemon::{self, DaemonClient},
};
use anyhow::{Context as _, Result, anyhow};
use collections::HashSet;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, Task};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use std::{rc::Rc, sync::Arc, time::Duration};

/// How a server reaches the relay. Tests put an in-process relay here.
pub struct RelayConnector(
    pub Rc<dyn Fn(daemon::FrameHandler, &mut App) -> Task<Result<Rc<DaemonClient>>>>,
);

impl Global for RelayConnector {}

pub struct AcpRelayHost {
    session: AnyProtoClient,
    daemon: Option<Rc<DaemonClient>>,
    connecting: Option<Shared<Task<Result<Rc<DaemonClient>, Arc<anyhow::Error>>>>>,
    links: HashSet<LinkId>,
}

pub fn init(session: &AnyProtoClient, cx: &mut App) -> Entity<AcpRelayHost> {
    let host = cx.new(|_| AcpRelayHost {
        session: session.clone(),
        daemon: None,
        connecting: None,
        links: HashSet::default(),
    });
    session.subscribe_to_entity(rpc::proto::REMOTE_SERVER_PROJECT_ID, &host);
    session.add_entity_request_handler(AcpRelayHost::handle_open);
    session.add_entity_request_handler(AcpRelayHost::handle_attach);
    session.add_entity_request_handler(AcpRelayHost::handle_send);
    session.add_entity_request_handler(AcpRelayHost::handle_list);
    session.add_entity_message_handler(AcpRelayHost::handle_ack);
    session.add_entity_message_handler(AcpRelayHost::handle_close);
    host
}

impl AcpRelayHost {
    fn daemon(
        this: &Entity<Self>,
        cx: &mut AsyncApp,
    ) -> Shared<Task<Result<Rc<DaemonClient>, Arc<anyhow::Error>>>> {
        this.update(cx, |this, cx| {
            if let Some(daemon) = &this.daemon
                && daemon.is_connected()
            {
                return Task::ready(Ok(daemon.clone())).shared();
            }
            if let Some(connecting) = &this.connecting {
                return connecting.clone();
            }
            let session = this.session.clone();
            let on_frame: daemon::FrameHandler = Rc::new(move |link_id, lseq, frame| {
                session.send(frame_message(link_id, lseq, frame)).ok();
            });
            let connect = match cx.try_global::<RelayConnector>() {
                Some(connector) => (connector.0.clone())(on_frame, cx),
                None => connect_to_daemon(on_frame, cx),
            };
            let host = cx.weak_entity();
            let task = cx
                .spawn(async move |_, cx| {
                    let daemon = connect.await.map_err(Arc::new)?;
                    host.update(cx, |this, cx| {
                        this.daemon = Some(daemon.clone());
                        this.connecting = None;
                        let host = cx.weak_entity();
                        let mut async_cx = cx.to_async();
                        daemon.on_disconnect(move || {
                            host.update(&mut async_cx, |this, _| this.daemon_lost()).ok();
                        });
                    })
                    .ok();
                    Ok(daemon)
                })
                .shared();
            this.connecting = Some(task.clone());
            task
        })
    }

    /// What a fresh server knows: no daemon connection and no links. Dropping
    /// the connection detaches this server's links on the relay's side.
    #[cfg(test)]
    pub(crate) fn forget_daemon_for_test(&mut self) {
        self.daemon = None;
        self.connecting = None;
        self.links.clear();
    }

    /// The daemon went away and took its agents with it.
    fn daemon_lost(&mut self) {
        log::error!("quiet-ui relay: lost the relay daemon");
        self.daemon = None;
        for link_id in self.links.drain() {
            self.session
                .send(frame_message(link_id, 0, LinkFrame::Exited(None)))
                .ok();
        }
    }

    async fn handle_open(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelayOpen>,
        mut cx: AsyncApp,
    ) -> Result<proto::AcpRelayOpened> {
        let daemon = Self::daemon(&this, &mut cx)
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        let request = envelope.payload;
        let link_id = daemon
            .open(AgentCommand {
                name: request.agent_name,
                path: request.path,
                args: request.args,
                env: request.env.into_iter().collect(),
                cwd: request.root_dir,
            })
            .await?;
        this.update(&mut cx, |this, _| this.links.insert(link_id));
        Ok(proto::AcpRelayOpened { link_id })
    }

    async fn handle_attach(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelayAttach>,
        mut cx: AsyncApp,
    ) -> Result<proto::AcpRelayAttached> {
        let link_id = envelope.payload.link_id;
        let carried = this.read_with(&cx, |this, _| {
            this.links.contains(&link_id)
                && this.daemon.as_ref().is_some_and(|daemon| daemon.is_connected())
        });
        if carried {
            // Same server: the connection replayed whatever the drop held up.
            return Ok(proto::AcpRelayAttached { next_cseq: 0 });
        }
        let daemon = Self::daemon(&this, &mut cx)
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        let next_cseq = daemon
            .attach(link_id, envelope.payload.last_lseq)
            .await?;
        this.update(&mut cx, |this, _| this.links.insert(link_id));
        Ok(proto::AcpRelayAttached { next_cseq })
    }

    async fn handle_send(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelaySend>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let request = envelope.payload;
        let daemon = this.read_with(&cx, |this, _| {
            this.links
                .contains(&request.link_id)
                .then(|| this.daemon.clone())
                .flatten()
        });
        let daemon = daemon.context("this server does not carry that link")?;
        daemon.send(request.link_id, request.cseq, request.line);
        Ok(proto::Ack {})
    }

    async fn handle_list(
        this: Entity<Self>,
        _: TypedEnvelope<proto::AcpRelayListSessions>,
        mut cx: AsyncApp,
    ) -> Result<proto::AcpRelaySessions> {
        let daemon = Self::daemon(&this, &mut cx)
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        let sessions = daemon.list().await?;
        Ok(proto::AcpRelaySessions {
            sessions: sessions
                .into_iter()
                .map(|session| proto::AcpRelaySession {
                    session_id: session.session_id,
                    agent_name: session.agent_name,
                    cwd: session.cwd,
                    title: session.title,
                    running: session.running,
                    attached: session.attached,
                })
                .collect(),
        })
    }

    async fn handle_ack(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelayAck>,
        cx: AsyncApp,
    ) -> Result<()> {
        this.read_with(&cx, |this, _| {
            if let Some(daemon) = &this.daemon {
                daemon.ack(envelope.payload.link_id, envelope.payload.lseq);
            }
        });
        Ok(())
    }

    async fn handle_close(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AcpRelayClose>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            let link_id = envelope.payload.link_id;
            this.links.remove(&link_id);
            if let Some(daemon) = &this.daemon {
                daemon.close(link_id);
            }
        });
        Ok(())
    }
}

fn frame_message(link_id: LinkId, lseq: u64, frame: LinkFrame) -> proto::AcpRelayFrame {
    proto::AcpRelayFrame {
        project_id: rpc::proto::REMOTE_SERVER_PROJECT_ID,
        link_id,
        lseq,
        kind: Some(match frame {
            LinkFrame::Line(line) => proto::acp_relay_frame::Kind::Line(line),
            LinkFrame::Stderr(line) => proto::acp_relay_frame::Kind::Stderr(line),
            LinkFrame::Exited(code) => {
                proto::acp_relay_frame::Kind::Exited(proto::AcpRelayExit { code })
            }
        }),
    }
}

pub fn task_spawner(cx: &App) -> TaskSpawner {
    let executor = cx.foreground_executor().clone();
    Rc::new(move |future| executor.spawn(future).detach())
}

pub fn relay_dir() -> std::path::PathBuf {
    paths::data_dir().join("acp-relay")
}

/// Connects to this build's daemon, starting it first if it is not running.
/// It is started detached from this server, so killing the server (which a
/// fresh connection from Zed does) leaves it and its agents running.
fn connect_to_daemon(
    on_frame: daemon::FrameHandler,
    cx: &mut App,
) -> Task<Result<Rc<DaemonClient>>> {
    let dir = relay_dir();
    let socket = daemon::socket_path(&dir, &crate::VERSION);
    let spawn_task = task_spawner(cx);
    let executor = cx.background_executor().clone();
    cx.spawn(async move |_| {
        if let Ok(client) = daemon::connect(&socket, &spawn_task, on_frame.clone()).await {
            return Ok(client);
        }
        let binary = std::env::current_exe().context("finding the server binary")?;
        log::info!("quiet-ui relay: starting the relay daemon");
        util::command::new_command(&binary)
            .arg("acp-relay")
            .arg("--dir")
            .arg(&dir)
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null())
            .spawn()
            .context("starting the relay daemon")?;
        for _ in 0..100 {
            executor.timer(Duration::from_millis(50)).await;
            if let Ok(client) = daemon::connect(&socket, &spawn_task, on_frame.clone()).await {
                return Ok(client);
            }
        }
        anyhow::bail!("the relay daemon did not start listening on {}", socket.display())
    })
}

/// The daemon's entry point: `remote_server acp-relay --dir <dir>`.
pub fn run_daemon(dir: std::path::PathBuf) -> Result<()> {
    #[cfg(unix)]
    // SAFETY: setsid has no preconditions; it detaches this process from the
    // server's session so nothing aimed at that session reaches the agents.
    unsafe {
        libc::setsid();
    }
    std::fs::create_dir_all(&dir).ok();
    let log_path = paths::logs_dir().join("acp-relay.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Info)
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
    }
    log::info!("quiet-ui relay: daemon {} starting", *crate::VERSION);
    acp_relay::daemon::run(dir, &crate::VERSION)
}
