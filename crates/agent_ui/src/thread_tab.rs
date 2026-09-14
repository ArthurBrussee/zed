//! Agent threads as tabs in the agent panel's own pane.
//!
//! A [`ThreadTab`] wraps a [`ConversationView`] and lives in the
//! [`AgentPanel`]'s thread pane, so the panel hosts thread tabs the same way
//! the terminal panel hosts terminal tabs. An open tab is what "open in Zed"
//! means: closing a tab closes the thread, cancelling any running turn. Tabs
//! are restored across restarts by the panel's own serialization, not by
//! workspace item serialization.

use acp_thread::ThreadStatus;
use editor::Editor;
use gpui::{
    Action, AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelCommon, LabelSize, Tooltip,
    agent_running_indicator, h_flex, utils::WithRemSize, v_flex,
};
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams, TabTooltipContent},
};

use crate::{
    AgentPanel, RenameThread,
    conversation_view::{AcpThreadViewEvent, ConversationView},
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
    thread_read_state::ThreadReadState,
};

/// What a thread tab shows next to its title, shared between local and
/// foreign thread tabs. Running uses the shared spinner glyph (the same one
/// as sidebar rows and the thread view); the unread accent dot only appears
/// once a turn has completed while the thread was not being viewed.
enum TabIndicator {
    Running,
    Attention(Color),
    Unread,
}

fn conversation_tab_indicator(
    conversation_view: &Entity<ConversationView>,
    cx: &App,
) -> Option<TabIndicator> {
    let thread = conversation_view.read(cx).root_thread(cx)?;
    let thread = thread.read(cx);
    if thread.is_waiting_for_confirmation() {
        Some(TabIndicator::Attention(Color::Warning))
    } else if thread.had_error() {
        Some(TabIndicator::Attention(Color::Error))
    } else if thread.status() != ThreadStatus::Idle {
        Some(TabIndicator::Running)
    } else if ThreadReadState::try_global(cx).is_some_and(|state| {
        state
            .read(cx)
            .is_unread(&conversation_view.read(cx).parent_id())
    }) {
        Some(TabIndicator::Unread)
    } else {
        None
    }
}

fn render_tab_indicator(indicator: TabIndicator, cx: &App) -> AnyElement {
    let dot = |color: Color| {
        div()
            .size(px(6.))
            .flex_none()
            .rounded_full()
            .bg(color.color(cx))
            .into_any_element()
    };
    match indicator {
        TabIndicator::Running => agent_running_indicator(),
        TabIndicator::Attention(color) => dot(color),
        TabIndicator::Unread => dot(Color::Accent),
    }
}

/// Tab content shared by [`ThreadTab`] and [`ForeignThreadTab`]: title plus a
/// status indicator.
fn render_tab_content(
    title: SharedString,
    agent_icon: IconName,
    brand_color: Option<gpui::Hsla>,
    indicator: Option<TabIndicator>,
    params: &TabContentParams,
    cx: &App,
) -> AnyElement {
    h_flex()
        .gap_1p5()
        .child(
            Icon::new(agent_icon).size(IconSize::Small).color(
                brand_color
                    .map(Color::Custom)
                    .unwrap_or_else(|| params.text_color()),
            ),
        )
        .children(indicator.map(|indicator| render_tab_indicator(indicator, cx)))
        .child(Label::new(title).color(params.text_color()))
        .into_any_element()
}

pub struct ThreadTab {
    conversation_view: Entity<ConversationView>,
    workspace: WeakEntity<Workspace>,
    weak_self: WeakEntity<Self>,
    /// Inline editor shown in the tab while the thread is being renamed.
    rename_editor: Option<Entity<Editor>>,
    _rename_editor_subscription: Option<Subscription>,
    _observation: Subscription,
    _read_state_observation: Subscription,
    _metadata_observation: Option<Subscription>,
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

        // Repaint the tab's unread dot when the shared read state changes.
        let read_state = ThreadReadState::global(cx);
        let read_state_observation = cx.observe(&read_state, |_this, _, cx| {
            cx.emit(ThreadTabEvent::UpdateTab);
            cx.notify();
        });

