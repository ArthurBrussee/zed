use std::rc::Rc;
use std::time::{Duration, Instant};

use acp_thread::{AgentConnection, LoadError};
use agent_servers::AcpConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{App, AppContext, Context, Entity, EventEmitter, SharedString, Subscription, Task};

use project::{AgentServerStore, AgentServersUpdated, Project};
use watch::Receiver;

use crate::Agent;

/// How long the path from opening a thread to the agent answering may take
/// before the launch is called failed. Generous, because a first launch
/// installs the agent's package over whatever network the machine has. What it
/// is really for is the launch that never finishes at all: a spinner that can
/// spin forever is not a state, it is the absence of one, and a connection
/// left in `Connecting` is also the entry every later thread joins — which is
/// how one wedged launch turns into every thread sitting in "loading", saying
/// nothing.
const LAUNCH_DEADLINE: Duration = Duration::from_secs(180);

pub enum AgentConnectionEntry {
    Connecting {
        connect_task: Shared<Task<Result<AgentConnectedState, LoadError>>>,
    },
    Connected(AgentConnectedState),
    Error {
        error: LoadError,
    },
}

#[derive(Clone)]
pub struct AgentConnectedState {
    pub connection: Rc<dyn AgentConnection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
}

impl AgentConnectionEntry {
    pub fn wait_for_connection(&self) -> Shared<Task<Result<AgentConnectedState, LoadError>>> {
        match self {
            AgentConnectionEntry::Connecting { connect_task } => connect_task.clone(),
            AgentConnectionEntry::Connected(state) => Task::ready(Ok(state.clone())).shared(),
            AgentConnectionEntry::Error { error } => Task::ready(Err(error.clone())).shared(),
        }
    }

    pub fn status(&self) -> AgentConnectionStatus {
        match self {
            AgentConnectionEntry::Connecting { .. } => AgentConnectionStatus::Connecting,
            AgentConnectionEntry::Connected(_) => AgentConnectionStatus::Connected,
            AgentConnectionEntry::Error { .. } => AgentConnectionStatus::Disconnected,
        }
    }
}

pub enum AgentConnectionEntryEvent {
    NewVersionAvailable(SharedString),
    LoadingStatusChanged(Option<SharedString>),
}

impl EventEmitter<AgentConnectionEntryEvent> for AgentConnectionEntry {}

#[derive(Clone)]
pub struct ActiveAcpConnection {
    pub agent_id: project::AgentId,
    pub connection: Rc<AcpConnection>,
}

