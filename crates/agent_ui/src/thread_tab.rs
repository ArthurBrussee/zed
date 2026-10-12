//! Agent threads as items in the agent panel's own pane. An open tab is what
//! "open" means: closing it closes the thread, cancelling any running turn.

use acp_thread::ThreadStatus;
use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, WeakEntity,
    Window, prelude::*,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Color, Label, LabelCommon, LabelSize, utils::WithRemSize, v_flex};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::{
    AgentPanel,
    conversation_view::{AcpThreadViewEvent, ConversationView},
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
    thread_read_state::ThreadReadState,
};

pub struct ThreadTab {
    conversation_view: Entity<ConversationView>,
    workspace: WeakEntity<Workspace>,
    _observation: Subscription,
    _read_state_observation: Subscription,
    _thread_view_subscription: Option<Subscription>,
}

pub enum ThreadTabEvent {
    UpdateTab,
}

impl EventEmitter<ThreadTabEvent> for ThreadTab {}

impl ThreadTab {
    pub fn new(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&conversation_view, |this, view, cx| {
            this.subscribe_to_thread_view(&view, cx);
            cx.emit(ThreadTabEvent::UpdateTab);
            cx.notify();
        });

        // Re-render so a thread marked unread while on screen is marked read.
        let read_state = ThreadReadState::global(cx);
        let read_state_observation = cx.observe(&read_state, |_this, _, cx| cx.notify());

        cx.on_release(|this: &mut Self, cx: &mut App| {
            // Deferred: tabs are usually released inside a pane update.
            let conversation_view = this.conversation_view.clone();
            cx.defer(move |cx| {
                Self::cancel_if_running(conversation_view, cx);
            });
        })
        .detach();

        let mut this = Self {
            conversation_view: conversation_view.clone(),
            workspace,
            _observation: observation,
            _read_state_observation: read_state_observation,
            _thread_view_subscription: None,
        };
        this.subscribe_to_thread_view(&conversation_view, cx);
        this
    }

    pub fn thread_id(&self, cx: &App) -> ThreadId {
        self.conversation_view.read(cx).parent_id()
    }

    pub(crate) fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    fn cancel_if_running(conversation_view: Entity<ConversationView>, cx: &mut App) {
        let running = conversation_view
            .read(cx)
            .root_thread_view()
            .is_some_and(|thread_view| {
                thread_view.read(cx).thread.read(cx).status() != ThreadStatus::Idle
            });
        if running {
            conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.cancel_generation(cx);
            });
        }
    }

    fn subscribe_to_thread_view(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        self._thread_view_subscription = conversation_view.read(cx).root_thread_view().map(|tv| {
            cx.subscribe(
                &tv,
                |this, _view, event: &AcpThreadViewEvent, cx| match event {
                    AcpThreadViewEvent::Interacted => {
                        let thread_id = this.thread_id(cx);
                        let workspace = this.workspace.clone();
                        cx.defer(move |cx| {
                            if let Some(panel) = workspace
                                .upgrade()
                                .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                            {
                                panel.update(cx, |panel, cx| {
                                    panel.thread_tab_interacted(thread_id, cx);
                                });
                            }
                        });
                    }
                },
            )
        });
    }
}

impl Focusable for ThreadTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).focus_handle(cx)
    }
}

impl Render for ThreadTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Rendering the conversation content in an active window is what
        // "viewing the thread" means: clear its unread marker everywhere.
        if window.is_window_active() {
            let thread_id = self.thread_id(cx);
            let is_unread = ThreadReadState::try_global(cx)
                .is_some_and(|state| state.read(cx).is_unread(&thread_id));
            if is_unread {
                cx.defer(move |cx| {
                    ThreadReadState::global(cx).update(cx, |state, cx| {
                        state.mark_read(&thread_id, cx);
                    });
                });
            }
        }

        WithRemSize::new(ThemeSettings::get_global(cx).agent_ui_font_size(cx))
            .size_full()
            .child(self.conversation_view.clone())
    }
}

impl Item for ThreadTab {
    type Event = ThreadTabEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ThreadTabEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).title(cx)
    }

    fn can_split(&self) -> bool {
        false
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Tab Opened")
    }
}

/// Pane item standing in for a thread whose tab lives in another workspace's
/// agent panel, so the strip carries the window-wide order. Activating it
/// switches to the owning workspace (handled by the panel).
pub struct ForeignThreadTab {
    thread_id: ThreadId,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl EventEmitter<()> for ForeignThreadTab {}

impl ForeignThreadTab {
    pub fn new(
        thread_id: ThreadId,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            thread_id,
            workspace,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub fn home_workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    fn home_conversation_view(&self, cx: &App) -> Option<Entity<ConversationView>> {
        let workspace = self.workspace.upgrade()?;
        let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
        panel.read(cx).conversation_view_for_id(&self.thread_id, cx)
    }

    fn title(&self, cx: &App) -> SharedString {
        if let Some(conversation_view) = self.home_conversation_view(cx) {
            return conversation_view.read(cx).title(cx);
        }
        ThreadMetadataStore::try_global(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .entry(self.thread_id)
                    .map(|metadata| metadata.display_title())
            })
            .unwrap_or_else(|| SharedString::from("Worktree"))
    }
}

impl Focusable for ForeignThreadTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ForeignThreadTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Normally never visible: activation re-routes to the owning workspace.
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .child(
                Label::new("Worktree is open in another workspace")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
    }
}

impl Item for ForeignThreadTab {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.title(cx)
    }

    fn can_split(&self) -> bool {
        false
    }
}