        // A rename from the sidebar only writes the metadata store's title
        // override; the tab title reads it, so repaint when the store changes.
        let metadata_observation = ThreadMetadataStore::try_global(cx).map(|store| {
            cx.observe(&store, |_this, _store, cx| {
                cx.emit(ThreadTabEvent::UpdateTab);
                cx.notify();
            })
        });

        cx.on_release(|this: &mut Self, cx: &mut App| {
            // Closing the tab closes the thread. Cancel any running turn
            // before the ConversationView drops and closes its sessions.
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
            weak_self: cx.weak_entity(),
            rename_editor: None,
            _rename_editor_subscription: None,
            _observation: observation,
            _read_state_observation: read_state_observation,
            _metadata_observation: metadata_observation,
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

    fn tab_indicator(&self, cx: &App) -> Option<TabIndicator> {
        conversation_tab_indicator(&self.conversation_view, cx)
    }

    /// Starts renaming the thread with an inline editor in the tab, seeded
    /// with the current title.
    pub fn begin_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_title = self.conversation_view.read(cx).title(cx);

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let subscription = cx.subscribe_in(&rename_editor, window, {
            let rename_editor = rename_editor.clone();
            move |_this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    // Defer to let focus settle (avoids canceling while the
                    // editor is still being focused).
                    let rename_editor = rename_editor.clone();
                    cx.defer_in(window, move |this: &mut Self, window, cx| {
                        let still_current = this
                            .rename_editor
                            .as_ref()
                            .is_some_and(|current| current == &rename_editor);
                        if still_current && !rename_editor.focus_handle(cx).is_focused(window) {
                            this.finish_rename(false, window, cx);
                        }
                    });
                }
            }
        });

        self.rename_editor = Some(rename_editor.clone());
        self._rename_editor_subscription = Some(subscription);

        rename_editor.update(cx, |editor, cx| {
            editor.set_text(current_title, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.emit(ThreadTabEvent::UpdateTab);
        cx.notify();
    }

    fn finish_rename(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self._rename_editor_subscription = None;

        if save {
            let title = SharedString::from(editor.read(cx).text(cx).trim().to_string());
            if !title.is_empty() && title != self.conversation_view.read(cx).title(cx) {
                self.apply_rename(title, window, cx);
            }
        }

        self.conversation_view.focus_handle(cx).focus(window, cx);
        cx.emit(ThreadTabEvent::UpdateTab);
        cx.notify();
    }

    /// Persists a user-supplied title. Prefer renaming through the live
    /// thread view (which also updates the agent-side title); fall back to
    /// the metadata store's title override so the sidebar row updates even
    /// while the thread view is still loading.
    fn apply_rename(&mut self, title: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread_view) = self.conversation_view.read(cx).root_thread_view() {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.rename(title, window, cx);
            });
            return;
        }

