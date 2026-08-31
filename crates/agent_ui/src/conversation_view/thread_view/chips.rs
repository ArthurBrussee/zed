//! What a chip is made of: the glyph it wears, the label it carries, and the
//! popover it opens.
//!
//! This is the fork's own vocabulary rather than an edit to Zed's, and it lives
//! beside the thread view instead of inside it so that upstream's file stays
//! close to upstream's. Everything here is presentation; what a command *did*
//! is decided in `acp_thread::command_parse`.

use std::sync::Arc;

use editor::RestoreOnlyUnstagedDiffHunkDelegate;
use project::project_settings::DiagnosticSeverity;

use super::*;
use crate::entry_view_state::diff_editor_text_style_refinement;

/// A hover card is a fixed-width window onto something longer, and the command
/// card is two of them stacked (the command, then what it printed), so both
/// halves share one width rather than each sizing to its own content.
const CARD_WIDTH: Rems = Rems(30.);

/// A diff wants width more than height: wrapping is what makes a hunk hard to
/// read, and a card that could be half again as wide spends its life scrolling
/// instead. Kept together with `DIFF_CARD_HEIGHT` and `DIFF_CARD_MAX_W` so a
/// declared edit and a command-changed file open at the same size.
const DIFF_CARD_WIDTH: Rems = Rems(48.);
/// A diff card's scroll region cannot be taller than this. Bigger than the
/// text cards get, because there is more to read at once and the enterable
/// card scrolls for what still doesn't fit.
const DIFF_CARD_HEIGHT: Rems = Rems(30.);
/// The container ceiling has to move with the scroll region or the region
/// won't get it. Sized to hold `DIFF_CARD_WIDTH` plus the card's own padding
/// without cropping.
const DIFF_CARD_MAX_W: Rems = Rems(56.);

/// How much of a command's output the card carries. The end is where a command
/// says how it went; the whole thing is one click away in the chip itself.
const OUTPUT_TAIL_LINES: usize = 200;

/// Puts a chip's picture on the clipboard. Data the agent sent is already in
/// hand; a file is read first, off the foreground, since the point of copying a
/// screenshot is not to stall the window while it happens.
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

/// The format a file's name claims, for the formats an image chip can show.
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

/// How many unchanged lines to keep around each hunk in a hover card's diff.
/// The same two the multibuffer uses everywhere else.
const DIFF_CONTEXT_LINES: u32 = 2;

/// A diff stat, when there is anything to say: a chip with `+0 -0` on it says
/// only that the file is in a list it is already in.
fn diff_stats(added: u32, deleted: u32) -> Option<action_log::DiffStats> {
    (added > 0 || deleted > 0).then_some(action_log::DiffStats {
        lines_added: added,
        lines_removed: deleted,
    })
}

/// A read-only diff editor for a hover card: the same stripped-down editor a
/// declared edit is shown in, over a file's own buffer and the repository's
/// diff for it.
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
        editor.set_diff_hunk_delegate(Some(Arc::new(RestoreOnlyUnstagedDiffHunkDelegate)), cx);
        editor.set_text_style_refinement(diff_editor_text_style_refinement(cx));
        editor
    })
}

/// The shared container for chip hover cards: styled like a popover (an
/// elevated, bordered surface) rather than a plain tooltip bubble.
pub(super) struct ChipHoverCard {
    pub(super) build: std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>,
}

impl Render for ChipHoverCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme::theme_settings(cx).ui_font(cx).clone();
        // The padding is part of the card, not a gap: with a hoverable
        // tooltip the pointer crosses it on the way in, and a real gap would
        // dismiss the card before it arrives.
        div().pl_2().pt_2p5().child(
            v_flex()
                .font(ui_font)
                .text_ui(cx)
                .text_color(cx.theme().colors().text)
                // One surface, said once: this was the same background,
                // border, radius and shadow that `elevation_2` carries.
                .elevation_2(cx)
                .p_2p5()
                .child((self.build)(window, cx)),
        )
    }
}

/// One phrasing for every search, whatever performed it: `Searched "query"`.
///
/// Agents word this differently, one saying `Search for x`, another `grep x`,
/// another just the pattern, and the shell's own searches arrive as a bare
/// query. A chip that reads differently for the same act is what this avoids;
/// the magnifying glass says it was a search, and the label says for what.
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
        // The prefix is matched against the lowercase copy, so the tail is cut
        // from the original by length rather than by the match itself.
        .map(|rest| &text[text.len() - rest.len()..])
        .unwrap_or(text)
        .trim()
        .trim_matches('"');
    (!query.is_empty()).then(|| format!("Searched {query:?}").into())
}

/// A scrollable region inside a hover card.
///
/// Tooltips are laid out with min-content available space, and a scrolling
/// element's automatic minimum size is zero rather than content based, so a
/// region like this collapses to a sliver unless it carries a width of its
/// own. That is the only reason the width is fixed here. `occlude` keeps the
/// wheel from reaching the transcript underneath, which would otherwise
/// scroll the thread out from under the pointer.
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

/// Wraps a card body in the shared popover-style container, in the shape the
/// `.tooltip(...)` API expects.
pub(super) fn chip_hover_card(
    build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let build: std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyElement> = std::rc::Rc::new(build);
    move |_window, cx| {
        let build = build.clone();
        cx.new(|_| ChipHoverCard { build }).into()
    }
}

/// A chip hover card whose body loads asynchronously: the card observes an
/// entity and re-renders when it notifies, so a body that returns `None` while
/// something loads gets a real chance to fill in once the load completes. A
/// plain `chip_hover_card` re-renders only when the card itself notifies, so
/// an `entity.notify()` on the thread cannot reach it.
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

