//! quiet-ui: a remote project learns its agents from the box over the
//! connection, and a thread can ask for one in the moment before that list
//! arrives (just after connecting, or reconnecting). Waiting for it is what
//! the user would do by hand; failing with "not registered" is not.

use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{AsyncApp, Entity};
use project::{AgentId, AgentServersUpdated, Project, agent_server_store::AgentServerStore};
use std::time::Duration;

const WAIT_FOR_AGENTS: Duration = Duration::from_secs(20);

pub async fn wait_until_registered(
    store: &Entity<AgentServerStore>,
    project: &Entity<Project>,
    agent_id: &AgentId,
    cx: &mut AsyncApp,
) {
    let registered = |cx: &mut AsyncApp| {
        store.update(cx, |store, _| store.get_external_agent(agent_id).is_some())
    };
    let is_remote = project.read_with(cx, |project, _| project.is_via_remote_server());
    if !is_remote || registered(cx) {
        return;
    }
    let (updated_tx, mut updated) = mpsc::unbounded::<()>();
    let _subscription = cx.update(|cx| {
        cx.subscribe(store, move |_, _: &AgentServersUpdated, _| {
            updated_tx.unbounded_send(()).ok();
        })
    });
    log::info!("quiet-ui launch: waiting for the box to list {agent_id}");
    let mut timeout = cx.background_executor().timer(WAIT_FOR_AGENTS).fuse();
    loop {
        futures::select! {
            _ = updated.next() => {
                if registered(cx) {
                    log::info!("quiet-ui launch: the box listed {agent_id}");
                    return;
                }
            }
            _ = timeout => return,
        }
    }
}
