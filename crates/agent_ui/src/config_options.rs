use std::{rc::Rc, sync::Arc};

use acp_thread::AgentSessionConfigOptions;
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;

use collections::HashSet;
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    Window, prelude::*,
};
use settings::{AgentConfigOptionValue, SettingsStore};
use ui::{
    Divider, ElevationIndex, IconButton, KeyBinding, PopoverMenu, PopoverMenuHandle, Switch,
    SwitchLabelPosition, ToggleState, Tooltip, prelude::*,
};
use unicode_segmentation::UnicodeSegmentation;
use util::ResultExt as _;
use zed_actions::agent::ToggleModelSelector;

use crate::{
    CycleFavoriteModels, CycleModeSelector, CycleThinkingEffort, ToggleProfileSelector,
    ToggleThinkingEffortMenu,
};

/// The external (ACP) agent's counterpart to the native agent's merged
/// model/effort/fast-mode picker: one input-bar trigger reading the agent's
/// salient values, opening one popover with a section per config option the
/// agent advertises. The agent decides which options exist; nothing here is
/// keyed on a specific option id.
pub struct ConfigOptionsView {
    config_options: Rc<dyn AgentSessionConfigOptions>,
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    config_option_ids: Vec<acp::SessionConfigId>,
    menu: Entity<ConfigOptionsMenu>,
    menu_handle: PopoverMenuHandle<ConfigOptionsMenu>,
    _refresh_task: Task<()>,
}

impl ConfigOptionsView {
    pub fn new(
        config_options: Rc<dyn AgentSessionConfigOptions>,
        agent_server: Rc<dyn AgentServer>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config_option_ids = Self::config_option_ids(&config_options);
        let menu = Self::build_menu(&config_options, &agent_server, &fs, cx);

        let rx = config_options.watch(cx);
        let refresh_task = cx.spawn_in(window, async move |this, cx| {
            if let Some(mut rx) = rx {
                while let Ok(()) = rx.recv().await {
                    this.update(cx, |this, cx| {
                        this.refresh(cx);
                    })
                    .log_err();
                }
            }
        });

        Self {
            config_options,
            agent_server,
            fs,
            config_option_ids,
            menu,
            menu_handle: PopoverMenuHandle::default(),
            _refresh_task: refresh_task,
        }
    }

