use crate::{CommonAnimationExt, DiffStat, GradientFade, HighlightedLabel, Tooltip, prelude::*};

use gpui::{
    Animation, AnimationExt, ClickEvent, FontWeight, Hsla, MouseButton, SharedString,
    WindowBackgroundAppearance, pulsating_between,
};
use itertools::Itertools as _;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentThreadStatus {
    #[default]
    Completed,
    Running,
    WaitingForConfirmation,
    Error,
}

/// The one "agent running" glyph: sidebar rows, thread tabs, and the thread
/// view's generating indicator all render this same rotating accent spinner.
pub fn agent_running_indicator() -> AnyElement {
    Icon::new(IconName::LoadCircle)
        .size(IconSize::Small)
        .color(Color::Accent)
        .with_rotate_animation(2)
        .into_any_element()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeKind {
    #[default]
    Main,
    Linked,
}

#[derive(Clone, Default)]
pub struct ThreadItemWorktreeInfo {
    pub worktree_name: Option<SharedString>,
    pub branch_name: Option<SharedString>,
    pub full_path: SharedString,
    pub highlight_positions: Vec<usize>,
    pub kind: WorktreeKind,
}

/// A prominent, clickable pull-request badge rendered in the row's metadata
/// line: a pill with the PR number, a state icon/color (open, draft, merged,
/// closed) and an optional CI/checks glyph (passing, failing, pending).
/// Clicking it opens `url` when one is set; badges without a URL (e.g. the
/// "no PR" indicator) are inert and rendered muted.
#[derive(Clone)]
pub struct ThreadItemPrChip {
    pub label: SharedString,
    pub state_icon: IconName,
    pub state_color: Color,
    pub checks: Option<(IconName, Color)>,
    pub url: Option<SharedString>,
    pub tooltip: SharedString,
    /// The badge's hover card. Without it the badge falls back to the plain
    /// `tooltip` text (the inert "no PR" pill has nothing to detail).
    pub detail: Option<PrChipDetail>,
}

/// What a PR badge shows on hover: the pull request's title and number, its
/// state, its checks, and its review state.
#[derive(Clone)]
pub struct PrChipDetail {
    pub title: SharedString,
    pub number: u64,
    pub state: SharedString,
    pub state_color: Color,
    pub checks: SharedString,
    pub checks_icon: Option<(IconName, Color)>,
    pub review: SharedString,
    /// The names of failing checks the card should list under the "checks
    /// failing" line, most useful ones first. Empty when the PR is passing or
    /// when the check data carried no names; capped by the producer so the
    /// card cannot grow without bound.
    pub failing_checks: Vec<SharedString>,
    /// How many failing checks the card is not listing. Zero when everything
    /// failing is listed; used to draw an "and N more" line below.
    pub extra_failing_checks: usize,
}

/// The renderable pill for a [`ThreadItemPrChip`], shared by the sidebar rows
/// and the agent input status bar so both read as the same badge.
#[derive(IntoElement)]
pub struct PrChip {
    id: ElementId,
    chip: ThreadItemPrChip,
    large: bool,
    surface: Option<Hsla>,
}

impl PrChip {
    pub fn new(id: impl Into<ElementId>, chip: ThreadItemPrChip) -> Self {
        Self {
            id: id.into(),
            chip,
            large: false,
            surface: None,
        }
    }

    /// A larger label for surfaces with more space than a sidebar row (the
    /// agent input status bar).
    pub fn large(mut self, large: bool) -> Self {
        self.large = large;
        self
    }

    /// The opaque colour the pill sits on. A fill is composited over it so the
    /// pill covers what it overlaps whatever alpha the theme's element colours
    /// carry. Defaults to the panel background, which is where every one of
    /// these is drawn.
    pub fn surface(mut self, surface: Hsla) -> Self {
        self.surface = Some(surface);
        self
    }
}

impl RenderOnce for PrChip {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let chip = self.chip;
        let clickable = chip.url.is_some();
        // Both fills are composited over the surface the pill sits on. A fill
        // that keeps any alpha of its own lets a long title read straight
        // through the pill, and `element_background` is a theme colour: the
        // default themes make it opaque, a user's theme need not.
        let surface = self.surface.unwrap_or_else(|| cx.theme().colors().panel_background);
        // A real PR badge is filled and bordered so it reads as a control; the
        // inert "no PR" pill keeps the same geometry (so a row does not change
        // shape when a PR lands) and a quieter fill. Quiet is a muted fill, not
        // no fill.
        let (label_color, label_weight, border_color, fill) = if clickable {
            (
                Color::Default,
                FontWeight::MEDIUM,
                cx.theme().colors().border,
                cx.theme().colors().element_background,
            )
        } else {
            (
                Color::Muted,
                FontWeight::NORMAL,
                cx.theme().colors().border.opacity(0.5),
                cx.theme().colors().element_background.opacity(0.5),
            )
        };
        let background = surface.blend(fill);
        let label_size = if self.large {
            LabelSize::Default
        } else {
            LabelSize::Small
        };

        h_flex()
            .id(self.id)
            .min_w_0()
            .flex_shrink_0()
            .h(rems_from_px(24_f32))
            .px_1p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(border_color)
            .bg(background)
            .when(clickable, |this| {
                this.hover(|s| s.bg(cx.theme().colors().element_hover))
            })
            .child(
                Icon::new(chip.state_icon)
                    .size(IconSize::Small)
                    .color(chip.state_color),
            )
            .child(
                Label::new(chip.label)
                    .size(label_size)
                    .weight(label_weight)
                    .color(label_color),
            )
            .when_some(chip.checks, |this, (icon, color)| {
                this.child(Icon::new(icon).size(IconSize::Small).color(color))
            })
            .map(|this| match chip.detail {
                Some(detail) => this.tooltip(Tooltip::element(move |_, _| {
                    let detail = detail.clone();
                    v_flex()
                        .gap_1()
                        .w_96()
                        .child(
                            // The title wraps within the card; the number stays
                            // pinned beside it rather than pushing it wider.
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_1()
                                .items_start()
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .child(Label::new(detail.title)),
                                )
                                .child(
                                    Label::new(format!("#{}", detail.number))
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    h_flex()
                                        .gap_0p5()
                                        .child(
                                            Icon::new(IconName::PullRequest)
                                                .size(IconSize::XSmall)
                                                .color(detail.state_color),
                                        )
                                        .child(
                                            Label::new(detail.state)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .when(!detail.checks.is_empty(), |this| {
                                    this.child(
                                        h_flex()
                                            .gap_0p5()
                                            .when_some(detail.checks_icon, |this, (icon, color)| {
                                                this.child(
                                                    Icon::new(icon)
                                                        .size(IconSize::XSmall)
                                                        .color(color),
                                                )
                                            })
                                            .child(
                                                Label::new(detail.checks)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                })
                                .child(
                                    Label::new(detail.review)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        // Names the failing checks, so the card answers the
                        // question a reader would open a browser to answer.
                        .when(!detail.failing_checks.is_empty(), |this| {
                            let extra = detail.extra_failing_checks;
                            this.child(
                                v_flex()
                                    .gap_0p5()
                                    .children(detail.failing_checks.into_iter().map(|name| {
                                        Label::new(name)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate()
                                    }))
                                    .when(extra > 0, |this| {
                                        this.child(
                                            Label::new(format!("and {extra} more"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                        })
                        .into_any_element()
                })),
                None => this.tooltip(Tooltip::text(chip.tooltip)),
            })
            .when_some(chip.url, |this, url| {
                this.cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.open_url(&url);
                    })
            })
    }
}

#[derive(IntoElement, RegisterComponent)]
pub struct ThreadItem {
    id: ElementId,
    icon: IconName,
    icon_char: Option<SharedString>,
    icon_color: Option<Color>,
    icon_visible: bool,
    custom_icon_from_external_svg: Option<SharedString>,
    title: SharedString,
    title_slot: Option<AnyElement>,
    title_label_color: Option<Color>,
    title_generating: bool,
    highlight_positions: Vec<usize>,
    timestamp: SharedString,
    /// The disk this row's worktree occupies, when it is worth saying. Empty
    /// for a row that is not a worktree of its own, or whose worktree is small.
    size: SharedString,
    notified: bool,
    status: AgentThreadStatus,
    selected: bool,
    focused: bool,
    hovered: bool,
    rounded: bool,
    is_truncated: bool,
    added: Option<usize>,
    removed: Option<usize>,
    project_paths: Option<Arc<[PathBuf]>>,
    project_name: Option<SharedString>,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    pr_chips: Vec<ThreadItemPrChip>,
    is_remote: bool,
    archived: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
    action_slot: Option<AnyElement>,
    base_bg: Option<Hsla>,
}

impl ThreadItem {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon: IconName::ZedAgent,
            icon_char: None,
            icon_color: None,
            icon_visible: true,
            custom_icon_from_external_svg: None,
            title: title.into(),
            title_slot: None,
            title_label_color: None,
            title_generating: false,
            highlight_positions: Vec::new(),
            timestamp: "".into(),
            size: "".into(),
            notified: false,
            status: AgentThreadStatus::default(),
            selected: false,
            focused: false,
            hovered: false,
            rounded: false,
            is_truncated: true,
            added: None,
            removed: None,
            project_paths: None,
            project_name: None,
            worktrees: Vec::new(),
            pr_chips: Vec::new(),
            is_remote: false,
            archived: false,
            on_click: None,
            on_hover: Box::new(|_, _, _| {}),
            action_slot: None,
            base_bg: None,
        }
    }

    pub fn size(mut self, size: impl Into<SharedString>) -> Self {
        self.size = size.into();
        self
    }

    pub fn timestamp(mut self, timestamp: impl Into<SharedString>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = icon;
        self
    }

    /// Renders the given character in place of the icon. Takes precedence over
    /// [`Self::icon`] and [`Self::custom_icon_from_external_svg`].
    pub fn icon_char(mut self, icon_char: impl Into<SharedString>) -> Self {
        self.icon_char = Some(icon_char.into());
        self
    }

    pub fn icon_color(mut self, color: Color) -> Self {
        self.icon_color = Some(color);
        self
    }

    pub fn icon_visible(mut self, visible: bool) -> Self {
        self.icon_visible = visible;
        self
    }

    pub fn custom_icon_from_external_svg(mut self, svg: impl Into<SharedString>) -> Self {
        self.custom_icon_from_external_svg = Some(svg.into());
        self
    }

    pub fn notified(mut self, notified: bool) -> Self {
        self.notified = notified;
        self
    }

    pub fn status(mut self, status: AgentThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn title_generating(mut self, generating: bool) -> Self {
        self.title_generating = generating;
        self
    }

    pub fn title_label_color(mut self, color: Color) -> Self {
        self.title_label_color = Some(color);
        self
    }

    pub fn title_slot(mut self, element: impl IntoElement) -> Self {
        self.title_slot = Some(element.into_any_element());
        self
    }

    pub fn highlight_positions(mut self, positions: Vec<usize>) -> Self {
        self.highlight_positions = positions;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn added(mut self, added: usize) -> Self {
        self.added = Some(added);
        self
    }

    pub fn removed(mut self, removed: usize) -> Self {
        self.removed = Some(removed);
        self
    }

    pub fn project_paths(mut self, paths: Arc<[PathBuf]>) -> Self {
        self.project_paths = Some(paths);
        self
    }

    pub fn project_name(mut self, name: impl Into<SharedString>) -> Self {
        self.project_name = Some(name.into());
        self
    }

    pub fn worktrees(mut self, worktrees: Vec<ThreadItemWorktreeInfo>) -> Self {
        self.worktrees = worktrees;
        self
    }

    pub fn pr_chips(mut self, pr_chips: Vec<ThreadItemPrChip>) -> Self {
        self.pr_chips = pr_chips;
        self
    }

    pub fn is_remote(mut self, is_remote: bool) -> Self {
        self.is_remote = is_remote;
        self
    }

    pub fn archived(mut self, archived: bool) -> Self {
        self.archived = archived;
        self
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn rounded(mut self, rounded: bool) -> Self {
        self.rounded = rounded;
        self
    }

    pub fn is_truncated(mut self, is_truncated: bool) -> Self {
        self.is_truncated = is_truncated;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Box::new(on_hover);
        self
    }

    pub fn action_slot(mut self, element: impl IntoElement) -> Self {
        self.action_slot = Some(element.into_any_element());
        self
    }

    pub fn base_bg(mut self, color: Hsla) -> Self {
        self.base_bg = Some(color);
        self
    }
}

impl RenderOnce for ThreadItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = cx.theme().colors();
        // The fade gradient paints a solid color over the title to blend it into
        // the row background, but a transparent window has no opaque surface to
        // fade into, so it renders as a visible patch; truncate the title instead.
        let opaque_window =
            cx.theme().window_background_appearance() == WindowBackgroundAppearance::Opaque;
        let sidebar_base_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let raw_bg = self.base_bg.unwrap_or(sidebar_base_bg);
        let apparent_bg = color.background.blend(raw_bg);

        let base_bg = if self.selected {
            apparent_bg.blend(color.element_active)
        } else {
            apparent_bg
        };

        let hover_color = color
            .element_active
            .blend(color.element_background.opacity(0.2));
        let hover_bg = apparent_bg.blend(hover_color);

        let gradient_overlay = GradientFade::new(base_bg, hover_bg, hover_bg)
            .width(px(64.0))
            .right(px(-10.0))
            .gradient_stop(0.7)
            .group_name("thread-item");

        let separator_color = Color::Custom(color.text_muted.opacity(0.4));
        let dot_separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(separator_color)
        };

        let icon_id = format!("icon-{}", self.id);
        let icon_visible = self.icon_visible;
        let icon_container = || {
            h_flex()
                .id(icon_id.clone())
                .size_4()
                .flex_none()
                .justify_center()
                .when(!icon_visible, |this| this.invisible())
        };
        let icon_color = self.icon_color.unwrap_or(Color::Muted);
        // An archived thread wears the archive glyph in place of its agent
        // logo: the row is otherwise identical to a live one, and this is the
        // only thing that marks the state.
        let agent_icon = if self.archived {
            Icon::new(IconName::Archive)
                .color(icon_color)
                .size(IconSize::Small)
                .into_any_element()
        } else if let Some(icon_char) = self.icon_char {
            Label::new(icon_char)
                .size(LabelSize::Small)
                .color(icon_color)
                .into_any_element()
        } else if let Some(custom_svg) = self.custom_icon_from_external_svg {
            Icon::from_external_svg(custom_svg)
                .color(icon_color)
                .size(IconSize::Small)
                .into_any_element()
        } else {
            Icon::new(self.icon)
                .color(icon_color)
                .size(IconSize::Small)
                .into_any_element()
        };

        // Read-state glyph: nothing when read, a dot when updated since last
        // viewed, a stronger amber dot when the thread needs action
        // (confirmation, elicitation, or error; the tooltip disambiguates).
        let status_icon = if matches!(
            self.status,
            AgentThreadStatus::Error | AgentThreadStatus::WaitingForConfirmation
        ) {
            Some(
                Icon::new(IconName::Circle)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
        } else if self.notified {
            Some(
                Icon::new(IconName::Circle)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
        } else {
            None
        };

        // The agent glyph keeps the leading slot. Which model is working is
        // worth seeing while it works, and the status used to sit on top of it.
        let icon = icon_container().child(agent_icon).into_any_element();

        // ...so the status goes to the right end of the title row instead. The
        // slot is always drawn, empty or not, so the row does not change shape
        // when a thread starts or stops.
        let status_indicator = if self.status == AgentThreadStatus::Running {
            Some(agent_running_indicator().into_any_element())
        } else {
            status_icon.map(|icon| icon.into_any_element())
        };
        let status_slot = h_flex()
            .id(SharedString::from(format!("status-{}", self.id)))
            .size_4()
            .flex_none()
            .justify_center()
            .children(status_indicator);

        let title = self.title;
        let highlight_positions = self.highlight_positions;
        let title_label_color = self.title_label_color;

        let title_label = if let Some(title_slot) = self.title_slot {
            title_slot
        } else if self.title_generating {
            Label::new(title)
                .color(Color::Muted)
                .with_animation(
                    "generating-title",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else if highlight_positions.is_empty() {
            Label::new(title)
                .when_some(title_label_color, |label, color| label.color(color))
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        } else {
            HighlightedLabel::new(title, highlight_positions)
                .when_some(title_label_color, |label, color| label.color(color))
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        };

        let has_diff_stats = self.added.is_some() || self.removed.is_some();
        let diff_stat_id = self.id.clone();
        let added_count = self.added.unwrap_or(0);
        let removed_count = self.removed.unwrap_or(0);

        let project_paths = self.project_paths.as_ref().and_then(|paths| {
            let paths_str = paths
                .as_ref()
                .iter()
                .filter_map(|p| p.file_name())
                .filter_map(|name| name.to_str())
                .join(", ");
            if paths_str.is_empty() {
                None
            } else {
                Some(paths_str)
            }
        });

        let has_project_name = self.project_name.is_some();
        let has_project_paths = project_paths.is_some();
        let has_timestamp = !self.timestamp.is_empty();
        let timestamp = self.timestamp;
        let has_size = !self.size.is_empty();
        let size = self.size;

        let show_tooltip = matches!(
            self.status,
            AgentThreadStatus::Error | AgentThreadStatus::WaitingForConfirmation
        );

        let linked_worktrees: Vec<ThreadItemWorktreeInfo> = self
            .worktrees
            .into_iter()
            .filter(|wt| wt.kind == WorktreeKind::Linked)
            .filter(|wt| wt.worktree_name.is_some())
            .collect();

        let has_worktree = !linked_worktrees.is_empty();

        let pr_chips = self.pr_chips;
        let has_pr_chips = !pr_chips.is_empty();

        let has_metadata = has_project_name
            || has_project_paths
            || has_worktree
            || has_pr_chips
            || has_diff_stats
            || has_size
            || has_timestamp;

        v_flex()
            .id(self.id.clone())
            .cursor_pointer()
            .group("thread-item")
            .relative()
            .flex_shrink_0()
            .overflow_hidden()
            .w_full()
            .py_1()
            .px_1p5()
            .when(self.selected, |s| s.bg(color.element_active))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.focused, |s| s.border_color(color.border_focused))
            .when(self.rounded, |s| s.rounded_sm())
            .hover(|s| s.bg(hover_color))
            .on_hover(self.on_hover)
            .child(
                h_flex()
                    .min_w_0()
                    .w_full()
                    .h_6()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("content")
                            .min_w_0()
                            .flex_1()
                            .gap_1p5()
                            .child(icon)
                            .child(title_label),
                    )
                    .when(self.is_truncated && opaque_window, |this| {
                        this.child(gradient_overlay)
                    })
                    .child(status_slot)
                    // The slot holds the row's buttons, hover-gated by whoever
                    // fills it. The PR chips are not in here: the title row
                    // cannot hold several of anything, and a row can carry
                    // several chips, so they get the metadata line below where
                    // there is always room. The fade keeps a long title
                    // dissolving under the buttons instead of colliding.
                    .when_some(self.action_slot, |this, slot| {
                        this.child(
                            h_flex()
                                .relative()
                                .pr_1p5()
                                .when(opaque_window, |this| {
                                    this.child(
                                        GradientFade::new(base_bg, hover_bg, hover_bg)
                                            .width(px(120.0))
                                            .right(px(8.))
                                            .gradient_stop(0.90)
                                            .group_name("thread-item"),
                                    )
                                })
                                .child(slot)
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                }),
                        )
                    }),
            )
            .when(has_metadata, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .child(icon_container()) // Icon Spacing
                        .when(
                            has_project_name || has_project_paths || has_worktree,
                            |this| {
                                this.when_some(self.project_name, |this, name| {
                                    this.child(
                                        Label::new(name).size(LabelSize::Small).color(Color::Muted),
                                    )
                                })
                                .when(
                                    has_project_name && (has_project_paths || has_worktree),
                                    |this| this.child(dot_separator()),
                                )
                                .when_some(project_paths, |this, paths| {
                                    this.child(
                                        Label::new(paths)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .when(has_project_paths && has_worktree, |this| {
                                    this.child(dot_separator())
                                })
                                .children(
                                    linked_worktrees.into_iter().map(|wt| {
                                        let worktree_label = wt.worktree_name.map(|name| {
                                            if wt.highlight_positions.is_empty() {
                                                Label::new(name)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate()
                                                    .into_any_element()
                                            } else {
                                                HighlightedLabel::new(
                                                    name,
                                                    wt.highlight_positions.clone(),
                                                )
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate()
                                                .into_any_element()
                                            }
                                        });

                                        h_flex()
                                            .min_w_0()
                                            .gap_0p5()
                                            .child(
                                                Icon::new(IconName::GitWorktree)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .when_some(worktree_label, |this, label| {
                                                this.child(label)
                                            })
                                    }),
                                )
                            },
                        )
                        .when(has_pr_chips, |this| {
                            this.when(
                                has_project_name || has_project_paths || has_worktree,
                                |this| this.child(dot_separator()),
                            )
                            .children(pr_chips.into_iter().enumerate().map(|(chip_ix, chip)| {
                                PrChip::new(("pr-chip", chip_ix), chip).surface(base_bg)
                            }))
                        })
                        .when(
                            (has_project_name || has_project_paths || has_worktree || has_pr_chips)
                                && (has_diff_stats || has_size || has_timestamp),
                            |this| this.child(dot_separator()),
                        )
                        .when(has_diff_stats, |this| {
                            this.child(DiffStat::new(diff_stat_id, added_count, removed_count))
                        })
                        .when(has_diff_stats && (has_size || has_timestamp), |this| {
                            this.child(dot_separator())
                        })
                        // What the worktree costs to keep, next to how old it
                        // is: a row that stands in for its own worktree has no
                        // header to carry this.
                        .when(has_size, |this| {
                            this.child(
                                Label::new(size.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .when(has_size && has_timestamp, |this| {
                            this.child(dot_separator())
                        })
                        .when(has_timestamp, |this| {
                            this.child(
                                Label::new(timestamp.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
            })
            .when(show_tooltip, |this| {
                let status = self.status;
                this.tooltip(Tooltip::element(move |_, _| match status {
                    AgentThreadStatus::Error => h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Close)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(Label::new("Agent Hit an Error"))
                        .into_any_element(),
                    AgentThreadStatus::WaitingForConfirmation => h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(Label::new("Waiting for Confirmation"))
                        .into_any_element(),
                    _ => gpui::Empty.into_any_element(),
                }))
            })
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
    }
}

impl Component for ThreadItem {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn description() -> &'static str {
        "A row representing an agent thread in a list, showing its title, status, \
        timestamp, and contextual metadata such as worktree and pull request information."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let container = || {
            v_flex()
                .w_72()
                .border_1()
                .border_color(color.border_variant)
                .bg(bg)
        };

        let thread_item_examples = vec![
            single_example(
                "Default",
                container()
                    .child(
                        ThreadItem::new("ti-1", "Linking to the Agent Panel Depending on Settings")
                            .icon(IconName::AiOpenAi)
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Waiting for Confirmation",
                container()
                    .child(
                        ThreadItem::new("ti-2b", "Execute shell command in terminal")
                            .timestamp("2h")
                            .status(AgentThreadStatus::WaitingForConfirmation),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Error",
                container()
                    .child(
                        ThreadItem::new("ti-2c", "Failed to connect to language server")
                            .timestamp("5h")
                            .status(AgentThreadStatus::Error),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Running Agent",
                container()
                    .child(
                        ThreadItem::new("ti-3", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("23h")
                            .status(AgentThreadStatus::Running),
                    )
                    .into_any_element(),
            ),
            single_example(
                "In Worktree",
                container()
                    .child(
                        ThreadItem::new("ti-4", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("2w")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("link-agent-panel".into()),
                                full_path: "link-agent-panel".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }]),
                    )
                    .into_any_element(),
            ),
            single_example(
                "With Changes",
                container()
                    .child(
                        ThreadItem::new("ti-5", "Managing user and project settings interactions")
                            .icon(IconName::AiClaude)
                            .timestamp("1mo")
                            .added(10)
                            .removed(3),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5b", "Full metadata example")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "my-project".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }])
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Branch + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5c", "Full metadata with branch")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature-branch".into()),
                            }])
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Branch + Changes (truncation)",
                container()
                    .child(
                        ThreadItem::new("ti-5d", "Metadata overflow with long branch name")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("fix-very-long-branch-name-here".into()),
                            }])
                            .added(108)
                            .removed(53)
                            .timestamp("2d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Main Worktree (hidden) + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5e", "Main worktree branch with diff stats")
                            .icon(IconName::ZedAgent)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("zed".into()),
                                full_path: "/projects/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Main,
                                branch_name: Some("sidebar-show-branch-name".into()),
                            }])
                            .added(23)
                            .removed(8)
                            .timestamp("5m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Worktree Name (truncation)",
                container()
                    .child(
                        ThreadItem::new("ti-5f", "Thread with a very long worktree name")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some(
                                    "very-long-worktree-name-that-should-truncate".into(),
                                ),
                                full_path: "/worktrees/very-long-worktree-name/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }])
                            .timestamp("1h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree with Search Highlights",
                container()
                    .child(
                        ThreadItem::new("ti-5g", "Filtered thread with highlighted worktree")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: vec![0, 1, 2, 3],
                                kind: WorktreeKind::Linked,
                                branch_name: Some("fix-scrolling".into()),
                            }])
                            .timestamp("3d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Multiple Worktrees (no branches)",
                container()
                    .child(
                        ThreadItem::new("ti-5h", "Thread spanning multiple worktrees")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("jade-glen".into()),
                                    full_path: "/worktrees/jade-glen/zed".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: None,
                                },
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("fawn-otter".into()),
                                    full_path: "/worktrees/fawn-otter/zed-slides".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: None,
                                },
                            ])
                            .timestamp("2h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Multiple Worktrees with Branches",
                container()
                    .child(
                        ThreadItem::new("ti-5i", "Multi-root with per-worktree branches")
                            .icon(IconName::ZedAgent)
                            .worktrees(vec![
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("jade-glen".into()),
                                    full_path: "/worktrees/jade-glen/zed".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: Some("fix".into()),
                                },
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("fawn-otter".into()),
                                    full_path: "/worktrees/fawn-otter/zed-slides".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: Some("main".into()),
                                },
                            ])
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Name + Worktree + Branch",
                container()
                    .child(
                        ThreadItem::new("ti-5j", "Thread with project context")
                            .icon(IconName::AiClaude)
                            .project_name("my-remote-server")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature-branch".into()),
                            }])
                            .timestamp("1d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Paths + Worktree (archive view)",
                container()
                    .child(
                        ThreadItem::new("ti-5k", "Archived thread with folder paths")
                            .icon(IconName::AiClaude)
                            .project_paths(Arc::from(vec![
                                PathBuf::from("/projects/zed"),
                                PathBuf::from("/projects/zed-slides"),
                            ]))
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature".into()),
                            }])
                            .timestamp("2mo"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "All Metadata",
                container()
                    .child(
                        ThreadItem::new("ti-5l", "Thread with every metadata field populated")
                            .icon(IconName::ZedAgent)
                            .project_name("remote-dev")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-worktree".into()),
                                full_path: "/worktrees/my-worktree/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("main".into()),
                            }])
                            .added(15)
                            .removed(4)
                            .timestamp("8h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "PR Chips",
                container()
                    .child(
                        ThreadItem::new("ti-5m", "Thread with pull request status")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("fix-scrolling".into()),
                            }])
                            .pr_chips(vec![
                                ThreadItemPrChip {
                                    label: "#10461".into(),
                                    state_icon: IconName::PullRequest,
                                    state_color: Color::Success,
                                    checks: Some((IconName::Check, Color::Success)),
                                    url: Some("https://example.com/pull/10461".into()),
                                    tooltip: "Fix things (checks passing)".into(),
                                    detail: Some(PrChipDetail {
                                        title: "Fix things".into(),
                                        number: 10461,
                                        state: "open".into(),
                                        state_color: Color::Success,
                                        checks: "checks passing".into(),
                                        checks_icon: Some((IconName::Check, Color::Success)),
                                        review: "approved".into(),
                                        failing_checks: Vec::new(),
                                        extra_failing_checks: 0,
                                    }),
                                },
                                ThreadItemPrChip {
                                    label: "#10502".into(),
                                    state_icon: IconName::PullRequest,
                                    state_color: Color::Muted,
                                    checks: Some((IconName::ArrowCircle, Color::Warning)),
                                    url: Some("https://example.com/pull/10502".into()),
                                    tooltip: "Follow-up (checks pending)".into(),
                                    detail: Some(PrChipDetail {
                                        title: "Follow-up".into(),
                                        number: 10502,
                                        state: "draft".into(),
                                        state_color: Color::Muted,
                                        checks: "checks pending".into(),
                                        checks_icon: Some((IconName::ArrowCircle, Color::Warning)),
                                        review: "review required".into(),
                                        failing_checks: Vec::new(),
                                        extra_failing_checks: 0,
                                    }),
                                },
                            ])
                            .timestamp("2h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Focused Item (Keyboard Selection)",
                container()
                    .child(
                        ThreadItem::new("ti-7", "Implement keyboard navigation")
                            .icon(IconName::AiClaude)
                            .timestamp("12h")
                            .focused(true),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Action Slot",
                container()
                    .child(
                        ThreadItem::new("ti-9", "Hover to see action button")
                            .icon(IconName::AiClaude)
                            .timestamp("6h")
                            .hovered(true)
                            .action_slot(
                                IconButton::new("delete", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted),
                            ),
                    )
                    .into_any_element(),
            ),
        ];

        example_group(thread_item_examples)
            .vertical()
            .into_any_element()
    }
}
