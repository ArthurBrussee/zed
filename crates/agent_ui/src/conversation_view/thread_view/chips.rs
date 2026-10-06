//! The thread view's action chips, kept out of `thread_view.rs` so that file
//! stays close to upstream's. What a command did is decided in
//! `acp_thread::command_parse`.

use std::sync::Arc;

use editor::HiddenUnstagedDiffHunkRenderer;
use project::project_settings::DiagnosticSeverity;

use super::*;
use crate::entry_view_state::diff_editor_text_style_refinement;

/// Shared by the command card's two stacked halves so they line up.
const CARD_WIDTH: Rems = Rems(30.);

const DIFF_CARD_WIDTH: Rems = Rems(48.);
const DIFF_CARD_HEIGHT: Rems = Rems(30.);
/// `DIFF_CARD_WIDTH` plus the card's padding; must move with it.
const DIFF_CARD_MAX_W: Rems = Rems(56.);

const OUTPUT_TAIL_LINES: usize = 200;

fn copy_chip_image(image: ChipImage, cx: &mut App) {
    match image {
        ChipImage::Data { image, .. } => cx.write_to_clipboard(ClipboardItem::new_image(&image)),
        ChipImage::File(path) => {
            let Some(format) = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(image_format_from_extension)
            else {
                return;
            };
            cx.spawn(async move |cx| {
                let bytes = cx
                    .background_spawn(async move { std::fs::read(&path) })
                    .await
                    .log_err()?;
                cx.update(|cx| {
                    cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
                        format, bytes,
                    )))
                });
                Some(())
            })
            .detach();
        }
    }
}

pub(super) async fn image_shape_of_file(
    fs: &Arc<dyn fs::Fs>,
    path: &std::path::Path,
) -> ImageShape {
    let Some(format) = path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(image_format_from_extension)
    else {
        return ImageShape::Unknown;
    };
    fs.load_bytes(path)
        .await
        .ok()
        .and_then(|bytes| acp_thread::ContentBlock::image_dimensions(&bytes, format))
        .map_or(ImageShape::Unknown, ImageShape::Known)
}

fn image_format_from_extension(extension: &str) -> Option<gpui::ImageFormat> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        "webp" => Some(gpui::ImageFormat::Webp),
        "gif" => Some(gpui::ImageFormat::Gif),
        "bmp" => Some(gpui::ImageFormat::Bmp),
        "tif" | "tiff" => Some(gpui::ImageFormat::Tiff),
        _ => None,
    }
}

fn diff_stats(added: u32, deleted: u32) -> Option<action_log::DiffStats> {
    (added > 0 || deleted > 0).then_some(action_log::DiffStats {
        lines_added: added,
        lines_removed: deleted,
    })
}

fn command_file_diff_editor(
    multibuffer: Entity<MultiBuffer>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::new(
            editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::SizeByContent,
            },
            multibuffer,
            None,
            window,
            cx,
        );
        editor.set_show_gutter(false, cx);
        editor.disable_diagnostics(cx);
        editor.set_max_diagnostics_severity(DiagnosticSeverity::Off, cx);
        editor.disable_expand_excerpt_buttons(cx);
        editor.set_show_vertical_scrollbar(false, cx);
        editor.set_minimap_visibility(editor::MinimapVisibility::Disabled, window, cx);
        editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
        editor.set_forbid_vertical_scroll(true);
        editor.set_show_indent_guides(false, cx);
        editor.set_read_only(true);
        editor.set_delegate_open_excerpts(true);
        editor.set_show_bookmarks(false, cx);
        editor.set_show_breakpoints(false, cx);
        editor.set_show_code_actions(false, cx);
        editor.set_show_git_diff_gutter(false, cx);
        editor.set_expand_all_diff_hunks(cx);
        editor.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
        editor.set_text_style_refinement(diff_editor_text_style_refinement(cx));
        editor
    })
}

pub(super) struct ChipHoverCard {
    pub(super) build: std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>,
}

impl Render for ChipHoverCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme::theme_settings(cx).ui_font(cx).clone();
        // Padding, not a gap: a gap would dismiss the hoverable card as the
        // pointer crosses it.
        div().pl_2().pt_2p5().child(
            v_flex()
                .font(ui_font)
                .text_ui(cx)
                .text_color(cx.theme().colors().text)
                .elevation_2(cx)
                .p_2p5()
                .child((self.build)(window, cx)),
        )
    }
}

/// One phrasing, `Searched "query"`, however the agent worded its search.
pub(super) fn search_chip_label(source: &str) -> Option<SharedString> {
    const VERBS: &[&str] = &[
        "searched for ",
        "searched ",
        "search for ",
        "searching for ",
        "searching ",
        "search ",
        "grepping for ",
        "grep for ",
        "grep ",
        "rg ",
    ];

    let text = source.trim().trim_matches('`').lines().next()?.trim();
    let lowercase = text.to_lowercase();
    let query = VERBS
        .iter()
        .find_map(|verb| lowercase.strip_prefix(verb))
        // Matched on the lowercase copy, so cut the original by length.
        .map(|rest| &text[text.len() - rest.len()..])
        .unwrap_or(text)
        .trim()
        .trim_matches('"');
    (!query.is_empty()).then(|| format!("Searched {query:?}").into())
}

/// Tooltips lay out at min-content and a scroller's minimum size is zero, so
/// the region needs its own width or it collapses. `occlude` keeps the wheel
/// off the transcript underneath.
pub(super) fn card_scroll_region(
    id: &'static str,
    width: gpui::Rems,
    max_height: gpui::Rems,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(width)
        .max_h(max_height)
        .overflow_y_scroll()
        .occlude()
}

pub(super) fn chip_hover_card(
    build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let build: std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyElement> = std::rc::Rc::new(build);
    move |_window, cx| {
        let build = build.clone();
        cx.new(|_| ChipHoverCard { build }).into()
    }
}

/// For a body that loads asynchronously: a plain card never sees the observed
/// entity's notify, so it would stay empty until hovered again.
pub(super) fn chip_hover_card_observing<T: 'static>(
    observed: WeakEntity<T>,
    build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let build: std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyElement> = std::rc::Rc::new(build);
    move |_window, cx| {
        let build = build.clone();
        let observed = observed.clone();
        cx.new(|cx| {
            if let Some(entity) = observed.upgrade() {
                cx.observe(&entity, |_, _, cx| cx.notify()).detach();
            }
            ChipHoverCard { build }
        })
        .into()
    }
}

/// Command ranges are kept so each is highlighted on its own: a label joined
/// from several clipped commands is not one shell line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CommandChipLabel {
    pub(super) text: String,
    pub(super) commands: Vec<Range<usize>>,
}

impl CommandChipLabel {
    /// For text-only joins; the chip draws a rule, since a bar reads as a pipe.
    pub(super) const SEPARATOR: &'static str = " · ";

    pub(super) fn prose(text: String) -> Self {
        Self {
            text,
            commands: Vec::new(),
        }
    }

    pub(super) fn command(text: String) -> Self {
        Self {
            commands: vec![0..text.len()],
            text,
        }
    }