/// A chip built from several clipped commands is not a shell line, so parsing
/// the whole label highlights neither command correctly. Keeping the pieces
/// lets each one be highlighted on its own, with the separators and any prose
/// left plain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CommandChipLabel {
    pub(super) text: String,
    /// Byte ranges of `text` that are commands.
    pub(super) commands: Vec<Range<usize>>,
}

impl CommandChipLabel {
    /// Joins a chain's acts where only text will do, as in a tooltip. The
    /// chip itself draws a rule instead; a bar character reads as one of the
    /// pipes in `a|b|c`, which is what half these labels contain.
    pub(super) const SEPARATOR: &'static str = " · ";

    /// A label that is a description rather than a command.
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

    /// Highlight runs for the whole label: each command parsed by itself,
    /// everything between them left in the base style.
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

/// The glyph on a command chip, or on one piece of a chained one. A program
/// that belongs to a language wears that language's own icon, which is how a
/// wall of chips reads at a glance: the Rust one is a build, the Python one is
/// a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ChipGlyph {
    Icon(IconName),
    /// An icon-theme file type, as named by [`acp_thread::program_language`].
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
            // A language icon is the real logo, so it keeps its own colors.
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

/// One act of a chained command line, as it appears on a collapsed chip: what
/// it was for, and what it was called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandChipPiece {
    pub(super) glyph: ChipGlyph,
    pub(super) label: CommandChipLabel,
    /// Whether this act ran in the line's devshell, on a line where only some
    /// of them did. The badge names the devshell once; this marks which acts it
    /// covered, which is the whole reason a mixed line is worth reading.
    pub(super) in_environment: bool,
}

/// The devshell a command line's work ran in, and whether it covered all of it.
pub(super) struct CommandEnvironment {
    pub(super) name: String,
    pub(super) partial: bool,
}

/// A collapsed command chip's contents. A line that did one thing is one
/// label; a chain is a piece per act, each with its own glyph, so the shape of
/// the line survives being shrunk to a chip.
pub(super) enum CollapsedCommand {
    Label(CommandChipLabel),
    Pieces(Vec<CommandChipPiece>),
}

pub(super) fn highlight_code_runs(
    code: &str,
    language: Option<&Arc<Language>>,
    code_text_style: TextStyle,
    markdown_style: &MarkdownStyle,
) -> Vec<TextRun> {
    if code.is_empty() {
        return Vec::new();
    }

    let Some(language) = language else {
        return vec![code_text_style.to_run(code.len())];
    };

    let mut runs = Vec::new();
    let mut offset = 0;
    for (range, highlight_id) in language.highlight_text(&Rope::from(code), 0..code.len()) {
        if range.start > offset {
            runs.push(code_text_style.to_run(range.start - offset));
        }

        let mut run_style = code_text_style.clone();
        if let Some(highlight) = markdown_style.syntax.get(highlight_id).cloned() {
            run_style = run_style.highlight(highlight);
        }
        runs.push(run_style.to_run(range.len()));
        offset = range.end;
    }

    if offset < code.len() {
        runs.push(code_text_style.to_run(code.len() - offset));
    }

    runs
}

/// The chip surface of a thread view. These are methods on `ThreadView`
/// rather than free functions because they read its expansion state, its
/// caches, and its workspace; keeping them in a child module leaves the
/// call sites in `thread_view.rs` untouched while the bulk lives here.
impl ThreadView {
    /// The machine a terminal call ran on, when it ran somewhere else.
    pub(super) fn command_host_for(&self, tool_call: &ToolCall, cx: &App) -> Option<String> {
        if tool_call.terminals().next().is_none() {
            return None;
        }
        self.chip_cache.command(tool_call, cx).host.clone()
    }

    /// The devshell a command ran in, when one wrapped it. The wrapper is
    /// stripped from the label (`nix develop … --command ruff check` is a ruff
    /// run), so this is the only thing that still says where it ran. A line
    /// only half inside the devshell says so rather than claiming all of it.
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
        match tool_call.kind {
            acp::ToolKind::Read => return Some(acp_thread::CommandClass::Read),
            acp::ToolKind::Search => return Some(acp_thread::CommandClass::Search),
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

    /// Chip grouping without a cache, for callers that have no view (tests).
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

        // Consecutive reads/searches (waits between them are invisible and do
        // not break the stretch) fold into one summary chip; a lone one keeps
        // its own chip. A stretch ends at any other visible chip.
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
                        // Waiting, prodding a process with no keystrokes, and
                        // looking up its own tools are not actions worth
                        // reporting: agents emit long stretches of them. They
                        // stay in the run so the chips around them keep one
                        // group.
                        continue;
                    }
                    // A picture is never noise: an image read keeps its own
                    // chip rather than folding into "read 4 files", where the
                    // image would have nowhere to appear.
                    if Self::tool_call_has_image(tool_call, cx) {
                        flush(&mut chips, &mut pending_low_value);
                        chips.push(ActionChip::ToolCall { entry_ix });
                        continue;
                    }
                    // A command that changed files is never quiet noise, however
                    // much it reads like a look-around: `sed -n` folds away,
                    // `sed -i` does not.
                    if Self::low_value_class(tool_call, cache, cx).is_some()
                        && Self::command_changed_files(tool_call, cx).is_empty()
                    {
                        pending_low_value.push(entry_ix);
                        continue;
                    }
                    flush(&mut chips, &mut pending_low_value);