pub struct AgentConnectionStore {
    project: Entity<Project>,
    entries: HashMap<Agent, Entity<AgentConnectionEntry>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentConnectionStore {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let subscription = cx.subscribe(&agent_server_store, Self::handle_agent_servers_updated);
        Self {
            project,
            entries: HashMap::default(),
            _subscriptions: vec![subscription],
        }
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn entry(&self, key: &Agent) -> Option<&Entity<AgentConnectionEntry>> {
        self.entries.get(key)
    }

    pub fn connection_status(&self, key: &Agent, cx: &App) -> AgentConnectionStatus {
        self.entries
            .get(key)
            .map(|entry| entry.read(cx).status())
            .unwrap_or(AgentConnectionStatus::Disconnected)
    }

    pub fn agent_version(&self, key: &Agent, cx: &App) -> Option<SharedString> {
        match self.entries.get(key)?.read(cx) {
            AgentConnectionEntry::Connected(state) => state.connection.agent_version(),
            AgentConnectionEntry::Connecting { .. } | AgentConnectionEntry::Error { .. } => None,
        }
    }

    pub fn active_acp_connections(&self, cx: &App) -> Vec<ActiveAcpConnection> {
        self.entries
            .values()
            .filter_map(|entry| match entry.read(cx) {
                AgentConnectionEntry::Connected(state) => state
                    .connection
                    .clone()
                    .downcast::<AcpConnection>()
                    .map(|connection| ActiveAcpConnection {
                        agent_id: state.connection.agent_id(),
                        connection,
                    }),
                AgentConnectionEntry::Connecting { .. } | AgentConnectionEntry::Error { .. } => {
                    None
                }
            })
            .collect()
    }

    pub fn restart_connection(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        if let Some(entry) = self.entries.get(&key) {
            if matches!(entry.read(cx), AgentConnectionEntry::Connecting { .. }) {
                return entry.clone();
            }
        }

        self.entries.remove(&key);
        self.request_connection(key, server, cx)
    }

    pub fn request_connection(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        if let Some(entry) = self.entries.get(&key) {
            return entry.clone();
        }

        let (mut new_version_rx, mut loading_status_rx, connect_task) =
            self.start_connection(server, cx);
        let connect_task = connect_task.shared();

        let entry = cx.new(|_cx| AgentConnectionEntry::Connecting {
            connect_task: connect_task.clone(),
        });

        self.entries.insert(key.clone(), entry.clone());
        cx.notify();

        cx.spawn({
            let key = key.clone();
            let entry = entry.downgrade();
            async move |this, cx| match connect_task.await {
                Ok(connected_state) => {
                    this.update(cx, move |this, cx| {
                        if this.entries.get(&key) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |entry, cx| {
                                if let AgentConnectionEntry::Connecting { .. } = entry {
                                    *entry = AgentConnectionEntry::Connected(connected_state);
                                    cx.notify();
                                }
                            })
                            .ok();
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, move |this, cx| {
                        if this.entries.get(&key) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |entry, cx| {
                                if let AgentConnectionEntry::Connecting { .. } = entry {
                                    *entry = AgentConnectionEntry::Error { error };
                                    cx.notify();
                                }
                            })
                            .ok();
                        this.entries.remove(&key);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        cx.spawn({
            let key = key.clone();
            let entry = entry.downgrade();
            async move |this, cx| {
                while let Ok(version) = new_version_rx.recv().await {
                    let Some(version) = version else {
                        continue;
                    };

                    this.update(cx, move |this, cx| {
                        if this.entries.get(&key) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |_entry, cx| {
                                cx.emit(AgentConnectionEntryEvent::NewVersionAvailable(
                                    version.into(),
                                ));
                            })
                            .ok();
                        this.entries.remove(&key);
                        cx.notify();
                    })
                    .ok();
                    break;
                }
            }
        })
        .detach();

        cx.spawn({
            let entry = entry.downgrade();
            async move |this, cx| {
                let started = Instant::now();
                while let Ok(status) = loading_status_rx.recv().await {
                    let status = status.map(SharedString::from);
                    // Each step of a launch, as it is reached. A launch that
                    // stops needs no process sample to say where: the last of
                    // these lines is the step it stopped at.
                    if let Some(step) = status.as_ref() {
                        log::info!(
                            "quiet-ui launch: {} after {:?}",
                            step,
                            started.elapsed()
                        );
                    }
                    let key = key.clone();
                    let entry = entry.clone();
                    this.update(cx, move |this, cx| {
                        if this.entries.get(&key) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |_entry, cx| {
                                cx.emit(AgentConnectionEntryEvent::LoadingStatusChanged(status));
                            })
                            .ok();
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        entry
    }

    fn handle_agent_servers_updated(
        &mut self,
        store: Entity<AgentServerStore>,
        _: &AgentServersUpdated,
        cx: &mut Context<Self>,
    ) {
        let store = store.read(cx);
        self.entries.retain(|key, _| match key {
            Agent::NativeAgent => true,
            Agent::Custom { id } => store.external_agents.contains_key(id),
            #[cfg(any(test, feature = "test-support"))]
            Agent::Stub => true,
        });
        cx.notify();
    }

    fn start_connection(
        &self,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> (
        Receiver<Option<String>>,
        Receiver<Option<String>>,
        Task<Result<AgentConnectedState, LoadError>>,
    ) {
        let (new_version_tx, new_version_rx) = watch::channel::<Option<String>>(None);
        let (loading_status_tx, loading_status_rx) = watch::channel::<Option<String>>(None);

        let agent_server_store = self.project.read(cx).agent_server_store().clone();
        let delegate = AgentServerDelegate::new(
            agent_server_store,
            Some(new_version_tx),
            Some(loading_status_tx),
        );

        let connect_task = server.connect(delegate, self.project.clone(), cx);
        let agent = server.agent_id();
        let connect_task = cx.spawn(async move |_this, cx| {
            let started = Instant::now();
            log::info!("quiet-ui launch: connecting to {agent}");
            let mut connect = connect_task.fuse();
            let mut deadline = cx.background_executor().timer(LAUNCH_DEADLINE).fuse();
            let result = futures::select_biased! {
                result = connect => result,
                _ = deadline => {
                    // Every step of the launch is logged as it is reached, so
                    // the last line before this one says where it stopped.
                    log::error!(
                        "quiet-ui launch: {agent} did not start within {}s; giving up",
                        LAUNCH_DEADLINE.as_secs()
                    );
                    return Err(LoadError::Other(SharedString::from(format!(
                        "{agent} did not start within {} seconds. \
                         The log says which step it stopped at.",
                        LAUNCH_DEADLINE.as_secs()
                    ))));
                }
            };
            match result {
                Ok(connection) => {
                    log::info!(
                        "quiet-ui launch: {agent} connected in {:?}",
                        started.elapsed()
                    );
                    Ok(AgentConnectedState { connection })
                }
                Err(err) => match err.downcast::<LoadError>() {
                    Ok(load_error) => {
                        log::error!(
                            "quiet-ui launch: {agent} failed after {:?}: {load_error}",
                            started.elapsed()
                        );
                        Err(load_error)
                    }
                    // The cause is the answer — "Too many open files", "No such
                    // file or directory" — and it only prints under `{:#}`.
                    Err(err) => {
                        log::error!(
                            "quiet-ui launch: {agent} failed after {:?}: {err:#}",
                            started.elapsed()
                        );
                        Err(LoadError::Other(SharedString::from(format!("{err:#}"))))
                    }
                },
            }
        });
        (new_version_rx, loading_status_rx, connect_task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::any::Any;

    /// An agent server whose launch never finishes — the shape a wedged
    /// spawn, a hung `npm install` or a login shell that never returns all
    /// take from here.
    struct NeverConnectingAgentServer;

    impl AgentServer for NeverConnectingAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> project::AgentId {
            project::AgentId::new("Stuck")
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            cx.spawn(async move |_cx| std::future::pending().await)
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    #[gpui::test]
    async fn a_launch_that_never_finishes_gives_up(cx: &mut TestAppContext) {
        crate::test_support::init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "file.txt": "" }))
            .await;
        let project = Project::test(fs, [std::path::Path::new("/project")], cx).await;
        let store = cx.new(|cx| AgentConnectionStore::new(project, cx));

        let key = Agent::Stub;
        store.update(cx, |store, cx| {
            store.request_connection(key.clone(), Rc::new(NeverConnectingAgentServer), cx);
        });
        cx.run_until_parked();
        store.read_with(cx, |store, cx| {
            assert_eq!(
                store.connection_status(&key, cx),
                AgentConnectionStatus::Connecting,
                "a launch in flight is connecting"
            );
        });

        cx.executor().advance_clock(LAUNCH_DEADLINE * 2);
        cx.run_until_parked();

        // The thread that asked gets an error it can see and retry, rather
        // than a spinner; and the entry the next thread would have joined is
        // gone, so the next one launches instead of waiting on this one.
        store.read_with(cx, |store, cx| {
            assert_eq!(
                store.connection_status(&key, cx),
                AgentConnectionStatus::Disconnected
            );
            assert!(!store.entries.contains_key(&key));
        });
    }
}
