//! Window-spanning registry of open thread tabs.
//!
//! Every [`AgentPanel`](crate::AgentPanel) publishes its own pane's whole tab
//! strip here — its real [`ThreadTab`](crate::thread_tab::ThreadTab)s plus the
//! [`ForeignThreadTab`](crate::thread_tab::ForeignThreadTab) proxies mirroring
//! the other workspaces of its window — and mirrors this registry back into
//! that strip, so every pane in a window shows one order spanning all of its
//! workspaces.
//!
//! That makes this list, not any one pane, the order the tabs are in: the
//! sidebar reads it to sort its rows, and a drag lands the same way whichever
//! pane it happened in and whichever kind of tab was dragged.

use std::collections::HashSet;

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

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalThreadTabsRegistry>()
            .map(|global| global.0.clone())
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

    /// Records `owner`'s pane as `strip`: its whole tab sequence, real tabs
    /// and foreign proxies alike, each carrying the workspace that owns the
    /// thread.
    ///
    /// Only the workspaces the strip names (plus `owner`, whose last tab may
    /// have just closed) are that pane's to speak for — those are the window's
    /// — so their entries are replaced wholesale, in the slots they already
    /// occupy. Every other window's entries keep theirs. Publishing the whole
    /// strip rather than only the owner's own tabs is what lets a tab dragged
    /// past another worktree's tabs stay where it was dropped: the drag is the
    /// order, and the panes that mirror this list follow it.
    pub fn set_window_tabs(
        &mut self,
        owner: WeakEntity<Workspace>,
        strip: Vec<ThreadTabsEntry>,
        cx: &mut Context<Self>,
    ) {
        let dropped_dead_entries = self.prune_dead_workspaces();

        let mut window_workspaces: HashSet<EntityId> = strip
            .iter()
            .map(|entry| entry.workspace.entity_id())
            .collect();
        window_workspaces.insert(owner.entity_id());

        let slots: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| window_workspaces.contains(&entry.workspace.entity_id()))
            .map(|(index, _)| index)
            .collect();

        let unchanged = slots.len() == strip.len()
            && slots.iter().zip(&strip).all(|(slot, wanted)| {
                let entry = &self.entries[*slot];
                entry.thread_id == wanted.thread_id
                    && entry.workspace.entity_id() == wanted.workspace.entity_id()
            });
        if unchanged && !dropped_dead_entries {
            return;
        }

        let shared = slots.len().min(strip.len());
        for (slot, entry) in slots.iter().zip(strip.iter()) {
            self.entries[*slot] = entry.clone();
        }
        if strip.len() > shared {
            // New tabs go in after the window's last one, so a window's tabs
            // stay together rather than scattering to the end of the list.
            let insert_at = slots.last().map_or(self.entries.len(), |slot| slot + 1);
            for (offset, entry) in strip[shared..].iter().enumerate() {
                self.entries.insert(insert_at + offset, entry.clone());
            }
        } else {
            for slot in slots[shared..].iter().rev() {
                self.entries.remove(*slot);
            }
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