                    // An edit is one chip per file it touched, each naming
                    // the file and carrying its own diff stat. This is also
                    // what keeps a generic title ("editing files") off the
                    // chip: the file names come from the call's own files,
                    // never from its label.
                    let files = Self::edited_files(tool_call, cx);
                    let is_edit = matches!(tool_call.kind, acp::ToolKind::Edit);
                    let failed = matches!(
                        tool_call.status,
                        ToolCallStatus::Rejected
                            | ToolCallStatus::Canceled
                            | ToolCallStatus::Failed
                    );
                    if files.is_empty() {
                        // An edit call with no files yet says nothing worth a
                        // chip ("editing files"); its per-file chips appear as
                        // the diffs arrive. Failures stay visible.
                        if !is_edit || failed {
                            chips.push(ActionChip::ToolCall { entry_ix });
                        }
                    } else {
                        for file_ix in 0..files.len() {
                            chips.push(ActionChip::EditFile { entry_ix, file_ix });
                        }
                    }

                    // What a command changed is known only after it ran, and
                    // only by the repository. The command keeps its own chip;
                    // these sit beside it, until there are so many that naming
                    // them says less than counting them.
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

    /// Whether a chip renders expanded. Chips expand only by being clicked; a
    /// thought never expands (its full text is a hover card), so it has no
    /// expansion state at all.
    pub(super) fn action_chip_expanded(&self, id: &ActionChipId, cx: &App) -> bool {
        if self.expanded_action_chip.as_ref() == Some(id) {
            return true;
        }
        // A call expanded through the tool call's own state (an auto-expanded
        // failure, or a caller reaching for it directly) shows its body too.
        match id {
            ActionChipId::ToolCall(tool_call_id) => self
                .entry_view_state
                .read(cx)
                .is_tool_call_expanded(tool_call_id),
            _ => false,
        }
    }

    /// Whether a tool call's chip renders expanded. A chip about an image
    /// starts expanded (the picture is what the chip is about) and stays so
    /// until collapsed; every other chip expands only by being clicked.
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

    /// Tells the list that an entry's height has changed. The list measures an
    /// entry once and remembers it, and a chip's body is drawn by the first
    /// entry of its run, so an expansion that nobody reports paints over
    /// whatever is below it. An image, being the tallest thing a chip opens, is
    /// where this shows worst.
    pub(super) fn remeasure_chip(&mut self, id: &ActionChipId, cx: &App) {
        let Some(entry_ix) = self.entry_ix_for_chip(id, cx) else {
            return;
        };
        let item = self.drawn_item_for_entry(entry_ix, cx);
        self.list_state.remeasure_items(item..item + 1);
    }

    /// The list item that draws an entry. A run of actions is drawn by its
    /// first entry as one block and the rest draw nothing, so an entry whose
    /// content grew changed the height of the item that draws it, not of its
    /// own. Remeasuring the wrong one leaves the block overlapping whatever is
    /// below it.
    pub(crate) fn drawn_item_for_entry(&self, entry_ix: usize, cx: &App) -> usize {
        // Called between frames, when an entry has just changed: the frame's
        // memo of which entries are chips was built before that change.
        self.chip_cache.frame_chip_entries.borrow_mut().clear();
        self.action_run_bounds(entry_ix, cx)
            .map_or(entry_ix, |(run_start, _)| run_start)
    }

