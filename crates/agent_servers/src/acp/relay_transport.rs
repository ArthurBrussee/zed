//! quiet-ui: an agent connection for a remote project, carried by the relay
//! on the box (`project::acp_relay_client`) instead of an `ssh host agent`
//! process. The agent lives on the box; a dropped connection is a pause here,
//! not an exit, so nothing above this is told to cancel or fail.

use super::debug_log::{AcpDebugLog, AcpDebugMessageDirection};
use super::transport::{AgentProcess, StdioProcess};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use futures::{FutureExt as _, StreamExt as _, channel::oneshot, future::Shared};
use gpui::{AsyncApp, Entity};
use project::{
    AgentId, Project,
    acp_relay_client::{RelayEvent, RelayLink},
    agent_server_store::AgentServerCommand,
};
use settings::SettingsStore;
use std::{io, process::ExitStatus};

pub(crate) struct RelayProcess {
    exit: Shared<oneshot::Receiver<Option<i32>>>,
    kill: Option<oneshot::Sender<()>>,
    reconnecting: Option<async_channel::Receiver<bool>>,
}

impl RelayProcess {
    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> + 'static {
        let exit = self.exit.clone();
        async move { Ok(exit_status(exit.await.ok().flatten())) }
    }

    /// Ends this connection's link. The agent's sessions keep running on the
    /// box until they are closed.
    pub fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            kill.send(()).ok();
        }
    }

    /// Whether the connection to the box is down, as it changes.
    pub fn take_reconnecting(&mut self) -> Option<async_channel::Receiver<bool>> {
        self.reconnecting.take()
    }
}

#[cfg(unix)]
fn exit_status(code: Option<i32>) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    match code {
        Some(code) => ExitStatus::from_raw(code << 8),
        None => ExitStatus::from_raw(9),
    }
}

#[cfg(windows)]
fn exit_status(code: Option<i32>) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt as _;
    ExitStatus::from_raw(code.unwrap_or(1) as u32)
}

/// Only an app that registered the agent settings has said whether it wants
/// the relay, which leaves tests that never asked for it on the stdio path.
pub(super) fn wants_relay(project: &Entity<Project>, cx: &AsyncApp) -> bool {
    project.read_with(cx, |project, cx| {
        project.acp_relay_client().is_some()
            && cx
                .try_global::<SettingsStore>()
                .and_then(|store| store.try_get::<AgentSettings>(None))
                .is_some_and(|settings| settings.remote_relay)
    })
}

pub(super) async fn spawn_relay(
    project: &Entity<Project>,
    agent_id: &AgentId,
    command: AgentServerCommand,
    cx: &mut AsyncApp,
) -> Result<StdioProcess> {
    let (relay, root_dir) = project.read_with(cx, |project, cx| {
        (
            project.acp_relay_client().cloned(),
            project
                .default_path_list(cx)
                .ordered_paths()
                .next()
                .map(|path| path.display().to_string()),
        )
    });
    let relay = relay.context("not a remote project")?;
    let RelayLink {
        link_id,
        incoming,
        outgoing,
    } = relay
        .update(cx, |relay, cx| {
            relay.open(agent_id.to_string(), command, root_dir, cx)
        })
        .await?;
    log::info!("quiet-ui launch: {agent_id} runs under the relay (link {link_id})");

    let debug_log = AcpDebugLog::default();
    let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (reconnecting_tx, reconnecting_rx) = async_channel::unbounded::<bool>();

    let incoming = futures::stream::unfold(
        (incoming, Some(exit_tx), debug_log.clone()),
        move |(mut events, mut exit_tx, debug_log)| {
            let reconnecting_tx = reconnecting_tx.clone();
            async move {
                loop {
                    match events.next().await {
                        Some(RelayEvent::Line(line)) => {
                            debug_log.record_line(AcpDebugMessageDirection::Incoming, &line);
                            return Some((Ok(line), (events, exit_tx, debug_log)));
                        }
                        Some(RelayEvent::Stderr(line)) => {
                            log::warn!("agent stderr: {line}");
                            debug_log.record_line(AcpDebugMessageDirection::Stderr, &line);
                        }
                        Some(RelayEvent::Reconnecting) => {
                            reconnecting_tx.try_send(true).ok();
                        }
                        Some(RelayEvent::Reconnected) => {
                            reconnecting_tx.try_send(false).ok();
                        }
                        Some(RelayEvent::Exited(code)) => {
                            if let Some(exit_tx) = exit_tx.take() {
                                exit_tx.send(code).ok();
                            }
                            return None;
                        }
                        None => {
                            if let Some(exit_tx) = exit_tx.take() {
                                exit_tx.send(None).ok();
                            }
                            return None;
                        }
                    }
                }
            }
        },
    )
    .take_until(kill_rx)
    .boxed();

    let outgoing = Box::pin(futures::sink::unfold(
        (outgoing, debug_log.clone()),
        async move |(outgoing, debug_log), line: String| {
            debug_log.record_line(AcpDebugMessageDirection::Outgoing, &line);
            outgoing
                .unbounded_send(line)
                .map_err(|_| io::Error::other("the relay link is closed"))?;
            Ok::<_, io::Error>((outgoing, debug_log))
        },
    ));

    Ok(StdioProcess {
        child: AgentProcess::Relay(RelayProcess {
            exit: exit_rx.shared(),
            kill: Some(kill_tx),
            reconnecting: Some(reconnecting_rx),
        }),
        incoming,
        outgoing,
        stderr: futures::future::ready(Ok(())).boxed(),
        debug_log,
    })
}