    pub(super) fn runs(
        &self,
        language: Option<&Arc<Language>>,
        text_style: TextStyle,
        markdown_style: &MarkdownStyle,
    ) -> Vec<TextRun> {
        let mut runs = Vec::new();
        let mut offset = 0;
        for range in &self.commands {
            if range.start > offset {
                runs.push(text_style.to_run(range.start - offset));
            }
            runs.extend(highlight_code_runs(
                &self.text[range.clone()],
                language,
                text_style.clone(),
                markdown_style,
            ));
            offset = range.end;
        }
        if offset < self.text.len() {
            runs.push(text_style.to_run(self.text.len() - offset));
        }
        runs
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ChipGlyph {
    Icon(IconName),
    /// An icon-theme file type from [`acp_thread::program_language`].
    Language(&'static str),
}

impl ChipGlyph {
    pub(super) fn for_segment(segment: &acp_thread::CommandSegment) -> Self {
        use acp_thread::{GitOperation, SegmentKind};

        if let Some(language) = segment.language() {
            return Self::Language(language);
        }
        Self::Icon(match &segment.kind {
            SegmentKind::Read { .. } => IconName::FileCode,
            SegmentKind::Search { .. } | SegmentKind::Lookup { .. } => IconName::MagnifyingGlass,
            SegmentKind::ListDirectory { .. } => IconName::Folder,
            SegmentKind::CountLines { .. } => IconName::FileCode,
            SegmentKind::Git { operation, .. } => match operation {
                GitOperation::ReadChanges => IconName::Diff,
                GitOperation::Inspect | GitOperation::Modify => IconName::GitBranch,
            },
            SegmentKind::GitHub { .. } => IconName::GitBranch,
            SegmentKind::WriteFile { .. } | SegmentKind::EditInPlace { .. } => IconName::Pencil,
            SegmentKind::Destructive { .. } => IconName::Trash,
            SegmentKind::Noop
            | SegmentKind::Wait { .. }
            | SegmentKind::InlineScript { .. }
            | SegmentKind::Run { .. } => IconName::ToolTerminal,
        })
    }

    pub(super) fn element(&self, color: Color, cx: &App) -> AnyElement {
        let path = match self {
            Self::Icon(icon) => {
                return Icon::new(*icon)
                    .size(IconSize::Small)
                    .color(color)
                    .into_any_element();
            }
            Self::Language(language) => FileIcons::get(cx).get_icon_for_type(language, cx),
        };
        match path {
            // A language logo keeps its own colors.
            Some(path) => Icon::from_path(path)
                .size(IconSize::Small)
                .into_any_element(),
            None => Icon::new(IconName::ToolTerminal)
                .size(IconSize::Small)
                .color(color)
                .into_any_element(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandChipPiece {
    pub(super) glyph: ChipGlyph,
    pub(super) label: CommandChipLabel,
    /// Set only on a line where some acts ran outside the devshell.
    pub(super) in_environment: bool,
}

pub(super) struct CommandEnvironment {
    pub(super) name: String,
    pub(super) partial: bool,
}

pub(super) enum CollapsedCommand {
    Label(CommandChipLabel),
    Pieces(Vec<CommandChipPiece>),
}

fn command_class_icon(class: acp_thread::CommandClass) -> IconName {
    match class {
        acp_thread::CommandClass::Search => IconName::MagnifyingGlass,
        acp_thread::CommandClass::Read => IconName::FileCode,
        acp_thread::CommandClass::ReadDiff => IconName::Diff,
        acp_thread::CommandClass::GitInfo => IconName::GitBranch,
        acp_thread::CommandClass::GitHub => IconName::PullRequest,
        acp_thread::CommandClass::Inspect | acp_thread::CommandClass::Other => {
            IconName::ToolTerminal
        }
    }
}

impl ThreadView {
    pub(super) fn command_host_for(&self, tool_call: &ToolCall, cx: &App) -> Option<String> {
        if tool_call.terminals().next().is_none() {
            return None;
        }
        self.chip_cache.command(tool_call, cx).host.clone()
    }

    /// The devshell wrapper is stripped from the label, so this is what still
    /// says where the command ran.
    pub(super) fn command_environment_for(
        &self,
        tool_call: &ToolCall,
        cx: &App,
    ) -> Option<CommandEnvironment> {
        if tool_call.terminals().next().is_none() {
            return None;
        }
        let parsed = &self.chip_cache.command(tool_call, cx).parsed;
        Some(CommandEnvironment {
            name: parsed.environment.clone()?,
            partial: parsed.environment_is_partial(),
        })
    }

    pub(super) fn low_value_class(
        tool_call: &ToolCall,
        cache: Option<&ChipCache>,
        cx: &App,
    ) -> Option<acp_thread::CommandClass> {
        match tool_call.kind() {
            acp_v2::ToolKind::Read => return Some(acp_thread::CommandClass::Read),
            acp_v2::ToolKind::Search => return Some(acp_thread::CommandClass::Search),
            _ => {}
        }
        if tool_call.terminals().next().is_some() {
            let class = match cache {
                Some(cache) => cache.command(tool_call, cx).class,
                None => acp_thread::classify_command(&strip_command_fences(
                    &tool_call.label.read(cx).source(),
                )),
            };
            match class {
                acp_thread::CommandClass::Other => None,
                class => Some(class),
            }
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(super) fn action_chips_in(
        entries: &[AgentThreadEntry],
        run_start: usize,
        run_len: usize,
        cx: &App,
    ) -> Vec<ActionChip> {
        Self::action_chips_in_cached(entries, run_start, run_len, None, cx)
    }

    pub(super) fn action_chips_in_cached(
        entries: &[AgentThreadEntry],
        run_start: usize,
        run_len: usize,
        cache: Option<&ChipCache>,
        cx: &App,
    ) -> Vec<ActionChip> {
        let mut chips: Vec<ActionChip> = Vec::new();

        // Consecutive reads/searches fold into one summary chip.
        let mut pending_low_value: Vec<usize> = Vec::new();
        fn flush(chips: &mut Vec<ActionChip>, pending: &mut Vec<usize>) {
            match pending.len() {
                0 => {}
                1 => chips.push(ActionChip::ToolCall {
                    entry_ix: pending[0],
                }),
                _ => chips.push(ActionChip::Collapsed {
                    entry_ixs: std::mem::take(pending),
                }),
            }
            pending.clear();
        }

        for entry_ix in run_start..(run_start + run_len).min(entries.len()) {
            match &entries[entry_ix] {
                AgentThreadEntry::ToolCall(tool_call) => {
                    if tool_call.is_wait(cx)
                        || tool_call.is_empty_stdin_write(cx)
                        || tool_call.is_tool_lookup(cx)
                    {
                        // Hidden, but kept in the run so neighbours stay grouped.
                        continue;
                    }
                    // An image read keeps its own chip so the image can show.
                    if Self::tool_call_has_image(tool_call, cx) {
                        flush(&mut chips, &mut pending_low_value);
                        chips.push(ActionChip::ToolCall { entry_ix });
                        continue;
                    }
                    // `sed -n` folds away, `sed -i` does not.
                    if Self::low_value_class(tool_call, cache, cx).is_some()
                        && Self::command_changed_files(tool_call, cx).is_empty()
                    {
                        pending_low_value.push(entry_ix);
                        continue;
                    }
                    flush(&mut chips, &mut pending_low_value);

                    // One chip per edited file, named from the call's files
                    // rather than its generic label.
                    let files = Self::edited_files(tool_call, cx);
                    let is_edit = matches!(tool_call.kind(), acp_v2::ToolKind::Edit);
                    let failed = matches!(
                        tool_call.status(),
                        ToolCallStatus::Rejected
                            | ToolCallStatus::Canceled
                            | ToolCallStatus::Failed
                    );
                    if files.is_empty() {
                        if !is_edit || failed {
                            chips.push(ActionChip::ToolCall { entry_ix });
                        }
                    } else {
                        for file_ix in 0..files.len() {
                            chips.push(ActionChip::EditFile { entry_ix, file_ix });
                        }
                    }

                    let changed = Self::command_changed_files(tool_call, cx).len();
                    if changed > MOST_NAMED_COMMAND_FILES {
                        chips.push(ActionChip::CommandFiles { entry_ix });
                    } else {
                        for path_ix in 0..changed {
                            chips.push(ActionChip::CommandFile { entry_ix, path_ix });
                        }
                    }
                }
                _ => flush(&mut chips, &mut pending_low_value),
            }
        }
        flush(&mut chips, &mut pending_low_value);

        chips
    }

    pub(super) fn action_chip_expanded(&self, id: &ActionChipId, cx: &App) -> bool {
        if self.expanded_action_chip.as_ref() == Some(id) {
            return true;
        }
        match id {
            ActionChipId::ToolCall(tool_call_id) => self
                .entry_view_state
                .read(cx)
                .is_tool_call_expanded(tool_call_id),
            _ => false,
        }
    }

    /// An image chip starts expanded; every other chip expands on click.
    pub(super) fn tool_call_chip_expanded(
        &self,
        tool_call: &ToolCall,
        id: &ActionChipId,
        cx: &App,
    ) -> bool {
        if self.tool_call_image(tool_call, cx).is_some() {
            return !self.collapsed_image_chips.contains(id);
        }
        self.action_chip_expanded(id, cx)
    }

    pub(super) fn toggle_image_chip(&mut self, id: ActionChipId, cx: &mut Context<Self>) {
        if !self.collapsed_image_chips.remove(&id) {
            self.collapsed_image_chips.insert(id.clone());
        }
        self.remeasure_chip(&id, cx);
        cx.notify();
    }

    /// The list keeps measured heights, so an unreported expansion paints over
    /// whatever is below it.
    pub(super) fn remeasure_chip(&mut self, id: &ActionChipId, cx: &App) {
        let Some(entry_ix) = self.entry_ix_for_chip(id, cx) else {
            return;
        };
        let item = self.drawn_item_for_entry(entry_ix, cx);
        self.list_state.remeasure_items(item..item + 1);
    }

    /// A run of actions is drawn as one block by its first entry, so that is
    /// the item to remeasure when any entry in it grows.
    pub(crate) fn drawn_item_for_entry(&self, entry_ix: usize, cx: &App) -> usize {
        // The frame memos predate the change that prompted this call.
        self.chip_cache.frame_chip_entries.borrow_mut().clear();
        self.chip_cache.frame_runs.borrow_mut().clear();
        self.action_run_bounds(entry_ix, cx)
            .map_or(entry_ix, |(run_start, _)| run_start)
    }

    fn entry_ix_for_chip(&self, id: &ActionChipId, cx: &App) -> Option<usize> {
        let (ActionChipId::ToolCall(wanted) | ActionChipId::Collapsed(wanted)) = id;
        self.thread
            .read(cx)
            .entries()
            .iter()
            .position(|entry| match entry {
                AgentThreadEntry::ToolCall(tool_call) => &tool_call.id == wanted,
                _ => false,
            })
    }

    pub(super) fn toggle_action_chip(
        &mut self,
        id: ActionChipId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One expanded chip at a time.
        if let Some(previous) = self.expanded_action_chip.take() {
            let toggled_same = previous == id;
            self.collapse_action_chip(&previous, cx);
            self.remeasure_chip(&previous, cx);
            if toggled_same {
                cx.notify();
                return;
            }
        }

        match &id {
            ActionChipId::ToolCall(tool_call_id) => {
                self.entry_view_state.update(cx, |state, _cx| {
                    state.set_tool_call_expanded(tool_call_id, true)
                });
                let tool_call_id = tool_call_id.clone();
                self.prepare_command_scripts(&tool_call_id, cx);
                // Some body views are only built for an opened call.
                if let Some(entry_ix) = self.entry_ix_for_chip(&id, cx) {
                    self.sync_entry_views(entry_ix, window, cx);
                }
            }
            ActionChipId::Collapsed(_) => {}
        }
        self.remeasure_chip(&id, cx);
        self.expanded_action_chip = Some(id);
        cx.notify();
    }

    pub(super) fn collapse_action_chip(&mut self, id: &ActionChipId, cx: &mut Context<Self>) {
        match id {
            ActionChipId::ToolCall(tool_call_id) => {
                self.entry_view_state.update(cx, |state, _cx| {
                    state.set_tool_call_expanded(tool_call_id, false)
                });
            }
            ActionChipId::Collapsed(_) => {}
        }
    }

    pub(super) fn edit_file_stats(
        &self,
        tool_call: &ToolCall,
        file: &EditedFile,
        cx: &App,
    ) -> Option<action_log::DiffStats> {
        let diff = self.diff_for_edited_file(tool_call, file, cx)?;
        let (_buffer, buffer_diff) = diff.read(cx).buffer_and_diff(cx)?;
        let stats = action_log::DiffStats::single_file(buffer_diff.read(cx));
        (stats.lines_added > 0 || stats.lines_removed > 0).then_some(stats)
    }

    /// Matched by file name: the diff's path may be absolute where the
    /// location's is not.
    pub(super) fn diff_for_edited_file<'a>(
        &self,
        tool_call: &'a ToolCall,
        file: &EditedFile,
        cx: &App,
    ) -> Option<&'a Entity<acp_thread::Diff>> {
        let target = file.path.file_name()?;
        tool_call.diffs().find(|diff| {
            diff.read(cx)
                .file_path(cx)
                .as_deref()
                .map(std::path::Path::new)
                .and_then(|path| path.file_name())
                .is_some_and(|name| name == target)
        })
    }

    pub(super) fn chip_edit_stats(
        &self,
        tool_call: &ToolCall,
        cx: &App,
    ) -> Option<action_log::DiffStats> {
        let mut stats = action_log::DiffStats::default();
        for diff in tool_call.diffs() {
            if let Some((_buffer, buffer_diff)) = diff.read(cx).buffer_and_diff(cx) {
                let file_stats = action_log::DiffStats::single_file(buffer_diff.read(cx));
                stats.lines_added += file_stats.lines_added;
                stats.lines_removed += file_stats.lines_removed;
            }
        }
        (stats.lines_added > 0 || stats.lines_removed > 0).then_some(stats)
    }

    pub(super) fn strip_edit_verb(headline: &str) -> &str {
        for verb in [
            "Edited ", "Edit ", "Editing ", "Wrote ", "Write ", "Writing ", "Created ", "Create ",
        ] {
            if let Some(rest) = headline.strip_prefix(verb) {
                return rest.trim_start();
            }
        }
        headline
    }

    /// The thought's first sentence, markdown stripped, truncated to fit a chip.
    pub(super) fn thought_summary(source: &str) -> SharedString {
        const MAX_CHARS: usize = 64;

        let Some(line) = source
            .lines()
            .map(|line| {
                line.trim()
                    .trim_start_matches(['#', '>', '-', '*', '+'])
                    .trim_start_matches(|character: char| character.is_ascii_digit())
                    .trim_start_matches(['.', ')'])
                    .trim_matches('*')
                    .trim_matches('`')
                    .trim()
            })
            .find(|line| !line.is_empty())
        else {
            return "Thinking".into();
        };

        let mut summary = line;
        if let Some(end) = line
            .char_indices()
            .zip(line.char_indices().skip(1))
            .find(|((_, terminator), (_, next))| {
                matches!(terminator, '.' | '!' | '?') && next.is_whitespace()
            })
            .map(|((ix, terminator), _)| ix + terminator.len_utf8())
        {
            summary = line[..end].trim_end_matches(['.', '!', '?']);
        }

        if summary.chars().count() > MAX_CHARS {
            let cut = summary
                .char_indices()
                .nth(MAX_CHARS)
                .map(|(ix, _)| ix)
                .unwrap_or(summary.len());
            let cut = summary[..cut]
                .rfind(char::is_whitespace)
                .unwrap_or(cut)
                .max(1);
            return format!("{}…", summary[..cut].trim_end()).into();
        }

        if summary.is_empty() {
            return "Thinking".into();
        }
        summary.to_string().into()
    }

    /// An expanded search chip lists the files it matched. `None` falls back to
    /// the tool's own output.
    pub(super) fn render_search_matches(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(tool_call.kind(), acp_v2::ToolKind::Search) || tool_call.locations.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = tool_call
            .locations
            .iter()
            .enumerate()
            .map(|(location_ix, location)| {
                let path: SharedString = location.path.to_string_lossy().into_owned().into();
                h_flex()
                    .id(("search-match", entry_ix * 1000 + location_ix))
                    .w_full()
                    .min_w_0()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Label::new(path)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate_start(),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_tool_call_location(entry_ix, location_ix, window, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        Some(
            v_flex()
                .ml(rems(0.4))
                .pl_3p5()
                .border_l_1()
                .border_color(self.tool_card_border_color(cx))
                .children(rows)
                .into_any_element(),
        )
    }

    pub(super) fn image_hover_card(
        &self,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let image = self.tool_call_image(tool_call, cx)?;
        // Only what the inline chip already read; the card never reads the disk.
        let dimensions = self.chip_image_dimensions(&image);
        Some(chip_hover_card(move |_window, _cx| {
            let picture = match image.clone() {
                ChipImage::File(path) => img(path),
                ChipImage::Data { image, .. } => img(image),
            };
            // A definite box, or the card resizes under the pointer as it loads.
            const HOVER_CARD_WIDTH: Rems = Rems(28.);
            div()
                .w(HOVER_CARD_WIDTH)
                .h(image_box_height(dimensions, HOVER_CARD_WIDTH))
                .child(picture.size_full().object_fit(ObjectFit::Contain))
                .into_any_element()
        }))
    }

    pub(super) fn read_hover_card(
        &self,
        tool_call: &ToolCall,
        location: &acp_v1::ToolCallLocation,
        _cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let name: SharedString = location
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| location.path.to_string_lossy().into_owned())
            .into();
        let path: SharedString = location.path.to_string_lossy().into_owned().into();

        // Built per chip per frame, so only take handles; read inside the card.
        let content = tool_call
            .content()
            .iter()
            .find_map(|content| match content {
                acp_thread::ToolCallContent::ContentBlock { block, .. } => {
                    block.plain_markdown().cloned()
                }
                _ => None,
            });

        Some(chip_hover_card(move |window, cx| {
            let code: Option<SharedString> = content.as_ref().and_then(|markdown| {
                let source = strip_command_fences(&markdown.read(cx).source())
                    .trim()
                    .to_string();
                (!source.is_empty()).then(|| source.into())
            });
            let language = content
                .as_ref()
                .and_then(|markdown| markdown.read(cx).first_code_block_language());
            let markdown_style =
                MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_buffer_font(cx);
            let mut code_text_style = markdown_style.base_text_style.clone();
            code_text_style.font_size = rems_from_px(12_f32).into();
            code_text_style.color = cx.theme().colors().text;

            v_flex()
                .gap_1p5()
                .max_w_128()
                .child(Label::new(name.clone()).size(LabelSize::Small))
                .child(
                    Label::new(path.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .when_some(code, |this, code| {
                    let runs = highlight_code_runs(
                        &code,
                        language.as_ref(),
                        code_text_style.clone(),
                        &markdown_style,
                    );
                    this.child(
                        card_scroll_region("read-hover-scroll", rems(26.), rems(24.))
                            .text_xs()
                            .child(StyledText::new(code).with_runs(runs)),
                    )
                })
                .into_any()
        }))
    }

    pub(super) fn search_hover_card(
        &self,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let query = search_chip_label(&tool_call.label.read(cx).source())?;
        let locations: Vec<SharedString> = tool_call
            .locations
            .iter()
            .map(|location| location.path.to_string_lossy().into_owned().into())
            .collect();
        let found: SharedString = match locations.len() {
            0 => "No matches".into(),
            1 => "1 file".into(),
            count => format!("{count} files").into(),
        };
        Some(chip_hover_card(move |_window, cx| {
            v_flex()
                .gap_1p5()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Label::new(query.clone()).size(LabelSize::Small))
                        .child(
                            Label::new(found.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .when(!locations.is_empty(), |this| {
                    this.child(
                        card_scroll_region("search-hover-scroll", rems(24.), rems(20.)).child(
                            v_flex().children(locations.iter().map(|path| {
                                Label::new(path.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate_start()
                            })),
                        ),
                    )
                })
                .into_any_element()
        }))
    }

    pub(super) fn command_hover_card(
        &self,
        tool_call: &ToolCall,
        _cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        // Built per chip per frame, so only take handles; read inside the card.
        let terminal = tool_call.terminals().next()?.clone();
        let label = tool_call.label.clone();

        Some(chip_hover_card(move |window, cx| {
            let terminal = terminal.read(cx);
            let command: SharedString = strip_command_fences(&label.read(cx).source())
                .trim()
                .to_string()
                .into();
            let language = label.read(cx).first_code_block_language();
            // A scroll region needs a fixed width, so only long commands get one.
            let scrolls = command.lines().count() > 12 || command.len() > 600;

            let mut meta: Vec<(SharedString, bool)> = Vec::new();
            if let Some(working_dir) = terminal.working_dir() {
                meta.push((working_dir.display().to_string().into(), false));
            }
            if let Some(output) = terminal.output() {
                let status: SharedString = match (
                    output.exit_status.exit_code,
                    output.exit_status.signal.as_ref(),
                ) {
                    (Some(code), _) => format!("exited {code}").into(),
                    (None, Some(signal)) => format!("terminated ({signal})").into(),
                    (None, None) => "exited".into(),
                };
                meta.push((status, output.failed()));
                meta.push((
                    format!(
                        "{:.1}s",
                        output
                            .ended_at
                            .duration_since(terminal.started_at())
                            .as_secs_f64()
                    )
                    .into(),
                    false,
                ));
            }

            let printed: Option<SharedString> = terminal.output().and_then(|output| {
                let content = output.content.trim_end();
                if content.is_empty() {
                    return None;
                }
                let tail: Vec<&str> = content
                    .lines()
                    .rev()
                    .take(OUTPUT_TAIL_LINES)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let elided = content.lines().count() > tail.len();
                let mut text = String::new();
                if elided {
                    text.push_str("…\n");
                }
                text.push_str(&tail.join("\n"));
                Some(text.into())
            });

            let markdown_style =
                MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_buffer_font(cx);
            let mut command_text_style = markdown_style.base_text_style.clone();
            command_text_style.font_size = rems_from_px(12_f32).into();
            command_text_style.color = cx.theme().colors().text;
            let runs = highlight_code_runs(
                &command,
                language.as_ref(),
                command_text_style,
                &markdown_style,
            );

            v_flex()
                .gap_1p5()
                .w(CARD_WIDTH)
                .child(
                    h_flex()
                        .gap_1()
                        .items_start()
                        .child(
                            Label::new("$")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .map(|this| {
                            let command = StyledText::new(command.clone()).with_runs(runs);
                            if scrolls {
                                this.child(
                                    card_scroll_region(
                                        "command-hover-scroll",
                                        CARD_WIDTH,
                                        rems(12.),
                                    )
                                    .text_xs()
                                    .child(command),
                                )
                            } else {
                                this.child(div().min_w_0().text_xs().child(command))
                            }
                        }),
                )
                .when(!meta.is_empty(), |this| {
                    this.child(
                        h_flex()
                            .gap_1p5()
                            .children(meta.iter().map(|(text, is_error)| {
                                Label::new(text.clone())
                                    .size(LabelSize::XSmall)
                                    .color(if *is_error {
                                        Color::Error
                                    } else {
                                        Color::Muted
                                    })
                                    .buffer_font(cx)
                            })),
                    )
                })
                .when_some(printed, |this, printed| {
                    this.child(
                        div()
                            .pt_1p5()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                card_scroll_region("command-hover-output", CARD_WIDTH, rems(18.))
                                    .text_xs()
                                    .font_buffer(cx)
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(printed),
                            ),
                    )
                })
                .into_any_element()
        }))
    }

    /// A run of agent actions as wrapping chips; an expanded chip's body renders
    /// below the row holding it.
    pub(super) fn render_action_group(
        &self,
        active_session_id: &acp_v1::SessionId,
        run_start: usize,
        run_len: usize,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let entries = self.thread.read(cx).entries();

        let mut segments: Vec<AnyElement> = Vec::new();
        let mut row: Vec<AnyElement> = Vec::new();
        let mut any_chip = false;
        // Computed once per call, not once per file chip.
        let mut edited_files: HashMap<usize, Vec<EditedFile>> = HashMap::default();
        let flush_row = |segments: &mut Vec<AnyElement>, row: &mut Vec<AnyElement>| {
            if !row.is_empty() {
                segments.push(
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .gap_1()
                        .children(row.drain(..))
                        .into_any_element(),
                );
            }
        };

        for chip in self.action_chips(run_start, run_len, cx) {
            match chip {
                ActionChip::ToolCall { entry_ix } => {
                    let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                        continue;
                    };
                    let id = ActionChipId::ToolCall(tool_call.id.clone());
                    let is_expanded = self.tool_call_chip_expanded(tool_call, &id, cx);

                    if is_expanded {
                        flush_row(&mut segments, &mut row);
                    }
                    row.push(self.render_tool_call_chip(
                        entry_ix,
                        tool_call,
                        is_expanded,
                        window,
                        cx,
                    ));
                    any_chip = true;

                    if is_expanded {
                        flush_row(&mut segments, &mut row);
                        if let Some(image) = self.tool_call_image(tool_call, cx) {
                            segments.push(self.render_inline_image(entry_ix, 0, image, cx));
                        } else if let Some(matches) =
                            self.render_search_matches(entry_ix, tool_call, cx)
                        {
                            segments.push(matches);
                        } else {
                            segments.push(
                                self.render_any_tool_call(
                                    active_session_id,
                                    entry_ix,
                                    tool_call,
                                    focus_handle,
                                    ToolCallLayout::ChipBody,
                                    window,
                                    cx,
                                )
                                .into_any_element(),
                            );
                            for (image_ix, path) in Self::command_output_images(tool_call, cx)
                                .into_iter()
                                .enumerate()
                            {
                                segments.push(self.render_inline_image(
                                    entry_ix,
                                    image_ix,
                                    ChipImage::File(path),
                                    cx,
                                ));
                            }
                        }
                    }
                }
                ActionChip::EditFile { entry_ix, file_ix } => {
                    let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                        continue;
                    };
                    let files = edited_files
                        .entry(entry_ix)
                        .or_insert_with(|| Self::edited_files(tool_call, cx));
                    let Some(file) = files.get(file_ix).cloned() else {
                        continue;
                    };
                    // Edit chips do not expand; they read as selected while
                    // their diff tab is open.
                    let diff_open = crate::tool_call_diff::is_tool_call_diff_open(
                        &crate::tool_call_diff::ToolCallDiffKey {
                            tool_call_id: tool_call.id.clone(),
                            path: file.path.clone(),
                        },
                        &self.workspace,
                        cx,
                    );
                    row.push(
                        self.render_edit_file_chip(
                            entry_ix, file_ix, &file, tool_call, diff_open, cx,
                        ),
                    );
                    any_chip = true;
                }
                ActionChip::CommandFiles { entry_ix } => {
                    let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                        continue;
                    };
                    let files = Self::command_changed_files(tool_call, cx);
                    if files.is_empty() {
                        continue;
                    }
                    row.push(self.render_command_files_chip(entry_ix, &files, cx));
                    any_chip = true;
                }
                ActionChip::CommandFile { entry_ix, path_ix } => {
                    let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                        continue;
                    };
                    let Some(file) = Self::command_changed_files(tool_call, cx)
                        .get(path_ix)
                        .cloned()
                    else {
                        continue;
                    };
                    row.push(self.render_command_file_chip(entry_ix, path_ix, &file, cx));
                    any_chip = true;
                }
                ActionChip::Collapsed { entry_ixs } => {
                    let unfolded = self.collapsed_chip_is_unfolded_for(&entry_ixs, cx);
                    if unfolded {
                        flush_row(&mut segments, &mut row);
                    }
                    row.push(self.render_collapsed_chip(&entry_ixs, cx));
                    any_chip = true;
                    if unfolded {
                        flush_row(&mut segments, &mut row);
                        segments.push(self.render_collapsed_chip_list(&entry_ixs, cx));
                    }
                }
            }
        }
        flush_row(&mut segments, &mut row);

        if !any_chip {
            return Empty.into_any_element();
        }

        v_flex().my_0p5().gap_1().children(segments).into_any()
    }

    pub(super) fn render_collapsed_chip(
        &self,
        entry_ixs: &[usize],
        cx: &Context<Self>,
    ) -> AnyElement {
        let entries = self.thread.read(cx).entries();
        let mut reads = 0usize;
        let mut searches = 0usize;
        let mut diffs = 0usize;
        let mut git_checks = 0usize;
        let mut github_checks = 0usize;
        let mut inspections = 0usize;
        let mut items: Vec<SharedString> = Vec::new();
        let mut first_id: Option<acp_v1::ToolCallId> = None;
        let mut member_ids: Vec<acp_v1::ToolCallId> = Vec::new();
        let mut any_running = false;
        for &entry_ix in entry_ixs {
            let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                continue;
            };
            first_id.get_or_insert_with(|| tool_call.id.clone());
            member_ids.push(tool_call.id.clone());
            any_running |= matches!(
                tool_call.status(),
                ToolCallStatus::InProgress | ToolCallStatus::Pending
            );
            let class = Self::low_value_class(tool_call, Some(&self.chip_cache), cx)
                .unwrap_or(acp_thread::CommandClass::Read);
            match class {
                acp_thread::CommandClass::Search => searches += 1,
                acp_thread::CommandClass::ReadDiff => diffs += 1,
                acp_thread::CommandClass::GitInfo => git_checks += 1,
                acp_thread::CommandClass::GitHub => github_checks += 1,
                acp_thread::CommandClass::Inspect => inspections += 1,
                _ => reads += 1,
            }
            items.push(self.low_value_item_label(tool_call, cx));
        }
        let Some(first_id) = first_id else {
            return Empty.into_any_element();
        };

        let mut label = [
            (reads, "Read", "file"),
            (searches, "searched", "place"),
            (diffs, "read", "diff"),
            (git_checks, "checked git", "time"),
            (github_checks, "checked GitHub", "time"),
            (inspections, "checked", "thing"),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, verb, noun)| {
            format!("{verb} {count} {noun}{}", if count == 1 { "" } else { "s" })
        })
        .collect::<Vec<_>>()
        .join(", ");
        if label.is_empty() {
            label = "Looked around".to_string();
        } else if reads == 0 {
            let mut chars = label.chars();
            if let Some(first) = chars.next() {
                label = first.to_uppercase().collect::<String>() + chars.as_str();
            }
        }

        let unfolded = self.collapsed_chip_is_unfolded(&first_id, &member_ids);
        let pulse_color = cx.theme().colors().text_accent;

        let chip = self
            .action_chip_base(
                SharedString::from(format!("collapsed-chip-{first_id}")),
                unfolded,
                cx,
            )
            .on_click(cx.listener({
                let first_id = first_id.clone();
                let member_ids = member_ids.clone();
                move |this, _, window, cx| {
                    this.toggle_collapsed_chip(first_id.clone(), &member_ids, window, cx);
                }
            }))
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .tooltip(Tooltip::element({
                move |_, _| {
                    v_flex()
                        .gap_0p5()
                        .max_w_128()
                        .children(items.iter().map(|item| {
                            Label::new(item.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        }))
                        .into_any()
                }
            }));

        if any_running && !unfolded {
            chip.with_animation(
                SharedString::from(format!("collapsed-chip-pulse-{first_id}")),
                Animation::new(Duration::from_secs(2))
                    .repeat()
                    .with_easing(pulsating_between(0.1, 0.35)),
                move |chip, delta| chip.bg(pulse_color.opacity(delta)),
            )
            .into_any_element()
        } else {
            chip.into_any_element()
        }
    }

    pub(super) fn collapsed_chip_is_unfolded_for(&self, entry_ixs: &[usize], cx: &App) -> bool {
        let entries = self.thread.read(cx).entries();
        let member_ids: Vec<acp_v1::ToolCallId> = entry_ixs
            .iter()
            .filter_map(|&entry_ix| match entries.get(entry_ix) {
                Some(AgentThreadEntry::ToolCall(tool_call)) => Some(tool_call.id.clone()),
                _ => None,
            })
            .collect();
        let Some(first_id) = member_ids.first() else {
            return false;
        };
        self.collapsed_chip_is_unfolded(first_id, &member_ids)
    }

    pub(super) fn render_collapsed_chip_list(
        &self,
        entry_ixs: &[usize],
        cx: &Context<Self>,
    ) -> AnyElement {
        let entries = self.thread.read(cx).entries();
        let rows: Vec<AnyElement> = entry_ixs
            .iter()
            .filter_map(|&entry_ix| {
                let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                    return None;
                };
                let icon = Self::low_value_class(tool_call, Some(&self.chip_cache), cx)
                    .map_or(IconName::FileCode, command_class_icon);
                Some(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1p5()
                        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                        .child(
                            div().min_w_0().flex_1().child(
                                Label::new(self.low_value_item_label(tool_call, cx))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                        )
                        .into_any_element(),
                )
            })
            .collect();

        v_flex()
            .w_full()
            .min_w_0()
            .gap_0p5()
            .ml(rems(0.4))
            .pl_3p5()
            .border_l_1()
            .border_color(self.tool_card_border_color(cx))
            .children(rows)
            .into_any_element()
    }

    pub(super) fn collapsed_chip_is_unfolded(
        &self,
        first_id: &acp_v1::ToolCallId,
        member_ids: &[acp_v1::ToolCallId],
    ) -> bool {
        match &self.expanded_action_chip {
            Some(ActionChipId::Collapsed(id)) => id == first_id,
            Some(ActionChipId::ToolCall(id)) => member_ids.contains(id),
            _ => false,
        }
    }

    /// Unfolded in any way (itself or a member expanded) folds everything.
    pub(super) fn toggle_collapsed_chip(
        &mut self,
        first_id: acp_v1::ToolCallId,
        member_ids: &[acp_v1::ToolCallId],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.collapsed_chip_is_unfolded(&first_id, member_ids) {
            if let Some(previous) = self.expanded_action_chip.take() {
                self.collapse_action_chip(&previous, cx);
                self.remeasure_chip(&previous, cx);
            }
            self.remeasure_chip(&ActionChipId::Collapsed(first_id), cx);
            cx.notify();
        } else {
            self.toggle_action_chip(ActionChipId::Collapsed(first_id), window, cx);
        }
    }

    pub(super) fn low_value_item_label(&self, tool_call: &ToolCall, cx: &App) -> SharedString {
        if let Some(location) = tool_call.locations.first()
            && matches!(tool_call.kind(), acp_v2::ToolKind::Read)
        {
            return location.path.to_string_lossy().into_owned().into();
        }
        if tool_call.terminals().next().is_some() {
            let facts = self.chip_cache.command(tool_call, cx);
            for segment in &facts.parsed.segments {
                let label = match &segment.kind {
                    acp_thread::SegmentKind::Read {
                        paths,
                        lines,
                        revision,
                    } if !paths.is_empty() => {
                        let mut label = match lines {
                            Some(lines) => {
                                format!("{}:{}-{}", paths.join(", "), lines.start, lines.end)
                            }
                            None => paths.join(", "),
                        };
                        if let Some(revision) = revision {
                            label.push_str(&format!(" @ {revision}"));
                        }
                        label
                    }
                    acp_thread::SegmentKind::Search { query: Some(query) } => query.clone(),
                    acp_thread::SegmentKind::ListDirectory { path } => match path {
                        Some(path) => path.clone(),
                        None => "directory".to_string(),
                    },
                    acp_thread::SegmentKind::Lookup {
                        program: Some(program),
                    } => program.clone(),
                    acp_thread::SegmentKind::CountLines { paths } if !paths.is_empty() => {
                        paths.join(", ")
                    }
                    acp_thread::SegmentKind::Git { operation, target } => {
                        let verb = match operation {
                            acp_thread::GitOperation::ReadChanges => "diff",
                            acp_thread::GitOperation::Inspect => "git",
                            acp_thread::GitOperation::Modify => "git",
                        };
                        match target {
                            Some(target) => format!("{verb} {target}"),
                            None => verb.to_string(),
                        }
                    }
                    _ => continue,
                };
                if !label.is_empty() {
                    return label.into();
                }
            }
            return acp_thread::command_display_prefix(&facts.command, 60).into();
        }
        let label = tool_call.label.read(cx).source().to_string();
        label
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('`')
            .to_string()
            .into()
    }

    pub(super) fn render_tool_call_chip(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        is_expanded: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = tool_call.id.clone();
        let pulse_color = cx.theme().colors().text_accent;
        let has_terminals = tool_call.terminals().next().is_some();
        let outcome = has_terminals
            .then(|| self.chip_cache.output(tool_call, cx).summary.clone())
            .flatten();
        let outcome_label = outcome.as_ref().and_then(|summary| summary.label());
        let first_error = outcome
            .as_ref()
            .and_then(|summary| summary.first_error.clone());
        let outcome_failed = outcome
            .as_ref()
            .is_some_and(|summary| summary.errors > 0 || summary.tests_failed > 0);

        let destructive = has_terminals && self.chip_cache.command(tool_call, cx).destructive;
        let is_edit = matches!(tool_call.kind(), acp_v2::ToolKind::Edit)
            || tool_call.diffs().next().is_some();
        let read_file = (matches!(tool_call.kind(), acp_v2::ToolKind::Read)
            && tool_call.locations.len() == 1)
            .then(|| tool_call.locations.first())
            .flatten();
        let is_image = self.tool_call_image(tool_call, cx).is_some();
        let is_search = matches!(tool_call.kind(), acp_v2::ToolKind::Search) && !has_terminals;
        let mut command_label: Option<CommandChipLabel> = None;
        let mut command_pieces: Option<Vec<CommandChipPiece>> = None;
        let headline: SharedString = if is_search {
            search_chip_label(&tool_call.label.read(cx).source())
                .unwrap_or_else(|| "Searched".into())
        } else if let Some(location) = read_file {
            location
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| location.path.to_string_lossy().into_owned())
                .into()
        } else if has_terminals {
            let facts = self.chip_cache.command(tool_call, cx);
            let collapsed = if is_expanded {
                CollapsedCommand::Label(CommandChipLabel::command(facts.command.clone()))
            } else {
                Self::collapsed_command(&facts, outcome_label.as_deref())
            };
            match collapsed {
                CollapsedCommand::Label(label) if !label.text.is_empty() => {
                    let text = SharedString::from(label.text.clone());
                    command_label = Some(label);
                    text
                }
                CollapsedCommand::Pieces(pieces) if !pieces.is_empty() => {
                    let text = SharedString::from(
                        pieces
                            .iter()
                            .map(|piece| piece.label.text.as_str())
                            .collect::<Vec<_>>()
                            .join(CommandChipLabel::SEPARATOR),
                    );
                    command_pieces = Some(pieces);
                    text
                }
                _ => "command".into(),
            }
        } else {
            let label = tool_call.label.read(cx).source().to_string();
            let first = label
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('`')
                .to_string();
            let first = if is_edit {
                Self::strip_edit_verb(&first).trim_matches('`').to_string()
            } else {
                first
            };
            if first.is_empty() {
                "action".into()
            } else {
                first.into()
            }
        };

        let label_element = if let Some(pieces) = command_pieces.as_ref() {
            let markdown_style = self.chip_cache.style(window, cx);
            let mut command_text_style = markdown_style.base_text_style.clone();
            command_text_style.font_size = rems_from_px(12_f32).into();
            command_text_style.color = cx.theme().colors().text_muted;
            let code_language = tool_call.label.read(cx).first_code_block_language();
            h_flex()
                // Auto basis; see the single-command branch below.
                .flex_initial()
                .min_w_0()
                .gap_1()
                .overflow_hidden()
                .text_xs()
                .children(pieces.iter().enumerate().map(|(piece_ix, piece)| {
                    let runs = self.chip_cache.highlight_label(
                        &piece.label,
                        code_language.as_ref(),
                        &command_text_style,
                        &markdown_style,
                    );
                    h_flex()
                        .flex_none()
                        .gap_0p5()
                        // A rule, not `|`, which would read as a shell pipe.
                        .when(piece_ix > 0, |this| {
                            this.child(
                                div()
                                    .flex_none()
                                    .mx_1()
                                    .w_px()
                                    .h(rems(0.875))
                                    .bg(cx.theme().colors().border),
                            )
                        })
                        .when(piece.in_environment, |this| {
                            this.child(ChipGlyph::Language("nix").element(Color::Muted, cx))
                        })
                        .child(piece.glyph.element(Color::Muted, cx))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(StyledText::new(piece.label.text.clone()).with_runs(runs)),
                        )
                }))
                .into_any_element()
        } else if has_terminals {
            let markdown_style = self.chip_cache.style(window, cx);
            let mut command_text_style = markdown_style.base_text_style.clone();
            command_text_style.font_size = rems_from_px(12_f32).into();
            command_text_style.color = cx.theme().colors().text_muted;
            let runs = self.chip_cache.highlight_label(
                &command_label.unwrap_or_else(|| CommandChipLabel::command(headline.to_string())),
                tool_call
                    .label
                    .read(cx)
                    .first_code_block_language()
                    .as_ref(),
                &command_text_style,
                &markdown_style,
            );
            div()
                // Not `flex_1`: a zero basis gives the label no intrinsic width,
                // so a content-sized chip would shrink it to an ellipsis.
                .flex_initial()
                .min_w_0()
                .debug_selector({
                    let tool_call_id = tool_call.id.clone();
                    move || format!("COMMAND_CHIP_LABEL-{tool_call_id}")
                })
                .map(|this| {
                    if is_expanded {
                        this.whitespace_normal()
                    } else {
                        this.overflow_hidden()
                            .whitespace_nowrap()
                            .line_clamp(1)
                            .text_ellipsis()
                    }
                })
                .text_xs()
                .child(StyledText::new(headline.clone()).with_runs(runs))
                .into_any_element()
        } else {
            Label::new(headline.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate_start()
                .into_any_element()
        };

        let terminal_output = tool_call
            .terminals()
            .next()
            .and_then(|terminal| terminal.read(cx).output());
        // A detached command's call reads `completed` while it runs on, and a
        // subagent's likewise; their own reported lifecycles say otherwise.
        let backgrounded = self
            .thread
            .read(cx)
            .tool_call_is_backgrounded(&tool_call.id);
        let subagent_state = self.thread.read(cx).subagent_state_for_tool_call(tool_call);
        let running = match subagent_state {
            Some(state) => !state.is_terminal(),
            None => {
                backgrounded
                    || matches!(
                        tool_call.status(),
                        ToolCallStatus::InProgress | ToolCallStatus::Pending
                    )
            }
        };
        // A user-stopped command exits non-zero but did not fail.
        let user_stopped = tool_call
            .terminals()
            .next()
            .is_some_and(|terminal| terminal.read(cx).was_stopped_by_user());
        let failed = !user_stopped
            && match subagent_state {
                Some(state) => matches!(state, acp_thread::AsyncTaskState::Failed),
                None => {
                    matches!(
                        tool_call.status(),
                        ToolCallStatus::Rejected
                            | ToolCallStatus::Canceled
                            | ToolCallStatus::Failed
                    ) || terminal_output.is_some_and(|output| output.failed())
                }
            };

        let icon_color = if failed || outcome_failed {
            Color::Error
        } else if destructive {
            Color::Warning
        } else {
            Color::Muted
        };
        let icon_element = if running {
            Some(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element(),
            )
        } else if user_stopped {
            Some(
                Icon::new(IconName::Stop)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            )
        } else if failed {
            let exit_code = terminal_output.and_then(|output| output.exit_status.exit_code);
            Some(
                div()
                    // Upstream's terminal header selector, which upstream's tests find.
                    .debug_selector(move || format!("terminal-tool-failed-{exit_code:?}"))
                    .child(
                        Icon::new(IconName::Close)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .into_any_element(),
            )
        } else if subagent_state.is_some_and(|state| state.is_terminal()) {
            Some(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            )
        } else if command_pieces.is_some() {
            // Each piece carries its own glyph.
            None
        } else if let Some(icon_path) = Self::tool_call_file_icon(tool_call, cx) {
            Some(
                Icon::from_path(icon_path)
                    .size(IconSize::Small)
                    .color(icon_color)
                    .into_any_element(),
            )
        } else if has_terminals {
            let facts = self.chip_cache.command(tool_call, cx);
            let mut acts = facts
                .parsed
                .segments
                .iter()
                .filter(|segment| !segment.kind.is_noop());
            let glyph = match (acts.next(), acts.next(), destructive) {
                (Some(only), None, false) => ChipGlyph::for_segment(only),
                _ if destructive => ChipGlyph::Icon(IconName::Trash),
                _ => ChipGlyph::Icon(command_class_icon(facts.class)),
            };
            Some(glyph.element(icon_color, cx))
        } else {
            Some(
                Icon::new(Self::tool_kind_icon(tool_call.kind()))
                    .size(IconSize::Small)
                    .color(icon_color)
                    .into_any_element(),
            )
        };

        let edit_stats = is_edit
            .then(|| self.chip_edit_stats(tool_call, cx))
            .flatten();

        let host = has_terminals
            .then(|| self.command_host_for(tool_call, cx))
            .flatten();
        let environment = has_terminals
            .then(|| self.command_environment_for(tool_call, cx))
            .flatten();
        let full_command =
            has_terminals.then(|| self.chip_cache.command(tool_call, cx).command.clone());
        // A display terminal has no process of ours to kill.
        let stoppable_terminal = running
            .then(|| tool_call.terminals().next())
            .flatten()
            .filter(|terminal| terminal.read(cx).is_process_backed())
            .cloned();
        // A backgrounded command is stopped by asking the agent.
        let stoppable_async_task = stoppable_terminal
            .is_none()
            .then(|| {
                self.thread
                    .read(cx)
                    .async_task_for_tool_call(&tool_call.id)
                    .filter(|task| task.can_stop)
                    .map(|task| task.id.clone())
            })
            .flatten();

        let chip_group = SharedString::from(format!("action-chip-{entry_ix}"));
        let chip = self
            .action_chip_base(("action-chip", entry_ix), is_expanded, cx)
            .group(chip_group.clone())
            .when(command_pieces.is_some(), |this| this.max_w_full())
            .when(is_expanded && has_terminals, |this| {
                this.w_full()
                    .max_w_full()
                    .h_auto()
                    .min_h(rems_from_px(24_f32))
                    .py_1()
            })
            .on_click(cx.listener({
                let id = id.clone();
                let opens_file = read_file.is_some() && !is_image;
                move |this, _, window, cx| {
                    if opens_file {
                        this.open_tool_call_location(entry_ix, 0, window, cx);
                    } else if is_image {
                        this.toggle_image_chip(ActionChipId::ToolCall(id.clone()), cx);
                    } else {
                        this.toggle_action_chip(ActionChipId::ToolCall(id.clone()), window, cx)
                    }
                }
            }))
            .children(icon_element)
            .when_some(host, |this, host| {
                this.child(
                    h_flex()
                        .id(("command-host", entry_ix))
                        .flex_none()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::Server)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(host.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .tooltip(Tooltip::text(format!("Ran on {host}"))),
                )
            })
            .when_some(environment, |this, environment| {
                let CommandEnvironment { name, partial } = environment;
                this.child(
                    h_flex()
                        .id(("command-environment", entry_ix))
                        .flex_none()
                        .gap_0p5()
                        .child(ChipGlyph::Language("nix").element(Color::Muted, cx))
                        .child(
                            Label::new(name.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .tooltip(Tooltip::text(if partial {
                            format!("Part of this line ran in the {name} Nix devshell")
                        } else {
                            format!("Ran in the {name} Nix devshell")
                        })),
                )
            })
            .child(label_element)
            .when_some(first_error, |this, location| {
                let label: SharedString = format!(
                    "{}:{}",
                    location
                        .path
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&location.path),
                    location.line
                )
                .into();
                let full: SharedString = format!("{}:{}", location.path, location.line).into();
                this.child(
                    h_flex()
                        .id(("first-error", entry_ix))
                        .flex_none()
                        .gap_0p5()
                        .px_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().status().error.opacity(0.35))
                        .bg(cx.theme().status().error.opacity(0.12))
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().status().error.opacity(0.25)))
                        .child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Error)
                                .buffer_font(cx),
                        )
                        .tooltip(Tooltip::text(format!("Go to {full}")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_output_location(&location, window, cx);
                        })),
                )
            })
            // Kills this one terminal; the turn keeps going.
            .when_some(stoppable_terminal, |this, terminal| {
                this.child(
                    IconButton::new(("stop-command", entry_ix), IconName::Stop)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop This Command"))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            terminal.update(cx, |terminal, cx| terminal.stop_by_user(cx));
                        }),
                )
            })
            .when_some(stoppable_async_task, |this, async_task_id| {
                this.child(
                    IconButton::new(("stop-background-command", entry_ix), IconName::Stop)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop This Command"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.stop_async_task(async_task_id.clone(), cx);
                        })),
                )
            })
            .when_some(full_command, |this, command| {
                this.child(
                    IconButton::new(("copy-command", entry_ix), IconName::Copy)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .visible_on_hover(chip_group.clone())
                        .tooltip(Tooltip::text("Copy Command"))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                        }),
                )
            })
            .when_some(edit_stats, |this, stats| {
                let id = id.clone();
                this.child(self.render_diff_stat_chip(
                    ("action-chip-diff", entry_ix),
                    stats,
                    move |this, window, cx| {
                        this.toggle_action_chip(ActionChipId::ToolCall(id.clone()), window, cx);
                    },
                    cx,
                ))
            })
            .map(|this| {
                if is_image && let Some(card) = self.image_hover_card(tool_call, cx) {
                    return this.hoverable_tooltip(card);
                }
                if is_search && let Some(card) = self.search_hover_card(tool_call, cx) {
                    return this.hoverable_tooltip(card);
                }
                match read_file.and_then(|location| self.read_hover_card(tool_call, location, cx)) {
                    Some(card) => this.hoverable_tooltip(card),
                    None => match self.command_hover_card(tool_call, cx) {
                        Some(card) => this.hoverable_tooltip(card),
                        None => this.tooltip(Tooltip::text(headline)),
                    },
                }
            });

        let chip = if running && !is_expanded {
            chip.border_color(pulse_color.opacity(0.5))
                .with_animation(
                    ("action-chip-pulse", entry_ix),
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.04, 0.18)),
                    move |chip, delta| chip.bg(pulse_color.opacity(delta)),
                )
                .into_any_element()
        } else {
            chip.into_any_element()
        };

        let entity = cx.entity();
        right_click_menu(("action-chip-bookmark", entry_ix))
            .trigger(move |_, _, _| chip)
            .menu(move |window, cx| {
                let entity = entity.clone();
                ContextMenu::build(window, cx, move |menu, _, cx| {
                    // Resolved on open: per frame it would make drawing quadratic.
                    let bookmarked = entity.read(cx).is_bookmarked(entry_ix, cx);
                    let entity = entity.clone();
                    menu.entry(
                        if bookmarked {
                            "Remove Bookmark"
                        } else {
                            "Bookmark This Point"
                        },
                        Some(Box::new(crate::ToggleBookmark)),
                        move |_, cx| {
                            entity.update(cx, |this, cx| {
                                this.toggle_bookmark_at(entry_ix, cx);
                            });
                        },
                    )
                })
            })
            .into_any_element()
    }

    /// What the line did plus what it concluded, falling back to the command
    /// text only when there is nothing to summarize.
    pub(super) fn collapsed_command(
        facts: &CommandFacts,
        outcome: Option<&str>,
    ) -> CollapsedCommand {
        const WIDTH: usize = 60;
        const PIECE_WIDTH: usize = 22;

        let command = facts.command.as_str();
        let parsed = &facts.parsed;
        if let Some(summary) = facts.summary.clone() {
            // A summary that quotes the command is highlighted as shell.
            let quotes_the_command = parsed.segments.iter().any(|segment| {
                segment
                    .work_text()
                    .trim_start()
                    .starts_with(summary.as_str())
            });
            let mut label = if quotes_the_command {
                CommandChipLabel::command(summary)
            } else {
                CommandChipLabel::prose(summary)
            };
            if let Some(outcome) = outcome {
                label.text.push_str(&format!(" · {outcome}"));
            }
            return CollapsedCommand::Label(label);
        }

        let segments: Vec<_> = parsed
            .segments
            .iter()
            .filter(|segment| segment.kind.is_worth_naming())
            .collect();
        match segments.len() {
            0 => CollapsedCommand::Label(CommandChipLabel::command(
                acp_thread::command_display_prefix(command, WIDTH),
            )),
            1 => CollapsedCommand::Label(CommandChipLabel::command(
                acp_thread::command_display_prefix(segments[0].work_text(), WIDTH),
            )),
            _ => {
                let partial_environment = parsed.environment_is_partial();
                CollapsedCommand::Pieces(
                    segments
                        .iter()
                        .map(|segment| CommandChipPiece {
                            glyph: ChipGlyph::for_segment(segment),
                            label: CommandChipLabel::command(acp_thread::command_display_prefix(
                                &segment.short_label(),
                                PIECE_WIDTH,
                            )),
                            in_environment: partial_environment && segment.environment.is_some(),
                        })
                        .collect(),
                )
            }
        }
    }

    pub(super) fn action_chip_base(
        &self,
        id: impl Into<ElementId>,
        is_expanded: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            // `min_w_0` lets the max-width cap bind over the content minimum;
            // `flex_shrink_0` makes a full row wrap rather than squeeze chips.
            .min_w_0()
            .flex_shrink_0()
            .max_w(relative(0.75))
            .h(rems_from_px(24_f32))
            .gap_1()
            .px_1p5()
            .rounded_md()
            .border_1()
            .border_color(self.tool_card_border_color(cx))
            .when(is_expanded, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
    }

    /// Empty until the command has exited and git status has settled.
    pub(super) fn command_changed_files(
        tool_call: &ToolCall,
        cx: &App,
    ) -> Vec<acp_thread::ChangedFile> {
        let mut files: Vec<acp_thread::ChangedFile> = Vec::new();
        for terminal in tool_call.terminals() {
            for file in terminal.read(cx).changed_files() {
                if !files.iter().any(|seen| seen.path == file.path) {
                    files.push(file.clone());
                }
            }
        }
        files
    }

    pub(super) fn command_output_images(tool_call: &ToolCall, cx: &App) -> Vec<std::path::PathBuf> {
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for terminal in tool_call.terminals() {
            for path in terminal.read(cx).output_images() {
                if !paths.iter().any(|seen| seen == path) {
                    paths.push(path.clone());
                }
            }
        }
        paths
    }

    /// `None` while loading; the first call starts the load.
    fn command_file_diff_editor(
        &self,
        entry_ix: usize,
        file: &acp_thread::ChangedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        let key = (entry_ix, file.path.clone());
        {
            let mut cache = self.command_file_diffs.borrow_mut();
            let used = cache.touch();
            if let Some(state) = cache.by_file.get_mut(&key) {
                return match state {
                    CommandFileDiff::Ready {
                        editor, last_used, ..
                    } => {
                        *last_used = used;
                        Some(editor.clone())
                    }
                    CommandFileDiff::Loading { .. } => None,
                };
            }
        }
        let project = self.project.upgrade()?;
        let path = file.path.clone();
        let display_path = file.path.path.as_unix_str().to_string();
        let found = file.pre_command_text.clone();
        let task = cx.spawn_in(window, {
            let key = key.clone();
            async move |this, cx| {
                let opened = async {
                    let buffer = project
                        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                        .await?;
                    // A clean file's pre-command text is HEAD, so none was captured.
                    let old_text = match found {
                        Some(found) => Some(found.to_string()),
                        None => {
                            let diff = project
                                .update(cx, |project, cx| {
                                    project.git_store().update(cx, |git_store, cx| {
                                        git_store.open_uncommitted_diff(buffer.clone(), cx)
                                    })
                                })
                                .await?;
                            cx.update(|_, cx| diff.read(cx).base_text_string(cx))?
                        }
                    };
                    let (new_text, languages) = cx.update(|_, cx| {
                        (buffer.read(cx).text(), project.read(cx).languages().clone())
                    })?;
                    let (diff, editor) = cx.update(|window, cx| {
                        let diff = cx.new(|cx| {
                            acp_thread::Diff::finalized(
                                display_path,
                                old_text,
                                new_text,
                                languages,
                                cx,
                            )
                        });
                        let multibuffer = diff.read(cx).multibuffer().clone();
                        (diff, command_file_diff_editor(multibuffer, window, cx))
                    })?;
                    anyhow::Ok((diff, editor))
                }
                .await;

                this.update(cx, |this, cx| {
                    match opened {
                        Ok((diff, editor)) => {
                            let mut cache = this.command_file_diffs.borrow_mut();
                            let last_used = cache.touch();
                            cache.by_file.insert(
                                key,
                                CommandFileDiff::Ready {
                                    editor,
                                    _diff: diff,
                                    last_used,
                                },
                            );
                            cache.evict_stale();
                        }
                        // Forget it so a later hover can retry.
                        Err(_) => {
                            this.command_file_diffs.borrow_mut().by_file.remove(&key);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        });
        self.command_file_diffs
            .borrow_mut()
            .by_file
            .insert(key, CommandFileDiff::Loading { _task: task });
        None
    }

    /// Shows this command's own change, or says it is the file's whole
    /// uncommitted diff when the pre-command text of a dirty file was not read.
    pub(super) fn command_file_hover_card(
        &self,
        entry_ix: usize,
        file: &acp_thread::ChangedFile,
        cx: &Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<> {
        let file = file.clone();
        let full: SharedString = file.path.path.as_unix_str().to_string().into();
        let stats = diff_stats(file.added, file.deleted);
        let wider_than_the_command = file.pre_command_dirty && file.pre_command_text.is_none();
        let this = cx.entity().downgrade();

        chip_hover_card_observing(this.clone(), move |window, cx| {
            let editor = this
                .update(cx, |this, cx| {
                    this.command_file_diff_editor(entry_ix, &file, window, cx)
                })
                .ok()
                .flatten();
            let heading = if wider_than_the_command {
                "Uncommitted changes to this file"
            } else {
                "Changed by this command"
            };
            v_flex()
                .gap_1p5()
                .max_w(DIFF_CARD_MAX_W)
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Label::new(full.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .when_some(stats, |this, stats| {
                            this.child(
                                Label::new(format!(
                                    "+{} -{}",
                                    stats.lines_added, stats.lines_removed
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                            )
                        }),
                )
                .child(Label::new(heading).size(LabelSize::XSmall))
                .when_some(editor, |this, editor| {
                    this.child(
                        card_scroll_region(
                            "command-file-hover-diff",
                            DIFF_CARD_WIDTH,
                            DIFF_CARD_HEIGHT,
                        )
                        .child(editor),
                    )
                })
                .into_any_element()
        })
    }

    pub(super) fn render_command_files_chip(
        &self,
        entry_ix: usize,
        files: &[acp_thread::ChangedFile],
        cx: &Context<Self>,
    ) -> AnyElement {
        let added: u32 = files.iter().map(|file| file.added).sum();
        let deleted: u32 = files.iter().map(|file| file.deleted).sum();
        let paths: Vec<SharedString> = files
            .iter()
            .map(|file| file.path.path.as_unix_str().to_string().into())
            .collect();

        self.render_command_change_chip(
            SharedString::from(format!("command-files-chip-{entry_ix}")),
            Icon::new(IconName::ToolPencil)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            format!("{} files changed", files.len()).into(),
            diff_stats(added, deleted),
            files[0].path.clone(),
            chip_hover_card(move |_window, cx| {
                v_flex()
                    .gap_1p5()
                    .child(
                        Label::new("Changed by this command")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        card_scroll_region("command-files-hover", CARD_WIDTH, rems(20.)).child(
                            v_flex().children(paths.iter().map(|path| {
                                Label::new(path.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate_start()
                            })),
                        ),
                    )
                    .into_any_element()
            }),
            cx,
        )
    }

    fn render_command_change_chip(
        &self,
        id: SharedString,
        icon: AnyElement,
        name: SharedString,
        stats: Option<action_log::DiffStats>,
        opens: project::ProjectPath,
        card: impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        self.action_chip_base(id.clone(), false, cx)
            .child(icon)
            .child(
                div()
                    // Auto basis: see the command chip's label.
                    .flex_initial()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate_start(),
                    ),
            )
            .when_some(stats, |this, stats| {
                let opens = opens.clone();
                this.child(self.render_diff_stat_chip(
                    SharedString::from(format!("{id}-diff")),
                    stats,
                    move |this, window, cx| this.open_command_file_diff(&opens, window, cx),
                    cx,
                ))
            })
            .hoverable_tooltip(card)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_command_file_diff(&opens, window, cx)
            }))
            .into_any_element()
    }

    pub(super) fn render_command_file_chip(
        &self,
        entry_ix: usize,
        path_ix: usize,
        file: &acp_thread::ChangedFile,
        cx: &Context<Self>,
    ) -> AnyElement {
        let name: SharedString = file
            .path
            .path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_else(|| file.path.path.as_unix_str().to_string())
            .into();
        let icon = match FileIcons::get_icon(std::path::Path::new(name.as_str()), cx) {
            Some(icon_path) => Icon::from_path(icon_path)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            None => Icon::new(IconName::ToolPencil)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        self.render_command_change_chip(
            SharedString::from(format!("command-file-chip-{entry_ix}-{path_ix}")),
            icon,
            name,
            diff_stats(file.added, file.deleted),
            file.path.clone(),
            self.command_file_hover_card(entry_ix, file, cx),
            cx,
        )
    }

    fn chip_image_dimensions(&self, image: &ChipImage) -> Option<gpui::Size<u32>> {
        match image {
            ChipImage::File(path) => {
                match self.chip_cache.image_shapes.borrow().get(path.as_path()) {
                    Some(ImageShape::Known(dimensions)) => Some(*dimensions),
                    _ => None,
                }
            }
            ChipImage::Data { dimensions, .. } => *dimensions,
        }
    }

    /// Starts reading the file's header the first time it is asked for.
    fn image_file_box_height(
        &self,
        path: &std::path::Path,
        entry_ix: usize,
        cx: &Context<Self>,
    ) -> Rems {
        let known = self.chip_cache.image_shapes.borrow().get(path).copied();
        match known {
            Some(ImageShape::Known(dimensions)) => {
                image_box_height(Some(dimensions), IMAGE_CHIP_WIDTH)
            }
            Some(ImageShape::Unknown) => IMAGE_CHIP_HEIGHT,
            Some(ImageShape::Reading) => IMAGE_CHIP_MAX_HEIGHT,
            None => {
                self.read_image_shape(path.to_path_buf(), entry_ix, cx);
                IMAGE_CHIP_MAX_HEIGHT
            }
        }
    }

    fn read_image_shape(&self, path: std::path::PathBuf, entry_ix: usize, cx: &Context<Self>) {
        let Some(fs) = self
            .project
            .upgrade()
            .map(|project| project.read(cx).fs().clone())
        else {
            return;
        };
        self.chip_cache
            .image_shapes
            .borrow_mut()
            .insert(path.clone(), ImageShape::Reading);

        cx.spawn(async move |this, cx| {
            let shape = image_shape_of_file(&fs, &path).await;
            this.update(cx, |this, cx| {
                this.chip_cache
                    .image_shapes
                    .borrow_mut()
                    .insert(path, shape);
                let item = this.drawn_item_for_entry(entry_ix, cx);
                this.list_state.remeasure_items(item..item + 1);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_inline_image(
        &self,
        entry_ix: usize,
        image_ix: usize,
        image: ChipImage,
        cx: &Context<Self>,
    ) -> AnyElement {
        let file = match &image {
            ChipImage::File(path) => Some(path.clone()),
            ChipImage::Data { .. } => None,
        };
        let copyable = image.clone();
        let box_height = match &image {
            ChipImage::File(path) => self.image_file_box_height(path, entry_ix, cx),
            ChipImage::Data { dimensions, .. } => image_box_height(*dimensions, IMAGE_CHIP_WIDTH),
        };
        let picture = match image {
            ChipImage::File(path) => img(path),
            ChipImage::Data { image, .. } => img(image),
        };

        let body = div()
            .id(SharedString::from(format!(
                "chip-image-{entry_ix}-{image_ix}"
            )))
            // A definite box: an image that grows after the list measured it
            // paints over the entries below.
            .w(IMAGE_CHIP_WIDTH)
            .h(box_height)
            .child(
                picture
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .rounded_md(),
            )
            .when_some(file.clone(), |this, path| {
                this.cursor_pointer()
                    .tooltip(Tooltip::text("Open Image"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_file(&path, window, cx);
                    }))
            })
            .into_any_element();

        div()
            .my_0p5()
            .ml_5()
            .mr_5()
            .child(
                right_click_menu(("chip-image-menu", entry_ix))
                    .trigger(move |_, _, _| body)
                    .menu(move |window, cx| {
                        let copyable = copyable.clone();
                        let path = file
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned());
                        ContextMenu::build(window, cx, move |menu, _, _| {
                            let copyable = copyable.clone();
                            menu.entry("Copy Image", None, move |_, cx| {
                                copy_chip_image(copyable.clone(), cx);
                            })
                            .when_some(path, |menu, path| {
                                menu.entry("Copy Image Path", None, move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                                })
                            })
                        })
                    }),
            )
            .into_any_element()
    }

    /// By project path when possible, so it shares the project panel's tab.
    pub(super) fn open_image_file(
        &self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_path = self
            .project
            .upgrade()
            .and_then(|project| project.read(cx).find_project_path(path, cx));
        let path = path.to_path_buf();
        let open = self
            .workspace
            .update(cx, |workspace, cx| match project_path {
                Some(project_path) => workspace.open_path(project_path, None, true, window, cx),
                None => workspace.open_abs_path(
                    path,
                    OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                ),
            })
            .log_err();
        if let Some(open) = open {
            open.detach_and_log_err(cx);
        }
    }

    fn open_command_file_diff(
        &mut self,
        path: &project::ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            git_ui::project_diff::ProjectDiff::deploy_at_project_path(
                workspace,
                path.clone(),
                window,
                cx,
            );
        });
    }

    pub(super) fn render_edit_file_chip(
        &self,
        entry_ix: usize,
        file_ix: usize,
        file: &EditedFile,
        tool_call: &ToolCall,
        is_expanded: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let name: SharedString = file
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.path.to_string_lossy().into_owned())
            .into();

        let icon_element = if let Some(icon_path) = file
            .path
            .extension()
            .and_then(|_| FileIcons::get_icon(&file.path, cx))
        {
            Icon::from_path(icon_path)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else {
            Icon::new(IconName::ToolPencil)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };

        let stats = self.edit_file_stats(tool_call, file, cx);
        self.action_chip_base(
            SharedString::from(format!("edit-file-chip-{entry_ix}-{file_ix}")),
            is_expanded,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_edit_file_diff(entry_ix, file_ix, window, cx);
        }))
        .child(icon_element)
        .child(
            Label::new(name.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate_start(),
        )
        .when_some(stats, |this, stats| {
            this.child(self.render_diff_stat_chip(
                SharedString::from(format!("edit-file-chip-diff-{entry_ix}-{file_ix}")),
                stats,
                move |this, window, cx| {
                    this.open_edit_file_diff(entry_ix, file_ix, window, cx);
                },
                cx,
            ))
        })
        .map(
            |this| match self.edit_hover_card(entry_ix, file, tool_call, cx) {
                Some(card) => this.hoverable_tooltip(card),
                None => this.tooltip(Tooltip::text(name)),
            },
        )
        .into_any_element()
    }

    pub(super) fn edit_hover_card(
        &self,
        entry_ix: usize,
        file: &EditedFile,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let diff = self.diff_for_edited_file(tool_call, file, cx)?;
        let editor = self
            .entry_view_state
            .read(cx)
            .entry(entry_ix)?
            .editor_for_diff(&diff)?;
        let path: SharedString = file.path.to_string_lossy().into_owned().into();

        Some(chip_hover_card(move |_window, _cx| {
            v_flex()
                .gap_1p5()
                .max_w(DIFF_CARD_MAX_W)
                .child(
                    Label::new(path.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(_cx),
                )
                .child(
                    card_scroll_region("edit-hover-diff", DIFF_CARD_WIDTH, DIFF_CARD_HEIGHT)
                        .child(editor.clone()),
                )
                .into_any_element()
        }))
    }
}