    fn build_menu(
        config_options: &Rc<dyn AgentSessionConfigOptions>,
        agent_server: &Rc<dyn AgentServer>,
        fs: &Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Entity<ConfigOptionsMenu> {
        let config_options = config_options.clone();
        let agent_server = agent_server.clone();
        let fs = fs.clone();
        cx.new(|cx| ConfigOptionsMenu::new(config_options, agent_server, fs, cx))
    }

    /// Opens the one settings popover, if the agent advertises an option of this
    /// category at all. There is no per-category popover to open: the category's
    /// section lives in the same popover as every other option's.
    pub fn toggle_category_picker(
        &mut self,
        category: acp::SessionConfigOptionCategory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .first_config_option_id_matching(category, |option| {
                matches!(&option.kind, acp::SessionConfigKind::Select(_))
            })
            .is_none()
        {
            return false;
        }

        self.menu_handle.toggle(window, cx);
        true
    }

    pub fn cycle_category_option(
        &mut self,
        category: acp::SessionConfigOptionCategory,
        favorites_only: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(config_id) = self.first_config_option_id_matching(category, |option| {
            Self::can_cycle_config_option(option, favorites_only)
        }) else {
            return false;
        };

        let Some(next_value) = self.next_value_for_config(&config_id, favorites_only, cx) else {
            return false;
        };

        set_config_option(
            &self.config_options,
            &self.agent_server,
            &self.fs,
            config_id,
            next_value,
            cx,
        );

        true
    }

    fn first_config_option_id_matching(
        &self,
        category: acp::SessionConfigOptionCategory,
        predicate: impl Fn(&acp::SessionConfigOption) -> bool,
    ) -> Option<acp::SessionConfigId> {
        self.config_options
            .config_options()
            .into_iter()
            .find(|option| option.category.as_ref() == Some(&category) && predicate(option))
            .map(|option| option.id)
    }

    fn can_cycle_config_option(option: &acp::SessionConfigOption, favorites_only: bool) -> bool {
        match &option.kind {
            acp::SessionConfigKind::Select(_) => true,
            acp::SessionConfigKind::Boolean(_) => !favorites_only,
            _ => false,
        }
    }

    fn next_value_for_config(
        &self,
        config_id: &acp::SessionConfigId,
        favorites_only: bool,
        cx: &mut Context<Self>,
    ) -> Option<acp::SessionConfigOptionValue> {
        let option = self
            .config_options
            .config_options()
            .into_iter()
            .find(|option| &option.id == config_id)?;

        match &option.kind {
            acp::SessionConfigKind::Select(_) => {
                let mut options = extract_options(&self.config_options, config_id);
                if options.is_empty() {
                    return None;
                }

                if favorites_only {
                    let favorites = self
                        .agent_server
                        .favorite_config_option_value_ids(config_id, cx);
                    options.retain(|option| favorites.contains(&option.value));
                    if options.is_empty() {
                        return None;
                    }
                }

                let current_value = get_current_select_value(&self.config_options, config_id);
                let current_index = current_value
                    .as_ref()
                    .and_then(|current| options.iter().position(|option| &option.value == current))
                    .unwrap_or(usize::MAX);

                let next_index = if current_index == usize::MAX {
                    0
                } else {
                    (current_index + 1) % options.len()
                };

                Some(acp::SessionConfigOptionValue::value_id(
                    options[next_index].value.clone(),
                ))
            }
            acp::SessionConfigKind::Boolean(boolean) => {
                if favorites_only {
                    None
                } else {
                    Some(acp::SessionConfigOptionValue::boolean(
                        !boolean.current_value,
                    ))
                }
            }
            _ => None,
        }
    }

    fn config_option_ids(
        config_options: &Rc<dyn AgentSessionConfigOptions>,
    ) -> Vec<acp::SessionConfigId> {
        config_options
            .config_options()
            .into_iter()
            .map(|option| option.id)
            .collect()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Config option updates can mutate option values for existing IDs (for
        // example, reasoning levels after a model switch). The menu reads the
        // options on every render, so only the cached ids need refreshing.
        self.config_option_ids = Self::config_option_ids(&self.config_options);
        self.menu.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// The trigger's label: the current value of every select option, joined the
    /// way the native picker joins model and effort ("Sonnet 4.5 / High").
    fn trigger_label(&self) -> SharedString {
        let values = self
            .config_options
            .config_options()
            .into_iter()
            .filter_map(|option| match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    find_option_name(&select.options, &select.current_value)
                }
                _ => None,
            })
            .map(|value| truncate_value(&value))
            .collect::<Vec<_>>();

        if values.is_empty() {
            SharedString::from("Settings")
        } else {
            SharedString::from(values.join(" / "))
        }
    }
}

impl Render for ConfigOptionsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let options = self.config_options.config_options();
        if options.is_empty() {
            return div().into_any_element();
        }

        let (color, chevron) = if self.menu_handle.is_deployed() {
            (Color::Accent, IconName::ChevronUp)
        } else {
            (Color::Muted, IconName::ChevronDown)
        };

        // An enabled boolean (web search, fast mode, ...) has no room in the
        // label, so it takes over the leading icon slot the way fast mode does
        // in the native picker.
        let enabled_boolean = options.iter().any(|option| {
            matches!(&option.kind, acp::SessionConfigKind::Boolean(boolean) if boolean.current_value)
        });
        let start_icon = if enabled_boolean {
            Icon::new(IconName::ZedAssistant)
                .color(Color::Accent)
                .size(IconSize::XSmall)
        } else {
            Icon::new(self.agent_server.logo())
                .color(color)
                .size(IconSize::XSmall)
        };

        let trigger = Button::new("agent-config-options", self.trigger_label())
            .label_size(LabelSize::Small)
            .color(color)
            .start_icon(start_icon)
            .end_icon(
                Icon::new(chevron)
                    .color(Color::Muted)
                    .size(IconSize::XSmall),
            );

