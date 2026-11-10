//! Window-spanning order of open thread tabs. Every agent panel publishes its
//! whole strip (own tabs and proxies) here and mirrors this list back, and the
//! sidebar sorts its rows by it.

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

    /// Replaces, in the slots they already occupy, the entries of the
    /// workspaces `strip` names plus `owner` (whose last tab may have just
    /// closed). Other windows' entries are untouched.
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

    pub fn remove_workspace(&mut self, workspace_id: EntityId, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.workspace.entity_id() != workspace_id);
        if self.entries.len() != before {
            cx.notify();
        }
    }

    /// Returns whether any entries were removed.
    fn prune_dead_workspaces(&mut self) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.workspace.upgrade().is_some());
        self.entries.len() != before
    }
}