    /// Which entry a chip belongs to, by the tool call it stands for.
    fn entry_ix_for_chip(&self, id: &ActionChipId, cx: &App) -> Option<usize> {
        let wanted = match id {
            ActionChipId::ToolCall(tool_call_id) | ActionChipId::Collapsed(tool_call_id) => {
                tool_call_id
            }
            ActionChipId::EditFile { tool_call_id, .. } => tool_call_id,
        };
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Single expand across the whole group: collapse whichever chip the user
        // had open, then open this one.
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
            // Drive the tool call's own expansion state so its expanded body
            // (diff, output, content) shows directly.
            ActionChipId::ToolCall(tool_call_id) => {
                self.entry_view_state.update(cx, |state, _cx| {
                    state.set_tool_call_expanded(tool_call_id, true)
                });
                let tool_call_id = tool_call_id.clone();
                self.prepare_command_scripts(&tool_call_id, cx);
            }
            // A per-file chip shows only its own file's diff, so it does not
            // touch the tool call's overall expansion state. The collapsed
            // summary only reveals its constituent chips.
            ActionChipId::EditFile { .. } | ActionChipId::Collapsed(_) => {}
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
            ActionChipId::EditFile { .. } | ActionChipId::Collapsed(_) => {}
        }
    }

    /// The added/removed line stats for one edited file of a tool call,
    /// identified by its distinct-file index, matched to its diff by path.
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

    /// The diff entity of a tool call whose file matches `location`, matched by
    /// file name (the diff's path may be absolute where the location's is not).
    /// The diff for one of an edit call's files, matched by file name whether
    /// the file came from a reported location or from the diff itself.
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

    /// Sum of the added/removed line counts across an edit tool call's diffs,
    /// for the chip's quiet +/- stat.
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

    /// Strips the leading verb from an edit tool call's headline: the pencil
    /// icon already says it is an edit, so the chip shows just the path.
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

    /// A thought's chip label: what the agent was thinking, not the word
    /// "Thinking". The first sentence (or line) of the thought, with markdown
    /// decoration stripped and truncated to what fits a chip. Empty thoughts
    /// fall back to a generic label.
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

        // Cut at the first sentence end, so a chip reads as one thought rather
        // than a fragment of a paragraph.
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

    /// The hover card for a terminal command chip: the full command rendered
    /// like a shell prompt (monospace, bash-highlighted through the same path
    /// the chip label uses), plus the working directory and, once the command
    /// has finished, its exit status and how long it took. `None` for tool calls
    /// that run no terminal.
    /// A read's hover card: the file's name and full path, plus the code the
    /// agent actually read, highlighted. The chip itself only has room for the
    /// file name.
    /// An expanded search chip: the files it matched, one clickable row each.
    ///
    /// The tool's own output is prose the agent wrote for itself, and reading
    /// it to find out which files matched is work the chip can do instead.
    /// `None` for a search that matched nothing the thread recorded, which
    /// falls back to showing that output.
    pub(super) fn render_search_matches(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(tool_call.kind, acp::ToolKind::Search) || tool_call.locations.is_empty() {
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

    /// A picture's hover card is the picture. A collapsed image chip says only
    /// that an image was read, which is the one kind of chip whose contents
    /// cannot be described in a line.
    pub(super) fn image_hover_card(
        &self,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let image = self.tool_call_image(tool_call, cx)?;
        let dimensions = image.dimensions();
        Some(chip_hover_card(move |_window, _cx| {
            let picture = match image.clone() {
                ChipImage::File(path) => img(path),
                ChipImage::Data { image, .. } => img(image),
            };
            // A definite box for the same reason the inline one has one: an
            // image contributes no size until it has loaded, and a card that
            // resizes under the pointer is a card that gets away. The card is
            // wider than the chip, so its height is computed at its own width.
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
        location: &acp::ToolCallLocation,
        _cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let name: SharedString = location
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| location.path.to_string_lossy().into_owned())
            .into();
        let path: SharedString = location.path.to_string_lossy().into_owned().into();

        // Only the handle is taken here. Every chip on screen builds its card's
        // closure on every frame, and a card that is not hovered is never
        // called, so reading the content out belongs inside.
        let content = tool_call.content.iter().find_map(|content| match content {
            acp_thread::ToolCallContent::ContentBlock(acp_thread::ContentBlock::Markdown {
                markdown,
            }) => Some(markdown.clone()),
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

    /// What a search chip searched, and where: the call's own label, which
    /// the chip itself no longer shows.
    pub(super) fn search_hover_card(
        &self,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> Option<impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<>> {
        let query = search_chip_label(&tool_call.label.read(cx).source())?;
        // What the search found, which is the part a chip has no room for.
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
        // Handles only: this runs for every command chip on screen on every
        // frame, while the card itself is built only for the one under the
        // pointer. Reading the command out, and highlighting it, belong there.
        let terminal = tool_call.terminals().next()?.clone();
        let label = tool_call.label.clone();

        Some(chip_hover_card(move |window, cx| {
            let terminal = terminal.read(cx);
            let command: SharedString = strip_command_fences(&label.read(cx).source())
                .trim()
                .to_string()
                .into();
            let language = label.read(cx).first_code_block_language();
            // Only a command tall enough to need scrolling pays the fixed width
            // a scroll region costs (see `card_scroll_region`); a short one
            // keeps sizing to its own text.
            let scrolls = command.lines().count() > 12 || command.len() > 600;

            // (text, is_error) for the meta line under the command.
            let mut meta: Vec<(SharedString, bool)> = Vec::new();
            if let Some(working_dir) = terminal.working_dir() {
                meta.push((working_dir.display().to_string().into(), false));
            }
            if let Some(output) = terminal.output() {
                let failed = output.exit_status.is_some_and(|status| !status.success());
                let status: SharedString = match output.exit_status.map(|status| status.code()) {
                    None => "exited".into(),
                    Some(None) => "terminated".into(),
                    Some(Some(code)) => format!("exited {code}").into(),
                };
                meta.push((status, failed));
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

            // What it printed. A card is a window onto a long run, so it
            // carries the end of the output, which is where a command says how
            // it went; the whole thing is in the chip's own expansion.
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
                                card_scroll_region(
                                    "command-hover-output",
                                    CARD_WIDTH,
                                    rems(18.),
                                )
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

    /// Renders a run of agent actions as chips that wrap: a chip is only as wide
    /// as its content needs, capped at a quarter of the row so a long label
    /// truncates instead of crowding out its neighbours. Clicking a chip expands
    /// exactly one at a time below the chips; the expanded body is that tool
    /// call's own per-kind rendering (terminal command + output, edit diff with
    /// go-to-file, read/other output), or the thought itself for a thinking chip.
    pub(super) fn render_action_group(
        &self,
        active_session_id: &acp::SessionId,
        run_start: usize,
        run_len: usize,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let entries = self.thread.read(cx).entries();

        // An expanded chip's body renders right below the row holding the
        // chip; chips after it start a fresh row underneath. There is no
        // single body slot at the end of the group.
        let mut segments: Vec<AnyElement> = Vec::new();
        let mut row: Vec<AnyElement> = Vec::new();
        let mut any_chip = false;
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

                    // An expanded chip gets a row of its own: it grows to show
                    // the full command, with the output directly beneath.
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
                        // A chip about an image shows the picture itself, not a
                        // description of where it came from.
                        if let Some(image) = self.tool_call_image(tool_call, cx) {
                            segments.push(self.render_inline_image(entry_ix, 0, image, cx));
                        } else if let Some(matches) =
                            self.render_search_matches(entry_ix, tool_call, cx)
                        {
                            // A search's results are the files it found, which
                            // its own output states in whatever shape the agent
                            // chose. The files are the answer, and each one
                            // opens.
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
                            // A command that wrote a picture prints where it
                            // put it and nothing else. The picture is the
                            // result; showing it beats making the reader leave
                            // the app to look.
                            for (image_ix, path) in
                                Self::command_output_images(tool_call, cx).into_iter().enumerate()
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
                    let Some(file) = Self::edited_files(tool_call, cx).get(file_ix).cloned() else {
                        continue;
                    };
                    // Edit chips do not expand inline: hover shows the diff, a
                    // click opens the real file with the change revealed. The
                    // chip reads as selected while that file's diff is open.
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

        // A run of nothing but hidden calls (waits) draws nothing, rather than
        // an empty row that still takes vertical space.
        if !any_chip {
            return Empty.into_any_element();
        }

        v_flex().my_0p5().gap_1().children(segments).into_any()
    }

    /// The reads-and-searches summary chip. Hover lists what was read and
    /// searched; a click reveals (or refolds) the individual chips.
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
        let mut items: Vec<SharedString> = Vec::new();
        let mut first_id: Option<acp::ToolCallId> = None;
        let mut member_ids: Vec<acp::ToolCallId> = Vec::new();
        let mut any_running = false;
        for &entry_ix in entry_ixs {
            let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.get(entry_ix) else {
                continue;
            };
            first_id.get_or_insert_with(|| tool_call.id.clone());
            member_ids.push(tool_call.id.clone());
            any_running |= matches!(
                tool_call.status,
                ToolCallStatus::InProgress | ToolCallStatus::Pending
            );
            let class = Self::low_value_class(tool_call, Some(&self.chip_cache), cx)
                .unwrap_or(acp_thread::CommandClass::Read);
            match class {
                acp_thread::CommandClass::Search => searches += 1,
                acp_thread::CommandClass::ReadDiff => diffs += 1,
                acp_thread::CommandClass::GitInfo => git_checks += 1,
                _ => reads += 1,
            }
            items.push(self.low_value_item_label(tool_call, cx));
        }
        let Some(first_id) = first_id else {
            return Empty.into_any_element();
        };

        // Reading changes is its own kind of looking around, so a run of
        // `git diff`s says so rather than counting as files read.
        let mut parts: Vec<String> = Vec::new();
        if reads > 0 {
            parts.push(format!(
                "Read {reads} file{}",
                if reads == 1 { "" } else { "s" }
            ));
        }
        if searches > 0 {
            parts.push(format!(
                "searched {searches} place{}",
                if searches == 1 { "" } else { "s" }
            ));
        }
        if diffs > 0 {
            parts.push(format!(
                "read {diffs} diff{}",
                if diffs == 1 { "" } else { "s" }
            ));
        }
        if git_checks > 0 {
            parts.push(format!(
                "checked git {git_checks} time{}",
                if git_checks == 1 { "" } else { "s" }
            ));
        }
        let mut label = parts.join(", ");
        if label.is_empty() {
            label = "Looked around".to_string();
        } else if reads == 0 {
            // The first clause leads the label, so it is capitalized.
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
        let member_ids: Vec<acp::ToolCallId> = entry_ixs
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

    /// The unfolded summary's contents: one quiet row per read or search.
    /// A list rather than a wrap of chips, which was hard to scan.
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
                let icon = match Self::low_value_class(tool_call, Some(&self.chip_cache), cx) {
                    Some(acp_thread::CommandClass::Search) => IconName::MagnifyingGlass,
                    Some(acp_thread::CommandClass::ReadDiff) => IconName::Diff,
                    Some(acp_thread::CommandClass::GitInfo) => IconName::GitBranch,
                    _ => IconName::FileCode,
                };
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
        first_id: &acp::ToolCallId,
        member_ids: &[acp::ToolCallId],
    ) -> bool {
        match &self.expanded_action_chip {
            Some(ActionChipId::Collapsed(id)) => id == first_id,
            Some(ActionChipId::ToolCall(id)) => member_ids.contains(id),
            _ => false,
        }
    }

    /// Clicking the summary chip: folded -> unfold; unfolded in any way
    /// (itself expanded, or a member chip expanded) -> fold everything.
    pub(super) fn toggle_collapsed_chip(
        &mut self,
        first_id: acp::ToolCallId,
        member_ids: &[acp::ToolCallId],
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

    /// One line of the summary chip's hover card: what a read or search was
    /// about.
    pub(super) fn low_value_item_label(&self, tool_call: &ToolCall, cx: &App) -> SharedString {
        if let Some(location) = tool_call.locations.first()
            && matches!(tool_call.kind, acp::ToolKind::Read)
        {
            return location.path.to_string_lossy().into_owned().into();
        }
        if tool_call.terminals().next().is_some() {
            // The parser already knows what the line was for; say that rather
            // than re-summarizing the text.
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
                        // Contents from a revision are not what is on disk, so
                        // the line says where they came from.
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

    /// One tool call's chip.
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
        // What the command reported about itself: counts, and where the first
        // problem is so the chip can go there.
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

        // Deleting, moving, and discarding are the chips worth catching in a
        // wall of them.
        let destructive = has_terminals && self.chip_cache.command(tool_call, cx).destructive;
        let is_edit =
            matches!(tool_call.kind, acp::ToolKind::Edit) || tool_call.diffs().next().is_some();
        // A read is about a file: it says the file's name, and the line range
        // it happened to read is not worth a chip's width.
        let read_file = (matches!(tool_call.kind, acp::ToolKind::Read)
            && tool_call.locations.len() == 1)
            .then(|| tool_call.locations.first())
            .flatten();
        // A chip about an image expands to show it inline instead of opening
        // anything.
        let is_image = self.tool_call_image(tool_call, cx).is_some();
        // A search chip just says so; what was searched, and where, is the
        // hover card's job.
        let is_search = matches!(tool_call.kind, acp::ToolKind::Search) && !has_terminals;
        // Set for command chips, whose label is highlighted piece by piece.
        let mut command_label: Option<CommandChipLabel> = None;
        // Set for a collapsed chain, which reads as a glyph and a name per act.
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
            // Expanded: the complete command, wrapped. Collapsed: what the line
            // did, either as one label or as a glyph and a name per act, so a
            // chain reads as a chain without becoming a chip per segment.
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
                    // The tooltip and any text-only reader still see the line
                    // as one string.
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

        // Commands keep their bash highlighting on the chip, reusing the
        // language the label markdown (a bash-tagged fenced code block)
        // already resolved.
        let label_element = if let Some(pieces) = command_pieces.as_ref() {
            let markdown_style = self.chip_cache.style(window, cx);
            let mut command_text_style = markdown_style.base_text_style.clone();
            command_text_style.font_size = rems_from_px(12_f32).into();
            command_text_style.color = cx.theme().colors().text_muted;
            let code_language = tool_call.label.read(cx).first_code_block_language();
            h_flex()
                .flex_1()
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
                        // A drawn rule rather than a bar character: a
                        // label full of `a|b|c` is exactly what a search
                        // query looks like, and the eye cannot tell which
                        // pipe belongs to the shell.
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
                        // On a line only half inside a devshell, the acts that
                        // were get the nix mark; the badge above already named
                        // it, so the mark carries no text of its own.
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
            // Each command in the label is parsed on its own: a label built
            // from clipped commands and separators is not a shell line, and
            // highlighting it as one confuses every piece of it.
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
                // flex_1 so the label receives a definite width once the chip
                // clamps against its cap: truncation only engages under a
                // definite measure (and only together with a line clamp).
                .flex_1()
                .min_w_0()
                .map(|this| {
                    if is_expanded {
                        // The full command, wrapped over as many lines as it
                        // takes.
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
            // Paths truncate from the left so the file name stays visible.
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
        let running = matches!(
            tool_call.status,
            ToolCallStatus::InProgress | ToolCallStatus::Pending
        );
        let failed = matches!(
            tool_call.status,
            ToolCallStatus::Rejected | ToolCallStatus::Canceled | ToolCallStatus::Failed
        ) || terminal_output
            .is_some_and(|output| output.exit_status.is_some_and(|status| !status.success()));

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
        } else if failed {
            Some(
                Icon::new(IconName::Close)
                    .size(IconSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            )
        } else if command_pieces.is_some() {
            // Every act of a chain wears its own glyph; one more in front of
            // them would only name the first.
            None
        } else if let Some(icon_path) = Self::tool_call_file_icon(tool_call, cx) {
            // A chip about one file reads like the project panel: the
            // file's own type icon, not a generic verb glyph.
            Some(
                Icon::from_path(icon_path)
                    .size(IconSize::Small)
                    .color(icon_color)
                    .into_any_element(),
            )
        } else if has_terminals {
            // A command's glyph says what the line was for, not just that a
            // terminal was involved: a lone `cargo test` wears Rust's icon.
            let facts = self.chip_cache.command(tool_call, cx);
            let mut acts = facts
                .parsed
                .segments
                .iter()
                .filter(|segment| !segment.kind.is_noop());
            let glyph = match (acts.next(), acts.next(), destructive) {
                (Some(only), None, false) => ChipGlyph::for_segment(only),
                _ => ChipGlyph::Icon(match facts.class {
                    _ if destructive => IconName::Trash,
                    acp_thread::CommandClass::Search => IconName::MagnifyingGlass,
                    acp_thread::CommandClass::Read => IconName::FileCode,
                    acp_thread::CommandClass::ReadDiff => IconName::Diff,
                    acp_thread::CommandClass::GitInfo => IconName::GitBranch,
                    acp_thread::CommandClass::Other => IconName::ToolTerminal,
                }),
            };
            Some(glyph.element(icon_color, cx))
        } else {
            Some(
                Icon::new(Self::tool_kind_icon(tool_call.kind))
                    .size(IconSize::Small)
                    .color(icon_color)
                    .into_any_element(),
            )
        };

        let edit_stats = is_edit
            .then(|| self.chip_edit_stats(tool_call, cx))
            .flatten();

        // A command that ran somewhere else says so, and says where on hover.
        let host = has_terminals
            .then(|| self.command_host_for(tool_call, cx))
            .flatten();
        let environment = has_terminals
            .then(|| self.command_environment_for(tool_call, cx))
            .flatten();
        let full_command =
            has_terminals.then(|| self.chip_cache.command(tool_call, cx).command.clone());
        // A collapsed chip that is still running says what it is up to; the
        // expanded one shows the output itself, so it needs no excerpt.
        let running_tail = (running && has_terminals && !is_expanded)
            .then(|| self.chip_cache.tail(tool_call, cx))
            .flatten();

        let chip_group = SharedString::from(format!("action-chip-{entry_ix}"));
        let chip = self
            .action_chip_base(("action-chip", entry_ix), is_expanded, cx)
            .group(chip_group.clone())
            // A chain names every act it performed, so the chip may need the
            // whole row for them rather than the three quarters a one-label
            // chip is capped at.
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
                // Image reads expand to show the image inline; other reads open
                // the file; everything else toggles its own expansion.
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
            // Where it ran comes before what ran: on another machine, that is
            // the first thing to know about the command.
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
            // The devshell it ran in, for a line whose wrapper was stripped
            // out of the label.
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
            // What it is doing right now, while it is still doing it: a long
            // test run and a hung one are otherwise the same pulsing chip. The
            // box is a fixed width and one line whatever the output does, so a
            // command printing at speed cannot reflow the chip grid under the
            // list's measured heights.
            .when_some(running_tail, |this, tail| {
                // Where the line states a real fraction of the work — pytest's
                // trailing `[ 50%]` — say so as a fraction. Everything else,
                // including the tools that print counts with no denominator,
                // keeps the line itself.
                let progress = acp_thread::progress_fraction(&tail);
                this.child(
                    div()
                        .flex_none()
                        .w(rems(9_f32))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .line_clamp(1)
                        .text_ellipsis()
                        .map(|this| match progress {
                            Some(fraction) => this.child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        div().flex_1().child(
                                            ui::ProgressBar::new(
                                                ("command-progress", entry_ix),
                                                fraction,
                                                1.0,
                                                cx,
                                            )
                                            .fg_color(cx.theme().colors().text_accent),
                                        ),
                                    )
                                    .child(
                                        Label::new(format!("{}%", (fraction * 100.0).round()))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .buffer_font(cx),
                                    ),
                            ),
                            None => this.child(
                                Label::new(tail)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx),
                            ),
                        }),
                )
            })
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
                // A command's hover card reads like a shell prompt: the
                // full command, bash-highlighted, plus where it ran and how
                // it finished. These cards can be entered and scrolled, the
                // way the editor's own hover popovers behave; a chip with
                // nothing but a headline keeps the plain tooltip.
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

        // Anything still executing pulses in place. That highlight is how a
        // running call reads as running: it stays in the transcript rather than
        // moving to the active area.
        if running && !is_expanded {
            // Unmistakably alive: an accent border plus an accent-tinted
            // pulse, not just a faint grey shimmer.
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
        }
    }

    /// A command chip's collapsed label: what the line did, plus what it
    /// concluded. A chained line is described by its acts, not its text, so a
    /// wall of `sed`s and `rg`s reads as one look-around. Only when there is
    /// nothing to summarize does it fall back to the commands themselves.
    pub(super) fn collapsed_command(
        facts: &CommandFacts,
        outcome: Option<&str>,
    ) -> CollapsedCommand {
        const WIDTH: usize = 60;
        // Enough pieces to see the shape of the line, before they become stubs.
        // A piece is named, not quoted: `cargo test`, not `cargo test -p x…`.
        const PIECE_WIDTH: usize = 22;

        let command = facts.command.as_str();
        let parsed = &facts.parsed;
        if let Some(summary) = facts.summary.clone() {
            // A line that is one real command is summarized by quoting it, so
            // that much is shell. "Read 4 files" and its like are prose.
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
            // What it was, then what it concluded: "pnpm lint · 3 errors".
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
                // Every act, not the first few and a count of the rest: a name
                // like "cargo fmt" costs little enough that a line's whole
                // shape fits, and "+2" told the reader nothing about what it
                // was hiding. A chain long enough to outrun the chip's width
                // is clipped by it, which at least clips the least recent.
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
                            in_environment: partial_environment
                                && segment.environment.is_some(),
                        })
                        .collect(),
                )
            }
        }
    }

    /// The shared shell of every chip: one uniform height and border, sized to
    /// its own content. There is no grid: chips wrap at their natural width, and
    /// only a very long label truncates against a generous cap.
    pub(super) fn action_chip_base(
        &self,
        id: impl Into<ElementId>,
        is_expanded: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .min_w_0()
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

    /// The files a tool call's commands changed, as the repository saw them.
    /// Empty until the command has exited and the status has settled.
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

    /// Pictures a tool call's commands wrote, in the order their output named
    /// them. Empty until a command has exited and the disk has vouched for the
    /// paths it printed.
    pub(super) fn command_output_images(
        tool_call: &ToolCall,
        cx: &App,
    ) -> Vec<std::path::PathBuf> {
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

    /// The diff editor for a file a command changed, once there is one. The
    /// first call starts opening the buffer and the repository's diff for it
    /// and returns nothing; the card asks again on its next frame.
    fn command_file_diff_editor(
        &self,
        path: &project::ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        if let Some(state) = self.command_file_diffs.borrow().get(path) {
            return match state {
                CommandFileDiff::Ready(editor) => Some(editor.clone()),
                CommandFileDiff::Loading { .. } => None,
            };
        }
        let project = self.project.upgrade()?;
        let task = cx.spawn_in(window, {
            let path = path.clone();
            async move |this, cx| {
                let opened = async {
                    let buffer = project
                        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                        .await?;
                    let diff = project
                        .update(cx, |project, cx| {
                            project.git_store().update(cx, |git_store, cx| {
                                git_store.open_uncommitted_diff(buffer.clone(), cx)
                            })
                        })
                        .await?;
                    let editor = cx.update(|window, cx| {
                        // Excerpts around the hunks, not the whole file: a
                        // declared edit's chip shows the change, and a change
                        // nobody declared should read the same way rather than
                        // making the reader find it.
                        let snapshot = buffer.read(cx).snapshot();
                        let ranges: Vec<std::ops::Range<language::Point>> = diff
                            .read(cx)
                            .snapshot(cx)
                            .hunks(&snapshot)
                            .map(|hunk| text::OffsetRangeExt::to_point(&hunk.buffer_range, &snapshot))
                            .collect();
                        let multibuffer = cx.new(|cx| {
                            let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
                            multibuffer.set_excerpts_for_path(
                                multi_buffer::PathKey::for_buffer(&buffer, cx),
                                buffer,
                                ranges,
                                DIFF_CONTEXT_LINES,
                                cx,
                            );
                            multibuffer.add_diff(diff, cx);
                            multibuffer
                        });
                        command_file_diff_editor(multibuffer, window, cx)
                    })?;
                    anyhow::Ok(editor)
                }
                .await;

                this.update(cx, |this, cx| {
                    match opened {
                        Ok(editor) => {
                            this.command_file_diffs
                                .borrow_mut()
                                .insert(path, CommandFileDiff::Ready(editor));
                        }
                        // A file the project cannot open (deleted by the
                        // command, or outside every worktree) has no diff to
                        // show. Forgetting it lets a later hover try again.
                        Err(_) => {
                            this.command_file_diffs.borrow_mut().remove(&path);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        });
        self.command_file_diffs
            .borrow_mut()
            .insert(path.clone(), CommandFileDiff::Loading { _task: task });
        None
    }

    /// The card behind a command-changed file: which file, how much of it
    /// moved, and the change itself. An edit nobody declared is still an edit,
    /// and reading it should not mean opening a tab.
    ///
    /// The diff shown is the file's uncommitted diff against HEAD, not the
    /// slice of it this command wrote. When the file was clean when the command
    /// started, HEAD is the pre-command content, so the uncommitted diff is the
    /// command's own change; when it was already dirty, the card carries the
    /// wider diff and the label says so. Either way the card is honest about
    /// what it is showing.
    pub(super) fn command_file_hover_card(
        &self,
        file: &acp_thread::ChangedFile,
        cx: &Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + use<> {
        let path = file.path.clone();
        let full: SharedString = path.path.as_unix_str().to_string().into();
        let stats = diff_stats(file.added, file.deleted);
        let pre_command_dirty = file.pre_command_dirty;
        let this = cx.entity().downgrade();

        // The build closure fetches the editor by path each frame; on the first
        // frame the load is still running and it returns `None`, and the load
        // task ends by notifying the thread view. The observing variant of the
        // hover card wraps that notify around to the card itself, so the empty
        // frame is replaced the moment the editor is ready rather than only on
        // the reader's next hover.
        chip_hover_card_observing(this.clone(), move |window, cx| {
            let editor = this
                .update(cx, |this, cx| {
                    this.command_file_diff_editor(&path, window, cx)
                })
                .ok()
                .flatten();
            let heading = if pre_command_dirty {
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

    /// What a command changed, when it changed more than a row can name: the
    /// count, and the total it moved. Hovering lists the files; clicking opens
    /// the diff.
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

    /// The shape both command-changed chips share: a glyph, what it is called,
    /// how much it moved, and a click that opens the diff. Only the name and
    /// the card behind it differ, so only those are arguments.
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
                    .flex_1()
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

    /// One file a command changed. It reads like an edit chip, because to the
    /// reader it is one: the difference is only that nobody declared it, so
    /// there is no per-call diff behind it and clicking opens the file's diff
    /// against the repository.
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
            // What the command did to the file, rather than how far the file
            // has drifted from HEAD: a file it added three lines to reads as
            // +3 even when it was already dirty.
            diff_stats(file.added, file.deleted),
            file.path.clone(),
            self.command_file_hover_card(file, cx),
            cx,
        )
    }

    /// The picture an expanded image chip shows. Clicking opens it where images
    /// open; right-clicking offers the picture itself, since a screenshot in a
    /// thread is usually wanted somewhere else.
    fn render_inline_image(
        &self,
        entry_ix: usize,
        image_ix: usize,
        image: ChipImage,
        cx: &Context<Self>,
    ) -> AnyElement {
        // An image that came from a file opens as that file; one the agent sent
        // inline has nothing behind it to open.
        let file = match &image {
            ChipImage::File(path) => Some(path.clone()),
            ChipImage::Data { .. } => None,
        };
        let copyable = image.clone();
        let dimensions = image.dimensions();
        let picture = match image {
            ChipImage::File(path) => img(path),
            ChipImage::Data { image, .. } => img(image),
        };

        let body = div()
            // A command can produce more than one picture, so the entry alone
            // does not identify the element.
            .id(SharedString::from(format!(
                "chip-image-{entry_ix}-{image_ix}"
            )))
            // A definite box. An image contributes no height until it has
            // loaded, and an entry that grows after the list has measured it
            // paints over the entries below. The box is sized to the picture
            // where the picture's shape is known, so fitting it inside costs
            // it nothing.
            .w(IMAGE_CHIP_WIDTH)
            .h(image_box_height(dimensions, IMAGE_CHIP_WIDTH))
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
                        let path = file.as_ref().map(|path| path.to_string_lossy().into_owned());
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

    /// Opens a picture in Zed's own image viewer, as a tab like any other. A
    /// file inside the project opens by its project path so it shares the tab
    /// the project panel would open; one outside opens by its absolute path.
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

    /// Opens a command-changed file's diff against the repository. There is no
    /// per-call diff to show: nobody declared this edit, so the repository's
    /// own view of the file is the only one there is.
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

        let chip_id = ActionChipId::EditFile {
            tool_call_id: tool_call.id.clone(),
            file_ix,
        };
        let stats = self.edit_file_stats(tool_call, file, cx);

        let _ = chip_id;
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
        .map(|this| {
            match self.edit_hover_card(entry_ix, file, tool_call, cx) {
                // Hoverable: the diff card can be entered and scrolled, the
                // way the editor's own hover popovers behave.
                Some(card) => this.hoverable_tooltip(card),
                None => this.tooltip(Tooltip::text(name)),
            }
        })
        .into_any_element()
    }

    /// The edit chip's hover card: the file, and the diff itself.
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
                    // The card is enterable, so a long diff can be scrolled
                    // rather than merely clipped.
                    card_scroll_region("edit-hover-diff", DIFF_CARD_WIDTH, DIFF_CARD_HEIGHT)
                        .child(editor.clone()),
                )
                .into_any_element()
        }))
    }
}
