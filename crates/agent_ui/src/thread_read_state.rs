//! Window-spanning unread state: a thread is unread when a turn completed
//! while it was not on screen in an active window.

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