        let tooltip = config_options_tooltip(&options);
        let menu = self.menu.clone();

        PopoverMenu::new("agent-config-options-popover")
            .menu(move |_window, _cx| Some(menu.clone()))
            .trigger_with_tooltip(trigger, tooltip)
            .anchor(gpui::Anchor::BottomRight)
            .with_handle(self.menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .into_any_element()
    }
}

fn config_options_tooltip(
    options: &[acp::SessionConfigOption],
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<> {
    let rows = options
        .iter()
        .map(|option| {
            let value: SharedString = match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    find_option_name(&select.options, &select.current_value)
                        .unwrap_or_else(|| "Unknown".to_string())
                        .into()
                }
                acp::SessionConfigKind::Boolean(boolean) => {
                    if boolean.current_value { "On" } else { "Off" }.into()
                }
                _ => SharedString::from("Unknown"),
            };
            (
                SharedString::from(option.name.clone()),
                value,
                option.category.clone(),
            )
        })
        .collect::<Vec<_>>();

    Tooltip::element(move |_window, cx| {
        let mut content = v_flex().gap_1().child(Label::new("Agent Settings"));

        for (name, value, _) in &rows {
            content = content.child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(value.clone()).size(LabelSize::Small)),
            );
        }

        let keybinding_row = |label: &str, keybinding: KeyBinding, cx: &App| {
            h_flex()
                .pt_1()
                .gap_2()
                .justify_between()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Label::new(label.to_string()))
                .child(keybinding)
        };

        for (_, _, category) in &rows {
            match category {
                Some(acp::SessionConfigOptionCategory::Model) => {
                    content = content
                        .child(keybinding_row(
                            "Open Settings",
                            KeyBinding::for_action(&ToggleModelSelector, cx),
                            cx,
                        ))
                        .child(keybinding_row(
                            "Cycle Favorite Models",
                            KeyBinding::for_action(&CycleFavoriteModels, cx),
                            cx,
                        ));
                }
                Some(acp::SessionConfigOptionCategory::Mode) => {
                    content = content
                        .child(keybinding_row(
                            "Change Mode",
                            KeyBinding::for_action(&ToggleProfileSelector, cx),
                            cx,
                        ))
                        .child(keybinding_row(
                            "Cycle Through Modes",
                            KeyBinding::for_action(&CycleModeSelector, cx),
                            cx,
                        ));
                }
                Some(acp::SessionConfigOptionCategory::ThoughtLevel) => {
                    content = content
                        .child(keybinding_row(
                            "Change Thinking Effort",
                            KeyBinding::for_action(&ToggleThinkingEffortMenu, cx),
                            cx,
                        ))
                        .child(keybinding_row(
                            "Cycle Thinking Effort",
                            KeyBinding::for_action(&CycleThinkingEffort, cx),
                            cx,
                        ));
                }
                _ => {}
            }
        }

        content.into_any()
    })
}

/// The one popover: a section per config option the agent advertises. Select
/// options list their values (favorites first, star to favorite); boolean
/// options render a switch.
struct ConfigOptionsMenu {
    config_options: Rc<dyn AgentSessionConfigOptions>,
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    _settings_subscription: Subscription,
}

impl ConfigOptionsMenu {
    fn new(
        config_options: Rc<dyn AgentSessionConfigOptions>,
        agent_server: Rc<dyn AgentServer>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Favorites live in settings, and the star buttons in here write them.
        let settings_subscription = cx.observe_global::<SettingsStore>(|_this, cx| cx.notify());

        Self {
            config_options,
            agent_server,
            fs,
            focus_handle: cx.focus_handle(),
            _settings_subscription: settings_subscription,
        }
    }

