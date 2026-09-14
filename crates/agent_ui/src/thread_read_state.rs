//! Window-spanning unread state for agent threads.
//!
//! A thread becomes unread when a turn completes (the `Stopped` event) while
//! its conversation view is not being viewed, and read again when the user
//! actually views it (its tab is rendered in an active window). Thread tabs
//! and the sidebar both read this one set, so the accent unread marker means
//! the same thing everywhere: "finished since you last looked", never "output
//! is streaming".

use collections::HashSet;
use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::thread_metadata_store::ThreadId;

#[derive(Default)]
pub struct ThreadReadState {
    unread: HashSet<ThreadId>,
}

struct GlobalThreadReadState(Entity<ThreadReadState>);

impl Global for GlobalThreadReadState {}

impl ThreadReadState {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalThreadReadState>() {
            let state = cx.new(|_| ThreadReadState::default());
            cx.set_global(GlobalThreadReadState(state));
        }
        cx.global::<GlobalThreadReadState>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalThreadReadState>()
            .map(|global| global.0.clone())
    }

    pub fn is_unread(&self, thread_id: &ThreadId) -> bool {
        self.unread.contains(thread_id)
    }

    pub fn unread_threads(&self) -> &HashSet<ThreadId> {
        &self.unread
    }

    pub fn mark_unread(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        if self.unread.insert(thread_id) {
            cx.notify();
        }
    }

    pub fn mark_read(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        if self.unread.remove(thread_id) {
            cx.notify();
        }
    }
}