        let thread_id = self.thread_id(cx);
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.set_title_override(thread_id, title, cx);
            });
        }
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

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let title = self.tab_content_text(params.detail.unwrap_or_default(), cx);
        let is_renaming = self.rename_editor.is_some();
        let rename = self.weak_self.clone();

        let agent_icon = self.conversation_view.read(cx).agent_logo();
        let brand_color =
            crate::agent_brand_color(&self.conversation_view.read(cx).agent_key().id());

        h_flex()
            .gap_1p5()
            .when(!params.selected, |this| {
                this.track_focus(&self.conversation_view.focus_handle(cx))
            })
            .on_action(move |_: &RenameThread, window, cx| {
                rename
                    .update(cx, |this, cx| this.begin_rename(window, cx))
                    .ok();
            })
            .child(
                Icon::new(agent_icon).size(IconSize::Small).color(
                    brand_color
                        .map(Color::Custom)
                        .unwrap_or_else(|| params.text_color()),
                ),
            )
            .children(
                self.tab_indicator(cx)
                    .map(|indicator| render_tab_indicator(indicator, cx)),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Label::new(title)
                            .color(params.text_color())
                            .when(is_renaming, |this| this.alpha(0.)),
                    )
                    .when_some(self.rename_editor.clone(), |this, editor| {
                        let confirm = self.weak_self.clone();
                        let cancel = self.weak_self.clone();
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .child(editor)
                                .on_action(move |_: &menu::Confirm, window, cx| {
                                    confirm
                                        .update(cx, |this, cx| this.finish_rename(true, window, cx))
                                        .ok();
                                })
                                .on_action(move |_: &menu::Cancel, window, cx| {
                                    cancel
                                        .update(cx, |this, cx| {
                                            this.finish_rename(false, window, cx)
                                        })
                                        .ok();
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn Action>)> {
        vec![("Rename Worktree".into(), Box::new(RenameThread))]
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let title = self.conversation_view.read(cx).title(cx);
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element(
            move |_, _| {
                ui::v_flex()
                    .child(Label::new(title.clone()))
                    .child(
                        Label::new("Agent")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            },
        ))))
    }

    fn can_split(&self) -> bool {
        false
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Tab Opened")
    }
}

/// Tab-strip proxy for a thread whose tab lives in another workspace's agent
/// panel. Shows the same title and status dot as the real [`ThreadTab`], but
/// never owns or renders a `ConversationView`; activating it switches to the
/// owning workspace instead (handled by the panel's pane-event handler).
pub struct ForeignThreadTab {
    thread_id: ThreadId,
    /// The workspace whose panel hosts the real thread tab.
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _observations: Vec<Subscription>,
}

pub enum ForeignThreadTabEvent {
    UpdateTab,
}

impl EventEmitter<ForeignThreadTabEvent> for ForeignThreadTab {}

impl ForeignThreadTab {
    pub fn new(
        thread_id: ThreadId,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut observations = Vec::new();
        // Live status comes from the owning workspace's panel; re-render the
        // tab whenever that panel or the thread metadata change.
        if let Some(panel) = workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
        {
            observations.push(cx.observe(&panel, |_this, _panel, cx| {
                cx.emit(ForeignThreadTabEvent::UpdateTab);
            }));
        }
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            observations.push(cx.observe(&store, |_this, _store, cx| {
                cx.emit(ForeignThreadTabEvent::UpdateTab);
            }));
        }
        let read_state = ThreadReadState::global(cx);
        observations.push(cx.observe(&read_state, |_this, _, cx| {
            cx.emit(ForeignThreadTabEvent::UpdateTab);
        }));
        Self {
            thread_id,
            workspace,
            focus_handle: cx.focus_handle(),
            _observations: observations,
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// The workspace hosting the real thread tab.
    pub fn home_workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    /// The real thread's conversation view, resolved through the owning
    /// workspace's panel. `None` when the workspace or panel is gone.
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
        // Normally never visible: activation immediately re-activates a
        // local tab and switches to the owning workspace.
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
    type Event = ForeignThreadTabEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ForeignThreadTabEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.title(cx)
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let home_conversation_view = self.home_conversation_view(cx);
        let indicator = home_conversation_view
            .as_ref()
            .and_then(|conversation_view| conversation_tab_indicator(conversation_view, cx));
        let (agent_icon, brand_color) = home_conversation_view
            .map(|conversation_view| {
                let conversation_view = conversation_view.read(cx);
                (
                    conversation_view.agent_logo(),
                    crate::agent_brand_color(&conversation_view.agent_key().id()),
                )
            })
            .unwrap_or((IconName::Thread, None));
        render_tab_content(
            self.title(cx),
            agent_icon,
            brand_color,
            indicator,
            &params,
            cx,
        )
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let title = self.title(cx);
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element(
            move |_, _| {
                v_flex()
                    .child(Label::new(title.clone()))
                    .child(
                        Label::new("Agent in another workspace")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            },
        ))))
    }

    fn can_split(&self) -> bool {
        false
    }
}
