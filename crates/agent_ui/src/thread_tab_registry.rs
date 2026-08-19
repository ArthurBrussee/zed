//! Window-spanning registry of open thread tabs.
//!
//! Every [`AgentPanel`](crate::AgentPanel) publishes its own pane's real
//! [`ThreadTab`](crate::thread_tab::ThreadTab) set here and mirrors the other
//! workspaces' entries as
//! [`ForeignThreadTab`](crate::thread_tab::ForeignThreadTab) proxies, so each
//! pane's tab strip spans all workspaces in the window. Entries keep global
//! insertion order; a workspace's own entries follow its pane order.

use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, WeakEntity};
use workspace::Workspace;

use crate::thread_metadata_store::ThreadId;

pub struct ThreadTabsRegistry {
    entries: Vec<ThreadTabsEntry>,
}

#[derive(Clone)]
pub struct ThreadTabsEntry {
    pub thread_id: ThreadId,
    pub workspace: WeakEntity<Workspace>,
}

struct GlobalThreadTabsRegistry(Entity<ThreadTabsRegistry>);

impl Global for GlobalThreadTabsRegistry {}

impl ThreadTabsRegistry {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalThreadTabsRegistry>() {
            let registry = cx.new(|_| ThreadTabsRegistry {
                entries: Vec::new(),
            });
            cx.set_global(GlobalThreadTabsRegistry(registry));
        }
        cx.global::<GlobalThreadTabsRegistry>().0.clone()
    }

    pub fn entries(&self) -> &[ThreadTabsEntry] {
        &self.entries
    }

    /// Number of open threads whose turn is currently running, across all
    /// workspaces. Conversation views poke this registry on thread status
    /// changes, so observers (the title bar) re-read this on time.
    pub fn running_turn_count(&self, cx: &App) -> usize {
        self.running_turn_count_impl(None, cx)
    }

    /// Like [`running_turn_count`](Self::running_turn_count) but skips the
    /// entry for `exclude`. A thread view calls this while it renders (it holds
    /// its own lease, so re-reading itself would panic) and adds its own status
    /// back separately.
    pub fn running_turn_count_excluding(&self, exclude: ThreadId, cx: &App) -> usize {
        self.running_turn_count_impl(Some(exclude), cx)
    }

    fn running_turn_count_impl(&self, exclude: Option<ThreadId>, cx: &App) -> usize {
        self.entries
            .iter()
            .filter(|entry| Some(entry.thread_id) != exclude)
            .filter(|entry| {
                entry
                    .workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<crate::AgentPanel>(cx))
                    .and_then(|panel| {
                        panel
                            .read(cx)
                            .conversation_view_for_id(&entry.thread_id, cx)
                    })
                    .and_then(|view| view.read(cx).root_thread(cx))
                    .is_some_and(|thread| {
                        thread.read(cx).status() != acp_thread::ThreadStatus::Idle
                    })
            })
            .count()
    }

    /// Replaces `workspace`'s entries with `thread_ids` (in pane order),
    /// preserving global insertion order: surviving threads keep their
    /// slots (reassigned in pane order) and new threads append at the end.
    pub fn set_workspace_threads(
        &mut self,
        workspace: WeakEntity<Workspace>,
        thread_ids: Vec<ThreadId>,
        cx: &mut Context<Self>,
    ) {
        let workspace_id = workspace.entity_id();
        let is_mine = |entry: &ThreadTabsEntry| entry.workspace.entity_id() == workspace_id;

        let old: Vec<ThreadId> = self
            .entries
            .iter()
            .filter(|entry| is_mine(entry))
            .map(|entry| entry.thread_id)
            .collect();
        let dropped_dead_entries = self.prune_dead_workspaces();
        if old == thread_ids && !dropped_dead_entries {
            return;
        }

        self.entries
            .retain(|entry| !is_mine(entry) || thread_ids.contains(&entry.thread_id));
        let mut ids = thread_ids.into_iter();
        for entry in self.entries.iter_mut().filter(|entry| is_mine(entry)) {
            if let Some(id) = ids.next() {
                entry.thread_id = id;
            }
        }
        for id in ids {
            self.entries.push(ThreadTabsEntry {
                thread_id: id,
                workspace: workspace.clone(),
            });
        }
        cx.notify();
    }

    /// Drops every entry belonging to `workspace_id`. Called when a panel is
    /// released (its workspace closed).
    pub fn remove_workspace(&mut self, workspace_id: EntityId, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.workspace.entity_id() != workspace_id);
        if self.entries.len() != before {
            cx.notify();
        }
    }

    /// Drops entries whose workspace entity is gone, returning whether any
    /// were removed.
    fn prune_dead_workspaces(&mut self) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.workspace.upgrade().is_some());
        self.entries.len() != before
    }
}
