use std::rc::Rc;

use acp_thread::{AgentModelIcon, AgentModelInfo, AgentModelSelector};
use gpui::{
    Animation, AnimationExt as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    pulsating_between,
};
use std::time::Duration;
use ui::{Divider, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};

use crate::ui::ModelSelectorTooltip;
use crate::{ModelSelector, model_selector::acp_model_selector};

#[derive(Clone)]
pub struct EffortOption {
    pub name: SharedString,
    pub value: SharedString,
    pub selected: bool,
}

/// Built by the thread view, which owns the thread and settings writes.
#[derive(Clone)]
pub struct EffortMenuSection {
    /// `(enabled, toggle)`, when the model allows disabling thinking.
    pub thinking_toggle: Option<(bool, Rc<dyn Fn(&mut Window, &mut App)>)>,
    pub effort_options: Vec<EffortOption>,
    pub on_select_effort: Rc<dyn Fn(SharedString, &mut Window, &mut App)>,
    pub selected_label: Option<SharedString>,
}

#[derive(Clone)]
pub struct FastModeSection {
    pub enabled: bool,
    pub toggle: Rc<dyn Fn(&mut Window, &mut App)>,
    /// The provider's warning, shown inline before enabling fast mode.
    pub confirmation: Option<FastModeConfirmationRows>,
}

#[derive(Clone)]
pub struct FastModeConfirmationRows {
    pub title: SharedString,
    pub message: SharedString,
    pub enable_and_dismiss: Rc<dyn Fn(&mut Window, &mut App)>,
}

pub struct ModelSelectorPopover {
    selector: Entity<ModelSelector>,
    menu: Entity<ModelEffortMenu>,
    menu_handle: PopoverMenuHandle<ModelEffortMenu>,
    /// The trigger glimmers while the thread is generating.
    working: bool,
}

impl ModelSelectorPopover {
    pub(crate) fn new(
        selector: Rc<dyn AgentModelSelector>,
        menu_handle: PopoverMenuHandle<ModelEffortMenu>,
        focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let selector =
            cx.new(move |cx| acp_model_selector(selector, focus_handle.clone(), window, cx));
        selector.update(cx, |picker, _| picker.set_popover());
        let menu = cx.new(|cx| ModelEffortMenu::new(selector.clone(), cx));
        Self {
            selector,
            menu,
            menu_handle,
            working: false,
        }
    }

    pub fn set_working(&mut self, working: bool, cx: &mut Context<Self>) {
        if self.working != working {
            self.working = working;
            cx.notify();
        }
    }

    pub fn toggle(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_handle.toggle(window, cx);
    }

    pub fn active_model<'a>(&self, cx: &'a App) -> Option<&'a AgentModelInfo> {
        self.selector.read(cx).delegate.active_model()
    }

    pub fn cycle_favorite_models(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.selector.update(cx, |selector, cx| {
            selector.delegate.cycle_favorite_models(window, cx);
        });
    }

    pub fn set_effort_section(&self, effort: Option<EffortMenuSection>, cx: &mut Context<Self>) {
        self.menu.update(cx, |menu, _cx| {
            menu.effort = effort;
        });
    }

    pub fn set_fast_mode_section(
        &self,
        fast_mode: Option<FastModeSection>,
        cx: &mut Context<Self>,
    ) {
        self.menu.update(cx, |menu, _cx| {
            menu.fast_mode = fast_mode;
        });
    }
}

impl Render for ModelSelectorPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selector = self.selector.read(cx);
        let model = selector.delegate.active_model();
        let model_name = model
            .as_ref()
            .map(|model| model.name.clone())
            .unwrap_or_else(|| SharedString::from("Select a Model"));

        let model_icon = model.as_ref().and_then(|model| model.icon.clone());

        let effort_label = self
            .menu
            .read(cx)
            .effort
            .as_ref()
            .and_then(|effort| effort.selected_label.clone());
        let label = match &effort_label {
            Some(effort) => SharedString::from(format!("{model_name} / {effort}")),
            None => model_name,
        };

        let fast_mode_on = self
            .menu
            .read(cx)
            .fast_mode
            .as_ref()
            .is_some_and(|fast_mode| fast_mode.enabled);

        let (color, icon) = if self.menu_handle.is_deployed() {
            (Color::Accent, IconName::ChevronUp)
        } else if self.working {
            (Color::Accent, IconName::ChevronDown)
        } else {
            (Color::Muted, IconName::ChevronDown)
        };

        let show_cycle_row = selector.delegate.favorites_count() > 1;

        let tooltip = Tooltip::element({
            move |_, _cx| {
                ModelSelectorTooltip::new()
                    .show_cycle_row(show_cycle_row)
                    .into_any_element()
            }
        });

        // Fast mode takes over the only leading icon slot.
        let start_icon = if fast_mode_on {
            Some(
                Icon::new(IconName::FastForward)
                    .color(Color::Accent)
                    .size(IconSize::XSmall),
            )
        } else {
            model_icon.map(|icon| {
                match icon {
                    AgentModelIcon::Path(path) => Icon::from_external_svg(path),
                    AgentModelIcon::Named(icon_name) => Icon::new(icon_name),
                }
                .color(color)
                .size(IconSize::XSmall)
            })
        };

        let trigger = Button::new("active-model", label)
            .label_size(LabelSize::Small)
            .color(color)
            .when_some(start_icon, |this, icon| this.start_icon(icon))
            .end_icon(Icon::new(icon).color(Color::Muted).size(IconSize::XSmall));

        let menu = self.menu.clone();
        let popover = PopoverMenu::new("model-effort-popover")
            .menu(move |_window, _cx| Some(menu.clone()))
            .trigger_with_tooltip(trigger, tooltip)
            .anchor(gpui::Anchor::BottomRight)
            .with_handle(self.menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            });

        if self.working {
            let glow = cx.theme().colors().text_accent;
            return div()
                .rounded_md()
                .child(popover)
                .with_animation(
                    "model-working-glimmer",
                    Animation::new(Duration::from_millis(1400))
                        .repeat()
                        .with_easing(pulsating_between(0.08, 0.3)),
                    move |this, delta| this.bg(glow.opacity(delta)),
                )
                .into_any_element();
        }
        div().child(popover).into_any_element()
    }
}

