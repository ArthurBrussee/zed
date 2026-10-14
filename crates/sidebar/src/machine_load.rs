//! What each machine is doing, for the sidebar's footer.
//!
//! This machine's load comes from `sysinfo`. A dev box's comes over the
//! connection the project already has, as one small request every few seconds
//! rather than a shell of its own per tick.

use std::collections::HashMap;
use std::time::Duration;
use gpui::{App, AppContext as _, Entity, Task};
use project::Project;
use proto::REMOTE_SERVER_PROJECT_ID;
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use sysinfo::System;
use ui::SharedString;
use workspace::Workspace;

/// Often enough to be live, rarely enough to be free.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(4);

/// Which machine a reading belongs to: `None` is this one, as the machine
/// groups key themselves.
pub(crate) type MachineKey = Option<RemoteConnectionOptions>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MachineLoad {
    /// The one-minute load average.
    pub one_minute: f64,
    /// `None` where the machine would not say, which makes a percentage a lie.
    pub cpu_count: Option<u32>,
}

impl MachineLoad {
    /// Load as a share of the cores, which is what reads like a CPU figure.
    pub fn percent(&self) -> Option<f64> {
        let cpu_count = self.cpu_count.filter(|count| *count > 0)? as f64;
        Some((self.one_minute / cpu_count) * 100.0)
    }

    pub fn label(&self) -> SharedString {
        match self.percent() {
            Some(percent) => format!("{:.0}%", percent).into(),
            None => format!("{:.2}", self.one_minute).into(),
        }
    }
}

/// One reading of every machine: this one from `sysinfo`, each box over the
/// connection its project already has.
pub(crate) fn read_machine_loads(
    workspaces: &[Entity<Workspace>],
    cx: &App,
) -> Task<HashMap<MachineKey, MachineLoad>> {
    let local = local_load();
    let requests: Vec<(RemoteConnectionOptions, Task<Option<MachineLoad>>)> =
        remote_projects(workspaces, cx)
            .into_iter()
            .filter_map(|(machine, project)| Some((machine, remote_load(&project, cx)?)))
            .collect();
    cx.background_spawn(async move {
        let mut loads = HashMap::default();
        if let Some(local) = local {
            loads.insert(None, local);
        }
        for (machine, request) in requests {
            if let Some(load) = request.await {
                loads.insert(Some(machine), load);
            }
        }
        loads
    })
}

fn local_load() -> Option<MachineLoad> {
    let one_minute = System::load_average().one;
    if !one_minute.is_finite() || one_minute < 0.0 {
        return None;
    }
    Some(MachineLoad {
        one_minute,
        cpu_count: System::physical_core_count().map(|count| count as u32),
    })
}

/// One project per remote machine; two worktrees on the same box share a
/// connection, so one of them answers for both.
fn remote_projects(
    workspaces: &[Entity<Workspace>],
    cx: &App,
) -> Vec<(RemoteConnectionOptions, Entity<Project>)> {
    let mut projects: Vec<(RemoteConnectionOptions, Entity<Project>)> = Vec::new();
    for workspace in workspaces {
        let project = workspace.read(cx).project().clone();
        let Some(options) = project.read(cx).remote_connection_options(cx) else {
            continue;
        };
        if projects.iter().any(|(known, _)| {
            same_remote_connection_identity(Some(known), Some(&options))
        }) {
            continue;
        }
        projects.push((options, project));
    }
    projects
}

fn remote_load(project: &Entity<Project>, cx: &App) -> Option<Task<Option<MachineLoad>>> {
    let remote_client = project.read(cx).remote_client()?;
    let proto_client = remote_client.read(cx).proto_client();
    Some(cx.background_spawn(async move {
        let response = proto_client
            .request(proto::GetSystemLoad {
                project_id: REMOTE_SERVER_PROJECT_ID,
            })
            .await
            .ok()?;
        if !response.load_average_one.is_finite() || response.load_average_one < 0.0 {
            return None;
        }
        Some(MachineLoad {
            one_minute: response.load_average_one,
            cpu_count: (response.cpu_count > 0).then_some(response.cpu_count),
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_reads_as_a_share_of_the_cores() {
        let load = MachineLoad {
            one_minute: 4.0,
            cpu_count: Some(8),
        };
        assert_eq!(load.percent(), Some(50.0));
        assert_eq!(load.label().as_ref(), "50%");

        // A machine that would not say how many cores it has gets the raw
        // average rather than a percentage of nothing.
        let load = MachineLoad {
            one_minute: 1.25,
            cpu_count: None,
        };
        assert_eq!(load.percent(), None);
        assert_eq!(load.label().as_ref(), "1.25");

        let load = MachineLoad {
            one_minute: 1.0,
            cpu_count: Some(0),
        };
        assert_eq!(load.percent(), None, "zero cores is no answer either");
    }
}

/// A session the relay on a box is running, whether or not anything here is
/// attached to it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelaySession {
    pub machine: RemoteConnectionOptions,
    pub session_id: String,
    pub agent_name: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub running: bool,
}

/// Every box's relayed sessions, asked over the connection each box's project
/// already has, on the same tick as its load.
pub(crate) fn read_relay_sessions(
    workspaces: &[Entity<Workspace>],
    cx: &App,
) -> Task<Vec<RelaySession>> {
    let requests = remote_projects(workspaces, cx)
        .into_iter()
        .filter_map(|(machine, project)| {
            let relay = project.read(cx).acp_relay_client()?;
            Some((machine, relay.read(cx).list_sessions()))
        })
        .collect::<Vec<_>>();
    cx.background_spawn(async move {
        let mut sessions = Vec::new();
        for (machine, request) in requests {
            let Ok(listed) = request.await else {
                continue;
            };
            sessions.extend(listed.into_iter().map(|session| RelaySession {
                machine: machine.clone(),
                session_id: session.session_id,
                agent_name: session.agent_name,
                cwd: session.cwd,
                title: session.title,
                running: session.running,
            }));
        }
        sessions
    })
}