    fn render_select_section(
        &self,
        option: &acp::SessionConfigOption,
        section_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let config_id = option.id.clone();
        let favorites = self
            .agent_server
            .favorite_config_option_value_ids(&config_id, cx);
        let values = extract_options(&self.config_options, &config_id);
        let current_value = get_current_select_value(&self.config_options, &config_id);
        let entries = options_to_entries(&values, &favorites);
        let hover_bg = cx.theme().colors().element_hover;

        v_flex()
            .id(("config-option-section", section_ix))
            .max_h(rems(18.))
            .overflow_y_scroll()
            .children(entries.into_iter().enumerate().map(|(ix, entry)| {
                match entry {
                    ConfigOptionEntry::Separator(title) => div()
                        .px_2()
                        .py_1()
                        .child(
                            Label::new(title)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                    ConfigOptionEntry::Option(value) => {
                        let is_selected = current_value.as_ref() == Some(&value.value);
                        let is_favorite = favorites.contains(&value.value);

                        h_flex()
                            .id(("config-option-value", section_ix * 1000 + ix))
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .justify_between()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover_bg))
                            .when_some(value.description.clone(), |this, description| {
                                this.tooltip(Tooltip::text(description))
                            })
                            .on_click({
                                let config_id = config_id.clone();
                                let value_id = value.value.clone();
                                cx.listener(move |this, _, _window, cx| {
                                    set_config_option(
                                        &this.config_options,
                                        &this.agent_server,
                                        &this.fs,
                                        config_id.clone(),
                                        acp::SessionConfigOptionValue::value_id(value_id.clone()),
                                        cx,
                                    );
                                    cx.emit(DismissEvent);
                                })
                            })
                            .child(Label::new(value.name.clone()).size(LabelSize::Small))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child({
                                        let (icon, color, tooltip) = if is_favorite {
                                            (IconName::StarFilled, Color::Accent, "Unfavorite")
                                        } else {
                                            (IconName::Star, Color::Muted, "Favorite")
                                        };
                                        let config_id = config_id.clone();
                                        let value_id = value.value;

                                        IconButton::new(
                                            ("toggle-favorite", section_ix * 1000 + ix),
                                            icon,
                                        )
                                        .layer(ElevationIndex::ElevatedSurface)
                                        .icon_color(color)
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text(tooltip))
                                        .on_click(
                                            cx.listener(move |this, _, _window, cx| {
                                                this.agent_server
                                                    .toggle_favorite_config_option_value(
                                                        config_id.clone(),
                                                        value_id.clone(),
                                                        !is_favorite,
                                                        this.fs.clone(),
                                                        cx,
                                                    );
                                            }),
                                        )
                                    })
                                    .when(is_selected, |this| {
                                        this.child(
                                            Icon::new(IconName::Check)
                                                .size(IconSize::XSmall)
                                                .color(Color::Accent),
                                        )
                                    }),
                            )
                            .into_any_element()
                    }
                }
            }))
            .into_any_element()
    }

    fn render_boolean_section(
        &self,
        option: &acp::SessionConfigOption,
        boolean: &acp::SessionConfigBoolean,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let config_id = option.id.clone();
        let current_value = boolean.current_value;
        let toggle_state = if current_value {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };

        div()
            .px_2()
            .py_1()
            .child(
                Switch::new(
                    ElementId::Name(format!("config-option-{}-switch", config_id.0).into()),
                    toggle_state,
                )
                .label(if current_value { "On" } else { "Off" })
                .label_position(SwitchLabelPosition::Start)
                .label_size(LabelSize::Small)
                .label_color(Color::Muted)
                .on_click(cx.listener(move |this, state, _window, cx| {
                    let next_value = matches!(state, ToggleState::Selected);
                    set_config_option(
                        &this.config_options,
                        &this.agent_server,
                        &this.fs,
                        config_id.clone(),
                        acp::SessionConfigOptionValue::boolean(next_value),
                        cx,
                    );
                    cx.emit(DismissEvent);
                })),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for ConfigOptionsMenu {}

impl Focusable for ConfigOptionsMenu {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConfigOptionsMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let options = self.config_options.config_options();

        v_flex()
            .track_focus(&self.focus_handle)
            .min_w(rems(16.))
            .p_1()
            .children(options.iter().enumerate().filter_map(|(ix, option)| {
                let body = match &option.kind {
                    acp::SessionConfigKind::Select(_) => self.render_select_section(option, ix, cx),
                    acp::SessionConfigKind::Boolean(boolean) => {
                        self.render_boolean_section(option, boolean, cx)
                    }
                    _ => return None,
                };

                let name: SharedString = option.name.clone().into();
                let description: Option<SharedString> = option.description.clone().map(Into::into);

                Some(
                    v_flex()
                        .when(ix > 0, |this| {
                            this.child(div().py_1().child(Divider::horizontal()))
                        })
                        .child(
                            div()
                                .id(("config-option-header", ix))
                                .px_2()
                                .py_1()
                                .when_some(description, |this, description| {
                                    this.tooltip(Tooltip::text(description))
                                })
                                .child(Label::new(name).size(LabelSize::Small).color(Color::Muted)),
                        )
                        .child(body),
                )
            }))
    }
}

/// Persists the value as the agent's default and applies it to the live session.
/// Both halves are what every entry point (a picker row, a switch, a cycle
/// keybinding) has always done.
fn set_config_option(
    config_options: &Rc<dyn AgentSessionConfigOptions>,
    agent_server: &Rc<dyn AgentServer>,
    fs: &Arc<dyn Fs>,
    config_id: acp::SessionConfigId,
    value: acp::SessionConfigOptionValue,
    cx: &mut App,
) {
    agent_server.set_default_config_option(
        config_id.0.as_ref(),
        setting_value_for_config_option_value(&value),
        fs.clone(),
        cx,
    );

    let task = config_options.set_config_option(config_id, value, cx);

    cx.spawn(async move |_| {
        if let Err(err) = task.await {
            log::error!("Failed to set config option: {:?}", err);
        }
    })
    .detach();
}

#[derive(Clone)]
enum ConfigOptionEntry {
    Separator(SharedString),
    Option(ConfigOptionValue),
}

#[derive(Clone)]
struct ConfigOptionValue {
    value: acp::SessionConfigValueId,
    name: String,
    description: Option<String>,
    group: Option<String>,
}

fn truncate_value(value: &str) -> String {
    let mut graphemes = value.graphemes(true);
    let truncated = graphemes.by_ref().take(32).collect::<String>();
    if graphemes.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn extract_options(
    config_options: &Rc<dyn AgentSessionConfigOptions>,
    config_id: &acp::SessionConfigId,
) -> Vec<ConfigOptionValue> {
    let Some(option) = config_options
        .config_options()
        .into_iter()
        .find(|opt| &opt.id == config_id)
    else {
        return Vec::new();
    };

    match &option.kind {
        acp::SessionConfigKind::Select(select) => match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .map(|opt| ConfigOptionValue {
                    value: opt.value.clone(),
                    name: opt.name.clone(),
                    description: opt.description.clone(),
                    group: None,
                })
                .collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| {
                    group.options.iter().map(|opt| ConfigOptionValue {
                        value: opt.value.clone(),
                        name: opt.name.clone(),
                        description: opt.description.clone(),
                        group: Some(group.name.clone()),
                    })
                })
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn get_current_select_value(
    config_options: &Rc<dyn AgentSessionConfigOptions>,
    config_id: &acp::SessionConfigId,
) -> Option<acp::SessionConfigValueId> {
    config_options
        .config_options()
        .into_iter()
        .find(|opt| &opt.id == config_id)
        .and_then(|opt| match &opt.kind {
            acp::SessionConfigKind::Select(select) => Some(select.current_value.clone()),
            _ => None,
        })
}

fn setting_value_for_config_option_value(
    value: &acp::SessionConfigOptionValue,
) -> Option<AgentConfigOptionValue> {
    match value {
        acp::SessionConfigOptionValue::ValueId { value } => {
            Some(AgentConfigOptionValue::ValueId(value.0.to_string()))
        }
        acp::SessionConfigOptionValue::Boolean { value } => {
            Some(AgentConfigOptionValue::Boolean(*value))
        }
        _ => None,
    }
}

fn options_to_entries(
    options: &[ConfigOptionValue],
    favorites: &HashSet<acp::SessionConfigValueId>,
) -> Vec<ConfigOptionEntry> {
    let mut entries = Vec::new();

    let favorite_options = options
        .iter()
        .filter(|option| favorites.contains(&option.value))
        .cloned()
        .collect::<Vec<_>>();

    if !favorite_options.is_empty() {
        entries.push(ConfigOptionEntry::Separator("Favorites".into()));
        for option in favorite_options {
            entries.push(ConfigOptionEntry::Option(option));
        }

        // If the remaining list would start ungrouped (group == None), insert a separator so
        // Favorites doesn't visually run into the main list.
        if let Some(option) = options.first()
            && option.group.is_none()
        {
            entries.push(ConfigOptionEntry::Separator("All Options".into()));
        }
    }

    let mut current_group: Option<String> = None;
    for option in options {
        if option.group != current_group {
            if let Some(group_name) = &option.group {
                entries.push(ConfigOptionEntry::Separator(group_name.clone().into()));
            }
            current_group = option.group.clone();
        }
        entries.push(ConfigOptionEntry::Option(option.clone()));
    }

    entries
}

fn find_option_name(
    options: &acp::SessionConfigSelectOptions,
    value_id: &acp::SessionConfigValueId,
) -> Option<String> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(opts) => opts
            .iter()
            .find(|o| &o.value == value_id)
            .map(|o| o.name.clone()),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups.iter().find_map(|group| {
            group
                .options
                .iter()
                .find(|o| &o.value == value_id)
                .map(|o| o.name.clone())
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::AgentConnection;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use parking_lot::Mutex;
    use project::{AgentId, Project};
    use std::{any::Any, cell::RefCell};

    fn test_view(
        config_options: Rc<TestSessionConfigOptions>,
        agent_server: Rc<TestAgentServer>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) -> Entity<ConfigOptionsView> {
        let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
        let agent_server: Rc<dyn AgentServer> = agent_server;
        cx.new(|cx| ConfigOptionsView {
            config_option_ids: ConfigOptionsView::config_option_ids(&config_options),
            menu: ConfigOptionsView::build_menu(&config_options, &agent_server, &fs, cx),
            config_options,
            agent_server,
            fs,
            menu_handle: PopoverMenuHandle::default(),
            _refresh_task: Task::ready(()),
        })
    }

    #[gpui::test]
    fn cycling_config_option_saves_selected_value_as_default(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::select(
                "mode",
                "Mode",
                "auto",
                vec![
                    acp::SessionConfigSelectOption::new("auto", "Auto"),
                    acp::SessionConfigSelectOption::new("manual", "Manual"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Mode),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let view = test_view(config_options.clone(), agent_server.clone(), fs, cx);

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::Mode, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "mode".to_string(),
                Some(AgentConfigOptionValue::ValueId("manual".to_string()))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "mode".to_string(),
                acp::SessionConfigOptionValue::value_id("manual")
            )]
        );
    }

    #[gpui::test]
    fn cycling_boolean_config_option_saves_selected_value_as_default(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::ModelConfig),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let view = test_view(config_options.clone(), agent_server.clone(), fs, cx);

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::ModelConfig, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "web_search".to_string(),
                Some(AgentConfigOptionValue::Boolean(true))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "web_search".to_string(),
                acp::SessionConfigOptionValue::boolean(true)
            )]
        );
    }

    #[gpui::test]
    fn cycling_category_cycles_boolean_config_option_first(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "small",
                vec![
                    acp::SessionConfigSelectOption::new("small", "Small"),
                    acp::SessionConfigSelectOption::new("large", "Large"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let view = test_view(config_options.clone(), agent_server.clone(), fs, cx);

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::Model, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "web_search".to_string(),
                Some(AgentConfigOptionValue::Boolean(true))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "web_search".to_string(),
                acp::SessionConfigOptionValue::boolean(true)
            )]
        );
    }

    #[gpui::test]
    fn toggling_category_picker_without_select_config_option_is_unhandled(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::Model),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let cx = cx.add_empty_window();
        let view = cx.update({
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = agent_server;
                cx.new(|cx| ConfigOptionsView::new(config_options, agent_server, fs, window, cx))
            }
        });

        let handled = cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.toggle_category_picker(acp::SessionConfigOptionCategory::Model, window, cx)
            })
        });

        assert!(!handled);
    }

    #[gpui::test]
    fn toggling_any_category_picker_opens_the_one_settings_popover(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "small",
                vec![
                    acp::SessionConfigSelectOption::new("small", "Small"),
                    acp::SessionConfigSelectOption::new("large", "Large"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigOption::select(
                "effort",
                "Reasoning Effort",
                "high",
                vec![
                    acp::SessionConfigSelectOption::new("low", "Low"),
                    acp::SessionConfigSelectOption::new("high", "High"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::ThoughtLevel),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let cx = cx.add_empty_window();
        let view = cx.update({
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = agent_server;
                cx.new(|cx| ConfigOptionsView::new(config_options, agent_server, fs, window, cx))
            }
        });

        // Every category the agent advertises opens the same single popover:
        // there is one trigger, not one per option.
        for category in [
            acp::SessionConfigOptionCategory::Model,
            acp::SessionConfigOptionCategory::ThoughtLevel,
        ] {
            let handled = cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.toggle_category_picker(category.clone(), window, cx)
                })
            });
            assert!(
                handled,
                "{category:?} has an option, so it opens the picker"
            );
        }

        // The trigger reads every select option's current value, the way the
        // native picker reads "Model / Effort".
        let label = cx.update(|_window, cx| view.read(cx).trigger_label());
        assert_eq!(label, SharedString::from("Small / High"));
    }

    #[derive(Default)]
    struct TestAgentServer {
        saved_defaults: Arc<Mutex<Vec<(String, Option<AgentConfigOptionValue>)>>>,
    }

    impl AgentServer for TestAgentServer {
        fn logo(&self) -> IconName {
            IconName::ZedAssistant
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("test-agent")
        }

        fn connect(
            &self,
            _delegate: agent_servers::AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Err(anyhow::anyhow!("test agent server cannot connect")))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }

        fn set_default_config_option(
            &self,
            config_id: &str,
            value: Option<AgentConfigOptionValue>,
            _fs: Arc<dyn Fs>,
            _cx: &mut App,
        ) {
            self.saved_defaults
                .lock()
                .push((config_id.to_string(), value));
        }
    }

    struct TestSessionConfigOptions {
        options: RefCell<Vec<acp::SessionConfigOption>>,
        set_values: RefCell<Vec<(String, acp::SessionConfigOptionValue)>>,
    }

    impl TestSessionConfigOptions {
        fn new(options: Vec<acp::SessionConfigOption>) -> Self {
            Self {
                options: RefCell::new(options),
                set_values: RefCell::new(Vec::new()),
            }
        }
    }

    impl AgentSessionConfigOptions for TestSessionConfigOptions {
        fn config_options(&self) -> Vec<acp::SessionConfigOption> {
            self.options.borrow().clone()
        }

        fn set_config_option(
            &self,
            config_id: acp::SessionConfigId,
            value: acp::SessionConfigOptionValue,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Vec<acp::SessionConfigOption>>> {
            self.set_values
                .borrow_mut()
                .push((config_id.0.to_string(), value.clone()));

            let options = {
                let mut options = self.options.borrow_mut();
                if let Some(option) = options.iter_mut().find(|option| option.id == config_id) {
                    match (&mut option.kind, value) {
                        (
                            acp::SessionConfigKind::Select(select),
                            acp::SessionConfigOptionValue::ValueId { value },
                        ) => {
                            select.current_value = value;
                        }
                        (
                            acp::SessionConfigKind::Boolean(boolean),
                            acp::SessionConfigOptionValue::Boolean { value },
                        ) => {
                            boolean.current_value = value;
                        }
                        _ => {}
                    }
                }
                options.clone()
            };

            Task::ready(Ok(options))
        }
    }
}