pub struct ModelEffortMenu {
    picker: Entity<ModelSelector>,
    effort: Option<EffortMenuSection>,
    fast_mode: Option<FastModeSection>,
    _dismiss_subscription: gpui::Subscription,
}

impl ModelEffortMenu {
    fn new(picker: Entity<ModelSelector>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&picker, |_this, _picker, &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            effort: None,
            fast_mode: None,
            _dismiss_subscription: subscription,
        }
    }
}

impl EventEmitter<DismissEvent> for ModelEffortMenu {}

impl Focusable for ModelEffortMenu {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ModelEffortMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hover_bg = cx.theme().colors().element_hover;
        v_flex()
            .min_w(rems(14.))
            // PopoverMenu renders this view bare, without PickerPopoverMenu's
            // surface chrome.
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_lg()
            .shadow_md()
            .overflow_hidden()
            .child(self.picker.clone())
            .when_some(self.effort.clone(), |this, effort| {
                let has_content =
                    effort.thinking_toggle.is_some() || !effort.effort_options.is_empty();
                if !has_content {
                    return this;
                }

                this.child(Divider::horizontal()).child(
                    v_flex()
                        .p_1()
                        .child(
                            Label::new("Thinking Effort")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when_some(effort.thinking_toggle.clone(), |this, (enabled, toggle)| {
                            this.child(effort_row(
                                "menu-thinking-toggle",
                                if enabled {
                                    "Thinking On"
                                } else {
                                    "Thinking Off"
                                },
                                enabled,
                                hover_bg,
                                cx.listener(move |_this, _, window, cx| {
                                    toggle(window, cx);
                                    cx.emit(DismissEvent);
                                }),
                            ))
                        })
                        .children(effort.effort_options.iter().enumerate().map(
                            |(index, option)| {
                                let value = option.value.clone();
                                let on_select = effort.on_select_effort.clone();
                                effort_row(
                                    ("menu-effort", index),
                                    option.name.clone(),
                                    option.selected,
                                    hover_bg,
                                    cx.listener(move |_this, _, window, cx| {
                                        on_select(value.clone(), window, cx);
                                        cx.emit(DismissEvent);
                                    }),
                                )
                            },
                        )),
                )
            })
            .when_some(self.fast_mode.clone(), |this, fast_mode| {
                this.child(Divider::horizontal()).child(
                    v_flex()
                        .p_1()
                        .child(
                            Label::new("Fast Mode")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(effort_row(
                            "menu-fast-mode",
                            if fast_mode.enabled {
                                "Fast Mode On"
                            } else {
                                "Fast Mode Off"
                            },
                            fast_mode.enabled,
                            hover_bg,
                            {
                                let toggle = fast_mode.toggle.clone();
                                cx.listener(move |_this, _, window, cx| {
                                    toggle(window, cx);
                                    cx.emit(DismissEvent);
                                })
                            },
                        ))
                        .when_some(fast_mode.confirmation, |this, confirmation| {
                            this.child(
                                v_flex()
                                    .px_2()
                                    .py_1()
                                    .max_w(rems(18.))
                                    .child(
                                        Label::new(confirmation.title.clone())
                                            .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new(confirmation.message.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(effort_row(
                                "menu-fast-mode-dismiss",
                                "Enable and Don't Show Again",
                                false,
                                hover_bg,
                                {
                                    let enable_and_dismiss =
                                        confirmation.enable_and_dismiss.clone();
                                    cx.listener(move |_this, _, window, cx| {
                                        enable_and_dismiss(window, cx);
                                        cx.emit(DismissEvent);
                                    })
                                },
                            ))
                        }),
                )
            })
    }
}

fn effort_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    hover_bg: gpui::Hsla,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id.into())
        .w_full()
        .px_2()
        .py_1()
        .gap_2()
        .justify_between()
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_click(on_click)
        .child(Label::new(label.into()).size(LabelSize::Small))
        .when(selected, |this| {
            this.child(
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
        })
}
