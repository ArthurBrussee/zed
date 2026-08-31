use crate::{
    DEFAULT_THREAD_TITLE, SelectPermissionGranularity,
    conversation_view::thread_search_bar::{ThreadSearchBar, ThreadSearchBarEvent},
    open_abs_path_at_point,
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
};
use agent_client_protocol::schema::v1 as acp;
use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;

use acp_thread::{
    AcpThreadEvent, Elicitation, ElicitationEntryId, ElicitationStatus, PlanEntry,
    SandboxAuthorizationDetails, SandboxFallbackAuthorizationDetails, SandboxNotAppliedReason,
    decode_path_escapes,
};
use agent::{
    SandboxStatusKey, SandboxStatusRefresh, SkillLoadingIssue, SkillLoadingIssueKind,
    SkillLoadingIssuesUpdated, ThreadSandbox, VerifiedSandboxStatus,
};
use agent_settings::UserAgentsMd;
use agent_skills::MAX_SKILL_DESCRIPTION_LEN;
use cloud_api_types::{SubmitAgentThreadFeedbackBody, SubmitAgentThreadFeedbackCommentsBody};
use editor::actions::OpenExcerpts;
use sandbox::{SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};

use crate::completion_provider::{AvailableSkill, PromptLocalCommand, pluralize};
use crate::message_editor::SharedSessionCapabilities;
use crate::ui::{
    SandboxGroup, SandboxRow, SandboxSection, SandboxStatusTooltip, TerminalSandboxWarning,
};
use crate::unicode_confusables;

use db::kvp::KeyValueStore;
use gpui::List;
use gpui::Stateful;
use gpui::TaskExt;
use heapless::Vec as ArrayVec;
use language_model::{
    FastModeConfirmation, LanguageModel, LanguageModelId, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelRegistry, Speed,
};
use notifications::status_toast::StatusToast;
use settings::{update_settings_file, update_settings_file_with_completion};
use ui::{
    ButtonLike, CalloutBorderPosition, Checkbox, SpinnerLabel, SpinnerVariant, Tab, ToggleState,
};
use util::markdown::{source_position_from_fragment, split_local_url_fragment};
use workspace::{OpenOptions, SERIALIZATION_THROTTLE_TIME};

use super::branch_diff_stats::{BranchDiffStats, DiffStatsBase};
use super::elicitation::{
    ElicitationCard, ElicitationCardHandlers, ElicitationFormState, should_render_elicitation,
};
use super::*;

const DATA_RETENTION_LEARN_MORE_URL: &str = "https://support.claude.com/en/articles/15425996-data-retention-practices-for-mythos-class-models";

/// The stand-in title shown from the moment the first message is sent until a
/// real title arrives: a short single line, not the whole message.
pub(crate) const PROVISIONAL_TITLE_LEN: usize = 48;
/// How much of the conversation the local title generation sends: the first few
/// messages, each truncated, are enough to name a thread.
const TITLE_REQUEST_MESSAGE_COUNT: usize = 4;
const TITLE_REQUEST_MESSAGE_LEN: usize = 2000;

/// The height an inline image occupies when nothing is known about its shape.
/// Big enough to read a screenshot, small enough that a picture does not push
/// the conversation off screen. Read by `render_agent_markdown` in
/// `conversation_view.rs` so an agent-authored image inside markdown holds the
/// same box as an image the chip layer draws.
pub(super) const IMAGE_CHIP_HEIGHT: Rems = Rems(20.);

/// The width an image chip's box is drawn at. A definite width is what makes
/// the height below arithmetic rather than a measurement.
pub(super) const IMAGE_CHIP_WIDTH: Rems = Rems(24.);

/// As tall as a picture is allowed to make its entry. A phone screenshot is
/// twice as tall as it is wide, and a full-page capture many times that; past
/// this the picture letterboxes rather than taking the whole transcript.
const IMAGE_CHIP_MAX_HEIGHT: Rems = Rems(32.);

/// A picture that is mostly a wide strip still needs a row to be recognisable.
const IMAGE_CHIP_MIN_HEIGHT: Rems = Rems(4.);

/// The height to reserve for a picture of known shape at a given width, so the
/// transcript measures the entry once and correctly. A `ListState` measures an
/// entry before the image has decoded and paints it at that height forever, so
/// a box that does not already fit the picture is a box the picture spills out
/// of, over the chips below.
///
/// The dimensions arrive with the image (`ContentBlock::Image`), so this is
/// arithmetic and not a measurement that has to wait for a decode. Without them
/// there is nothing to compute from and the fixed box stands.
pub(super) fn image_box_height(dimensions: Option<gpui::Size<u32>>, width: Rems) -> Rems {
    let Some(dimensions) = dimensions.filter(|size| size.width > 0 && size.height > 0) else {
        return IMAGE_CHIP_HEIGHT;
    };
    let needed = width.0 * dimensions.height as f32 / dimensions.width as f32;
    Rems(needed.clamp(IMAGE_CHIP_MIN_HEIGHT.0, IMAGE_CHIP_MAX_HEIGHT.0))
}

#[derive(Default)]
struct ThreadFeedbackState {
    feedback: Option<ThreadFeedback>,
    comments_editor: Option<Entity<Editor>>,
}

impl ThreadFeedbackState {
    pub fn submit(
        &mut self,
        thread: Entity<AcpThread>,
        feedback: ThreadFeedback,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(telemetry) = thread.read(cx).connection().telemetry() else {
            return;
        };

        let project = thread.read(cx).project().read(cx);
        let client = project.client();
        let user_store = project.user_store();
        let organization = user_store.read(cx).current_organization();

        if self.feedback == Some(feedback) {
            return;
        }

        self.feedback = Some(feedback);
        match feedback {
            ThreadFeedback::Positive => {
                self.comments_editor = None;
            }
            ThreadFeedback::Negative => {
                self.comments_editor = Some(Self::build_feedback_comments_editor(window, cx));
            }
        }
        let session_id = thread.read(cx).session_id().clone();
        let parent_session_id = thread.read(cx).parent_session_id().cloned();
        let agent_telemetry_id = thread.read(cx).connection().telemetry_id();
        let task = telemetry.thread_data(&session_id, cx);
        let rating = match feedback {
            ThreadFeedback::Positive => "positive",
            ThreadFeedback::Negative => "negative",
        };
        cx.background_spawn(async move {
            let thread = task.await?;

            client
                .cloud_client()
                .submit_agent_feedback(SubmitAgentThreadFeedbackBody {
                    organization_id: organization.map(|organization| organization.id.clone()),
                    agent: agent_telemetry_id.to_string(),
                    session_id: session_id.to_string(),
                    parent_session_id: parent_session_id.map(|id| id.to_string()),
                    rating: rating.to_string(),
                    thread,
                })
                .await?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn submit_comments(&mut self, thread: Entity<AcpThread>, cx: &mut App) {
        let Some(telemetry) = thread.read(cx).connection().telemetry() else {
            return;
        };

        let Some(comments) = self
            .comments_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx))
            .filter(|text| !text.trim().is_empty())
        else {
            return;
        };

        self.comments_editor.take();

        let project = thread.read(cx).project().read(cx);
        let client = project.client();
        let user_store = project.user_store();
        let organization = user_store.read(cx).current_organization();

        let session_id = thread.read(cx).session_id().clone();
        let agent_telemetry_id = thread.read(cx).connection().telemetry_id();
        let task = telemetry.thread_data(&session_id, cx);
        cx.background_spawn(async move {
            let thread = task.await?;

            client
                .cloud_client()
                .submit_agent_feedback_comments(SubmitAgentThreadFeedbackCommentsBody {
                    organization_id: organization.map(|organization| organization.id.clone()),
                    agent: agent_telemetry_id.to_string(),
                    session_id: session_id.to_string(),
                    comments,
                    thread,
                })
                .await?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn clear(&mut self) {
        *self = Self::default()
    }

    pub fn dismiss_comments(&mut self) {
        self.comments_editor.take();
    }

    fn build_feedback_comments_editor(window: &mut Window, cx: &mut App) -> Entity<Editor> {
        let buffer = cx.new(|cx| {
            let empty_string = String::new();
            MultiBuffer::singleton(cx.new(|cx| Buffer::local(empty_string, cx)), cx)
        });

        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                editor::EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(4),
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_placeholder_text(
                "What went wrong? Share your feedback so we can improve.",
                window,
                cx,
            );
            editor
        });

        editor.read(cx).focus_handle(cx).focus(window, cx);
        editor
    }
}

struct GeneratingSpinner {
    variant: SpinnerVariant,
}

impl GeneratingSpinner {
    fn new(variant: SpinnerVariant) -> Self {
        Self { variant }
    }
}

impl Render for GeneratingSpinner {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SpinnerLabel::with_variant(self.variant).size(LabelSize::Small)
    }
}

#[derive(IntoElement)]
struct GeneratingSpinnerElement {
    variant: SpinnerVariant,
}

impl GeneratingSpinnerElement {
    fn new(variant: SpinnerVariant) -> Self {
        Self { variant }
    }
}

impl RenderOnce for GeneratingSpinnerElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = match self.variant {
            SpinnerVariant::Dots => "generating-spinner-view",
            SpinnerVariant::Sand => "confirmation-spinner-view",
            _ => "spinner-view",
        };
        window.with_id(id, |window| {
            window.use_state(cx, |_, _| GeneratingSpinner::new(self.variant))
        })
    }
}

pub enum AcpThreadViewEvent {
    Interacted,
}

impl EventEmitter<AcpThreadViewEvent> for ThreadView {}

/// `cat -n`-style numbered code block, already stripped of its line-number
/// prefixes and ready to render. Line numbers are guaranteed to be contiguous
/// starting at `first_number`, so we only store the first number and the line
/// count rather than allocating a per-line `Vec`.
struct ParsedCatNumberedCode {
    code: String,
    first_number: u32,
    line_count: usize,
}

fn parse_cat_numbered_markdown_code_block(markdown: &str) -> Option<ParsedCatNumberedCode> {
    let (_tag, code) = parse_single_fenced_code_block(markdown)?;
    parse_cat_numbered_code(code)
}

fn parse_single_fenced_code_block(markdown: &str) -> Option<(&str, &str)> {
    let first_non_backtick = markdown.find(|character| character != '`')?;
    if first_non_backtick < 3 {
        return None;
    }

    let fence = &markdown[..first_non_backtick];
    let after_opening_fence = &markdown[first_non_backtick..];
    let tag_end = after_opening_fence.find('\n')?;
    let tag = &after_opening_fence[..tag_end];
    let after_tag = &after_opening_fence[tag_end + 1..];
    let closing_fence = format!("\n{fence}\n");
    let code = after_tag.strip_suffix(&closing_fence)?;
    Some((tag, code))
}

/// Walks `code` exactly once: for each line it validates and strips the
/// `NNN\t` prefix, then pushes the line's content into the accumulating
/// code buffer (with `\n` between lines, no trailing newline). Verifies that
/// the line numbers form a contiguous, increasing sequence.
fn parse_cat_numbered_code(code: &str) -> Option<ParsedCatNumberedCode> {
    if code.is_empty() {
        return None;
    }

    let mut output = String::with_capacity(code.len());
    let mut first_number = None;
    let mut expected_number = None;
    let mut line_count: usize = 0;
    for raw_line in code.split_inclusive('\n') {
        let line = strip_line_ending(raw_line);
        let (number, text) = parse_cat_numbered_line(line)?;
        if let Some(expected) = expected_number {
            if number != expected {
                return None;
            }
        } else {
            first_number = Some(number);
        }
        expected_number = number.checked_add(1);
        if line_count > 0 {
            output.push('\n');
        }
        output.push_str(text);
        line_count += 1;
    }

    Some(ParsedCatNumberedCode {
        code: output,
        first_number: first_number?,
        line_count,
    })
}

fn strip_line_ending(line: &str) -> &str {
    let without_lf = line.strip_suffix('\n').unwrap_or(line);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

mod chips;
use chips::*;

/// Strips the markdown code fences around a terminal command label,
/// tolerating an optional language tag on the opening fence.
fn strip_command_fences(source: &str) -> Cow<'_, str> {
    let inner = strip_fences_only(source);
    match strip_outer_quotes(inner.trim_matches(['\r'])) {
        Cow::Borrowed(text) => Cow::Borrowed(text.trim_end()),
        Cow::Owned(text) => Cow::Owned(text.trim_end().to_string()),
    }
}

fn strip_fences_only(source: &str) -> &str {
    let Some(after_fence) = source.strip_prefix("```") else {
        return source;
    };
    let Some(newline_ix) = after_fence.find('\n') else {
        return source;
    };
    let body = &after_fence[newline_ix + 1..];
    body.strip_suffix("\n```").unwrap_or(body)
}

/// Some agents send the whole command wrapped in one pair of quotes; shown
/// verbatim it highlights as a single string literal. Strip the pair only
/// when it wraps everything (the quote char appears nowhere inside).
fn strip_outer_quotes(command: &str) -> Cow<'_, str> {
    let trimmed = command.trim();
    for quote in ['\'', '"'] {
        if trimmed.len() < 2 || !trimmed.starts_with(quote) || !trimmed.ends_with(quote) {
            continue;
        }
        let inner = &trimmed[1..trimmed.len() - 1];
        // A wrapper whose own quote never appears inside is plainly a wrapper.
        if !inner.contains(quote) {
            return Cow::Borrowed(inner);
        }
        // Agents also send the whole line double-quoted with its inner quotes
        // escaped; that is still a wrapper, and the escapes are not part of
        // the command the user ran.
        let escaped = format!("\\{quote}");
        if inner.matches(quote).count() == inner.matches(&escaped).count() {
            return Cow::Owned(inner.replace(&escaped, &quote.to_string()));
        }
    }
    Cow::Borrowed(trimmed)
}

fn parse_cat_numbered_line(line: &str) -> Option<(u32, &str)> {
    let (prefix, text) = line.split_once('\t')?;
    let number = prefix.trim();
    if number.is_empty()
        || !prefix
            .chars()
            .all(|character| character == ' ' || character.is_ascii_digit())
    {
        return None;
    }

    Some((number.parse().ok()?, text))
}

fn render_cat_numbered_code_block(
    parsed: ParsedCatNumberedCode,
    language: Option<Arc<Language>>,
    markdown_style: MarkdownStyle,
    copy_button_id: String,
    cx: &App,
) -> AnyElement {
    use std::fmt::Write as _;

    let ParsedCatNumberedCode {
        code,
        first_number,
        line_count,
    } = parsed;

    // Line numbers are contiguous (verified during parsing), so the largest
    // line number is `first_number + line_count - 1`. Sizing the gutter to
    // that number's digit count means every rendered line contributes exactly
    // `gutter_width` bytes to the gutter, plus a newline between adjacent
    // lines.
    let last_number = first_number
        .saturating_add(u32::try_from(line_count.saturating_sub(1)).unwrap_or(u32::MAX));
    let gutter_width = last_number.to_string().len().max(1);
    let gutter_capacity = line_count * gutter_width + line_count.saturating_sub(1);

    let mut gutter = String::with_capacity(gutter_capacity);
    for i in 0..line_count {
        if i > 0 {
            gutter.push('\n');
        }
        let line_number = first_number.saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
        // Writes to a `String` are infallible, so the `Result` can be ignored.
        let _ = write!(&mut gutter, "{line_number:>gutter_width$}");
    }

    let mut code_text_style = markdown_style.base_text_style.clone();
    code_text_style.refine(&markdown_style.code_block.text);

    let mut gutter_text_style = code_text_style.clone();
    gutter_text_style.color = cx.theme().colors().text_muted;

    let gutter_len = gutter.len();
    let gutter = StyledText::new(gutter).with_runs(vec![gutter_text_style.to_run(gutter_len)]);

    // Share `code` between syntax highlighting, the rendered `StyledText`, and
    // the copy button via a single `SharedString` (cheap `Arc` clones) instead
    // of cloning the underlying `String`.
    let code: SharedString = code.into();
    let code_runs = highlight_code_runs(&code, language.as_ref(), code_text_style, &markdown_style);
    let code_text = StyledText::new(code.clone()).with_runs(code_runs);

    let code_block_id = format!("read-file-code-block-{copy_button_id}");
    let code_scroll_id = format!("read-file-code-scroll-{copy_button_id}");
    let mut container = div()
        .id(code_block_id)
        .group("read-file-code-block")
        .relative()
        .w_full()
        .whitespace_nowrap();
    container.style().refine(&markdown_style.code_block);

    // `overflow_x_scroll` only actually scrolls when the container is laid out
    // as a flex container: in GPUI the default `Display` is `Block`, and a
    // block-level child fills its parent's content width instead of overflowing
    // it, so there is nothing for the scroll viewport to scroll. Using `flex()`
    // on the scroll wrapper plus `flex_none()` on the inner item lets the inner
    // item take its natural width (the unwrapped code), which is what overflows.
    // `restrict_scroll_to_axis` then keeps vertical wheel events flowing through
    // to the outer thread scroller. This mirrors the standard markdown
    // code-block path in `crates/markdown/src/markdown.rs`.
    let code_scroll = div()
        .id(code_scroll_id)
        .flex()
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .restrict_scroll_to_axis()
        .child(div().flex_none().child(code_text));

    container
        .child(
            h_flex()
                .items_start()
                .min_w_0()
                .w_full()
                .child(div().flex_none().pr_3().child(gutter))
                .child(code_scroll),
        )
        .child(
            h_flex()
                .w_4()
                .absolute()
                .top_0()
                .right_0()
                .justify_end()
                .visible_on_hover("read-file-code-block")
                .child(CopyButton::new(copy_button_id, code).tooltip_label("Copy Code")),
        )
        .into_any_element()
}

#[cfg(test)]
mod numbered_code_block_tests {
    use super::*;

    #[test]
    fn parses_cat_numbered_markdown_code_block() {
        let parsed = parse_cat_numbered_markdown_code_block(
            "```rs zed/crates/example.rs\n     2\tfn main() {\n     3\t    println!(\"hi\");\n     4\t}\n```\n",
        )
        .expect("cat-numbered block should parse");

        assert_eq!(parsed.line_count, 3);
        assert_eq!(parsed.first_number, 2);
        assert_eq!(parsed.code, "fn main() {\n    println!(\"hi\");\n}");
    }

    #[test]
    fn parses_cat_numbered_code_with_crlf_line_endings() {
        let parsed = parse_cat_numbered_code("     1\tline one\r\n     2\tline two\r\n")
            .expect("crlf-terminated cat-numbered code should parse");

        assert_eq!(parsed.line_count, 2);
        assert_eq!(parsed.first_number, 1);
        assert_eq!(parsed.code, "line one\nline two");
    }

    #[test]
    fn rejects_non_cat_numbered_code_block() {
        assert!(parse_cat_numbered_markdown_code_block("```rs\nfn main() {}\n```\n").is_none());
    }

    #[test]
    fn rejects_non_contiguous_cat_numbers() {
        assert!(
            parse_cat_numbered_markdown_code_block(
                "```rs\n     2\tlet a = 1;\n     4\tlet b = 2;\n```\n"
            )
            .is_none()
        );
    }
}

/// Tracks the user's permission dropdown selection state for a specific tool call.
///
/// Default (no entry in the map) means the last dropdown choice is selected,
/// which is typically "Only this time".
#[derive(Clone)]
pub(crate) enum PermissionSelection {
    /// A specific choice from the dropdown (e.g., "Always for terminal", "Only this time").
    /// The index corresponds to the position in the `choices` list from `PermissionOptions`.
    Choice(usize),
    /// "Select options…" mode where individual command patterns can be toggled.
    /// Contains the indices of checked patterns in the `patterns` list.
    /// All patterns start checked when this mode is first activated.
    SelectedPatterns(Vec<usize>),
}

impl PermissionSelection {
    /// Returns the choice index if a specific dropdown choice is selected,
    /// or `None` if in per-command pattern mode.
    pub(crate) fn choice_index(&self) -> Option<usize> {
        match self {
            Self::Choice(index) => Some(*index),
            Self::SelectedPatterns(_) => None,
        }
    }

    fn is_pattern_checked(&self, index: usize) -> bool {
        match self {
            Self::SelectedPatterns(checked) => checked.contains(&index),
            _ => false,
        }
    }

    fn has_any_checked_patterns(&self) -> bool {
        match self {
            Self::SelectedPatterns(checked) => !checked.is_empty(),
            _ => false,
        }
    }

    fn toggle_pattern(&mut self, index: usize) {
        if let Self::SelectedPatterns(checked) = self {
            if let Some(pos) = checked.iter().position(|&i| i == index) {
                checked.swap_remove(pos);
            } else {
                checked.push(index);
            }
        }
    }
}

pub struct ThreadView {
    pub(crate) root_thread_id: ThreadId,
    pub session_id: acp::SessionId,
    pub parent_session_id: Option<acp::SessionId>,
    pub thread: Entity<AcpThread>,
    pub(crate) conversation: Entity<super::Conversation>,
    pub server_view: WeakEntity<ConversationView>,
    pub agent_icon: IconName,
    pub agent_icon_from_external_svg: Option<SharedString>,
    pub agent_id: AgentId,
    pub agent_display_name: SharedString,
    pub focus_handle: FocusHandle,
    pub workspace: WeakEntity<Workspace>,
    pub entry_view_state: Entity<EntryViewState>,
    pub title_editor: Entity<Editor>,
    pub config_options_view: Option<Entity<ConfigOptionsView>>,
    pub mode_selector: Option<Entity<ModeSelector>>,
    pub model_selector: Option<Entity<ModelSelectorPopover>>,
    pub profile_selector: Option<Entity<ProfileSelector>>,
    pub permission_dropdown_handle: PopoverMenuHandle<ContextMenu>,
    pub thread_retry_status: Option<RetryStatus>,
    pub(super) thread_error: Option<ThreadError>,
    pub thread_error_markdown: Option<Entity<Markdown>>,
    pub token_limit_callout_dismissed: bool,
    pub last_token_limit_telemetry: Option<acp_thread::TokenUsageRatio>,
    thread_feedback: ThreadFeedbackState,
    pub list_state: ListState,
    pub session_capabilities: SharedSessionCapabilities,
    pub expanded_tool_call_raw_inputs: HashSet<acp::ToolCallId>,
    /// The one action chip the user expanded by clicking it. Only one is
    /// expanded at a time across the whole group.
    expanded_action_chip: Option<ActionChipId>,
    /// Markdown entities for the scripts a command carried (heredoc bodies,
    /// `-c` payloads), built the first time the call is expanded so the code
    /// can be shown highlighted rather than as one long line.
    command_script_markdown:
        RefCell<HashMap<acp::ToolCallId, Vec<(SharedString, Entity<Markdown>)>>>,
    /// Image read chips the user explicitly collapsed. Image chips start
    /// expanded (seeing the image is the point), so this records the
    /// exception rather than the rule.
    collapsed_image_chips: HashSet<ActionChipId>,
    /// Answers about commands and their output that rendering would otherwise
    /// recompute for every chip on every frame.
    chip_cache: ChipCache,
    /// Diff editors for the files a command changed, built the first time one
    /// is hovered. Nobody declared these edits, so there is no ACP diff behind
    /// them: the file's buffer against the repository is the only diff there
    /// is, and opening it is worth doing once rather than per hover.
    command_file_diffs: RefCell<HashMap<project::ProjectPath, CommandFileDiff>>,
    /// The real-file editors an edit chip decorated with the agent's diff,
    /// keyed by project path, so a chip does not re-decorate an editor that is
    /// already showing the diff (opening the file is idempotent; adding the
    /// diff is not).
    /// The thought currently shown beside the progress indicator, pinned for a
    /// minimum time so fast streams stay readable.
    displayed_thought: Option<((usize, usize), std::time::Instant)>,
    thought_hold_timer: Option<Task<()>>,
    /// In-flight local title generation for an agent that supplies no title of
    /// its own (see [`Self::generate_title_if_needed`]).
    title_generation: Option<Task<()>>,
    collapsed_sandbox_authorization_details: HashSet<acp::ToolCallId>,
    collapsed_sandbox_network_details: HashSet<acp::ToolCallId>,
    /// Sandbox escalation prompts whose "surprising Unicode" warning the user
    /// has explicitly acknowledged. Until a prompt's tool call is in this set,
    /// its allow buttons stay disabled. See [`Self::sandbox_confusable_findings`].
    acknowledged_confusable_warnings: HashSet<acp::ToolCallId>,
    pub subagent_scroll_handles: RefCell<HashMap<acp::SessionId, ScrollHandle>>,
    pub edits_expanded: bool,
    pub plan_expanded: bool,
    pub queue_expanded: bool,
    pub editor_expanded: bool,
    pub should_be_following: bool,
    pub editing_message: Option<usize>,
    pub message_queue: MessageQueue,
    pub turn_fields: TurnFields,
    pub discarded_partial_edits: HashSet<acp::ToolCallId>,
    pub is_loading_contents: bool,
    pub new_server_version_available: Option<SharedString>,
    pub resumed_without_history: bool,
    pub(crate) permission_selections: HashMap<acp::ToolCallId, PermissionSelection>,
    elicitation_form_states: HashMap<ElicitationEntryId, ElicitationFormState>,
    pub _cancel_task: Option<Task<()>>,
    _save_task: Option<Task<()>>,
    _draft_resolve_task: Option<Task<()>>,
    _sandbox_status_refresh_task: Option<Task<()>>,
    pub hovered_edited_file_buttons: Option<usize>,
    pub in_flight_prompt: Option<Vec<acp::ContentBlock>>,
    pub _subscriptions: Vec<Subscription>,
    pub message_editor: Entity<MessageEditor>,
    pub add_context_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub thinking_effort_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub project: WeakEntity<Project>,
    /// Cache + worktree snapshot for resolving paths in markdown code spans.
    /// Cloned from the parent `ConversationView` so the cache is shared and the
    /// snapshot stays in sync via the parent's project-event subscription.
    pub(crate) code_span_resolver: AgentCodeSpanResolver,
    pub show_external_source_prompt_warning: bool,
    pub show_codex_windows_warning: bool,
    sandbox_status: Option<VerifiedSandboxStatus>,
    sandbox_status_key: Option<SandboxStatusKey>,
    pending_sandbox_status_key: Option<SandboxStatusKey>,
    pub multi_root_callout_dismissed: bool,
    pub skill_loading_issues: Vec<SkillLoadingIssue>,
    /// Issues the user has explicitly dismissed. Each entry is matched against
    /// emitted issues by full equality; when an issue no longer appears in the
    /// latest replacement list (because the underlying file was fixed/removed), it's
    /// dropped from this set so a future regression of the same kind would
    /// re-show.
    dismissed_skill_loading_issues: HashSet<SkillLoadingIssue>,
    pub(crate) thread_search_bar: Option<Entity<super::thread_search_bar::ThreadSearchBar>>,
    pub(crate) thread_search_visible: bool,
    /// The input status bar's diff readout: this thread's worktree against the
    /// start of its branch, straight from git.
    branch_diff_stats: Entity<BranchDiffStats>,
}

/// Where a fresh draft's first message should land. The new-worktree option
/// records the base branch to create off; nothing is created until send.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DraftWorktreeChoice {
    /// Send in the current worktree/project.
    Current,
    /// Create a worktree off the repository's default branch, resolved at
    /// send time (freshly fetched); the standing default for new drafts.
    #[default]
    NewWorktreeDefault,
    /// Create a new git worktree off this base on first send, and start the
    /// thread there.
    NewWorktree(zed_actions::NewWorktreeBranchTarget),
}

impl Focusable for ThreadView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ThreadView {
    pub(crate) fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        if self.parent_session_id.is_some() {
            self.focus_handle.clone()
        } else {
            self.active_editor(cx).focus_handle(cx)
        }
    }
}

#[derive(Default)]
pub struct TurnFields {
    pub _turn_timer_task: Option<Task<()>>,
    pub last_turn_duration: Option<Duration>,
    pub last_turn_tokens: Option<u64>,
    pub turn_generation: usize,
    pub turn_started_at: Option<Instant>,
    pub turn_tokens: Option<u64>,
}

/// How a tool call is rendered relative to its surroundings.
///
/// `Standalone` draws its own border/margin/location header. `Embedded` is
/// hosted by a container that provides its own framing (e.g. the subagent
/// card). `Floating` is like `Embedded`, but used for the floating
/// awaiting-permission row above the message editor: the tool call's content
/// is height-capped and scrollable so the row can never grow to consume the
/// entire panel and squeeze the conversation list out of view.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ToolCallLayout {
    Standalone,
    Embedded,
    Floating,
    /// The body of an expanded action chip. The chip itself is the toggle, so
    /// the header inside is inert: no hover, no click, no disclosure, output
    /// always shown.
    ChipBody,
}

/// One chip of an action group: an agent action (a tool call, or a run of
/// adjacent wait calls collapsed into one) or one of an assistant message's
/// thoughts. Chips are derived from thread entries at render time, so a chip is
/// addressed by the entry it came from rather than owning any state.
/// A file an edit tool call touched. `location_ix` is the call's own location
/// for it, when the call reported one; a call that only sends diffs has none,
/// and its file is named by the diff itself.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditedFile {
    path: std::path::PathBuf,
    location_ix: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ActionChip {
    ToolCall {
        entry_ix: usize,
    },
    /// A run's reads and searches, folded into one quiet summary chip
    /// ("Read 3 files, searched 2"). Expanding shows the individual chips.
    Collapsed {
        entry_ixs: Vec<usize>,
    },
    /// One edited file of a multi-file edit tool call. `file_ix` indexes the
    /// call's distinct edited files, so a call touching several files reads as
    /// one chip per file rather than a single "N files" chip.
    EditFile {
        entry_ix: usize,
        file_ix: usize,
    },
    /// One file a command changed, as the repository saw it. A command that
    /// rewrites files says nothing about them itself, so these come from
    /// watching the worktree rather than from reading the command.
    CommandFile {
        entry_ix: usize,
        path_ix: usize,
    },
    /// The same, when there are too many to name: a codemod or a formatter can
    /// touch a hundred files, and a hundred chips is not a report, it is a
    /// wall. One chip says how many and opens the diff over all of them.
    CommandFiles {
        entry_ix: usize,
    },
}

/// Past this many files, a command's changes read as a count rather than as a
/// chip each.
const MOST_NAMED_COMMAND_FILES: usize = 6;

/// What a chip needs to know about a command, and what its output reported.
///
/// Rendering asks these questions of every chip on every frame, and answering
/// them costs a full parse of the command line and a scan of every line of its
/// output. Computing them per frame is what made a long thread crawl, so they
/// are computed once and reused until the thing they describe changes.
#[derive(Default)]
struct ChipCache {
    commands: RefCell<HashMap<acp::ToolCallId, Rc<CommandFacts>>>,
    outputs: RefCell<HashMap<acp::ToolCallId, Rc<OutputFacts>>>,
    /// Syntax highlighting for chip labels. Highlighting a label means parsing
    /// it, which a wall of chips cannot afford to do on every frame it is
    /// scrolled past; the text of a chip rarely changes, and the style it is
    /// drawn in changes only with the theme.
    highlights: RefCell<HighlightCache>,
    /// The last line each running command has printed. A running chip repaints
    /// continuously for its pulse and reading the line walks the terminal's
    /// grid, so it is sampled on a slow interval rather than read per frame —
    /// which also makes it readable, since a stream of output redrawn at frame
    /// rate is a blur.
    tails: RefCell<HashMap<acp::ToolCallId, CommandTail>>,
    /// Answers that hold for the length of one frame, thrown away at the start
    /// of the next: the style chips are drawn in, and which entries are chips.
    /// Both are asked for once per visible entry, and the run each entry
    /// belongs to is found by scanning its neighbours, so the same questions
    /// come back many times over within a single frame.
    frame_style: RefCell<Option<MarkdownStyle>>,
    frame_chip_entries: RefCell<Vec<Option<bool>>>,
}

/// Highlight runs, kept for as long as the style they were built in holds.
#[derive(Default)]
struct HighlightCache {
    /// What the runs were built for: the base font and colour, the syntax
    /// theme, and the language. Any of these moving invalidates all of them.
    token: Option<(gpui::Font, gpui::Hsla, usize, usize)>,
    runs: HashMap<HighlightKey, Rc<Vec<gpui::TextRun>>>,
}

/// A label as far as highlighting is concerned: its text, and which of it is
/// shell. The same words highlight differently as a command and as prose.
#[derive(PartialEq, Eq, Hash)]
struct HighlightKey {
    text: SharedString,
    commands: Vec<Range<usize>>,
}

struct CommandFacts {
    /// The label these were read from. A command that changes is reparsed.
    source: SharedString,
    /// The command with its markdown fences stripped.
    command: String,
    parsed: acp_thread::ParsedCommand,
    class: acp_thread::CommandClass,
    destructive: bool,
    /// Where it ran, when that is not here. A line only partly remote still
    /// ran somewhere else, and that is worth saying.
    host: Option<String>,
    /// What the line did, for the chip's label.
    summary: Option<String>,
}

struct OutputFacts {
    /// How much output had arrived when this was scanned. Output only grows,
    /// so a length that has not changed means a scan that need not be redone.
    scanned_len: usize,
    summary: Option<acp_thread::OutputSummary>,
}

/// The last line a command had printed when it was last looked at.
struct CommandTail {
    sampled_at: std::time::Instant,
    line: Option<SharedString>,
}

impl CommandFacts {
    /// The facts of a bare command line, for callers with no tool call in hand.
    #[cfg(test)]
    fn for_command(command: &str) -> Self {
        let parsed = acp_thread::parse_command(command);
        Self {
            source: command.to_string().into(),
            class: acp_thread::classify_command(command),
            destructive: parsed
                .segments
                .iter()
                .any(|segment| matches!(segment.kind, acp_thread::SegmentKind::Destructive { .. })),
            host: parsed.host.clone(),
            summary: acp_thread::summarize_command(&parsed),
            command: command.to_string(),
            parsed,
        }
    }
}

impl ChipCache {
    /// Drops what only held for the frame that has just ended.
    fn begin_frame(&self) {
        self.frame_style.borrow_mut().take();
        self.frame_chip_entries.borrow_mut().clear();
    }

    /// The style chip labels are drawn in, built once for the frame.
    fn style(&self, window: &Window, cx: &App) -> MarkdownStyle {
        self.frame_style
            .borrow_mut()
            .get_or_insert_with(|| {
                MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_buffer_font(cx)
            })
            .clone()
    }

    /// Highlight runs for one chip label. The same label is highlighted on
    /// every frame it is on screen, and highlighting parses it, so the runs
    /// live until the style they were built in moves.
    fn highlight_label(
        &self,
        label: &chips::CommandChipLabel,
        language: Option<&Arc<Language>>,
        text_style: &TextStyle,
        markdown_style: &MarkdownStyle,
    ) -> Vec<gpui::TextRun> {
        let token = (
            text_style.font(),
            text_style.color,
            Arc::as_ptr(&markdown_style.syntax) as usize,
            language.map_or(0, |language| Arc::as_ptr(language) as usize),
        );
        let mut cache = self.highlights.borrow_mut();
        // A thread long enough to hold thousands of distinct labels has left
        // the early ones far off screen; keeping them all would be a leak in
        // everything but name.
        if cache.token.as_ref() != Some(&token) || cache.runs.len() > 4096 {
            cache.token = Some(token);
            cache.runs.clear();
        }
        let key = HighlightKey {
            text: label.text.clone().into(),
            commands: label.commands.clone(),
        };
        if let Some(runs) = cache.runs.get(&key) {
            return runs.as_ref().clone();
        }
        let runs = Rc::new(label.runs(language, text_style.clone(), markdown_style));
        cache.runs.insert(key, runs.clone());
        runs.as_ref().clone()
    }

    fn command(&self, tool_call: &ToolCall, cx: &App) -> Rc<CommandFacts> {
        let source = tool_call.label.read(cx).source();
        if let Some(facts) = self.commands.borrow().get(&tool_call.id)
            && facts.source == *source
        {
            return facts.clone();
        }

        let command = strip_command_fences(&source).to_string();
        let parsed = acp_thread::parse_command(&command);
        let facts = Rc::new(CommandFacts {
            class: acp_thread::classify_command(&command),
            destructive: parsed
                .segments
                .iter()
                .any(|segment| matches!(segment.kind, acp_thread::SegmentKind::Destructive { .. })),
            host: parsed.host.clone().or_else(|| {
                parsed
                    .segments
                    .iter()
                    .find_map(|segment| segment.host.clone())
            }),
            summary: acp_thread::summarize_command(&parsed),
            source: source.clone(),
            command,
            parsed,
        });
        self.commands
            .borrow_mut()
            .insert(tool_call.id.clone(), facts.clone());
        facts
    }

    fn output(&self, tool_call: &ToolCall, cx: &App) -> Rc<OutputFacts> {
        let content = tool_call
            .terminals()
            .next()
            .and_then(|terminal| terminal.read(cx).output())
            .map(|output| output.content.clone());
        let scanned_len = content.as_ref().map_or(0, |content| content.len());
        if let Some(facts) = self.outputs.borrow().get(&tool_call.id)
            && facts.scanned_len == scanned_len
        {
            return facts.clone();
        }

        let facts = Rc::new(OutputFacts {
            scanned_len,
            summary: content
                .as_deref()
                .map(acp_thread::summarize_output)
                .filter(|summary| !summary.is_empty()),
        });
        self.outputs
            .borrow_mut()
            .insert(tool_call.id.clone(), facts.clone());
        facts
    }

    /// The last line a still-running command has printed, resampled at most
    /// once a second. A long test run and a hung one look the same without it.
    fn tail(&self, tool_call: &ToolCall, cx: &App) -> Option<SharedString> {
        const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

        if let Some(tail) = self.tails.borrow().get(&tool_call.id)
            && tail.sampled_at.elapsed() < SAMPLE_INTERVAL
        {
            return tail.line.clone();
        }

        let line = tool_call
            .terminals()
            .next()
            .and_then(|terminal| terminal.read(cx).last_output_line(cx))
            .map(SharedString::from);
        self.tails.borrow_mut().insert(
            tool_call.id.clone(),
            CommandTail {
                sampled_at: std::time::Instant::now(),
                line: line.clone(),
            },
        );
        line
    }
}

/// A command-changed file's diff, from the moment it is first asked for.
enum CommandFileDiff {
    /// The buffer and its diff are being opened; the task is held so that
    /// closing the thread stops the work.
    Loading { _task: Task<()> },
    Ready(Entity<Editor>),
}

/// A picture a chip stands for, however the agent delivered it.
#[derive(Clone)]
enum ChipImage {
    /// An image on disk, which may live outside the project. Its shape is not
    /// known without reading it, which is why only the variant below can size
    /// its own box.
    File(std::path::PathBuf),
    /// Image data the call carried, with no file behind it, and the dimensions
    /// decoded alongside it.
    Data {
        image: Arc<gpui::Image>,
        dimensions: Option<gpui::Size<u32>>,
    },
}

impl ChipImage {
    /// The shape of the picture, when it is known.
    fn dimensions(&self) -> Option<gpui::Size<u32>> {
        match self {
            ChipImage::File(_) => None,
            ChipImage::Data { dimensions, .. } => *dimensions,
        }
    }
}

/// What a chip click expands. Tool-call expansion is keyed by id (it drives the
/// tool call's own `EntryViewState` expansion). Thoughts are not here: they do
/// not expand, so they carry no click-expansion state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ActionChipId {
    ToolCall(acp::ToolCallId),
    /// The reads-and-searches summary chip, keyed by its first call.
    Collapsed(acp::ToolCallId),
    /// One file of a multi-file edit call. Expanding it shows only that file's
    /// diff, so it does not drive the tool call's own expansion state.
    EditFile {
        tool_call_id: acp::ToolCallId,
        file_ix: usize,
    },
}

impl ToolCallLayout {
    /// Stable discriminant used to disambiguate element ids when the same tool
    /// call is rendered in more than one layout at once (e.g. inline in the
    /// list *and* in the floating awaiting-permission row).
    fn id_str(self) -> &'static str {
        match self {
            ToolCallLayout::Standalone => "standalone",
            ToolCallLayout::Embedded => "embedded",
            ToolCallLayout::Floating => "floating",
            ToolCallLayout::ChipBody => "chip-body",
        }
    }
}

fn full_path_for_empty_project_path(file: &dyn language::File, cx: &App) -> Option<String> {
    if file.path().file_name().is_some() {
        return None;
    }

    let full_path = file.full_path(cx).display().to_string();
    (!full_path.is_empty()).then_some(full_path)
}

fn skill_issue_file_label(path: &std::path::Path) -> String {
    let file_name = path.file_name().and_then(|name| name.to_str());
    let parent_name = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str());

    match (parent_name, file_name) {
        (Some(parent_name), Some(file_name)) => format!("{parent_name}/{file_name}"),
        (_, Some(file_name)) => file_name.to_string(),
        _ => path.display().to_string(),
    }
}

pub fn open_markdown_in_workspace(
    title: String,
    markdown: String,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let markdown_language_task = workspace
        .read(cx)
        .app_state()
        .languages
        .language_for_name("Markdown");
    let project = workspace.read(cx).project().clone();

    window.spawn(cx, async move |cx| {
        let markdown_language = markdown_language_task.await?;

        let buffer = project
            .update(cx, |project, cx| {
                project.create_buffer(Some(markdown_language), false, cx)
            })
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(markdown, cx);
            buffer.set_capability(language::Capability::ReadWrite, cx);
        });

        workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));

            workspace.add_item_to_active_pane(
                Box::new(cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                    editor.set_breadcrumb_header(title);
                    editor.disable_mouse_wheel_zoom();
                    editor
                })),
                None,
                true,
                window,
                cx,
            );
        })?;
        anyhow::Ok(())
    })
}

impl ThreadView {
    pub(crate) fn new(
        root_thread_id: ThreadId,
        thread: Entity<AcpThread>,
        conversation: Entity<super::Conversation>,
        server_view: WeakEntity<ConversationView>,
        agent_icon: IconName,
        agent_icon_from_external_svg: Option<SharedString>,
        agent_id: AgentId,
        agent_display_name: SharedString,
        workspace: WeakEntity<Workspace>,
        entry_view_state: Entity<EntryViewState>,
        config_options_view: Option<Entity<ConfigOptionsView>>,
        mode_selector: Option<Entity<ModeSelector>>,
        model_selector: Option<Entity<ModelSelectorPopover>>,
        profile_selector: Option<Entity<ProfileSelector>>,
        list_state: ListState,
        session_capabilities: SharedSessionCapabilities,
        resumed_without_history: bool,
        project: WeakEntity<Project>,
        code_span_resolver: AgentCodeSpanResolver,
        thread_store: Option<Entity<ThreadStore>>,
        initial_content: Option<AgentInitialContent>,
        mut subscriptions: Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session_id = thread.read(cx).session_id().clone();
        let parent_session_id = thread.read(cx).parent_session_id().cloned();

        let has_slash_completions = session_capabilities.read().has_slash_completions();
        let placeholder = placeholder_text(agent_display_name.as_ref(), has_slash_completions);

        let mut should_auto_submit = false;
        let mut show_external_source_prompt_warning = false;

        let message_editor = cx.new(|cx| {
            let mut editor = MessageEditor::new(
                workspace.clone(),
                project.clone(),
                thread_store,
                session_capabilities.clone(),
                agent_id.clone(),
                &placeholder,
                editor::EditorMode::AutoHeight {
                    min_lines: AgentSettings::get_global(cx).message_editor_min_lines,
                    max_lines: Some(AgentSettings::get_global(cx).set_message_editor_max_lines()),
                },
                window,
                cx,
            );
            if let Some(content) = initial_content {
                match content {
                    AgentInitialContent::ThreadSummary { session_id, title } => {
                        editor.insert_thread_summary(session_id, title, window, cx);
                    }
                    AgentInitialContent::ContentBlock {
                        blocks,
                        auto_submit,
                    } => {
                        should_auto_submit = auto_submit;
                        editor.set_message(blocks, window, cx);
                    }
                    AgentInitialContent::FromExternalSource(prompt) => {
                        show_external_source_prompt_warning = true;
                        // SECURITY: Be explicit about not auto submitting prompt from external source.
                        should_auto_submit = false;
                        editor.set_message(
                            vec![acp::ContentBlock::Text(acp::TextContent::new(
                                prompt.into_string(),
                            ))],
                            window,
                            cx,
                        );
                    }
                }
            } else if let Some(draft) = thread.read(cx).draft_prompt() {
                editor.set_message(draft.to_vec(), window, cx);
            }
            editor
        });

        let show_codex_windows_warning = cfg!(windows)
            && project.upgrade().is_some_and(|p| p.read(cx).is_local())
            && agent_id.as_ref() == "Codex";

        if let Some(project) = project.upgrade() {
            subscriptions.push(cx.subscribe(&project, {
                let resolver = code_span_resolver.clone();
                move |_this: &mut Self, _project, event: &project::Event, cx| {
                    if matches!(
                        event,
                        project::Event::WorktreeAdded(_)
                            | project::Event::WorktreeRemoved(_)
                            | project::Event::WorktreeUpdatedEntries(_, _)
                    ) {
                        resolver.clear_cache();
                        cx.notify();
                    }
                }
            }));
        }

        let title_editor = {
            let metadata = ThreadMetadataStore::try_global(cx)
                .and_then(|store| store.read(cx).entry(root_thread_id).cloned());
            let initial_title = if parent_session_id.is_none() {
                metadata.as_ref().and_then(|m| m.title())
            } else {
                thread.read(cx).title()
            }
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into());
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(initial_title, window, cx);
                editor
            });
            subscriptions.push(cx.subscribe_in(&editor, window, Self::handle_title_editor_event));
            editor
        };

        subscriptions.push(cx.subscribe_in(
            &entry_view_state,
            window,
            Self::handle_entry_view_event,
        ));

        subscriptions.push(cx.subscribe_in(
            &message_editor,
            window,
            Self::handle_message_editor_event,
        ));

        // The edits summary reads the action log's changed buffers.
        let action_log = thread.read(cx).action_log().clone();
        subscriptions.push(cx.observe(&action_log, |_this, _action_log, cx| {
            cx.notify();
        }));
        subscriptions.push(cx.subscribe(
            &thread,
            |this: &mut Self, _thread, event: &AcpThreadEvent, cx| {
                if matches!(
                    event,
                    AcpThreadEvent::NewEntry
                        | AcpThreadEvent::EntryUpdated(_)
                        | AcpThreadEvent::EntriesRemoved(_)
                        | AcpThreadEvent::StatusChanged
                        | AcpThreadEvent::Stopped(_)
                ) {
                    // The agent reports its work dirs as the session runs, and
                    // they scope the diff readout to this thread's worktree.
                    this.sync_branch_diff_work_dirs(cx);
                    cx.notify();
                }
            },
        ));

        // Repaint the input status bar's PR badges when gh_status polling lands
        // fresh data for the thread's branches.
        if let Some(gh_store) = gh_status::GhStatusStore::try_global(cx) {
            subscriptions.push(cx.observe(&gh_store, |_this, _store, cx| cx.notify()));
        }

        // If this thread is backed by a NativeAgent, listen for skill loading
        // issues so we can surface them as banners. The agent emits a single
        // replacement-style event per project refresh, so we overwrite our
        // local list rather than appending — this also clears stale issues
        // once a user resolves them.
        if let Some(native_connection) = thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<agent::NativeAgentConnection>()
        {
            let project_id = thread.read(cx).project().entity_id();
            subscriptions.push(cx.subscribe(
                &native_connection.0,
                move |this: &mut Self, _agent, event: &SkillLoadingIssuesUpdated, cx| {
                    if event.project_id != project_id {
                        return;
                    }
                    // Drop dismissals for issues that no longer appear in the emitted
                    // list — the underlying file must have been fixed or removed, so a
                    // future regression should re-show.
                    this.dismissed_skill_loading_issues
                        .retain(|dismissed| event.issues.contains(dismissed));

                    // Show only issues that haven't been dismissed.
                    this.skill_loading_issues = event
                        .issues
                        .iter()
                        .filter(|issue| !this.dismissed_skill_loading_issues.contains(issue))
                        .cloned()
                        .collect();
                    cx.notify();
                },
            ));

            // A "no model selected" error is stale as soon as the thread has a
            // usable model
            if let Some(native_thread) = native_connection.thread(thread.read(cx).session_id(), cx)
            {
                subscriptions.push(cx.subscribe(
                    &native_thread,
                    |this: &mut Self, _thread, _event: &agent::ModelChanged, cx| {
                        if matches!(this.thread_error, Some(ThreadError::NoModelSelected)) {
                            this.clear_thread_error(cx);
                        }
                    },
                ));
            }
        }

        subscriptions.push(cx.observe(&message_editor, |this, editor, cx| {
            let is_empty = editor.read(cx).text(cx).is_empty();
            let draft_contents_task = if is_empty {
                None
            } else {
                Some(editor.update(cx, |editor, cx| editor.draft_contents(cx)))
            };
            this._draft_resolve_task = Some(cx.spawn(async move |this, cx| {
                let draft = if let Some(task) = draft_contents_task {
                    let blocks = task.await.ok().filter(|b| !b.is_empty());
                    blocks
                } else {
                    None
                };
                this.update(cx, |this, cx| {
                    this.thread.update(cx, |thread, cx| {
                        thread.set_draft_prompt(draft, cx);
                    });
                    this.schedule_save(cx);
                })
                .ok();
            }));
        }));

        let branch_diff_stats = cx.new(|cx| BranchDiffStats::new(project.clone(), cx));
        subscriptions.push(cx.observe(&branch_diff_stats, |_this, _stats, cx| cx.notify()));

        let mut this = Self {
            root_thread_id,
            session_id,
            parent_session_id,
            focus_handle: cx.focus_handle(),
            thread,
            conversation,
            server_view,
            agent_icon,
            agent_icon_from_external_svg,
            agent_id,
            agent_display_name,
            workspace,
            entry_view_state,
            title_editor,
            config_options_view,
            mode_selector,
            model_selector,
            profile_selector,
            list_state,
            session_capabilities,
            resumed_without_history,
            _subscriptions: subscriptions,
            permission_dropdown_handle: PopoverMenuHandle::default(),
            thread_retry_status: None,
            thread_error: None,
            thread_error_markdown: None,
            token_limit_callout_dismissed: false,
            last_token_limit_telemetry: None,
            thread_feedback: Default::default(),
            expanded_tool_call_raw_inputs: HashSet::default(),
            expanded_action_chip: None,
            command_script_markdown: RefCell::default(),
            collapsed_image_chips: HashSet::default(),
            chip_cache: ChipCache::default(),
            command_file_diffs: RefCell::default(),
            displayed_thought: None,
            thought_hold_timer: None,
            title_generation: None,
            collapsed_sandbox_authorization_details: HashSet::default(),
            collapsed_sandbox_network_details: HashSet::default(),
            acknowledged_confusable_warnings: HashSet::default(),
            subagent_scroll_handles: RefCell::new(HashMap::default()),
            edits_expanded: false,
            plan_expanded: false,
            queue_expanded: true,
            editor_expanded: false,
            should_be_following: false,
            editing_message: None,
            message_queue: MessageQueue::default(),
            turn_fields: TurnFields::default(),
            discarded_partial_edits: HashSet::default(),
            is_loading_contents: false,
            new_server_version_available: None,
            permission_selections: HashMap::default(),
            elicitation_form_states: HashMap::default(),
            _cancel_task: None,
            _save_task: None,
            _draft_resolve_task: None,
            _sandbox_status_refresh_task: None,
            hovered_edited_file_buttons: None,
            in_flight_prompt: None,
            message_editor,
            add_context_menu_handle: PopoverMenuHandle::default(),
            thinking_effort_menu_handle: PopoverMenuHandle::default(),
            project,
            code_span_resolver,
            show_external_source_prompt_warning,
            show_codex_windows_warning,
            sandbox_status: None,
            sandbox_status_key: None,
            pending_sandbox_status_key: None,
            multi_root_callout_dismissed: false,
            skill_loading_issues: Vec::new(),
            dismissed_skill_loading_issues: HashSet::default(),
            thread_search_bar: None,
            thread_search_visible: false,
            branch_diff_stats,
        };

        this.sync_branch_diff_work_dirs(cx);
        this.sync_editor_mode(cx);
        this.sync_existing_elicitation_states(window, cx);
        let list_state_for_scroll = this.list_state.clone();
        let thread_view = cx.entity().downgrade();

        this.list_state
            .set_scroll_handler(move |_event, _window, cx| {
                let list_state = list_state_for_scroll.clone();
                let thread_view = thread_view.clone();
                // N.B. We must defer because the scroll handler is called while the
                // ListState's RefCell is mutably borrowed. Reading logical_scroll_top()
                // directly would panic from a double borrow.
                cx.defer(move |cx| {
                    let scroll_top = list_state.logical_scroll_top();
                    let _ = thread_view.update(cx, |this, cx| {
                        if let Some(thread) = this.as_native_thread(cx) {
                            thread.update(cx, |thread, _cx| {
                                thread.set_ui_scroll_position(Some(scroll_top));
                            });
                        }
                        this.schedule_save(cx);
                    });
                });
            });

        if should_auto_submit {
            this.send(window, cx);
        }
        this
    }

    /// Schedule a throttled save of the thread state (draft prompt, scroll position, etc.).
    /// Multiple calls within `SERIALIZATION_THROTTLE_TIME` are coalesced into a single save.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SERIALIZATION_THROTTLE_TIME)
                .await;
            this.update(cx, |this, cx| {
                if let Some(thread) = this.as_native_thread(cx) {
                    thread.update(cx, |_thread, cx| cx.notify());
                }
            })
            .ok();
        }));
    }

    pub fn handle_message_editor_event(
        &mut self,
        _editor: &Entity<MessageEditor>,
        event: &MessageEditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The three skill-watcher trigger points all live here:
        // - `Focus` fires when the user clicks into the input box.
        // - `SlashAutocompleteOpened` fires when the completion
        //   provider is asked for slash commands.
        // - `Send` fires when the user submits the conversation.
        // All three triggers are idempotent; firing the same one
        // repeatedly is a no-op once a scan or watch is active.
        if matches!(
            event,
            MessageEditorEvent::Focus
                | MessageEditorEvent::SlashAutocompleteOpened
                | MessageEditorEvent::Send
        ) {
            if let Some(connection) = self.as_native_connection(cx) {
                connection.ensure_skills_scan_started(cx);
                if let Some(project) = self.project.upgrade() {
                    connection.refresh_skills_for_project(project, cx);
                }
            }
        }

        match event {
            MessageEditorEvent::Send => self.send(window, cx),
            MessageEditorEvent::SendImmediately => self.interrupt_and_send(window, cx),
            MessageEditorEvent::Cancel => {
                if !self.close_thread_search(window, cx) {
                    self.cancel_generation(cx);
                }
            }
            MessageEditorEvent::Focus => {
                self.cancel_editing(&Default::default(), window, cx);
            }
            MessageEditorEvent::LostFocus => {}
            MessageEditorEvent::SlashAutocompleteOpened => {}
            MessageEditorEvent::LocalCommandInvoked(command) => {
                self.run_local_command(*command, window, cx);
            }
            MessageEditorEvent::InputAttempted { .. } => {}
            MessageEditorEvent::Edited => {}
        }
    }

    fn run_local_command(
        &mut self,
        command: PromptLocalCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PromptLocalCommand::ThumbsUp => {
                self.handle_feedback_click(ThreadFeedback::Positive, window, cx);
                self.show_local_command_toast("Thanks for your feedback!", cx);
            }
            PromptLocalCommand::ThumbsDown => {
                self.handle_feedback_click(ThreadFeedback::Negative, window, cx);
            }
        }
    }

    fn show_local_command_toast(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        // Shown after positive feedback, replacing the inline button state.
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success),
                )
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    pub(crate) fn as_native_connection(
        &self,
        cx: &App,
    ) -> Option<Rc<agent::NativeAgentConnection>> {
        let acp_thread = self.thread.read(cx);
        acp_thread.connection().clone().downcast()
    }

    pub fn as_native_thread(&self, cx: &App) -> Option<Entity<agent::Thread>> {
        let acp_thread = self.thread.read(cx);
        self.as_native_connection(cx)?
            .thread(acp_thread.session_id(), cx)
    }

    /// Resolves the message editor's contents into content blocks. For profiles
    /// that do not enable any tools, directory mentions are expanded to inline
    /// file contents since the agent can't read files on its own.
    fn resolve_message_contents(
        &self,
        message_editor: &Entity<MessageEditor>,
        cx: &mut App,
    ) -> Task<Result<(Vec<acp::ContentBlock>, Vec<Entity<Buffer>>)>> {
        let expand = self.as_native_thread(cx).is_some_and(|thread| {
            let thread = thread.read(cx);
            AgentSettings::get_global(cx)
                .profiles
                .get(thread.profile())
                .is_some_and(|profile| profile.tools.is_empty())
        });
        message_editor.update(cx, |message_editor, cx| message_editor.contents(expand, cx))
    }

    pub fn current_model_id(&self, cx: &App) -> Option<String> {
        let selector = self.model_selector.as_ref()?;
        let model = selector.read(cx).active_model(cx)?;
        Some(model.id.to_string())
    }

    pub fn current_mode_id(&self, cx: &App) -> Option<Arc<str>> {
        if let Some(thread) = self.as_native_thread(cx) {
            Some(thread.read(cx).profile().0.clone())
        } else {
            let mode_selector = self.mode_selector.as_ref()?;
            Some(mode_selector.read(cx).mode().0)
        }
    }

    fn is_subagent(&self) -> bool {
        self.parent_session_id.is_some()
    }

    /// Returns the currently active editor, either for a message that is being
    /// edited or the editor for a new message.
    pub(crate) fn active_editor(&self, cx: &App) -> Entity<MessageEditor> {
        if let Some(index) = self.editing_message
            && let Some(editor) = self
                .entry_view_state
                .read(cx)
                .entry(index)
                .and_then(|entry| entry.message_editor())
                .cloned()
        {
            editor
        } else {
            self.message_editor.clone()
        }
    }

    pub fn has_queued_messages(&self) -> bool {
        !self.message_queue.is_empty()
    }

    // events

    pub fn handle_entry_view_event(
        &mut self,
        _: &Entity<EntryViewState>,
        event: &EntryViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &event.view_event {
            ViewEvent::NewDiff(tool_call_id) => {
                if AgentSettings::get_global(cx).expand_edit_card {
                    self.entry_view_state.update(cx, |state, _cx| {
                        state.expand_tool_call(tool_call_id.clone());
                    });
                }
            }
            ViewEvent::NewTerminal(tool_call_id) => {
                if AgentSettings::get_global(cx).expand_terminal_card {
                    self.entry_view_state.update(cx, |state, _cx| {
                        state.expand_tool_call(tool_call_id.clone());
                    });
                }
            }
            ViewEvent::TerminalMovedToBackground(tool_call_id) => {
                self.entry_view_state.update(cx, |state, _cx| {
                    state.collapse_tool_call(tool_call_id);
                });
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Focus) => {
                if let Some(AgentThreadEntry::UserMessage(user_message)) =
                    self.thread.read(cx).entries().get(event.entry_index)
                    && self.thread.read(cx).supports_truncate(cx)
                    && user_message.client_id.is_some()
                    && !self.is_subagent()
                {
                    self.editing_message = Some(event.entry_index);
                    cx.notify();
                }
            }
            ViewEvent::MessageEditorEvent(editor, MessageEditorEvent::LostFocus) => {
                if let Some(AgentThreadEntry::UserMessage(user_message)) =
                    self.thread.read(cx).entries().get(event.entry_index)
                    && self.thread.read(cx).supports_truncate(cx)
                    && user_message.client_id.is_some()
                    && !self.is_subagent()
                {
                    if editor.read(cx).text(cx).as_str() == user_message.content.to_markdown(cx) {
                        self.editing_message = None;
                        cx.notify();
                    }
                }
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::SendImmediately) => {}
            ViewEvent::MessageEditorEvent(editor, MessageEditorEvent::Send) => {
                if !self.is_subagent() {
                    self.regenerate(event.entry_index, editor.clone(), window, cx);
                }
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Cancel) => {
                self.cancel_editing(&Default::default(), window, cx);
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::SlashAutocompleteOpened) => {
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::LocalCommandInvoked(_)) => {}
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Edited) => {}
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::InputAttempted { .. }) => {}
            ViewEvent::OpenDiffLocation {
                path,
                position,
                split,
            } => {
                self.open_diff_location(path, *position, *split, window, cx);
            }
        }
    }

    fn open_diff_location(
        &self,
        path: &str,
        position: Point,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let Some(project_path) = project.read(cx).find_project_path(path, cx) else {
            return;
        };

        let open_task = if split {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.split_path(project_path, window, cx)
                })
                .log_err()
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
                .log_err()
        };

        let Some(open_task) = open_task else {
            return;
        };

        window
            .spawn(cx, async move |cx| {
                let item = open_task.await?;
                let Some(editor) = item.downcast::<Editor>() else {
                    return anyhow::Ok(());
                };
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([position..position]);
                        },
                    );
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    // turns

    pub fn start_turn(&mut self, cx: &mut Context<Self>) -> usize {
        self.turn_fields.turn_generation += 1;
        let generation = self.turn_fields.turn_generation;
        self.turn_fields.turn_started_at = Some(Instant::now());
        self.turn_fields.last_turn_duration = None;
        self.turn_fields.last_turn_tokens = None;
        self.turn_fields.turn_tokens = Some(0);
        self.turn_fields._turn_timer_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
        generation
    }

    pub fn stop_turn(&mut self, generation: usize, _cx: &mut Context<Self>) {
        if self.turn_fields.turn_generation != generation {
            return;
        }
        self.turn_fields.last_turn_duration = self
            .turn_fields
            .turn_started_at
            .take()
            .map(|started| started.elapsed());
        self.turn_fields.last_turn_tokens = self.turn_fields.turn_tokens.take();
        self.turn_fields._turn_timer_task = None;
    }

    pub fn update_turn_tokens(&mut self, cx: &App) {
        if let Some(usage) = self.thread.read(cx).token_usage() {
            if let Some(tokens) = &mut self.turn_fields.turn_tokens {
                *tokens += usage.output_tokens;
                self.emit_token_limit_telemetry_if_needed(cx);
            }
        }
    }

    fn emit_token_limit_telemetry_if_needed(&mut self, cx: &App) {
        let (ratio, agent_telemetry_id, session_id) = {
            let thread_data = self.thread.read(cx);
            let Some(token_usage) = thread_data.token_usage() else {
                return;
            };
            (
                token_usage.ratio(),
                thread_data.connection().telemetry_id(),
                thread_data.session_id().clone(),
            )
        };

        let kind = match ratio {
            acp_thread::TokenUsageRatio::Normal => {
                self.last_token_limit_telemetry = None;
                return;
            }
            acp_thread::TokenUsageRatio::Warning => "warning",
            acp_thread::TokenUsageRatio::Exceeded => "exceeded",
        };

        let should_skip = self
            .last_token_limit_telemetry
            .as_ref()
            .is_some_and(|last| *last >= ratio);
        if should_skip {
            return;
        }

        self.last_token_limit_telemetry = Some(ratio);

        telemetry::event!(
            "Agent Token Limit Warning",
            agent = agent_telemetry_id,
            session_id = session_id,
            kind = kind,
        );
    }

    // sending

    fn clear_external_source_prompt_warning(&mut self, cx: &mut Context<Self>) {
        if self.show_external_source_prompt_warning {
            self.show_external_source_prompt_warning = false;
            cx.notify();
        }
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;

        if self.is_loading_contents {
            return;
        }

        let message_editor = self.message_editor.clone();

        let is_editor_empty = message_editor.read(cx).is_empty(cx);
        let is_generating = thread.read(cx).status() != ThreadStatus::Idle;

        if is_editor_empty {
            // Leaving review comments on a diff and pressing send with an empty
            // input still sends: the comments become the message.
            let review_blocks = self.take_pending_review_blocks(cx);
            if !review_blocks.is_empty() {
                cx.emit(AcpThreadViewEvent::Interacted);
                if is_generating {
                    self.add_to_queue(review_blocks, Vec::new(), window, cx);
                } else {
                    self.send_content(
                        Task::ready(Ok(Some((review_blocks, Vec::new())))),
                        false,
                        window,
                        cx,
                    );
                }
                return;
            }
            if let Some(entry) = self.message_queue.try_fast_track(is_generating) {
                self.dispatch_queued_entry(entry, window, cx);
            }
            return;
        }

        if is_generating {
            cx.emit(AcpThreadViewEvent::Interacted);
            self.queue_message(message_editor, window, cx);
            // Pending review comments ride along with the queued message.
            let review_blocks = self.take_pending_review_blocks(cx);
            if !review_blocks.is_empty() {
                self.add_to_queue(review_blocks, Vec::new(), window, cx);
            }
            return;
        }

        let text = message_editor.read(cx).text(cx);
        let text = text.trim();
        if text == "/login" || text == "/logout" {
            let connection = thread.read(cx).connection().clone();
            let can_login = !connection.auth_methods().is_empty();
            // Does the agent have a specific logout command? Prefer that in case they need to reset internal state.
            let logout_supported = text == "/logout"
                && self
                    .session_capabilities
                    .read()
                    .available_commands()
                    .iter()
                    .any(|available_command| available_command.name == "logout");
            if can_login && !logout_supported {
                message_editor.update(cx, |editor, cx| editor.clear(window, cx));
                self.clear_external_source_prompt_warning(cx);

                let connection = self.thread.read(cx).connection().clone();
                window.defer(cx, {
                    let server_view = self.server_view.clone();
                    move |window, cx| {
                        ConversationView::handle_auth_required(
                            server_view.clone(),
                            AuthRequired::new(),
                            connection,
                            window,
                            cx,
                        );
                    }
                });
                cx.notify();
                return;
            }
        }

        // A built-in command (e.g. `/compact`): run the bare command without
        // echoing it as a user message, and queue any trailing text the user
        // typed so it isn't silently dropped.
        let native_command =
            leading_native_command(text, self.session_capabilities.read().available_commands());
        if let Some(command_name) = native_command {
            cx.emit(AcpThreadViewEvent::Interacted);
            self.send_command_queueing_remainder(message_editor, command_name, window, cx);
            return;
        }

        cx.emit(AcpThreadViewEvent::Interacted);
        self.send_impl(message_editor, window, cx)
    }

    /// Sends a bare `/command` turn and queues everything the user typed after
    /// it as a follow-up message. The queued remainder auto-processes when the
    /// command turn stops, so e.g. `/compact do X` compacts and then runs `do X`
    /// rather than discarding it.
    fn send_command_queueing_remainder(
        &mut self,
        message_editor: Entity<MessageEditor>,
        command_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the editor contents before clearing it: the resolve task
        // reads the editor lazily, so clearing first would wipe the contents.
        let contents = self.resolve_message_contents(&message_editor, cx);
        self.thread_error.take();
        self.thread_feedback.clear();
        self.editing_message.take();

        cx.spawn_in(window, async move |this, cx| {
            let (mut content, tracked_buffers) = contents.await?;

            cx.update(|window, cx| {
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
            })?;

            // Strip the leading `/command` from the first text block; whatever
            // remains (including any later mention blocks) becomes the queued
            // follow-up message.
            if let Some(acp::ContentBlock::Text(text_content)) = content.first_mut() {
                text_content.text = strip_leading_command(&text_content.text, &command_name);
            }
            if matches!(
                content.first(),
                Some(acp::ContentBlock::Text(text)) if text.text.trim().is_empty()
            ) {
                content.remove(0);
            }

            let command_block =
                acp::ContentBlock::Text(acp::TextContent::new(format!("/{command_name}")));

            this.update_in(cx, |this, window, cx| {
                // Queue the remainder first, then start the command turn; the
                // queue auto-processes when the command turn stops.
                if !content.is_empty() {
                    this.add_to_queue(content, tracked_buffers, window, cx);
                }
                this.send_content(
                    Task::ready(Ok(Some((vec![command_block], Vec::new())))),
                    true,
                    window,
                    cx,
                );
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Pending review comments left on the workspace's diff editors, composed
    /// into content blocks and cleared. Empty unless the user has left review
    /// comments on a "changes since" diff.
    fn take_pending_review_blocks(&self, cx: &mut App) -> Vec<acp::ContentBlock> {
        self.workspace
            .upgrade()
            .map(|workspace| crate::diff_review::take_pending_review_blocks(&workspace, cx))
            .unwrap_or_default()
    }

    /// Number of review comments pending on the workspace's diff editors, for
    /// the "N review comments will be attached" indicator by the input.
    fn pending_review_comment_count(&self, cx: &App) -> usize {
        self.workspace
            .upgrade()
            .map(|workspace| crate::diff_review::pending_review_comment_count(&workspace, cx))
            .unwrap_or(0)
    }

    fn clear_pending_review_comments(&mut self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            crate::diff_review::clear_pending_review_comments(&workspace, cx);
        }
        cx.notify();
    }

    /// The branches of the worktrees THIS thread works in. One agent runs per
    /// worktree, so the thread's own work dirs (its worktree roots before the
    /// agent reports any) select the worktrees; the repository's other linked
    /// worktrees belong to other threads and must not contribute branches.
    fn thread_branches(&self, cx: &App) -> Vec<(PathBuf, String)> {
        let Some(project) = self.project.upgrade() else {
            return Vec::new();
        };
        let project = project.read(cx);

        // Every worktree in the project that has a branch, keyed by its path:
        // the repositories themselves plus the linked worktrees they know of.
        let mut worktree_branches: Vec<(PathBuf, String)> = Vec::new();
        for repo in project.repositories(cx).values() {
            let snapshot = repo.read(cx).snapshot();
            if let Some(branch) = &snapshot.branch {
                worktree_branches.push((
                    snapshot.work_directory_abs_path.to_path_buf(),
                    branch.name().to_string(),
                ));
            }
            for linked in snapshot.linked_worktrees() {
                if let Some(branch) = linked.branch_name() {
                    worktree_branches.push((linked.path.to_path_buf(), branch.to_string()));
                }
            }
        }

        let thread_paths = self.thread_work_dirs(cx);
        branches_for_thread_paths(&thread_paths, &worktree_branches)
    }

    /// The directories THIS thread works in: the ones the agent reports, or,
    /// until it reports any, its project's worktree roots.
    fn thread_work_dirs(&self, cx: &App) -> Vec<PathBuf> {
        match self.thread.read(cx).work_dirs() {
            Some(work_dirs) if !work_dirs.paths().is_empty() => work_dirs
                .paths()
                .iter()
                .map(|path| path.to_path_buf())
                .collect(),
            _ => self
                .project
                .upgrade()
                .map(|project| {
                    project
                        .read(cx)
                        .visible_worktrees(cx)
                        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn sync_branch_diff_work_dirs(&mut self, cx: &mut Context<Self>) {
        let work_dirs = self.thread_work_dirs(cx);
        self.branch_diff_stats
            .update(cx, |stats, cx| stats.set_work_dirs(work_dirs, cx));
    }

    /// PR badges for the thread's own worktree branch(es), sourced from the
    /// shared gh_status store (the sidebar keeps the branches watched). A branch
    /// with no PR gets a muted "no PR" pill so PR state is always visible.
    fn thread_pr_chips(&self, cx: &App) -> Vec<ui::ThreadItemPrChip> {
        let branches = self.thread_branches(cx);
        let store = gh_status::GhStatusStore::try_global(cx);
        let store = store.as_ref().map(|store| store.read(cx));
        // The same answer the sidebar row gives, from the same function: the
        // snapshot fallback and the "no PR" pill are not the sidebar's, they
        // are the thread's.
        gh_status::thread_pr_chips(
            branches
                .iter()
                .map(|(path, branch)| (path.as_path(), branch.as_str())),
            store,
            || {
                ThreadMetadataStore::try_global(cx).and_then(|store| {
                    store
                        .read(cx)
                        .pr_snapshot(self.root_thread_id)
                        .map(|snapshot| snapshot.prs.clone())
                })
            },
        )
    }

    pub fn send_impl(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let contents = self.resolve_message_contents(&message_editor, cx);
        // Pending review comments on the workspace's diffs attach to this
        // message. Taken now (synchronously) so they clear as the turn starts.
        let review_blocks = self.take_pending_review_blocks(cx);

        self.thread_error.take();
        self.thread_feedback.clear();
        self.editing_message.take();
        // Sending a message is active engagement: un-freeze the queue if it
        // was paused by a manual stop.
        self.message_queue.resume();

        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }

        let contents_task = cx.spawn_in(window, async move |_this, cx| {
            let (mut contents, tracked_buffers) = contents.await?;

            if contents.is_empty() && review_blocks.is_empty() {
                return Ok(None);
            }

            // The user's own message leads; the review comments follow it.
            contents.extend(review_blocks);

            let _ = cx.update(|window, cx| {
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
            });

            Ok(Some((contents, tracked_buffers)))
        });

        self.send_content(contents_task, false, window, cx);
    }

    pub fn send_content(
        &mut self,
        contents_task: Task<anyhow::Result<Option<(Vec<acp::ContentBlock>, Vec<Entity<Buffer>>)>>>,
        is_native_command: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.thread.read(cx).session_id().clone();
        let parent_session_id = self.thread.read(cx).parent_session_id().cloned();
        let agent_telemetry_id = self.thread.read(cx).connection().telemetry_id();
        let is_first_message = self.thread.read(cx).entries().is_empty();
        let thread = self.thread.downgrade();

        self.is_loading_contents = true;

        let model_id = self.current_model_id(cx);
        let mode_id = self.current_mode_id(cx);
        let guard = cx.new(|_| ());
        cx.observe_release(&guard, |this, _guard, cx| {
            this.is_loading_contents = false;
            cx.notify();
        })
        .detach();

        let side = crate::agent_sidebar_side(cx);

        let task = cx.spawn_in(window, async move |this, cx| {
            let Some((contents, tracked_buffers)) = contents_task.await? else {
                return Ok(());
            };

            let generation = this.update(cx, |this, cx| {
                this.clear_external_source_prompt_warning(cx);
                let generation = this.start_turn(cx);
                this.in_flight_prompt = Some(contents.clone());
                generation
            })?;

            this.update_in(cx, |this, _window, cx| {
                this.set_editor_is_expanded(false, cx);
            })?;

            let _ = this.update(cx, |this, cx| {
                this.list_state.scroll_to_end();
                cx.notify();
            });

            let _stop_turn = defer({
                let this = this.clone();
                let mut cx = cx.clone();
                move || {
                    this.update(&mut cx, |this, cx| {
                        this.stop_turn(generation, cx);
                        cx.notify();
                    })
                    .ok();
                }
            });
            if is_first_message && thread.read_with(cx, |thread, _cx| thread.title().is_none())? {
                let text: String = contents
                    .iter()
                    .filter_map(|block| match block {
                        acp::ContentBlock::Text(text_content) => Some(text_content.text.clone()),
                        acp::ContentBlock::ResourceLink(resource_link) => {
                            Some(format!("@{}", resource_link.name))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = text.lines().next().unwrap_or("").trim();
                if !text.is_empty() {
                    let title: SharedString =
                        util::truncate_and_trailoff(text, PROVISIONAL_TITLE_LEN).into();
                    thread.update(cx, |thread, cx| {
                        thread.set_provisional_title(title, cx);
                    })?;
                }
            }

            let turn_start_time = Instant::now();
            let send = thread.update(cx, |thread, cx| {
                thread.action_log().update(cx, |action_log, cx| {
                    for buffer in tracked_buffers {
                        action_log.buffer_read(buffer, cx)
                    }
                });
                drop(guard);

                telemetry::event!(
                    "Agent Message Sent",
                    agent = agent_telemetry_id,
                    session = session_id,
                    parent_session_id = parent_session_id.as_ref().map(|id| id.to_string()),
                    model = model_id,
                    mode = mode_id,
                    side = side
                );

                if is_native_command {
                    thread.send_command(contents, cx)
                } else {
                    thread.send(contents, cx)
                }
            })?;

            this.update(cx, |_this, cx| cx.notify()).log_err();

            let res = send.await;
            let turn_time_ms = turn_start_time.elapsed().as_millis();
            drop(_stop_turn);
            let status = if res.is_ok() {
                let _ = this.update(cx, |this, _| this.in_flight_prompt.take());
                "success"
            } else {
                "failure"
            };
            telemetry::event!(
                "Agent Turn Completed",
                agent = agent_telemetry_id,
                session = session_id,
                parent_session_id = parent_session_id.as_ref().map(|id| id.to_string()),
                model = model_id,
                mode = mode_id,
                status,
                turn_time_ms,
                side = side
            );
            res.map(|_| ())
        });

        cx.spawn(async move |this, cx| {
            if let Err(err) = task.await {
                this.update(cx, |this, cx| {
                    this.handle_thread_error(err, cx);
                })
                .ok();
            } else {
                this.update(cx, |this, cx| {
                    let should_be_following = this
                        .workspace
                        .update(cx, |workspace, _| {
                            workspace.is_being_followed(CollaboratorId::Agent)
                        })
                        .unwrap_or_default();
                    this.should_be_following = should_be_following;
                    this.generate_title_if_needed(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Gives the thread a short generated title when its agent supplied none.
    ///
    /// External ACP agents are supposed to send a title through
    /// `SessionInfoUpdate`; Claude does, Codex never does, and the thread is
    /// then left showing the provisional title taken from the user's first
    /// message. At the end of a turn, any thread that still has no
    /// agent-supplied title (only the provisional one, or none) gets one
    /// generated locally with the summarization model, the same machinery the
    /// native agent's titles use. Threads whose agent did supply a title, user
    /// renames (a metadata title override), subagents, and native threads
    /// (which generate their own) are all left alone.
    fn generate_title_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.is_subagent() || self.title_generation.is_some() {
            return;
        }
        if self.as_native_thread(cx).is_some() {
            return;
        }

        let thread = self.thread.read(cx);
        let agent_supplied_title = thread.title().is_some() && !thread.has_provisional_title();
        if agent_supplied_title {
            return;
        }

        let thread_id = self.root_thread_id;
        let user_renamed = ThreadMetadataStore::try_global(cx).is_some_and(|store| {
            store
                .read(cx)
                .entry(thread_id)
                .is_some_and(|metadata| metadata.title_override.is_some())
        });
        if user_renamed {
            return;
        }

        let Some(request) = self.build_title_request(cx) else {
            return;
        };
        let Some(model) = LanguageModelRegistry::read_global(cx).thread_summary_model(cx) else {
            return;
        };
        let model = model.model;

        self.title_generation = Some(cx.spawn(async move |this, cx| {
            let title = agent::stream_thread_title(model, request, cx)
                .await
                .log_err();
            this.update(cx, |this, cx| {
                this.title_generation = None;
                let Some(title) = title else {
                    return;
                };
                let title: SharedString = title.trim().trim_matches('"').to_string().into();
                if title.is_empty() {
                    return;
                }
                this.thread
                    .update(cx, |thread, cx| thread.set_title(title.clone(), cx))
                    .detach_and_log_err(cx);
                if let Some(store) = ThreadMetadataStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store.set_generated_title(thread_id, title, cx);
                    });
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// A summarization request over the thread's messages, ending in the same
    /// "name this thread" prompt the native agent's title generation uses.
    fn build_title_request(&self, cx: &App) -> Option<language_model::LanguageModelRequest> {
        use language_model::{LanguageModelRequest, LanguageModelRequestMessage, Role};

        let mut messages: Vec<LanguageModelRequestMessage> = Vec::new();
        for entry in self.thread.read(cx).entries() {
            let (role, text) = match entry {
                AgentThreadEntry::UserMessage(message) => {
                    (Role::User, message.content.to_markdown(cx).to_string())
                }
                AgentThreadEntry::AssistantMessage(message) => {
                    (Role::Assistant, message.to_markdown(cx))
                }
                _ => continue,
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            messages.push(LanguageModelRequestMessage {
                role,
                content: vec![util::truncate_and_trailoff(text, TITLE_REQUEST_MESSAGE_LEN).into()],
                cache: false,
                reasoning_details: None,
            });
            if messages.len() == TITLE_REQUEST_MESSAGE_COUNT {
                break;
            }
        }
        if messages.is_empty() {
            return None;
        }

        messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![agent_settings::SUMMARIZE_THREAD_PROMPT.into()],
            cache: false,
            reasoning_details: None,
        });

        Some(LanguageModelRequest {
            intent: Some(language_model::CompletionIntent::ThreadSummarization),
            messages,
            ..Default::default()
        })
    }

    pub fn interrupt_and_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;

        if self.is_loading_contents {
            return;
        }

        cx.emit(AcpThreadViewEvent::Interacted);

        let message_editor = self.message_editor.clone();
        if thread.read(cx).status() == ThreadStatus::Idle {
            self.send_impl(message_editor, window, cx);
            return;
        }

        self.stop_current_and_send_new_message(message_editor, window, cx);
    }

    fn stop_current_and_send_new_message(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = self.thread.clone();
        self.message_queue.pause();

        let cancelled = thread.update(cx, |thread, cx| thread.cancel(cx));

        cx.spawn_in(window, async move |this, cx| {
            cancelled.await;

            this.update_in(cx, |this, window, cx| {
                this.send_impl(message_editor, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn handle_thread_error(
        &mut self,
        error: impl Into<ThreadError>,
        cx: &mut Context<Self>,
    ) {
        let error = error.into();
        self.emit_thread_error_telemetry(&error, cx);
        self.thread_error = Some(error);
        // The rendered markdown is built from the error it is shown with; a new
        // error must not keep showing the previous one's text.
        self.thread_error_markdown = None;
        cx.notify();
    }

    fn emit_thread_error_telemetry(&self, error: &ThreadError, cx: &mut Context<Self>) {
        let (error_kind, acp_error_code, message): (&str, Option<SharedString>, SharedString) =
            match error {
                ThreadError::PaymentRequired => (
                    "payment_required",
                    None,
                    "You reached your free usage limit. Upgrade to Zed Pro for more prompts."
                        .into(),
                ),
                ThreadError::Refusal => {
                    let model_or_agent_name = self.current_model_name(cx);
                    let message = format!(
                        "{} refused to respond to this prompt. This can happen when a model believes the prompt violates its content policy or safety guidelines, so rephrasing it can sometimes address the issue.",
                        model_or_agent_name
                    );
                    ("refusal", None, message.into())
                }
                ThreadError::DataRetentionConsentRequired => {
                    let message = format!(
                        "{} is not available with Zero Data Retention.",
                        self.current_model_name(cx)
                    );
                    ("data_retention_consent_required", None, message.into())
                }
                ThreadError::AuthenticationRequired(message) => {
                    ("authentication_required", None, message.clone())
                }
                ThreadError::RateLimitExceeded { provider } => (
                    "rate_limit_exceeded",
                    None,
                    format!("{provider}'s rate limit was reached.").into(),
                ),
                ThreadError::ServerOverloaded { provider } => (
                    "server_overloaded",
                    None,
                    format!("{provider}'s servers are temporarily unavailable.").into(),
                ),
                ThreadError::PromptTooLarge => (
                    "prompt_too_large",
                    None,
                    "Context too large for the model's context window.".into(),
                ),
                ThreadError::NoCredentials { provider } => (
                    "no_api_key",
                    None,
                    format!("No credentials configured for {provider}.").into(),
                ),
                ThreadError::StreamError { provider } => (
                    "stream_error",
                    None,
                    format!("Connection to {provider}'s API was interrupted.").into(),
                ),
                ThreadError::AuthenticationFailed { provider } => (
                    "invalid_api_key",
                    None,
                    format!("Authentication with {provider} failed.").into(),
                ),
                ThreadError::PermissionDenied { provider, message } => (
                    "permission_denied",
                    None,
                    message.clone().unwrap_or_else(|| {
                        format!(
                            "{provider}'s API rejected the request due to insufficient permissions."
                        )
                        .into()
                    }),
                ),
                ThreadError::ProviderRejection { message } => {
                    ("provider_rejection", None, message.clone())
                }
                ThreadError::MaxOutputTokens => (
                    "max_output_tokens",
                    None,
                    "Model reached its maximum output length.".into(),
                ),
                ThreadError::NoModelSelected => {
                    ("no_model_selected", None, "No model selected.".into())
                }
                ThreadError::ApiError { provider } => (
                    "api_error",
                    None,
                    format!("{provider}'s API returned an unexpected error.").into(),
                ),
                ThreadError::Other {
                    acp_error_code,
                    message,
                } => ("other", acp_error_code.clone(), message.clone()),
            };

        let agent_telemetry_id = self.thread.read(cx).connection().telemetry_id();
        let session_id = self.thread.read(cx).session_id().clone();
        let parent_session_id = self
            .thread
            .read(cx)
            .parent_session_id()
            .map(|id| id.to_string());

        telemetry::event!(
            "Agent Panel Error Shown",
            agent = agent_telemetry_id,
            session_id = session_id,
            parent_session_id = parent_session_id,
            kind = error_kind,
            acp_error_code = acp_error_code,
            message = message,
        );
    }

    pub fn cancel_generation(&mut self, cx: &mut Context<Self>) {
        self.thread_retry_status.take();
        self.thread_error.take();
        self.message_queue.pause();
        self._cancel_task = Some(self.thread.update(cx, |thread, cx| thread.cancel(cx)));
        cx.notify();
    }

    pub fn retry_generation(&mut self, cx: &mut Context<Self>) {
        self.thread_error.take();

        let thread = &self.thread;
        if !thread.read(cx).can_retry(cx) {
            return;
        }

        let task = thread.update(cx, |thread, cx| thread.retry(cx));
        cx.emit(AcpThreadViewEvent::Interacted);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;

            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.handle_thread_error(err, cx);
                }
            })
        })
        .detach();
    }

    pub fn regenerate(
        &mut self,
        entry_ix: usize,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_loading_contents {
            return;
        }
        let thread = self.thread.clone();

        let Some(client_id) = thread.update(cx, |thread, _| {
            thread
                .entries()
                .get(entry_ix)?
                .user_message()?
                .client_id
                .clone()
        }) else {
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            // Check if there are any edits from prompts before the one being regenerated.
            //
            // If there are, we keep/accept them since we're not regenerating the prompt that created them.
            //
            // If editing the prompt that generated the edits, they are auto-rejected
            // through the `rewind` function in the `acp_thread`.
            //
            // Subagent edits never show up as diffs in the parent thread's entries (they
            // are only forwarded to the parent's action log), so treat any earlier
            // subagent tool call as potentially having edits. Keeping all edits is a
            // no-op when the subagent didn't make any.
            let has_earlier_edits = thread.read_with(cx, |thread, _| {
                thread.entries().iter().take(entry_ix).any(|entry| {
                    entry.diffs().next().is_some()
                        || matches!(
                            entry,
                            AgentThreadEntry::ToolCall(tool_call) if tool_call.is_subagent()
                        )
                })
            });

            if has_earlier_edits {
                thread.update(cx, |thread, cx| {
                    thread.action_log().update(cx, |action_log, cx| {
                        action_log.keep_all_edits(None, cx);
                    });
                });
            }

            thread
                .update(cx, |thread, cx| thread.rewind(client_id, cx))
                .await?;
            this.update_in(cx, |thread, window, cx| {
                cx.emit(AcpThreadViewEvent::Interacted);
                thread.send_impl(message_editor, window, cx);
                thread.activation_focus_handle(cx).focus(window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    // message queueing

    fn queue_message(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_idle = self.thread.read(cx).status() == acp_thread::ThreadStatus::Idle;

        if is_idle {
            self.send_impl(message_editor, window, cx);
            return;
        }

        let contents = self.resolve_message_contents(&message_editor, cx);

        cx.spawn_in(window, async move |this, cx| {
            let (content, tracked_buffers) = contents.await?;

            if content.is_empty() {
                return Ok::<(), anyhow::Error>(());
            }

            this.update_in(cx, |this, window, cx| {
                this.add_to_queue(content, tracked_buffers, window, cx);
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
                cx.notify();
            })?;
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn add_to_queue(
        &mut self,
        content: Vec<acp::ContentBlock>,
        tracked_buffers: Vec<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The ID must be allocated up front so the editor event subscription
        // can capture it before the entry (which owns the subscription) exists.
        let id = self.message_queue.next_id();

        let editor = cx.new(|cx| {
            let mut editor = MessageEditor::new(
                self.workspace.clone(),
                self.project.clone(),
                None,
                self.session_capabilities.clone(),
                self.agent_id.clone(),
                "",
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(10),
                },
                window,
                cx,
            );
            editor.set_read_only(true, cx);
            editor.set_message(content.clone(), window, cx);
            editor
        });

        let subscription =
            cx.subscribe_in(&editor, window, move |this, _editor, event, window, cx| {
                this.handle_queue_editor_event(id, event, window, cx);
            });

        self.message_queue.enqueue(QueueEntry {
            id,
            content,
            tracked_buffers,
            steer: false,
            editor,
            _subscription: subscription,
        });
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    fn handle_queue_editor_event(
        &mut self,
        id: QueueEntryId,
        event: &MessageEditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            MessageEditorEvent::InputAttempted {
                attempt,
                cursor_offset,
            } => {
                self.move_queued_message_to_main_editor(
                    id,
                    Some(attempt.clone()),
                    Some(*cursor_offset),
                    window,
                    cx,
                );
            }
            MessageEditorEvent::LostFocus => {
                self.save_queued_message(id, cx);
            }
            MessageEditorEvent::Cancel | MessageEditorEvent::Send => {
                window.focus(&self.message_editor.focus_handle(cx), cx);
            }
            MessageEditorEvent::SendImmediately => {
                self.send_queued_message_now(id, window, cx);
            }
            _ => {}
        }
    }

    fn save_queued_message(&mut self, id: QueueEntryId, cx: &mut Context<Self>) {
        let Some(entry) = self.message_queue.entry_by_id(id) else {
            return;
        };
        let contents_task = entry
            .editor
            .update(cx, |editor, cx| editor.contents(false, cx));

        cx.spawn(async move |this, cx| {
            let (content, tracked_buffers) = contents_task.await?;

            this.update(cx, |this, cx| {
                if let Some(entry) = this.message_queue.entry_by_id_mut(id) {
                    entry.content = content;
                    entry.tracked_buffers = tracked_buffers;
                }
                cx.notify();
            })?;

            Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
    }

    pub fn remove_from_queue(
        &mut self,
        id: QueueEntryId,
        cx: &mut Context<Self>,
    ) -> Option<QueueEntry> {
        let removed = self.message_queue.remove(id);
        if removed.is_some() {
            self.sync_queue_flag_to_native_thread(cx);
        }
        removed
    }

    fn toggle_queue_entry_steer(&mut self, id: QueueEntryId, cx: &mut Context<Self>) {
        self.message_queue.toggle_steer(id);
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    pub fn sync_queue_flag_to_native_thread(&self, cx: &mut Context<Self>) {
        if let Some(native_thread) = self.as_native_thread(cx) {
            // By default queued messages wait for the turn to fully complete.
            // Only a "steering" front message ends the turn at the next boundary.
            let end_at_boundary = self.message_queue.front_wants_steer();
            native_thread.update(cx, |thread, _| {
                thread.set_end_turn_at_next_boundary(end_at_boundary);
            });
        }
    }

    pub fn send_queued_message_now(
        &mut self,
        id: QueueEntryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_generating = self.thread.read(cx).status() == acp_thread::ThreadStatus::Generating;
        if let Some(entry) = self.message_queue.send_now(id, is_generating) {
            self.dispatch_queued_entry(entry, window, cx);
        }
    }

    /// The shared "actually send this entry" path, used by fast-track,
    /// auto-processing on Stopped, and "Send Now". The entry must already have
    /// been removed from the queue.
    pub fn dispatch_queued_entry(
        &mut self,
        entry: QueueEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_queue_flag_to_native_thread(cx);

        cx.emit(AcpThreadViewEvent::Interacted);

        self.message_editor.focus_handle(cx).focus(window, cx);

        let content = entry.content;
        let tracked_buffers = entry.tracked_buffers;

        // A queued message can itself be a built-in command (e.g. the user typed
        // `/compact` while a turn was generating). Detect that so we run it as a
        // command turn without echoing it as a user message, matching the
        // non-queued path.
        let is_native_command = content
            .first()
            .and_then(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .and_then(|text| {
                leading_native_command(text, self.session_capabilities.read().available_commands())
            })
            .is_some();

        let cancelled = self.thread.update(cx, |thread, cx| thread.cancel(cx));

        let workspace = self.workspace.clone();

        let should_be_following = self.should_be_following;
        let contents_task = cx.spawn_in(window, async move |_this, cx| {
            cancelled.await;
            if should_be_following {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.follow(CollaboratorId::Agent, window, cx);
                    })
                    .ok();
            }

            Ok(Some((content, tracked_buffers)))
        });

        self.send_content(contents_task, is_native_command, window, cx);
    }

    pub fn move_queued_message_to_main_editor(
        &mut self,
        id: QueueEntryId,
        attempt: Option<InputAttempt>,
        cursor_offset: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(queued_message) = self.remove_from_queue(id, cx) else {
            return false;
        };
        let queued_content = queued_message.content;
        let message_editor = self.message_editor.clone();

        window.focus(&message_editor.focus_handle(cx), cx);

        let adjusted_cursor_offset = if message_editor.read(cx).is_empty(cx) {
            message_editor.update(cx, |editor, cx| {
                editor.set_message(queued_content, window, cx);
            });
            cursor_offset
        } else {
            let existing_len = message_editor.read(cx).text(cx).len();
            let separator = "\n\n";
            message_editor.update(cx, |editor, cx| {
                editor.append_message(queued_content, Some(separator), window, cx);
            });
            cursor_offset.map(|offset| existing_len + separator.len() + offset)
        };

        message_editor.update(cx, |editor, cx| {
            if let Some(offset) = adjusted_cursor_offset {
                editor.set_cursor_offset(offset, window, cx);
            }
            match attempt {
                Some(InputAttempt::Text(text)) => {
                    editor.insert_text(&text, window, cx);
                }
                Some(InputAttempt::Paste(clipboard)) => {
                    editor.paste_item(&clipboard, window, cx);
                }
                None => {}
            }
        });

        cx.notify();
        true
    }

    fn handle_message_editor_move_up(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.message_editor.read(cx).is_empty(cx) {
            cx.propagate();
            return;
        }
        let Some(last_id) = self.message_queue.last_id() else {
            cx.propagate();
            return;
        };
        self.move_queued_message_to_main_editor(last_id, None, None, window, cx);
    }

    // editor methods

    pub fn expand_message_editor(
        &mut self,
        _: &ExpandMessageEditor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.list_state.item_count() == 0 {
            return;
        }
        self.set_editor_is_expanded(!self.editor_expanded, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub fn set_editor_is_expanded(&mut self, is_expanded: bool, cx: &mut Context<Self>) {
        self.editor_expanded = is_expanded;
        self.sync_editor_mode(cx);
        cx.notify();
    }

    pub fn handle_title_editor_event(
        &mut self,
        title_editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                // We only want to set the title if the user has actively edited
                // it. If the title editor is not focused, we programmatically
                // changed the text, so we don't want to set the title again.
                if !title_editor.read(cx).is_focused(window) {
                    return;
                }

                let new_title = title_editor.read(cx).text(cx);
                if new_title.is_empty() {
                    return;
                }
                self.apply_renamed_title(SharedString::from(new_title), cx);
            }
            EditorEvent::Blurred => {
                if title_editor.read(cx).text(cx).is_empty() {
                    title_editor.update(cx, |editor, cx| {
                        editor.set_text(DEFAULT_THREAD_TITLE, window, cx);
                    });
                }
            }
            _ => {}
        }
    }

    /// Renames the thread, mirroring the editor text and persisting the new
    /// title. Used by callers outside of the title editor (e.g. the sidebar's
    /// inline rename) so that they go through the same persistence path as
    /// the in-thread title editor.
    pub fn rename(&mut self, title: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_editor.read(cx).text(cx) != title.as_ref() {
            self.title_editor.update(cx, |editor, cx| {
                editor.set_text(title.clone(), window, cx);
            });
        }
        self.apply_renamed_title(title, cx);
    }

    fn apply_renamed_title(&mut self, title: SharedString, cx: &mut Context<Self>) {
        if let Some(store) = ThreadMetadataStore::try_global(cx)
            && !self.is_subagent()
        {
            let thread_id = self.root_thread_id;
            store.update(cx, |store, cx| {
                store.set_title_override(thread_id, title.clone(), cx);
            });
        }
        self.thread.update(cx, |thread, cx| {
            if thread.can_set_title(cx) {
                thread.set_title(title, cx).detach_and_log_err(cx);
            }
        });
    }

    pub fn cancel_editing(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.editing_message.take()
            && let Some(editor) = &self
                .entry_view_state
                .read(cx)
                .entry(index)
                .and_then(|e| e.message_editor())
                .cloned()
        {
            editor.update(cx, |editor, cx| {
                if let Some(user_message) = self
                    .thread
                    .read(cx)
                    .entries()
                    .get(index)
                    .and_then(|e| e.user_message())
                {
                    editor.set_message(user_message.chunks.clone(), window, cx);
                }
            })
        };
        self.message_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub fn authorize_tool_call(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        outcome: SelectedPermissionOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_tool_call(session_id, tool_call_id, outcome, cx);
        });
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
    }

    pub fn allow_always(&mut self, _: &AllowAlways, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_allow_blocked_by_confusables(cx) {
            return;
        }
        self.authorize_pending_tool_call(acp::PermissionOptionKind::AllowAlways, window, cx);
    }

    pub fn allow_once(&mut self, _: &AllowOnce, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_allow_blocked_by_confusables(cx) {
            return;
        }
        self.authorize_pending_with_granularity(true, window, cx);
    }

    /// Whether the currently pending permission prompt is blocked by an
    /// unacknowledged surprising-Unicode warning, so the keyboard allow
    /// shortcuts must be ignored (mirroring the disabled allow buttons).
    fn pending_allow_blocked_by_confusables(&self, cx: &Context<Self>) -> bool {
        let session_id = self.thread.read(cx).session_id().clone();
        let Some((_, tool_call_id, _)) = self
            .conversation
            .read(cx)
            .pending_tool_call(&session_id, cx)
        else {
            return false;
        };
        self.thread.read(cx).entries().iter().any(|entry| {
            matches!(
                entry,
                AgentThreadEntry::ToolCall(call)
                    if call.id == tool_call_id && self.sandbox_confusables_block_allow(call, cx)
            )
        })
    }

    pub fn reject_once(&mut self, _: &RejectOnce, window: &mut Window, cx: &mut Context<Self>) {
        self.authorize_pending_with_granularity(false, window, cx);
    }

    pub fn authorize_pending_tool_call(
        &mut self,
        kind: acp::PermissionOptionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let session_id = self.thread.read(cx).session_id().clone();
        self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_pending_tool_call(&session_id, kind, cx)
        })?;
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
        Some(())
    }

    fn has_pending_request_elicitation(&self, cx: &App) -> bool {
        self.server_view
            .read_with(cx, |server_view, cx| {
                server_view
                    .request_elicitation_store()
                    .is_some_and(|store| {
                        store.read(cx).elicitations().iter().any(|elicitation| {
                            matches!(elicitation.status, ElicitationStatus::Pending { .. })
                        })
                    })
            })
            .unwrap_or(false)
    }

    pub fn sync_elicitation_state_for_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let elicitation_id = {
            let thread = self.thread.read(cx);
            let Some(AgentThreadEntry::Elicitation(elicitation_id)) = thread.entries().get(index)
            else {
                return;
            };
            elicitation_id.clone()
        };

        let thread = self.thread.read(cx);
        let entry = thread.elicitation(&elicitation_id).map(|(_, elicitation)| {
            (
                elicitation_id.clone(),
                matches!(elicitation.status, ElicitationStatus::Pending { .. }),
                match &elicitation.request.mode {
                    acp::ElicitationMode::Form(mode) => Some(mode.requested_schema.clone()),
                    _ => None,
                },
            )
        });

        let Some((id, is_pending, schema)) = entry else {
            return;
        };

        if is_pending
            && let Some(schema) = schema
            && !self.elicitation_form_states.contains_key(&id)
        {
            self.elicitation_form_states
                .insert(id, ElicitationFormState::new(&schema, window, cx));
        } else if !is_pending {
            self.elicitation_form_states.remove(&id);
        }
    }

    fn sync_existing_elicitation_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry_count = self.thread.read(cx).entries().len();
        for index in 0..entry_count {
            self.sync_elicitation_state_for_entry(index, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_elicitation_form_state(&self, id: &ElicitationEntryId) -> bool {
        self.elicitation_form_states.contains_key(id)
    }

    fn submit_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self
            .thread
            .read(cx)
            .elicitation(&elicitation_id)
            .map(|(_, elicitation)| elicitation.request.mode.clone());

        let Some(mode) = mode else {
            return;
        };

        match mode {
            acp::ElicitationMode::Form(mode) => {
                let Some(state) = self.elicitation_form_states.get_mut(&elicitation_id) else {
                    return;
                };
                let Some(submission) = state.begin_submission(cx) else {
                    return;
                };
                let schema = mode.requested_schema;
                let validation_task = cx.background_spawn(async move {
                    let result = submission.validate(&schema);
                    (submission, result)
                });
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let (submission, result) = validation_task.await;
                    this.update(cx, |this, cx| {
                        let is_current = this
                            .elicitation_form_states
                            .get_mut(&elicitation_id)
                            .is_some_and(|state| {
                                state.validation_matches_current_values(&submission, cx)
                            });
                        if !is_current {
                            cx.notify();
                            return;
                        }
                        match result {
                            Ok(content) => {
                                this.respond_to_elicitation(
                                    elicitation_id,
                                    acp::CreateElicitationResponse::new(
                                        acp::ElicitationAction::Accept(
                                            acp::ElicitationAcceptAction::new().content(content),
                                        ),
                                    ),
                                    cx,
                                );
                            }
                            Err(errors) => {
                                if let Some(state) =
                                    this.elicitation_form_states.get_mut(&elicitation_id)
                                {
                                    state.set_errors(errors);
                                }
                                cx.notify();
                            }
                        }
                    })
                    .log_err();
                })
                .detach();
            }
            acp::ElicitationMode::Url(_) => {
                self.respond_to_elicitation(
                    elicitation_id,
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                        acp::ElicitationAcceptAction::new(),
                    )),
                    cx,
                );
            }
            _ => {}
        }
    }

    fn decline_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
            cx,
        );
    }

    fn cancel_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
            cx,
        );
    }

    fn dismiss_url_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.elicitation_form_states.remove(&elicitation_id);
        self.thread.update(cx, |thread, cx| {
            thread.cancel_elicitation(&elicitation_id, cx);
        });
        cx.notify();
    }

    fn respond_to_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        response: acp::CreateElicitationResponse,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.session_id.clone();
        self.elicitation_form_states.remove(&elicitation_id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.respond_to_elicitation(session_id, elicitation_id, response, cx);
        });
        cx.notify();
    }

    fn handle_authorize_tool_call(
        &mut self,
        action: &AuthorizeToolCall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());
        let option_id = acp::PermissionOptionId::new(action.option_id.clone());
        let option_kind = match action.option_kind.as_str() {
            "AllowOnce" => acp::PermissionOptionKind::AllowOnce,
            "AllowAlways" => acp::PermissionOptionKind::AllowAlways,
            "RejectOnce" => acp::PermissionOptionKind::RejectOnce,
            "RejectAlways" => acp::PermissionOptionKind::RejectAlways,
            _ => acp::PermissionOptionKind::AllowOnce,
        };

        let session_id = self.thread.read(cx).session_id().clone();
        self.authorize_tool_call(
            session_id,
            tool_call_id,
            SelectedPermissionOutcome::new(option_id, option_kind),
            window,
            cx,
        );
    }

    pub fn handle_select_permission_granularity(
        &mut self,
        action: &SelectPermissionGranularity,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());
        self.permission_selections
            .insert(tool_call_id, PermissionSelection::Choice(action.index));

        cx.notify();
    }

    pub fn handle_toggle_command_pattern(
        &mut self,
        action: &crate::ToggleCommandPattern,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());

        match self.permission_selections.get_mut(&tool_call_id) {
            Some(PermissionSelection::SelectedPatterns(checked)) => {
                // Already in pattern mode — toggle the individual pattern.
                if let Some(pos) = checked.iter().position(|&i| i == action.pattern_index) {
                    checked.swap_remove(pos);
                } else {
                    checked.push(action.pattern_index);
                }
            }
            _ => {
                // First click: activate "Select options" with all patterns checked.
                let thread = self.thread.read(cx);
                let pattern_count = thread
                    .entries()
                    .iter()
                    .find_map(|entry| {
                        if let AgentThreadEntry::ToolCall(call) = entry {
                            if call.id == tool_call_id {
                                if let ToolCallStatus::WaitingForConfirmation { options, .. } =
                                    &call.status
                                {
                                    if let PermissionOptions::DropdownWithPatterns {
                                        patterns,
                                        ..
                                    } = options
                                    {
                                        return Some(patterns.len());
                                    }
                                }
                            }
                        }
                        None
                    })
                    .unwrap_or(0);
                self.permission_selections.insert(
                    tool_call_id,
                    PermissionSelection::SelectedPatterns((0..pattern_count).collect()),
                );
            }
        }
        cx.notify();
    }

    fn authorize_pending_with_granularity(
        &mut self,
        is_allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let session_id = self.thread.read(cx).session_id().clone();
        let (returned_session_id, tool_call_id, _) = self
            .conversation
            .read(cx)
            .pending_tool_call(&session_id, cx)?;
        self.authorize_with_granularity(returned_session_id, tool_call_id, is_allow, window, cx)
    }

    fn authorize_with_granularity(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        is_allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let selection = self.permission_selections.get(&tool_call_id).cloned();
        let result = self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_with_granularity(
                session_id,
                tool_call_id,
                selection.as_ref(),
                is_allow,
                cx,
            )
        });
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
        result
    }

    // edits

    pub fn keep_all(&mut self, _: &KeepAll, _window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;
        let telemetry = ActionLogTelemetry::from(thread.read(cx));
        let action_log = thread.read(cx).action_log().clone();
        action_log.update(cx, |action_log, cx| {
            action_log.keep_all_edits(Some(telemetry), cx)
        });
    }

    pub fn reject_all(&mut self, _: &RejectAll, _window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;
        let telemetry = ActionLogTelemetry::from(thread.read(cx));
        let action_log = thread.read(cx).action_log().clone();
        let has_changes = action_log.read(cx).changed_buffers(cx).next().is_some();

        action_log
            .update(cx, |action_log, cx| {
                action_log.reject_all_edits(Some(telemetry), cx)
            })
            .detach();

        if has_changes {
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    crate::ui::show_undo_reject_toast(workspace, action_log, cx);
                });
            }
        }
    }

    pub fn undo_last_reject(
        &mut self,
        _: &UndoLastReject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &self.thread;
        let action_log = thread.read(cx).action_log().clone();
        action_log
            .update(cx, |action_log, cx| action_log.undo_last_reject(cx))
            .detach()
    }

    pub fn open_edited_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &self.thread;

        let Some(diff) =
            AgentDiffPane::deploy(thread.clone(), self.workspace.clone(), window, cx).log_err()
        else {
            return;
        };

        diff.update(cx, |diff, cx| {
            diff.move_to_path(PathKey::for_buffer(buffer, cx), window, cx)
        })
    }

    // thread stuff

    pub fn restore_checkpoint(&mut self, client_id: &ClientUserMessageId, cx: &mut Context<Self>) {
        self.thread
            .update(cx, |thread, cx| {
                thread.restore_checkpoint(client_id.clone(), cx)
            })
            .detach_and_log_err(cx);
    }

    pub fn clear_thread_error(&mut self, cx: &mut Context<Self>) {
        self.thread_error = None;
        self.thread_error_markdown = None;
        self.token_limit_callout_dismissed = true;
        cx.notify();
    }

    fn callout_border_position(&self) -> CalloutBorderPosition {
        if self.list_state.item_count() > 0 {
            CalloutBorderPosition::Top
        } else {
            CalloutBorderPosition::Bottom
        }
    }

    pub fn render_thread_retry_status_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        let state = self.thread_retry_status.as_ref()?;

        if let Some(fallback_model) = acp_thread::refusal_fallback_model_from_meta(&state.meta) {
            return Some(
                Callout::new()
                    .icon(IconName::Warning)
                    .severity(Severity::Warning)
                    .title(state.last_error.clone())
                    .description(format!("Retrying with {fallback_model}"))
                    .dismiss_action(
                        IconButton::new("dismiss-refusal-fallback", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.thread_retry_status = None;
                                cx.notify();
                            })),
                    ),
            );
        }

        let next_attempt_in = state
            .duration
            .saturating_sub(Instant::now().saturating_duration_since(state.started_at));
        if next_attempt_in.is_zero() {
            return None;
        }

        let next_attempt_in_secs = next_attempt_in.as_secs() + 1;

        let retry_message = if state.max_attempts == 1 {
            if next_attempt_in_secs == 1 {
                "Retrying. Next attempt in 1 second.".to_string()
            } else {
                format!("Retrying. Next attempt in {next_attempt_in_secs} seconds.")
            }
        } else if next_attempt_in_secs == 1 {
            format!(
                "Retrying. Next attempt in 1 second (Attempt {} of {}).",
                state.attempt, state.max_attempts,
            )
        } else {
            format!(
                "Retrying. Next attempt in {next_attempt_in_secs} seconds (Attempt {} of {}).",
                state.attempt, state.max_attempts,
            )
        };

        Some(
            Callout::new()
                .border_position(self.callout_border_position())
                .icon(IconName::Warning)
                .severity(Severity::Warning)
                .title(state.last_error.clone())
                .description(retry_message),
        )
    }

    fn activity_bar_bg(&self, cx: &Context<Self>) -> Hsla {
        let editor_bg_color = cx.theme().colors().editor_background;
        let active_color = cx.theme().colors().element_selected;
        editor_bg_color.blend(active_color.opacity(0.3))
    }

    pub fn render_activity_bar(
        &self,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let action_log = thread.action_log();
        let telemetry = ActionLogTelemetry::from(thread);
        let changed_buffers = action_log.read(cx).changed_buffers(cx).collect::<Vec<_>>();
        let queue_is_empty = !self.has_queued_messages();

        let awaiting_permission = self
            .render_main_agent_awaiting_permission(window, cx)
            .or_else(|| self.render_subagents_awaiting_permission(cx));
        let has_awaiting_permission = awaiting_permission.is_some();

        // The plan is not here: it lives in the thread's working indicator, the
        // one plan surface.
        if changed_buffers.is_empty() && queue_is_empty && !has_awaiting_permission {
            return None;
        }

        // Temporarily always enable ACP edit controls. This is temporary, to lessen the
        // impact of a nasty bug that causes them to sometimes be disabled when they shouldn't
        // be, which blocks you from being able to accept or reject edits. This switches the
        // bug to be that sometimes it's enabled when it shouldn't be, which at least doesn't
        // block you from using the panel.
        let pending_edits = false;

        let edits_expanded = self.edits_expanded;
        let queue_expanded = self.queue_expanded;

        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        // Drop shadows have no opaque surface to blend into on a transparent
        // window, so they render as a dark halo; only apply them when opaque.
        let opaque_window =
            cx.theme().window_background_appearance() == gpui::WindowBackgroundAppearance::Opaque;

        h_flex()
            .w_full()
            .px_2()
            .justify_center()
            .child(
                v_flex()
                    .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                    .when(max_content_width.is_none(), |this| this.w_full())
                    .flex_shrink_1()
                    .flex_grow_0()
                    .max_w_full()
                    .bg(self.activity_bar_bg(cx))
                    .border_1()
                    .border_b_0()
                    .border_color(cx.theme().colors().border)
                    .rounded_t_md()
                    .when(opaque_window, |this| {
                        this.shadow(vec![
                            gpui::BoxShadow::new(px(1.), px(-1.), gpui::black().opacity(0.12))
                                .blur_radius(px(2.)),
                        ])
                    })
                    .when_some(awaiting_permission, |this, element| this.child(element))
                    .when(
                        has_awaiting_permission && (!changed_buffers.is_empty() || !queue_is_empty),
                        |this| this.child(Divider::horizontal().color(DividerColor::Border)),
                    )
                    .when(
                        !changed_buffers.is_empty() && thread.parent_session_id().is_none(),
                        |this| {
                            this.child(self.render_edits_summary(
                                &changed_buffers,
                                edits_expanded,
                                pending_edits,
                                cx,
                            ))
                            .when(edits_expanded, |parent| {
                                parent.child(self.render_edited_files(
                                    action_log,
                                    telemetry.clone(),
                                    &changed_buffers,
                                    pending_edits,
                                    cx,
                                ))
                            })
                        },
                    )
                    .when(!queue_is_empty, |this| {
                        this.when(!changed_buffers.is_empty(), |this| {
                            this.child(Divider::horizontal().color(DividerColor::Border))
                        })
                        .child(self.render_message_queue_summary(window, cx))
                        .when(queue_expanded, |parent| {
                            parent.child(self.render_message_queue_entries(window, cx))
                        })
                    }),
            )
            .into_any()
            .into()
    }

    fn render_edited_files(
        &self,
        action_log: &Entity<ActionLog>,
        telemetry: ActionLogTelemetry,
        changed_buffers: &[(Entity<Buffer>, Entity<BufferDiff>)],
        pending_edits: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let editor_bg_color = cx.theme().colors().editor_background;

        // Sort edited files alphabetically for consistency with Git diff view
        let mut sorted_buffers: Vec<_> = changed_buffers.iter().collect();
        sorted_buffers.sort_by(|(buffer_a, _), (buffer_b, _)| {
            let path_a = buffer_a.read(cx).file().map(|f| f.path().clone());
            let path_b = buffer_b.read(cx).file().map(|f| f.path().clone());
            path_a.cmp(&path_b)
        });

        v_flex()
            .id("edited_files_list")
            .max_h_40()
            .overflow_y_scroll()
            .child(
                v_flex().children(sorted_buffers.into_iter().enumerate().flat_map(
                    |(index, (buffer, diff))| {
                        let file = buffer.read(cx).file()?;
                        let path = file.path();
                        let path_style = file.path_style(cx);
                        let separator = file.path_style(cx).primary_separator();

                        let fallback_full_path =
                            full_path_for_empty_project_path(file.as_ref(), cx);

                        let file_path = path.parent().and_then(|parent| {
                            if parent.is_empty() {
                                None
                            } else {
                                Some(
                                    Label::new(format!(
                                        "{}{separator}",
                                        parent.display(path_style)
                                    ))
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                                )
                            }
                        });

                        let file_name = path
                            .file_name()
                            .map(|name| {
                                Label::new(name.to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .ml_1()
                            })
                            .or_else(|| {
                                fallback_full_path.as_ref().map(|path| {
                                    Label::new(path.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .ml_1()
                                })
                            });

                        let full_path = fallback_full_path
                            .unwrap_or_else(|| path.display(path_style).to_string());

                        let file_icon = FileIcons::get_icon(path.as_std_path(), cx)
                            .map(Icon::from_path)
                            .map(|icon| icon.color(Color::Muted).size(IconSize::Small))
                            .unwrap_or_else(|| {
                                Icon::new(IconName::File)
                                    .color(Color::Muted)
                                    .size(IconSize::Small)
                            });

                        let file_stats = DiffStats::single_file(diff.read(cx));

                        let buttons = self.render_edited_files_buttons(
                            index,
                            buffer,
                            action_log,
                            &telemetry,
                            pending_edits,
                            editor_bg_color,
                            cx,
                        );

                        let element = h_flex()
                            .group("edited-code")
                            .id(("file-container", index))
                            .relative()
                            .min_w_0()
                            .p_1p5()
                            .gap_2()
                            .justify_between()
                            .bg(editor_bg_color)
                            .when(index < changed_buffers.len() - 1, |parent| {
                                parent.border_color(cx.theme().colors().border).border_b_1()
                            })
                            .child(
                                h_flex()
                                    .id(("file-name-path", index))
                                    .cursor_pointer()
                                    .pr_0p5()
                                    .gap_0p5()
                                    .rounded_xs()
                                    .child(file_icon)
                                    .children(file_name)
                                    .children(file_path)
                                    .child(
                                        DiffStat::new(
                                            "file",
                                            file_stats.lines_added as usize,
                                            file_stats.lines_removed as usize,
                                        )
                                        .label_size(LabelSize::XSmall),
                                    )
                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                    .tooltip({
                                        move |_, cx| {
                                            Tooltip::with_meta(
                                                "Go to File",
                                                None,
                                                full_path.clone(),
                                                cx,
                                            )
                                        }
                                    })
                                    .on_click({
                                        let buffer = buffer.clone();
                                        cx.listener(move |this, _, window, cx| {
                                            this.open_edited_buffer(&buffer, window, cx);
                                        })
                                    }),
                            )
                            .child(buttons);

                        Some(element)
                    },
                )),
            )
            .into_any_element()
    }

    fn render_edited_files_buttons(
        &self,
        index: usize,
        buffer: &Entity<Buffer>,
        action_log: &Entity<ActionLog>,
        telemetry: &ActionLogTelemetry,
        pending_edits: bool,
        editor_bg_color: Hsla,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id("edited-buttons-container")
            .visible_on_hover("edited-code")
            .absolute()
            .right_0()
            .px_1()
            .gap_1()
            .bg(editor_bg_color)
            .on_hover(cx.listener(move |this, is_hovered, _window, cx| {
                if *is_hovered {
                    this.hovered_edited_file_buttons = Some(index);
                } else if this.hovered_edited_file_buttons == Some(index) {
                    this.hovered_edited_file_buttons = None;
                }
                cx.notify();
            }))
            .child(
                Button::new("review", "Review")
                    .label_size(LabelSize::Small)
                    .on_click({
                        let buffer = buffer.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.open_edited_buffer(&buffer, window, cx);
                        })
                    }),
            )
            .child(
                Button::new(("reject-file", index), "Reject")
                    .label_size(LabelSize::Small)
                    .disabled(pending_edits)
                    .on_click({
                        let buffer = buffer.clone();
                        let action_log = action_log.clone();
                        let telemetry = telemetry.clone();
                        move |_, _, cx| {
                            action_log.update(cx, |action_log, cx| {
                                action_log
                                    .reject_edits_in_ranges(
                                        buffer.clone(),
                                        vec![Anchor::min_max_range_for_buffer(
                                            buffer.read(cx).remote_id(),
                                        )],
                                        Some(telemetry.clone()),
                                        cx,
                                    )
                                    .0
                                    .detach_and_log_err(cx);
                            })
                        }
                    }),
            )
            .child(
                Button::new(("keep-file", index), "Keep")
                    .label_size(LabelSize::Small)
                    .disabled(pending_edits)
                    .on_click({
                        let buffer = buffer.clone();
                        let action_log = action_log.clone();
                        let telemetry = telemetry.clone();
                        move |_, _, cx| {
                            action_log.update(cx, |action_log, cx| {
                                action_log.keep_edits_in_range(
                                    buffer.clone(),
                                    Anchor::min_max_range_for_buffer(buffer.read(cx).remote_id()),
                                    Some(telemetry.clone()),
                                    cx,
                                );
                            })
                        }
                    }),
            )
    }

    fn collect_subagent_items_for_sessions(
        entries: &[AgentThreadEntry],
        awaiting_session_ids: &[acp::SessionId],
        cx: &App,
    ) -> Vec<(SharedString, usize)> {
        let tool_calls_by_session: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .filter_map(|(entry_ix, entry)| {
                let AgentThreadEntry::ToolCall(tool_call) = entry else {
                    return None;
                };
                let info = tool_call.subagent_session_info.as_ref()?;
                let summary_text = tool_call.label.read(cx).source().to_string();
                let subagent_summary = if summary_text.is_empty() {
                    SharedString::from("Subagent")
                } else {
                    SharedString::from(summary_text)
                };
                Some((info.session_id.clone(), (subagent_summary, entry_ix)))
            })
            .collect();

        awaiting_session_ids
            .iter()
            .filter_map(|session_id| tool_calls_by_session.get(session_id).cloned())
            .collect()
    }

    fn render_subagents_awaiting_permission(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let awaiting = self.conversation.read(cx).subagents_awaiting_permission(cx);

        if awaiting.is_empty() {
            return None;
        }

        let awaiting_session_ids: Vec<_> = awaiting
            .iter()
            .map(|(session_id, _)| session_id.clone())
            .collect();

        let thread = self.thread.read(cx);
        let entries = thread.entries();
        let subagent_items =
            Self::collect_subagent_items_for_sessions(entries, &awaiting_session_ids, cx);

        if subagent_items.is_empty() {
            return None;
        }

        let item_count = subagent_items.len();

        Some(
            v_flex()
                .child(
                    h_flex()
                        .py_1()
                        .px_2()
                        .w_full()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new("Subagents Awaiting Permission:")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(item_count.to_string()).size(LabelSize::Small)),
                )
                .child(
                    v_flex().children(subagent_items.into_iter().enumerate().map(
                        |(ix, (label, entry_ix))| {
                            let is_last = ix == item_count - 1;
                            let group = format!("group-{}", entry_ix);

                            h_flex()
                                .cursor_pointer()
                                .id(format!("subagent-permission-{}", entry_ix))
                                .group(&group)
                                .p_1()
                                .pl_2()
                                .min_w_0()
                                .w_full()
                                .gap_1()
                                .justify_between()
                                .bg(cx.theme().colors().editor_background)
                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                .when(!is_last, |this| {
                                    this.border_b_1().border_color(cx.theme().colors().border)
                                })
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .child(
                                            Icon::new(IconName::Circle)
                                                .size(IconSize::XSmall)
                                                .color(Color::Warning),
                                        )
                                        .child(
                                            Label::new(label)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    div().visible_on_hover(&group).child(
                                        Label::new("Scroll to Subagent")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.list_state.scroll_to(ListOffset {
                                        item_ix: entry_ix,
                                        offset_in_item: px(0.0),
                                    });
                                    cx.notify();
                                }))
                        },
                    )),
                )
                .into_any(),
        )
    }

    pub(crate) fn render_main_agent_awaiting_permission(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if self.is_subagent() {
            return None;
        }

        let active_session_id = self.thread.read(cx).session_id().clone();
        let conversation = self.conversation.read(cx);
        let tool_call_id = conversation.pending_tool_call_for_session(&active_session_id, cx)?;
        let pending_count = conversation.pending_tool_call_count_for_session(&active_session_id);

        let thread = self.thread.read(cx);
        let (entry_ix, tool_call) = thread.tool_call(&tool_call_id)?;

        let scroll_icon = if self.list_state.item_is_above_viewport(entry_ix)? {
            IconName::ArrowUp
        } else if self.list_state.item_is_below_viewport(entry_ix)? {
            IconName::ArrowDown
        } else {
            return None;
        };

        let focus_handle = self.focus_handle(cx);

        let card = self.render_any_tool_call(
            &active_session_id,
            entry_ix,
            tool_call,
            &focus_handle,
            ToolCallLayout::Floating,
            window,
            cx,
        );

        let label: SharedString = if pending_count > 1 {
            format!("Awaiting Confirmation ({pending_count})").into()
        } else {
            "Awaiting Confirmation".into()
        };

        let header = h_flex()
            .p_1p5()
            .pl_2()
            .w_full()
            .gap_1p5()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(GeneratingSpinnerElement::new(SpinnerVariant::Sand)),
                    )
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                Button::new("main-agent-permission-scroll-to", "Scroll")
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(scroll_icon)
                            .size(IconSize::XSmall)
                            .color(Color::Default),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.list_state.scroll_to(ListOffset {
                            item_ix: entry_ix,
                            offset_in_item: px(0.0),
                        });
                        cx.notify();
                    })),
            );

        Some(v_flex().child(header).child(card).into_any())
    }

    fn render_message_queue_summary(
        &self,
        _window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let queue_count = self.message_queue.len();
        let title: SharedString = if queue_count == 1 {
            "1 Queued Message".into()
        } else {
            format!("{} Queued Messages", queue_count).into()
        };

        h_flex()
            .p_1()
            .w_full()
            .gap_1()
            .justify_between()
            .when(self.queue_expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(
                h_flex()
                    .id("queue_summary")
                    .gap_1()
                    .child(Disclosure::new("queue_disclosure", self.queue_expanded))
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.queue_expanded = !this.queue_expanded;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("clear_queue", "Clear All")
                    .label_size(LabelSize::Small)
                    .key_binding(
                        KeyBinding::for_action(&ClearMessageQueue, cx)
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_queue(cx);
                    })),
            )
            .into_any_element()
    }

    fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.message_queue.clear();
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    /// The compaction barrier for agents that report compaction as a tool
    /// call: the same transcript-wide marker as native compaction, with
    /// nothing to expand.
    fn render_compaction_barrier(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        cx: &Context<Self>,
    ) -> AnyElement {
        let header_label = match tool_call.status {
            ToolCallStatus::Pending | ToolCallStatus::InProgress => "Compacting Context…",
            ToolCallStatus::Canceled | ToolCallStatus::Rejected | ToolCallStatus::Failed => {
                "Compaction Canceled"
            }
            _ => "Context Compacted",
        };
        let accent = cx.theme().colors().text_accent;

        let marker = h_flex()
            .id(("compaction-barrier", entry_ix))
            .gap_1()
            .px_2()
            .py_0p5()
            .flex_none()
            .rounded_md()
            .border_1()
            .border_color(accent.opacity(0.4))
            .bg(accent.opacity(0.1))
            .child(
                Icon::new(IconName::Compact)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
            .child(
                Label::new(header_label)
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::MEDIUM)
                    .color(Color::Accent),
            );

        div()
            .px_5()
            .w_full()
            .child(
                h_flex()
                    .pt_1p5()
                    .mb_1p5()
                    .gap_2()
                    .w_full()
                    .child(Divider::horizontal())
                    .child(marker)
                    .child(Divider::horizontal()),
            )
            .into_any_element()
    }

    fn render_context_compaction(
        &self,
        entry_ix: usize,
        compaction: &acp_thread::ContextCompaction,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_compacting = compaction.is_in_progress();
        let summary = compaction.summary.clone();
        let is_expanded = self
            .entry_view_state
            .read(cx)
            .is_compaction_expanded(entry_ix);

        let header_label = match compaction.status {
            acp_thread::ContextCompactionStatus::InProgress => "Compacting Context…",
            acp_thread::ContextCompactionStatus::Completed => "Context Compacted",
            acp_thread::ContextCompactionStatus::Canceled => "Compaction Canceled",
        };
        // Compaction is a break in the conversation, not a message in it: it
        // renders as one unmistakable marker across the transcript. Only a
        // compaction that produced a summary has anything to expand (external
        // agents compact without one).
        let expandable = summary.is_some() && !is_compacting;
        let chevron_end = if is_expanded {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };
        let accent = cx.theme().colors().text_accent;

        let marker = h_flex()
            .id(("context-compaction", entry_ix))
            .gap_1()
            .px_2()
            .py_0p5()
            .flex_none()
            .rounded_md()
            .border_1()
            .border_color(accent.opacity(0.4))
            .bg(accent.opacity(0.1))
            .child(
                Icon::new(IconName::Compact)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
            .child(
                Label::new(header_label)
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::MEDIUM)
                    .color(Color::Accent),
            )
            .when(expandable, |this| {
                this.cursor_pointer()
                    .child(
                        Icon::new(chevron_end)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .tooltip(Tooltip::text(if is_expanded {
                        "Collapse Compaction Summary"
                    } else {
                        "Expand Compaction Summary"
                    }))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.toggle_compaction_expansion(entry_ix, window, cx);
                    }))
            });

        let header = h_flex()
            .gap_2()
            .w_full()
            .child(Divider::horizontal())
            .child(marker)
            .child(Divider::horizontal());

        div()
            .px_5()
            .w_full()
            .child(
                v_flex()
                    .pt_1p5()
                    .mb_1p5()
                    .gap_1p5()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .rounded_sm()
                    .child(header)
                    .when_some(summary.filter(|_| is_expanded), |this, summary| {
                        this.border_color(self.tool_card_border_color(cx))
                            .bg(cx.theme().colors().editor_background.opacity(0.2))
                            .child(
                                div()
                                    .id(("compaction-summary", entry_ix))
                                    .p_2()
                                    .text_ui(cx)
                                    .child(self.render_markdown(
                                        summary,
                                        MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                        cx,
                                    )),
                            )
                            .child(
                                h_flex()
                                    .border_t_1()
                                    .border_color(self.tool_card_border_color(cx))
                                    .child(
                                        IconButton::new(
                                            ("compaction-summary-collapse", entry_ix),
                                            IconName::ChevronUp,
                                        )
                                        .full_width()
                                        .on_click(
                                            cx.listener(
                                                move |this, _event: &ClickEvent, window, cx| {
                                                    this.entry_view_state.update(
                                                        cx,
                                                        |state, _cx| {
                                                            state.collapse_compaction(entry_ix);
                                                        },
                                                    );
                                                    this.refresh_thread_search(window, cx);
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                    ),
                            )
                    }),
            )
            .into_any()
    }

    fn toggle_compaction_expansion(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If tail following is active and the entry is not yet expanded, we'll
        // want to anchor the list's scroll position, to prevent it from
        // automatically scrolling to the end of the compaction context element,
        // which would feel off, as we assume the user is trying to read it from
        // top to bottom.
        if self.list_state.is_following_tail()
            && !self
                .entry_view_state
                .read(cx)
                .is_compaction_expanded(entry_ix)
        {
            self.list_state.pause_following_tail();
        }

        self.entry_view_state.update(cx, |state, _cx| {
            state.toggle_compaction_expansion(entry_ix);
        });
        self.list_state.remeasure_items(entry_ix..entry_ix + 1);
        self.refresh_thread_search(window, cx);
        cx.notify();
    }

    fn render_edits_summary(
        &self,
        changed_buffers: &[(Entity<Buffer>, Entity<BufferDiff>)],
        expanded: bool,
        pending_edits: bool,
        cx: &Context<Self>,
    ) -> Div {
        const EDIT_NOT_READY_TOOLTIP_LABEL: &str = "Wait until file edits are complete.";

        let focus_handle = self.focus_handle(cx);

        h_flex()
            .p_1()
            .justify_between()
            .flex_wrap()
            .when(expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(
                h_flex()
                    .id("edits-container")
                    .cursor_pointer()
                    .gap_1()
                    .child(Disclosure::new("edits-disclosure", expanded))
                    .map(|this| {
                        if pending_edits {
                            this.child(
                                Label::new(format!(
                                    "Editing {} {}…",
                                    changed_buffers.len(),
                                    if changed_buffers.len() == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                ))
                                .color(Color::Muted)
                                .size(LabelSize::Small)
                                .with_animation(
                                    "edit-label",
                                    Animation::new(Duration::from_secs(2))
                                        .repeat()
                                        .with_easing(pulsating_between(0.3, 0.7)),
                                    |label, delta| label.alpha(delta),
                                ),
                            )
                        } else {
                            let stats = DiffStats::all_files(changed_buffers.iter().cloned(), cx);
                            let dot_divider = || {
                                Label::new("•")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled)
                            };

                            this.child(
                                Label::new("Edits")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(dot_divider())
                            .child(
                                Label::new(format!(
                                    "{} {}",
                                    changed_buffers.len(),
                                    if changed_buffers.len() == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(dot_divider())
                            .child(DiffStat::new(
                                "total",
                                stats.lines_added as usize,
                                stats.lines_removed as usize,
                            ))
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.edits_expanded = !this.edits_expanded;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("review-changes", IconName::ListTodo)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_window, cx| {
                                    Tooltip::for_action_in(
                                        "Review Changes",
                                        &OpenAgentDiff,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(OpenAgentDiff.boxed_clone(), cx);
                            })),
                    )
                    .child(Divider::vertical().color(DividerColor::Border))
                    .child(
                        Button::new("reject-all-changes", "Reject All")
                            .label_size(LabelSize::Small)
                            .disabled(pending_edits)
                            .when(pending_edits, |this| {
                                this.tooltip(Tooltip::text(EDIT_NOT_READY_TOOLTIP_LABEL))
                            })
                            .key_binding(
                                KeyBinding::for_action_in(&RejectAll, &focus_handle.clone(), cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reject_all(&RejectAll, window, cx);
                            })),
                    )
                    .child(
                        Button::new("keep-all-changes", "Keep All")
                            .label_size(LabelSize::Small)
                            .disabled(pending_edits)
                            .when(pending_edits, |this| {
                                this.tooltip(Tooltip::text(EDIT_NOT_READY_TOOLTIP_LABEL))
                            })
                            .key_binding(
                                KeyBinding::for_action_in(&KeepAll, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.keep_all(&KeepAll, window, cx);
                            })),
                    ),
            )
    }

    fn is_subagent_canceled_or_failed(&self, cx: &App) -> bool {
        let Some(parent_session_id) = self.parent_session_id.as_ref() else {
            return false;
        };

        let my_session_id = self.thread.read(cx).session_id().clone();

        self.server_view
            .upgrade()
            .and_then(|sv| sv.read(cx).thread_view(parent_session_id))
            .is_some_and(|parent_view| {
                parent_view
                    .read(cx)
                    .thread
                    .read(cx)
                    .tool_call_for_subagent(&my_session_id)
                    .is_some_and(|tc| {
                        matches!(
                            tc.status,
                            ToolCallStatus::Canceled
                                | ToolCallStatus::Failed
                                | ToolCallStatus::Rejected
                        )
                    })
            })
    }

    pub(crate) fn render_subagent_titlebar(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        if self.parent_session_id.is_none() {
            return None;
        }
        let parent_session_id = self.thread.read(cx).parent_session_id()?.clone();

        let server_view = self.server_view.clone();
        let thread = self.thread.clone();
        let is_done = thread.read(cx).status() == ThreadStatus::Idle;
        let is_canceled_or_failed = self.is_subagent_canceled_or_failed(cx);

        let max_content_width = AgentSettings::get_global(cx).max_content_width;

        Some(
            h_flex()
                .w_full()
                .h(Tab::container_height(cx))
                .border_b_1()
                .when(is_done && is_canceled_or_failed, |this| {
                    this.border_dashed()
                })
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background.opacity(0.2))
                .child(
                    h_flex()
                        .size_full()
                        .when_some(max_content_width, |this, max_w| this.max_w(max_w).mx_auto())
                        .pl_2()
                        .pr_1()
                        .flex_shrink_0()
                        .justify_between()
                        .gap_1()
                        .child(
                            h_flex()
                                .flex_1()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::ForwardArrowUp)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(self.title_editor.clone())
                                .when(is_done && is_canceled_or_failed, |this| {
                                    this.child(Icon::new(IconName::Close).color(Color::Error))
                                })
                                .when(is_done && !is_canceled_or_failed, |this| {
                                    this.child(Icon::new(IconName::Check).color(Color::Success))
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_0p5()
                                .when(!is_done, |this| {
                                    this.child(
                                        IconButton::new("stop_subagent", IconName::Stop)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Error)
                                            .tooltip(Tooltip::text("Stop Subagent"))
                                            .on_click(move |_, _, cx| {
                                                thread.update(cx, |thread, cx| {
                                                    thread.cancel(cx).detach();
                                                });
                                            }),
                                    )
                                })
                                .child(
                                    IconButton::new("minimize_subagent", IconName::Dash)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Minimize Subagent"))
                                        .on_click(move |_, window, cx| {
                                            let _ = server_view.update(cx, |server_view, cx| {
                                                server_view.navigate_to_thread(
                                                    parent_session_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }),
                                ),
                        ),
                ),
        )
    }

    /// The row above the message input: the pending review comments chip on the
    /// left and, while a turn generates, the prominent stop button on the right.
    /// The stop lives here (not in the status bar, and never gated on the
    /// input's contents) so cancelling a turn is always one obvious click.
    /// Above the input: the pending-review-comments chip, and, while a turn is
    /// generating, the working indicator (spinner, plan, elapsed, tokens) on the
    /// same line as the Stop button. Stop is never gated on the input's
    /// The active area sits above the input box: the pending review comments,
    /// the plan, and the working indicator. The plan gets a row to itself; it
    /// used to share the indicator's row, where it was laid out with a zero
    /// flex basis and so collapsed to an ellipsis whenever the row's other
    /// content took the width. Stop lives in the input box, not here.
    fn render_active_area(
        &mut self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let is_generating = self.thread.read(cx).status() != ThreadStatus::Idle;
        let active_thought = self.render_active_thought(cx);
        if !is_generating && active_thought.is_none() {
            return None;
        }

        let confirmation = self.thread.read(cx).is_waiting_for_confirmation()
            || self.has_pending_request_elicitation(cx);

        Some(
            v_flex()
                .w_full()
                .px_1()
                .gap_1()
                // The live thought sits above the plan: it is what the agent
                // is doing right now, the plan is where that is going.
                .when_some(
                    (!confirmation).then_some(active_thought).flatten(),
                    |this, thought| this.child(h_flex().w_full().min_w_0().child(thought)),
                )
                .children((!confirmation).then(|| self.render_plan(cx)).flatten())
                .when(is_generating, |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .child(self.render_generating(confirmation, cx)),
                    )
                })
                .into_any_element(),
        )
    }

    /// The thought the agent is having right now, shown beside the progress
    /// indicator. Thinking is only ever shown there: the transcript skips
    /// thoughts-only messages, and there are no thought chips.
    fn render_active_thought(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        const MIN_THOUGHT_DISPLAY: Duration = Duration::from_secs(1);

        let thread = self.thread.read(cx);
        if thread.status() != ThreadStatus::Generating {
            self.displayed_thought = None;
            self.thought_hold_timer = None;
            return None;
        }
        // The latest thought of the current turn, wherever it sits: it stays
        // up while the commands it led to run, and only leaves when a newer
        // thought replaces it or the turn ends.
        let entries = thread.entries();
        let (latest_key, latest) = entries
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, entry)| !matches!(entry, AgentThreadEntry::UserMessage(_)))
            .find_map(|(entry_ix, entry)| {
                let AgentThreadEntry::AssistantMessage(message) = entry else {
                    return None;
                };
                let (chunk_ix, markdown) = Self::thought_chunks(message, cx).last()?;
                Some(((entry_ix, chunk_ix), markdown))
            })?;

        // A newer thought does not replace the shown one until the shown one
        // has been up for a beat; otherwise a fast stream is unreadable.
        if let Some((shown_key, shown_at)) = self.displayed_thought
            && shown_key != latest_key
        {
            let remaining = MIN_THOUGHT_DISPLAY.saturating_sub(shown_at.elapsed());
            if !remaining.is_zero()
                && let Some(held) = Self::thought_chunk_at(entries, shown_key, cx)
            {
                if self.thought_hold_timer.is_none() {
                    self.thought_hold_timer = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(remaining).await;
                        this.update(cx, |this, cx| {
                            this.thought_hold_timer = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
                return Some(self.render_thought_chip(shown_key, &held, cx));
            }
        }

        if self.displayed_thought.map(|(key, _)| key) != Some(latest_key) {
            self.displayed_thought = Some((latest_key, std::time::Instant::now()));
        }
        Some(self.render_thought_chip(latest_key, &latest, cx))
    }

    fn thought_chunk_at(
        entries: &[AgentThreadEntry],
        key: (usize, usize),
        cx: &App,
    ) -> Option<Entity<Markdown>> {
        let AgentThreadEntry::AssistantMessage(message) = entries.get(key.0)? else {
            return None;
        };
        Self::thought_chunks(message, cx)
            .find(|(chunk_ix, _)| *chunk_ix == key.1)
            .map(|(_, markdown)| markdown)
    }

    /// The working indicator, live thought, and plan, aligned to the input's
    /// content width. Rendered above the activity bar (edits, queued
    /// messages): what the agent is doing now reads before what is waiting.
    pub(crate) fn render_active_area_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        self.render_active_area(window, cx).map(|area| {
            // min_w_0 on both levels: without it the column cannot shrink
            // below its content's intrinsic width, and a narrow window clips
            // the plan instead of truncating its rows.
            h_flex()
                .w_full()
                .min_w_0()
                .justify_center()
                .child(
                    v_flex()
                        .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                        .when(max_content_width.is_none(), |this| this.w_full())
                        .min_w_0()
                        .flex_shrink_1()
                        .px_2()
                        .child(area),
                )
                .into_any_element()
        })
    }

    pub(crate) fn render_message_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.is_subagent() {
            return div().into_any_element();
        }

        let editor_bg_color = cx.theme().colors().editor_background;

        let editor_expanded = self.editor_expanded;

        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        let has_messages = self.list_state.item_count() > 0;
        let fills_container = !has_messages || editor_expanded;

        v_flex()
            .w_full()
            .when(!has_messages, |this| this.flex_1().size_full())
            .child(
                h_flex()
                    // A little more air above the box than below it, so the
                    // composer sits apart from the thread's output.
                    .pt_4()
                    .pb_2()
                    .bg(editor_bg_color)
                    .justify_center()
                    .on_action(cx.listener(Self::handle_message_editor_move_up))
                    .map(|this| {
                        if has_messages {
                            this.on_action(cx.listener(Self::expand_message_editor))
                                .border_t_1()
                                .border_color(cx.theme().colors().border)
                                .when(editor_expanded, |this| this.h(vh(0.8, window)))
                        } else {
                            this.flex_1().size_full()
                        }
                    })
                    .child(
                        v_flex()
                            .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                            .when(max_content_width.is_none(), |this| this.w_full())
                            .when(fills_container, |this| this.h_full())
                            .px_2()
                            .flex_shrink_1()
                            .flex_grow_0()
                            .justify_between()
                            .gap_2()
                            .child(
                                v_flex()
                                    .relative()
                                    .w_full()
                                    .min_h_0()
                                    .when(fills_container, |this| this.flex_1())
                                    .pt_1()
                                    .pr_2p5()
                                    .child(self.message_editor.clone()),
                            )
                            .child(self.render_input_status_bar(cx)),
                    ),
            )
            .into_any()
    }

    fn render_queue_steer_button(
        &self,
        entry_id: QueueEntryId,
        index: usize,
        is_next: bool,
        steer_on: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let focus_handle = self.message_editor.focus_handle(cx);

        Button::new(("steer", index), "Steer")
            .label_size(LabelSize::Small)
            .toggle_state(steer_on)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .when(is_next, |this| {
                this.key_binding(
                    KeyBinding::for_action_in(&ToggleSteerFirstQueuedMessage, &focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
            })
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    "Steer",
                    None,
                    "Interrupt the agent at its next step to send this message. \
                     When off, queued messages wait for the agent to finish.",
                    cx,
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_queue_entry_steer(entry_id, cx);
            }))
    }

    fn render_message_queue_entries(
        &self,
        _window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let message_editor = self.message_editor.read(cx);
        let focus_handle = message_editor.focus_handle(cx);

        let queue_len = self.message_queue.len();
        let can_fast_track = self.message_queue.can_fast_track();
        let is_native = self.as_native_thread(cx).is_some();

        v_flex()
            .id("message_queue_list")
            .max_h_40()
            .overflow_y_scroll()
            .children(self.message_queue.iter().enumerate().map(|(index, entry)| {
                let entry_id = entry.id;
                let editor = &entry.editor;
                let is_next = index == 0;
                let (icon_color, tooltip_text) = if is_next {
                    (Color::Accent, "Next in Queue")
                } else {
                    (Color::Muted, "In Queue")
                };

                let editor_focused = editor.focus_handle(cx).is_focused(_window);
                let keybinding_size = rems_from_px(12_f32);
                let steer_on = entry.steer;

                // A message queued with review comments carries them just like a
                // sent one, so it shows the same review-comment visual instead of
                // the composed text in its read-only editor.
                let review = crate::diff_review::review_blocks(&entry.content);
                let review_only = !review.is_empty()
                    && crate::diff_review::without_review_blocks(entry.content.clone()).is_empty();

                let min_width = rems_from_px(160_f32);

                h_flex()
                    .group("queue_entry")
                    .w_full()
                    .p_1p5()
                    .gap_1()
                    .bg(cx.theme().colors().editor_background)
                    .when(index < queue_len - 1, |this| {
                        this.border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                    })
                    .child(
                        div()
                            .id("next_in_queue")
                            .child(
                                Icon::new(IconName::Circle)
                                    .size(IconSize::Small)
                                    .color(icon_color),
                            )
                            .tooltip(Tooltip::text(tooltip_text)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .when(!review_only, |this| this.child(editor.clone()))
                            .when(!review.is_empty(), |this| {
                                this.child(self.render_review_comments(
                                    ("queued-review", index),
                                    &review,
                                    cx,
                                ))
                            }),
                    )
                    .child(if editor_focused {
                        h_flex()
                            .gap_1()
                            .min_w(min_width)
                            .justify_end()
                            .child(
                                IconButton::new(("edit", index), IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::with_meta(
                                            "Edit Queued Message",
                                            None,
                                            "Type anything to edit",
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.move_queued_message_to_main_editor(
                                            entry_id, None, None, window, cx,
                                        );
                                    })),
                            )
                            .when(is_native, |row| {
                                row.child(self.render_queue_steer_button(
                                    entry_id, index, is_next, steer_on, cx,
                                ))
                            })
                            .child(
                                Button::new(("send_now_focused", index), "Send Now")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Outlined)
                                    .key_binding(
                                        KeyBinding::for_action_in(
                                            &SendImmediately,
                                            &editor.focus_handle(cx),
                                            cx,
                                        )
                                        .map(|kb| kb.size(keybinding_size)),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.send_queued_message_now(entry_id, window, cx);
                                    })),
                            )
                    } else {
                        h_flex()
                            .when(!is_next, |this| this.visible_on_hover("queue_entry"))
                            .gap_1()
                            .min_w(min_width)
                            .justify_end()
                            .child(
                                IconButton::new(("delete", index), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        move |_window, cx| {
                                            if is_next {
                                                Tooltip::for_action_in(
                                                    "Remove Message from Queue",
                                                    &RemoveFirstQueuedMessage,
                                                    &focus_handle,
                                                    cx,
                                                )
                                            } else {
                                                Tooltip::simple("Remove Message from Queue", cx)
                                            }
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_from_queue(entry_id, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new(("edit", index), IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        move |_window, cx| {
                                            if is_next {
                                                Tooltip::for_action_in(
                                                    "Edit",
                                                    &EditFirstQueuedMessage,
                                                    &focus_handle,
                                                    cx,
                                                )
                                            } else {
                                                Tooltip::simple("Edit", cx)
                                            }
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.move_queued_message_to_main_editor(
                                            entry_id, None, None, window, cx,
                                        );
                                    })),
                            )
                            .when(is_native, |row| {
                                row.child(self.render_queue_steer_button(
                                    entry_id, index, is_next, steer_on, cx,
                                ))
                            })
                            .child(
                                Button::new(("send_now", index), "Send Now")
                                    .label_size(LabelSize::Small)
                                    .when(is_next, |this| this.style(ButtonStyle::Outlined))
                                    .when(is_next && message_editor.is_empty(cx), |this| {
                                        let action: Box<dyn gpui::Action> = if can_fast_track {
                                            Box::new(Chat)
                                        } else {
                                            Box::new(SendNextQueuedMessage)
                                        };

                                        this.key_binding(
                                            KeyBinding::for_action_in(
                                                action.as_ref(),
                                                &focus_handle.clone(),
                                                cx,
                                            )
                                            .map(|kb| kb.size(keybinding_size)),
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.send_queued_message_now(entry_id, window, cx);
                                    })),
                            )
                    })
            }))
            .into_any_element()
    }

    fn supports_split_token_display(&self, cx: &App) -> bool {
        self.as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .is_some_and(|model| model.supports_split_token_display())
    }

    /// How full the context window is, pinned to the top right of the
    /// conversation (not the input status bar) so it reads as a property of the
    /// thread rather than of the message being composed.
    /// How full the context window is, shown in the input bar beside the
    /// thread's diff and PR chips rather than floating over the transcript,
    /// where it sat on top of the conversation it was describing.
    fn render_context_window_indicator(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let usage = self.render_token_usage(cx)?;
        Some(h_flex().flex_none().child(usage).into_any_element())
    }

    fn render_token_usage(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let thread = self.thread.read(cx);
        let usage = thread.token_usage()?;
        let show_split = self.supports_split_token_display(cx);

        let cost_label = thread.cost().map(|cost| {
            let precision = if cost.amount > 0.0 && cost.amount < 0.01 {
                4
            } else {
                2
            };
            format!("{:.prec$} {}", cost.amount, cost.currency, prec = precision)
        });

        let progress_color = |ratio: f32| -> Hsla {
            if ratio >= 0.85 {
                cx.theme().status().warning
            } else {
                cx.theme().colors().text_muted
            }
        };

        let used = crate::humanize_token_count(usage.used_tokens);
        let max = crate::humanize_token_count(usage.max_tokens);
        let input_tokens_label = crate::humanize_token_count(usage.input_tokens);
        let output_tokens_label = crate::humanize_token_count(usage.output_tokens);

        let progress_ratio = if usage.max_tokens > 0 {
            usage.used_tokens as f32 / usage.max_tokens as f32
        } else {
            0.0
        };

        let ring_size = px(16.0);
        let stroke_width = px(2.);

        let percentage = format!("{}%", (progress_ratio * 100.0).round() as u32);

        let tooltip_separator_color = Color::Custom(cx.theme().colors().text_disabled.opacity(0.6));

        let (project_rules_count, project_entry_ids) = self
            .as_native_thread(cx)
            .map(|thread| {
                let project_context = thread.read(cx).project_context().read(cx);
                let project_entry_ids = project_context
                    .worktrees
                    .iter()
                    .filter_map(|wt| wt.rules_file.as_ref())
                    .map(|rf| ProjectEntryId::from_usize(rf.project_entry_id))
                    .collect::<Vec<_>>();
                let project_rules_count = project_entry_ids.len();
                (project_rules_count, project_entry_ids)
            })
            .unwrap_or_default();

        let global_agents_md_loaded = UserAgentsMd::global(cx)
            .and_then(|md| md.content())
            .is_some();

        let workspace = self.workspace.clone();

        let max_output_tokens = self
            .as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .and_then(|model| model.max_output_tokens())
            .unwrap_or(0);
        let input_max_label =
            crate::humanize_token_count(usage.max_tokens.saturating_sub(max_output_tokens));
        let output_max_label = crate::humanize_token_count(max_output_tokens);

        let build_tooltip = {
            move |_window: &mut Window, cx: &mut App| {
                let percentage = percentage.clone();
                let used = used.clone();
                let max = max.clone();
                let input_tokens_label = input_tokens_label.clone();
                let output_tokens_label = output_tokens_label.clone();
                let input_max_label = input_max_label.clone();
                let output_max_label = output_max_label.clone();
                let project_entry_ids = project_entry_ids.clone();
                let workspace = workspace.clone();
                let cost_label = cost_label.clone();
                cx.new(move |_cx| TokenUsageTooltip {
                    percentage,
                    used,
                    max,
                    input_tokens: input_tokens_label,
                    output_tokens: output_tokens_label,
                    input_max: input_max_label,
                    output_max: output_max_label,
                    show_split,
                    cost_label,
                    separator_color: tooltip_separator_color,
                    global_agents_md_loaded,
                    project_rules_count,
                    project_entry_ids,
                    workspace,
                })
                .into()
            }
        };

        if show_split {
            let input_max_raw = usage.max_tokens.saturating_sub(max_output_tokens);
            let output_max_raw = max_output_tokens;

            let input_ratio = if input_max_raw > 0 {
                usage.input_tokens as f32 / input_max_raw as f32
            } else {
                0.0
            };
            let output_ratio = if output_max_raw > 0 {
                usage.output_tokens as f32 / output_max_raw as f32
            } else {
                0.0
            };

            Some(
                h_flex()
                    .id("split_token_usage")
                    .flex_shrink_0()
                    .gap_1p5()
                    .mr_1()
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::ArrowUp)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                CircularProgress::new(
                                    usage.input_tokens as f32,
                                    input_max_raw as f32,
                                    ring_size,
                                    cx,
                                )
                                .stroke_width(stroke_width)
                                .progress_color(progress_color(input_ratio)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::ArrowDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                CircularProgress::new(
                                    usage.output_tokens as f32,
                                    output_max_raw as f32,
                                    ring_size,
                                    cx,
                                )
                                .stroke_width(stroke_width)
                                .progress_color(progress_color(output_ratio)),
                            ),
                    )
                    .hoverable_tooltip(build_tooltip)
                    .into_any_element(),
            )
        } else {
            Some(
                h_flex()
                    .id("circular_progress_tokens")
                    .mt_px()
                    .mr_1()
                    .child(
                        CircularProgress::new(
                            usage.used_tokens as f32,
                            usage.max_tokens as f32,
                            ring_size,
                            cx,
                        )
                        .stroke_width(stroke_width)
                        .progress_color(progress_color(progress_ratio)),
                    )
                    .hoverable_tooltip(build_tooltip)
                    .into_any_element(),
            )
        }
    }

    fn fast_mode_available(&self, cx: &Context<Self>) -> bool {
        self.as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .map(|model| model.supports_fast_mode())
            .unwrap_or(false)
    }

    fn refresh_sandbox_status(&mut self, cx: &mut Context<Self>) -> Option<VerifiedSandboxStatus> {
        let thread = self.as_native_thread(cx)?;
        let (key, refresh) =
            thread.update(cx, |thread, cx| thread.refresh_verified_sandbox_status(cx))?;

        if self.sandbox_status_key.as_ref() == Some(&key) {
            return self.sandbox_status.clone();
        }

        match refresh {
            SandboxStatusRefresh::Ready(status) => {
                self.sandbox_status = Some(status.clone());
                self.sandbox_status_key = Some(key);
                self.pending_sandbox_status_key = None;
                Some(status)
            }
            SandboxStatusRefresh::Pending(task) => {
                if self.pending_sandbox_status_key.as_ref() != Some(&key) {
                    self.sandbox_status = None;
                    self.sandbox_status_key = None;
                    self.pending_sandbox_status_key = Some(key.clone());
                    self._sandbox_status_refresh_task = Some(cx.spawn(async move |this, cx| {
                        let status = task.await;
                        this.update(cx, |this, cx| {
                            if this.pending_sandbox_status_key.as_ref() == Some(&key) {
                                this.sandbox_status = Some(status);
                                this.sandbox_status_key = Some(key);
                                this.pending_sandbox_status_key = None;
                                cx.notify();
                            }
                        })
                        .ok();
                    }));
                }
                None
            }
        }
    }

    pub fn render_sandbox_status(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.refresh_sandbox_status(cx)?;
        let settings_sandbox = status.settings_sandbox.clone();
        let thread_sandbox = status.thread_sandbox.clone();
        let baseline = status.baseline_writable_paths;

        // The lock is struck only when the *merged* result is unsandboxed (the
        // agent runs with ambient permissions). A layer that is merely wide open
        // but still sandboxed keeps the closed lock.
        let (icon, icon_color) = if settings_sandbox
            .clone()
            .merge(thread_sandbox.clone())
            .is_unsandboxed()
        {
            (IconName::LockOff, Color::Muted)
        } else {
            (IconName::Lock, Color::Default)
        };

        let tooltip = match (settings_sandbox, thread_sandbox) {
            // No sandbox at all because the user turned it off in settings: the
            // per-thread layer is moot, so don't show it.
            (ThreadSandbox::Unsandboxed, _) => SandboxStatusTooltip::disabled_in_settings(),
            // Sandboxed by settings, but disabled for this thread: show the
            // settings scope (greyed) for context above the disabled status.
            (ThreadSandbox::Sandboxed(settings_policy), ThreadSandbox::Unsandboxed) => {
                let settings = augment_settings_sandbox_policy(&settings_policy, baseline);
                SandboxStatusTooltip::disabled_for_thread(sandbox_section(
                    "Defined in your settings:",
                    &settings,
                    true,
                ))
            }
            (
                ThreadSandbox::Sandboxed(settings_policy),
                ThreadSandbox::Sandboxed(thread_policy),
            ) => {
                let settings = augment_settings_sandbox_policy(&settings_policy, baseline);
                let thread = SandboxPolicyDisplay::from_policy(&thread_policy);
                // Omit the per-thread section when it grants nothing extra.
                let thread = (!sandbox_policy_grants_nothing(&thread))
                    .then(|| sandbox_section("Allowed for this conversation:", &thread, false));
                SandboxStatusTooltip::enabled(
                    sandbox_section("Defined in your settings:", &settings, true),
                    thread,
                )
            }
        };

        Some(
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("sandbox-status", icon)
                        .icon_size(IconSize::Small)
                        .icon_color(icon_color)
                        .tooltip(Tooltip::element(move |_window, _cx| {
                            tooltip.clone().into_any_element()
                        }))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::OpenSettingsAt {
                                    path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                    target: None,
                                }),
                                cx,
                            );
                        }),
                )
                .child(Divider::vertical())
                .into_any_element(),
        )
    }

    /// The fast-mode content merged into the model popover's third section: a
    /// fast-mode on/off row plus, when the provider warns before enabling it,
    /// the warning inline with an enable-and-dismiss row. Returns `None` for
    /// models without fast mode so the popover shows only model and effort.
    fn fast_mode_menu_section(
        &self,
        cx: &Context<Self>,
    ) -> Option<crate::model_selector_popover::FastModeSection> {
        use crate::model_selector_popover::{FastModeConfirmationRows, FastModeSection};

        if !self.fast_mode_available(cx) {
            return None;
        }

        let thread = self.as_native_thread(cx)?;
        let is_fast = matches!(thread.read(cx).speed(), Some(Speed::Fast));
        let weak_self = cx.weak_entity();

        let toggle: Rc<dyn Fn(&mut Window, &mut App)> = Rc::new({
            let weak_self = weak_self.clone();
            move |_window: &mut Window, cx: &mut App| {
                weak_self
                    .update(cx, |this, cx| {
                        let new_speed = if is_fast {
                            Speed::Standard
                        } else {
                            Speed::Fast
                        };
                        this.apply_fast_mode_speed(new_speed, cx);
                    })
                    .ok();
            }
        });

        let confirmation = (!is_fast)
            .then(|| self.pending_fast_mode_confirmation(cx))
            .flatten()
            .map(|(provider_id, model_id, confirmation)| {
                let weak_self = weak_self.clone();
                let enable_and_dismiss: Rc<dyn Fn(&mut Window, &mut App)> =
                    Rc::new(move |_window: &mut Window, cx: &mut App| {
                        weak_self
                            .update(cx, |this, cx| {
                                this.apply_fast_mode_speed(Speed::Fast, cx);
                            })
                            .ok();
                        set_fast_mode_warning_dismissed(&provider_id, &model_id, cx);
                    });
                FastModeConfirmationRows {
                    title: confirmation.title.clone(),
                    message: confirmation.message,
                    enable_and_dismiss,
                }
            });

        Some(FastModeSection {
            enabled: is_fast,
            toggle,
            confirmation,
        })
    }

    fn pending_fast_mode_confirmation(
        &self,
        cx: &App,
    ) -> Option<(
        LanguageModelProviderId,
        LanguageModelId,
        FastModeConfirmation,
    )> {
        let thread = self.as_native_thread(cx)?.read(cx);
        let model = thread.model()?;
        let provider_id = model.provider_id();
        let model_id = model.id();
        let confirmation = LanguageModelRegistry::read_global(cx)
            .provider(&provider_id)
            .and_then(|provider| provider.fast_mode_confirmation(cx))?;
        if fast_mode_warning_dismissed(&provider_id, &model_id, cx) {
            return None;
        }
        Some((provider_id, model_id, confirmation))
    }

    /// The thinking/effort content merged into the model popover's second
    /// section: a thinking on/off toggle (when the model allows disabling it)
    /// and the supported effort levels. Returns `None` for models without
    /// thinking so the model popover shows only the model list.
    fn effort_menu_section(
        &self,
        cx: &Context<Self>,
    ) -> Option<crate::model_selector_popover::EffortMenuSection> {
        use crate::model_selector_popover::{EffortMenuSection, EffortOption};

        let thread = self.as_native_thread(cx)?;
        let thread_read = thread.read(cx);
        let model = thread_read.model()?;
        if !model.supports_thinking() {
            return None;
        }

        let can_disable = model.supports_disabling_thinking();
        let thinking_enabled = thread_read.thinking_enabled();
        let effort_levels = model.supported_effort_levels();
        // Match the standalone control: effort only applies when thinking is on
        // (or can't be turned off).
        let show_effort = !effort_levels.is_empty() && (!can_disable || thinking_enabled);

        let selected_value = thread_read.thinking_effort().cloned();
        let selected_level = selected_value
            .as_ref()
            .and_then(|value| effort_levels.iter().find(|level| &level.value == value))
            .or_else(|| effort_levels.iter().find(|level| level.is_default))
            .cloned();

        let weak_self = cx.weak_entity();

        let thinking_toggle = can_disable.then(|| {
            let weak_self = weak_self.clone();
            let toggle: Rc<dyn Fn(&mut Window, &mut App)> =
                Rc::new(move |_window: &mut Window, cx: &mut App| {
                    weak_self
                        .update(cx, |this, cx| {
                            if let Some(thread) = this.as_native_thread(cx) {
                                let enable = !thread.read(cx).thinking_enabled();
                                Self::persist_thinking_enabled(&thread, enable, cx);
                            }
                        })
                        .ok();
                });
            (thinking_enabled, toggle)
        });

        let effort_options = if show_effort {
            effort_levels
                .iter()
                .map(|level| EffortOption {
                    name: level.name.clone(),
                    value: level.value.clone(),
                    selected: selected_level
                        .as_ref()
                        .is_some_and(|selected| selected.value == level.value),
                })
                .collect()
        } else {
            Vec::new()
        };

        let on_select_effort: Rc<dyn Fn(SharedString, &mut Window, &mut App)> = Rc::new(
            move |value: SharedString, _window: &mut Window, cx: &mut App| {
                weak_self
                    .update(cx, |this, cx| {
                        if let Some(thread) = this.as_native_thread(cx) {
                            Self::persist_thinking_effort(&thread, value.to_string(), cx);
                        }
                    })
                    .ok();
            },
        );

        let selected_label = show_effort
            .then(|| selected_level.as_ref().map(|level| level.name.clone()))
            .flatten();

        Some(EffortMenuSection {
            thinking_toggle,
            effort_options,
            on_select_effort,
            selected_label,
        })
    }

    fn persist_thinking_enabled(thread: &Entity<agent::Thread>, enable: bool, cx: &mut App) {
        thread.update(cx, |thread, cx| {
            thread.set_thinking_enabled(enable, cx);
            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.enable_thinking = enable;
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.enable_thinking = enable
                        });
                    }
                }
            });
        });
    }

    fn persist_thinking_effort(thread: &Entity<agent::Thread>, effort: String, cx: &mut App) {
        thread.update(cx, |thread, cx| {
            thread.set_thinking_effort(Some(effort.clone()), cx);
            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.effort = Some(effort.clone());
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.effort = Some(effort.clone())
                        });
                    }
                }
            });
        });
    }

    /// The persistent bar directly below the message input. It hosts the
    /// option controls (add-context, profile, mode, the merged agent settings
    /// picker) on the left and, on the right, this thread's worktree diff
    /// against the start of its branch (clicking it opens that same diff) and
    /// the thread's PR
    /// badges. Running state lives in the tabs, the thread's working indicator,
    /// and the stop button above the input; the context-window indicator sits at
    /// the bottom right of the conversation. There is no send button: Enter
    /// sends via the editor's Chat action.
    fn render_input_status_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let branch_diff_stats = self.branch_diff_stats.read(cx);
        let is_generating = self.thread.read(cx).status() != ThreadStatus::Idle;
        // An unstarted draft: the same state that hosts the worktree choice.
        let is_draft = self.list_state.item_count() == 0;
        let diff_stats = branch_diff_stats.stats();
        let diff_tooltip = match branch_diff_stats.base() {
            DiffStatsBase::DefaultBranch(base) => {
                format!("Open this worktree's diff since {base}")
            }
            DiffStatsBase::Head => "Open this worktree's diff since the last commit".to_string(),
            DiffStatsBase::NoRepository => "This worktree is not in a git repository".to_string(),
        };

        // Model, thinking effort, and fast mode are one control: the popover
        // carries a section each, and the trigger reads "Model / Effort" with a
        // fast-mode icon when it is on.
        if let Some(model_selector) = self.model_selector.clone() {
            let effort_section = self.effort_menu_section(cx);
            let fast_mode_section = self.fast_mode_menu_section(cx);
            let working = self.thread.read(cx).status() != ThreadStatus::Idle;
            model_selector.update(cx, |selector, cx| {
                selector.set_effort_section(effort_section, cx);
                selector.set_fast_mode_section(fast_mode_section, cx);
                selector.set_working(working, cx);
            });
        }

        h_flex()
            .w_full()
            .flex_none()
            .flex_wrap()
            .py_1()
            .min_h(rems_from_px(30_f32))
            .gap_1()
            .justify_between()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_0p5()
                    .child(self.render_add_context_button(cx))
                    .children(self.profile_selector.clone())
                    .map(|this| match self.config_options_view.clone() {
                        // The model picker only survives beside the config
                        // options when the agent's own options do not offer the
                        // model (see ConversationView's selector construction).
                        Some(config_view) => this
                            .children(self.model_selector.clone())
                            .child(config_view),
                        None => this
                            .children(self.mode_selector.clone())
                            .children(self.model_selector.clone()),
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    // How full the context window is, ahead of what the branch
                    // looks like: it is about the conversation, and it is the
                    // one of these that changes as you type.
                    .children(self.render_context_window_indicator(cx))
                    // A draft has done no work and may not even end up on this
                    // branch (it can start a new worktree on send), so the
                    // branch's diff and PRs are not its own and are not shown.
                    .when(!is_draft, |this| {
                        this
                            // Always rendered, +0/-0 included, so the bar keeps its
                            // shape when the first edit lands.
                            // Reads as a button/chip (border, hover, pointer), not inert
                            // text, so its click-to-open-diff affordance is obvious.
                            .child(
                                h_flex()
                                    .id("thread-diff-stat")
                                    .px_1()
                                    .py_0p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .cursor_pointer()
                                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                                    .tooltip(Tooltip::text(diff_tooltip))
                                    .on_click(cx.listener(|_this, _, window, cx| {
                                        if let Ok(action) = cx.build_action("git::BranchDiff", None)
                                        {
                                            window.dispatch_action(action, cx);
                                        }
                                    }))
                                    .child(DiffStat::new(
                                        "thread-diff",
                                        diff_stats.added as usize,
                                        diff_stats.deleted as usize,
                                    )),
                            )
                            .children(self.thread_pr_chips(cx).into_iter().enumerate().map(
                                |(index, chip)| {
                                    ui::PrChip::new(("thread-pr-chip", index), chip).large(true)
                                },
                            ))
                    })
                    .children(self.render_pending_review_comments(cx))
                    .child(self.render_input_run_indicator(cx))
                    .when(is_generating, |this| {
                        this.child(self.render_stop_button(cx))
                    }),
            )
            .into_any()
    }

    /// The Stop button, in the status bar under the input rather than over it.
    /// It is never gated on the input's contents, so cancelling a turn is
    /// always one click.
    fn render_stop_button(&self, cx: &Context<Self>) -> AnyElement {
        Button::new("stop-generation", "Stop")
            .label_size(LabelSize::Small)
            .start_icon(
                Icon::new(IconName::Stop)
                    .size(IconSize::Small)
                    .color(Color::Error),
            )
            .style(ButtonStyle::Tinted(TintColor::Error))
            .tooltip(move |_window, cx| {
                Tooltip::for_action("Stop Generation", &editor::actions::Cancel, cx)
            })
            .on_click(cx.listener(|this, _event, _, cx| this.cancel_generation(cx)))
            .into_any_element()
    }

    /// The pending review comments, shown inside the input box: they attach to
    /// the next message, so they belong with the message being composed.
    fn render_pending_review_comments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.pending_review_comment_count(cx);
        if count == 0 {
            return None;
        }
        Some(
            h_flex()
                .id("pending-review-comments")
                .gap_1()
                .child(
                    Icon::new(IconName::Chat)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
                .child(
                    Label::new(format!(
                        "{count} review comment{} attached",
                        if count == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    IconButton::new("clear-review-comments", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Discard pending review comments"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.clear_pending_review_comments(cx)
                        })),
                )
                .tooltip(Tooltip::text(
                    "These review comments attach to your next message",
                ))
                .into_any_element(),
        )
    }

    /// The right end of the input status bar: the loading spinner while added
    /// context resolves, and nothing otherwise. There is no send affordance at
    /// all (not even a hint): Enter sends via the editor's Chat action.
    fn render_input_run_indicator(&self, _cx: &mut Context<Self>) -> AnyElement {
        if self.is_loading_contents {
            return div()
                .id("loading-message-content")
                .px_1()
                .tooltip(Tooltip::text("Loading Added Context…"))
                .child(loading_contents_spinner(IconSize::default()))
                .into_any_element();
        }

        Empty.into_any_element()
    }

    fn render_add_context_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.message_editor.focus_handle(cx);
        let weak_self = cx.weak_entity();

        PopoverMenu::new("add-context-menu")
            .trigger_with_tooltip(
                IconButton::new("add-context", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                {
                    move |_window, cx| {
                        Tooltip::for_action_in(
                            "Add Context",
                            &OpenAddContextMenu,
                            &focus_handle,
                            cx,
                        )
                    }
                },
            )
            .anchor(gpui::Anchor::BottomLeft)
            .with_handle(self.add_context_menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .menu(move |window, cx| {
                weak_self
                    .update(cx, |this, cx| this.build_add_context_menu(window, cx))
                    .ok()
            })
    }

    fn build_add_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let message_editor = self.message_editor.clone();
        let workspace = self.workspace.clone();
        let session_capabilities = self.session_capabilities.read();
        let supports_images = session_capabilities.supports_images();
        let supports_embedded_context = session_capabilities.supports_embedded_context();
        let available_skills = session_capabilities.completion_skills();
        drop(session_capabilities);

        let has_editor_selection = workspace
            .upgrade()
            .and_then(|ws| {
                ws.read(cx)
                    .active_item(cx)
                    .and_then(|item| item.downcast::<Editor>())
            })
            .is_some_and(|editor| {
                editor.update(cx, |editor, cx| {
                    editor.has_non_empty_selection(&editor.display_snapshot(cx))
                })
            });

        let has_terminal_selection = workspace
            .upgrade()
            .and_then(|ws| ws.read(cx).panel::<TerminalPanel>(cx))
            .is_some_and(|panel| !panel.read(cx).terminal_selections(cx).is_empty());

        let has_selection = has_editor_selection || has_terminal_selection;

        ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.key_context("AddContextMenu")
                .item(
                    ContextMenuEntry::new("Files & Directories")
                        .icon(IconName::File)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("file", window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Symbols")
                        .icon(IconName::Code)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("symbol", window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Threads")
                        .icon(IconName::Thread)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("thread", window, cx);
                                });
                            }
                        }),
                )
                .when(!available_skills.is_empty(), |this| {
                    this.submenu_with_colored_icon("Skills", IconName::Sparkle, Color::Muted, {
                        let message_editor = message_editor.clone();
                        let available_skills = available_skills.clone();
                        move |mut menu, _window, _cx| {
                            for skill in &available_skills {
                                menu = menu
                                    .item(Self::skill_menu_entry(skill, message_editor.clone()));
                            }
                            menu
                        }
                    })
                })
                .item(
                    ContextMenuEntry::new("Image")
                        .icon(IconName::Image)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!supports_images)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.add_images_from_picker(window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Selection")
                        .icon(IconName::CursorIBeam)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!has_selection)
                        .handler({
                            move |window, cx| {
                                window.dispatch_action(
                                    zed_actions::agent::AddSelectionToThread.boxed_clone(),
                                    cx,
                                );
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Branch Diff")
                        .icon(IconName::GitBranch)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!supports_embedded_context)
                        .handler({
                            move |window, cx| {
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_branch_diff_crease(window, cx);
                                });
                            }
                        }),
                )
        })
    }

    fn skill_menu_entry(
        skill: &AvailableSkill,
        message_editor: Entity<crate::message_editor::MessageEditor>,
    ) -> ContextMenuEntry {
        let label = format!("{} ({})", skill.name, skill.source);
        let skill = skill.clone();

        ContextMenuEntry::new(label)
            .icon(IconName::Sparkle)
            .icon_color(Color::Muted)
            .icon_size(IconSize::XSmall)
            .handler(move |window, cx| {
                message_editor.focus_handle(cx).focus(window, cx);
                message_editor.update(cx, |editor, cx| {
                    editor.insert_skill_crease(&skill, window, cx);
                });
            })
    }
}

struct TokenUsageTooltip {
    percentage: String,
    used: String,
    max: String,
    input_tokens: String,
    output_tokens: String,
    input_max: String,
    output_max: String,
    show_split: bool,
    cost_label: Option<String>,
    separator_color: Color,
    global_agents_md_loaded: bool,
    project_rules_count: usize,
    project_entry_ids: Vec<ProjectEntryId>,
    workspace: WeakEntity<Workspace>,
}

impl Render for TokenUsageTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let separator_color = self.separator_color;
        let percentage = self.percentage.clone();
        let used = self.used.clone();
        let max = self.max.clone();
        let input_tokens = self.input_tokens.clone();
        let output_tokens = self.output_tokens.clone();
        let input_max = self.input_max.clone();
        let output_max = self.output_max.clone();
        let show_split = self.show_split;
        let cost_label = self.cost_label.clone();
        let global_agents_md_loaded = self.global_agents_md_loaded;
        let project_rules_count = self.project_rules_count;
        let project_entry_ids = self.project_entry_ids.clone();
        let workspace = self.workspace.clone();

        ui::tooltip_container(cx, move |container, cx| {
            container
                .min_w_40()
                .child(
                    Label::new("Context")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .when(!show_split, |this| {
                    this.child(
                        h_flex()
                            .gap_0p5()
                            .child(Label::new(percentage.clone()))
                            .child(Label::new("\u{2022}").color(separator_color).mx_1())
                            .child(Label::new(used.clone()))
                            .child(Label::new("/").color(separator_color))
                            .child(Label::new(max.clone()).color(Color::Muted)),
                    )
                })
                .when(show_split, |this| {
                    this.child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(Label::new("Input:").color(Color::Muted).mr_0p5())
                                    .child(Label::new(input_tokens))
                                    .child(Label::new("/").color(separator_color))
                                    .child(Label::new(input_max).color(Color::Muted)),
                            )
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(Label::new("Output:").color(Color::Muted).mr_0p5())
                                    .child(Label::new(output_tokens))
                                    .child(Label::new("/").color(separator_color))
                                    .child(Label::new(output_max).color(Color::Muted)),
                            ),
                    )
                })
                .when_some(cost_label, |this, cost_label| {
                    this.child(
                        v_flex()
                            .mt_1p5()
                            .pt_1p5()
                            .gap_0p5()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new("Cost")
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                            .child(Label::new(cost_label)),
                    )
                })
                .when(
                    global_agents_md_loaded || project_rules_count > 0,
                    move |this| {
                        this.child(
                            v_flex()
                                .mt_1p5()
                                .pt_1p5()
                                .pb_0p5()
                                .gap_0p5()
                                .border_t_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(
                                    Label::new("Rules")
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    v_flex()
                                        .mx_neg_1()
                                        .when(global_agents_md_loaded, {
                                            let workspace = workspace.clone();
                                            move |this| {
                                                this.child(
                                                    Button::new(
                                                        "open-global-agents-md",
                                                        "1 global rule",
                                                    )
                                                    .end_icon(
                                                        Icon::new(IconName::ArrowUpRight)
                                                            .color(Color::Muted)
                                                            .size(IconSize::XSmall),
                                                    )
                                                    .on_click(move |_, window, cx| {
                                                        workspace
                                                            .update(cx, |workspace, cx| {
                                                                workspace
                                                                    .open_abs_path(
                                                                        paths::agents_file()
                                                                            .clone(),
                                                                        workspace::OpenOptions {
                                                                            focus: Some(true),
                                                                            ..Default::default()
                                                                        },
                                                                        window,
                                                                        cx,
                                                                    )
                                                                    .detach_and_log_err(cx);
                                                            })
                                                            .log_err();
                                                    }),
                                                )
                                            }
                                        })
                                        .when(project_rules_count > 0, move |this| {
                                            let workspace = workspace.clone();
                                            let project_entry_ids = project_entry_ids.clone();
                                            this.child(
                                                Button::new(
                                                    "open-project-rules",
                                                    format!(
                                                        "{} {}",
                                                        project_rules_count,
                                                        pluralize(
                                                            "project rule",
                                                            project_rules_count
                                                        )
                                                    ),
                                                )
                                                .end_icon(
                                                    Icon::new(IconName::ArrowUpRight)
                                                        .color(Color::Muted)
                                                        .size(IconSize::XSmall),
                                                )
                                                .on_click(move |_, window, cx| {
                                                    let _ =
                                                        workspace.update(cx, |workspace, cx| {
                                                            let project =
                                                                workspace.project().read(cx);
                                                            let paths = project_entry_ids
                                                                .iter()
                                                                .flat_map(|id| {
                                                                    project.path_for_entry(*id, cx)
                                                                })
                                                                .collect::<Vec<_>>();
                                                            for path in paths {
                                                                workspace
                                                                    .open_path(
                                                                        path, None, true, window,
                                                                        cx,
                                                                    )
                                                                    .detach_and_log_err(cx);
                                                            }
                                                        });
                                                }),
                                            )
                                        }),
                                ),
                        )
                    },
                )
        })
    }
}

/// A display-ready snapshot of a sandbox policy for the status tooltip.
///
/// The opaque `HostFilesystemLocation`s in a policy are stringified up front,
/// when this is built, so the tooltip state (which outlives the build and is
/// captured by the lazy tooltip closure) never holds the locations' fds open.
#[derive(Clone)]
struct SandboxPolicyDisplay {
    fs: SandboxFsDisplay,
    network: SandboxNetPolicy,
}

/// The filesystem write-access portion of a [`SandboxPolicyDisplay`].
#[derive(Clone)]
enum SandboxFsDisplay {
    Unrestricted,
    Restricted(Vec<WritableEntryDisplay>),
}

/// A single writable entry to display in the sandbox tooltip: either a real host
/// location (already stringified for display) or the Linux-only host-isolated
/// `/tmp` overlay, which has no backing host path and is purely a label.
#[derive(Clone)]
enum WritableEntryDisplay {
    Path(String),
    // Only ever constructed on Linux (the bwrap `--tmpfs /tmp` overlay), so the
    // variant is gated to match and avoid dead-code warnings elsewhere.
    #[cfg(target_os = "linux")]
    IsolatedTmp,
}

impl SandboxPolicyDisplay {
    /// Display a policy verbatim (used for the per-thread overrides, which carry
    /// no implicit baseline grants). Takes the policy by reference and stringifies
    /// its locations immediately, so no fd is retained past this call.
    fn from_policy(policy: &SandboxPolicy) -> Self {
        let fs = match &policy.fs {
            SandboxFsPolicy::Unrestricted { .. } => SandboxFsDisplay::Unrestricted,
            SandboxFsPolicy::Restricted { writable_paths, .. } => SandboxFsDisplay::Restricted(
                writable_paths
                    .iter()
                    .map(|location| {
                        WritableEntryDisplay::Path(location.untrusted_path_display().to_string())
                    })
                    .collect(),
            ),
        };
        SandboxPolicyDisplay {
            fs,
            network: policy.network.clone(),
        }
    }
}

/// Fold the always-granted baseline writable paths (the project's worktree
/// roots, derived from the same source the terminal tool uses) and, on Linux,
/// the host-isolated `/tmp` overlay into a settings policy for display. These
/// are part of what the sandbox grants whenever it's active but aren't
/// persistent-settings entries, so they're shown in the "from your settings"
/// section rather than stored. A no-op when the fs is unrestricted (rendered as
/// "All paths"), since there's nothing to scope.
fn augment_settings_sandbox_policy(
    policy: &SandboxPolicy,
    baseline: Vec<PathBuf>,
) -> SandboxPolicyDisplay {
    let fs = match &policy.fs {
        SandboxFsPolicy::Unrestricted { .. } => SandboxFsDisplay::Unrestricted,
        SandboxFsPolicy::Restricted { writable_paths, .. } => {
            // Dedup by display string. We deliberately don't open the locations'
            // fds to dedup by inode here: this is a display-only tooltip and the
            // string is the location's identity for that purpose. The string can
            // only diverge from the captured inode while a symlink-swap is
            // actively in progress, and in that case the bind validator refuses
            // to run the command at all (see the `sandbox` crate) — so showing the
            // requested path is always safe, and not worth a blocking syscall on
            // the render path.
            let mut merged: Vec<String> = Vec::new();
            let baseline_paths = baseline.iter().map(|path| path.display().to_string());
            let granted_paths = writable_paths
                .iter()
                .map(|location| location.untrusted_path_display().to_string());
            for path in baseline_paths.chain(granted_paths) {
                if !merged.contains(&path) {
                    merged.push(path);
                }
            }
            // `mut` is only needed on Linux, where the isolated `/tmp` entry is
            // pushed below.
            #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
            let mut entries: Vec<WritableEntryDisplay> =
                merged.into_iter().map(WritableEntryDisplay::Path).collect();
            // The ephemeral, host-isolated tmpfs at /tmp is Linux-specific (the
            // bwrap `--tmpfs /tmp` overlay). It's a display-only label, not a
            // real host path, so it can't be a captured location.
            #[cfg(target_os = "linux")]
            entries.push(WritableEntryDisplay::IsolatedTmp);
            SandboxFsDisplay::Restricted(entries)
        }
    };
    SandboxPolicyDisplay {
        fs,
        network: policy.network.clone(),
    }
}

fn sandbox_section(title: &str, policy: &SandboxPolicyDisplay, show_empty: bool) -> SandboxSection {
    let write_empty = fs_grants_nothing(&policy.fs);
    let network_empty = network_grants_nothing(&policy.network);
    let mut section = SandboxSection::new(title.to_string());

    if show_empty || !write_empty {
        section =
            section.group(SandboxGroup::new("Write Access").rows(sandbox_fs_rows(&policy.fs)));
    }

    if show_empty || !network_empty {
        section = section
            .group(SandboxGroup::new("Network Access").rows(sandbox_network_rows(&policy.network)));
    }

    section
}

/// Whether a policy grants nothing worth surfacing, used to decide whether to
/// show the per-thread overrides section at all.
fn sandbox_policy_grants_nothing(policy: &SandboxPolicyDisplay) -> bool {
    fs_grants_nothing(&policy.fs) && network_grants_nothing(&policy.network)
}

fn fs_grants_nothing(fs: &SandboxFsDisplay) -> bool {
    matches!(fs, SandboxFsDisplay::Restricted(entries) if entries.is_empty())
}

fn network_grants_nothing(network: &SandboxNetPolicy) -> bool {
    match network {
        SandboxNetPolicy::Blocked => true,
        SandboxNetPolicy::Restricted { allowed_domains } => allowed_domains.is_empty(),
        SandboxNetPolicy::Unrestricted => false,
    }
}

/// Rows for the write-access group: a message for the "all"/"none" cases, or one
/// row per granted path.
fn sandbox_fs_rows(fs: &SandboxFsDisplay) -> Vec<SandboxRow> {
    match fs {
        SandboxFsDisplay::Unrestricted => vec![SandboxRow::message(
            "All paths except protected Git metadata",
        )],
        SandboxFsDisplay::Restricted(entries) if entries.is_empty() => {
            vec![SandboxRow::message("None")]
        }
        SandboxFsDisplay::Restricted(entries) => entries
            .iter()
            .map(|entry| match entry {
                // The display string was captured up front.
                WritableEntryDisplay::Path(path) => SandboxRow::path(PathBuf::from(path)),
                #[cfg(target_os = "linux")]
                WritableEntryDisplay::IsolatedTmp => {
                    SandboxRow::path(PathBuf::from("/tmp (isolated)"))
                }
            })
            .collect(),
    }
}

/// Rows for the network-access group: a message for the "all"/"none" cases, or
/// one row per allowed domain.
fn sandbox_network_rows(network: &SandboxNetPolicy) -> Vec<SandboxRow> {
    match network {
        SandboxNetPolicy::Unrestricted => vec![SandboxRow::message("All domains (unrestricted)")],
        SandboxNetPolicy::Blocked => vec![SandboxRow::message("None")],
        SandboxNetPolicy::Restricted { allowed_domains } if allowed_domains.is_empty() => {
            vec![SandboxRow::message("None")]
        }
        SandboxNetPolicy::Restricted { allowed_domains } => allowed_domains
            .iter()
            .map(|domain| SandboxRow::domain(domain.clone()))
            .collect(),
    }
}

impl ThreadView {
    fn render_entries(&mut self, cx: &mut Context<Self>) -> List {
        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        let centered_container = move |content: AnyElement| {
            h_flex().w_full().justify_center().child(
                div()
                    .when_some(max_content_width, |this, max_w| this.max_w(max_w))
                    .w_full()
                    .child(content),
            )
        };

        list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let entries = this.thread.read(cx).entries();
                if let Some(entry) = entries.get(index) {
                    let rendered = this.render_entry(index, entries.len(), entry, window, cx);
                    centered_container(rendered.into_any_element()).into_any_element()
                } else {
                    Empty.into_any()
                }
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_grow_1()
    }

    fn render_entry(
        &self,
        entry_ix: usize,
        total_entries: usize,
        entry: &AgentThreadEntry,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        // Thinking is shown beside the progress indicator while it happens and
        // is not part of the transcript. Render it empty to keep the list index
        // 1:1 with the thread entries.
        if self
            .thread
            .read(cx)
            .entries()
            .get(entry_ix)
            .is_some_and(|entry| Self::is_thoughts_only_message(entry, cx))
        {
            return Empty.into_any_element();
        }

        let is_indented = entry.is_indented();
        let is_first_indented = is_indented
            && self
                .thread
                .read(cx)
                .entries()
                .get(entry_ix.saturating_sub(1))
                .is_none_or(|entry| !entry.is_indented());

        let primary = match &entry {
            AgentThreadEntry::UserMessage(message) => {
                let Some(editor) = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.message_editor())
                    .cloned()
                else {
                    return Empty.into_any_element();
                };

                let editing = self.editing_message == Some(entry_ix);
                let editor_focus = editor.focus_handle(cx).is_focused(window);
                let focus_border = cx.theme().colors().border_focused;
                // Drop shadows render as a dark halo on transparent windows.
                let opaque_window = cx.theme().window_background_appearance()
                    == gpui::WindowBackgroundAppearance::Opaque;

                let has_checkpoint_button = message
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.show);

                let is_subagent = self.is_subagent();
                let can_rewind = self.thread.read(cx).supports_truncate(cx);
                let is_editable = can_rewind && message.client_id.is_some() && !is_subagent;
                let agent_name = if is_subagent {
                    "subagents".into()
                } else {
                    self.agent_id.clone()
                };

                v_flex()
                    .id(("user_message", entry_ix))
                    .map(|this| {
                        if is_first_indented {
                            this.pt_0p5()
                        } else {
                            this.pt_2()
                        }
                    })
                    .pb_3()
                    .px_2()
                    .gap_1p5()
                    .w_full()
                    .when(is_editable && has_checkpoint_button, |this| {
                        this.children(message.client_id.clone().map(|client_id| {
                            h_flex()
                                .px_3()
                                .gap_2()
                                .child(Divider::horizontal())
                                .child(
                                    Button::new("restore-checkpoint", "Restore Checkpoint")
                                        .start_icon(Icon::new(IconName::Undo).size(IconSize::XSmall).color(Color::Muted))
                                        .label_size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .tooltip(Tooltip::text("Restores all files in the project to the content they had at this point in the conversation."))
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.restore_checkpoint(&client_id, cx);
                                        }))
                                )
                                .child(Divider::horizontal())
                        }))
                    })
                    .child(
                        // Chat-bubble layout: the message sits right-aligned
                        // at up to 80% of the row width, tinted with the
                        // accent color instead of a full-width bordered card.
                        h_flex().w_full().justify_end().child(
                        div()
                            .relative()
                            .w_full()
                            .max_w(relative(0.8))
                            .child(
                                div()
                                    .py_3()
                                    .px_2()
                                    .rounded_lg()
                                    .bg(cx
                                        .theme()
                                        .colors()
                                        .editor_background
                                        .blend(cx.theme().colors().text_accent.opacity(0.06)))
                                    .border_1()
                                    .when(is_indented, |this| {
                                        this.py_2().px_2().when(opaque_window, |this| {
                                            this.shadow_sm()
                                        })
                                    })
                                    .border_color(cx.theme().colors().text_accent.opacity(0.15))
                                    .map(|this| {
                                        if !is_editable {
                                            if is_subagent {
                                                return this.border_dashed();
                                            }
                                            return this;
                                        }
                                        if editing && editor_focus {
                                            return this.border_color(focus_border);
                                        }
                                        if editing && !editor_focus {
                                            return this.border_dashed()
                                        }
                                        this.when(opaque_window, |this| this.shadow_md())
                                            .hover(|s| {
                                                s.border_color(focus_border.opacity(0.8))
                                            })
                                    })
                                    .text_xs()
                                    .child(editor.clone().into_any_element())
                                    .children(self.render_sent_review_comments(entry_ix, message, cx))
                            )
                            .when(editor_focus, |this| {
                                let base_container = h_flex()
                                    .absolute()
                                    .top_neg_3p5()
                                    .right_3()
                                    .gap_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .bg(cx.theme().colors().editor_background)
                                    .overflow_hidden();

                                let is_loading_contents = self.is_loading_contents;
                                if is_editable {
                                    this.child(
                                        base_container
                                            .child(
                                                IconButton::new("cancel", IconName::Close)
                                                    .disabled(is_loading_contents)
                                                    .icon_color(Color::Error)
                                                    .icon_size(IconSize::XSmall)
                                                    .on_click(cx.listener(Self::cancel_editing))
                                            )
                                            .child(
                                                if is_loading_contents {
                                                    div()
                                                        .id("loading-edited-message-content")
                                                        .tooltip(Tooltip::text("Loading Added Context…"))
                                                        .child(loading_contents_spinner(IconSize::XSmall))
                                                        .into_any_element()
                                                } else {
                                                    IconButton::new("regenerate", IconName::Return)
                                                        .icon_color(Color::Muted)
                                                        .icon_size(IconSize::XSmall)
                                                        .tooltip(Tooltip::text(
                                                            "Editing will restart the conversation from this point."
                                                        ))
                                                        .on_click(cx.listener({
                                                            let editor = editor.clone();
                                                            move |this, _, window, cx| {
                                                                this.regenerate(
                                                                    entry_ix, editor.clone(), window, cx,
                                                                );
                                                            }
                                                        })).into_any_element()
                                                }
                                            )
                                    )
                                } else {
                                    this.child(
                                        base_container
                                            .border_dashed()
                                            .child(IconButton::new("non_editable", IconName::PencilUnavailable)
                                                .icon_size(IconSize::Small)
                                                .icon_color(Color::Muted)
                                                .style(ButtonStyle::Transparent)
                                                .tooltip(Tooltip::element({
                                                    let agent_name = agent_name.clone();
                                                    move |_, _| {
                                                        v_flex()
                                                            .gap_1()
                                                            .child(Label::new("Unavailable Editing"))
                                                            .child(
                                                                div().max_w_64().child(
                                                                    Label::new(format!(
                                                                        "Editing previous messages is not available for {} yet.",
                                                                        agent_name
                                                                    ))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Muted),
                                                                ),
                                                            )
                                                            .into_any_element()
                                                    }
                                                }))),
                                    )
                                }
                            }),
                    ))
                    .into_any()
            }
            AgentThreadEntry::AssistantMessage(message) => {
                // A message that is nothing but thoughts is part of the action
                // group around it: its thoughts are chips like any other action,
                // drawn by the run's first entry (see the tool call arm).
                if let Some((run_start, run_len)) = self.action_run_bounds(entry_ix, cx) {
                    if entry_ix != run_start {
                        return Empty.into_any();
                    }
                    return self.render_action_group(
                        self.thread.read(cx).session_id(),
                        run_start,
                        run_len,
                        &self.focus_handle(cx),
                        window,
                        cx,
                    );
                }

                // Thinking is only ever shown as the live thought beside the
                // progress indicator; the transcript renders just the prose.
                let mut prose_blank = true;
                let is_last = entry_ix + 1 == total_entries;

                let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                let message_body = v_flex()
                    .w_full()
                    .gap_3()
                    .children(message.chunks.iter().filter_map(|chunk| match chunk {
                        AssistantMessageChunk::Message { block, .. } => {
                            block.markdown().and_then(|md| {
                                let this_is_blank = md.read(cx).source().trim().is_empty();
                                prose_blank = prose_blank && this_is_blank;
                                if this_is_blank {
                                    return None;
                                }
                                Some(
                                    self.render_markdown(md.clone(), style.clone(), cx)
                                        .into_any_element(),
                                )
                            })
                        }
                        AssistantMessageChunk::Thought { .. } => None,
                    }))
                    .into_any();

                if prose_blank {
                    Empty.into_any()
                } else {
                    let prose_bubble = (!prose_blank).then(|| {
                        // Chat-bubble layout: assistant prose sits near flush
                        // left in a subtly tinted, rounded bubble at up to 96% of
                        // the row width.
                        v_flex()
                            .group("agent-message")
                            .relative()
                            .px_2()
                            .py_1p5()
                            .when(is_last, |this| this.pb_4())
                            .w_full()
                            .text_ui(cx)
                            .child(h_flex().w_full().justify_start().child(
                                div().w_full().max_w(relative(0.96)).py_2().child(
                                    self.render_message_context_menu(entry_ix, message_body, cx),
                                ),
                            ))
                            .child(
                                // Hover-revealed copy button at the response's
                                // top-right; reuses the context menu's copy logic.
                                div()
                                    .absolute()
                                    .top_2()
                                    .right_2()
                                    .visible_on_hover("agent-message")
                                    .child(
                                        IconButton::new(
                                            ("copy-agent-response", entry_ix),
                                            IconName::Copy,
                                        )
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text("Copy Response"))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                let entries = this.thread.read(cx).entries();
                                                if let Some(text) = Self::get_agent_message_content(
                                                    entries, entry_ix, cx,
                                                ) {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(text),
                                                    );
                                                }
                                            }),
                                        ),
                                    ),
                            )
                            .when_some(
                                self.entry_view_state
                                    .read(cx)
                                    .entry(entry_ix)
                                    .and_then(|entry| entry.focus_handle(cx)),
                                |this, handle| this.track_focus(&handle),
                            )
                    });

                    v_flex().w_full().children(prose_bubble).into_any()
                }
            }
            AgentThreadEntry::ToolCall(tool_call) if tool_call.is_compaction(cx) => {
                self.render_compaction_barrier(entry_ix, tool_call, cx)
            }
            AgentThreadEntry::ToolCall(tool_call) => {
                // A run of consecutive agent actions renders as one compact grid
                // of headline chips (a group ends at the next assistant text
                // message). The run's first entry draws the whole group; the
                // rest draw nothing so the list index stays 1:1 with the thread
                // entries. Permission prompts and subagents are not chips and
                // fall through to their full rendering below.
                if let Some((run_start, run_len)) = self.action_run_bounds(entry_ix, cx) {
                    if entry_ix != run_start {
                        return Empty.into_any();
                    }
                    return self.render_action_group(
                        self.thread.read(cx).session_id(),
                        run_start,
                        run_len,
                        &self.focus_handle(cx),
                        window,
                        cx,
                    );
                }

                // A canceled tool call that produced visible output is still worth
                // showing, but one that was canceled before producing anything just
                // renders as a useless "Canceled" card — hide those entirely.
                if matches!(tool_call.status, ToolCallStatus::Canceled) {
                    let has_visible_content =
                        tool_call.content.iter().any(|content| match content {
                            ToolCallContent::ContentBlock(block) => block.visible_content(cx),
                            ToolCallContent::Diff(_) | ToolCallContent::Terminal(_) => true,
                        });
                    if !has_visible_content {
                        return Empty.into_any();
                    }
                }

                let tool_call = self.render_any_tool_call(
                    self.thread.read(cx).session_id(),
                    entry_ix,
                    tool_call,
                    &self.focus_handle(cx),
                    ToolCallLayout::Standalone,
                    window,
                    cx,
                );

                if let Some(handle) = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.focus_handle(cx))
                {
                    tool_call.track_focus(&handle).into_any()
                } else {
                    tool_call.into_any()
                }
            }
            AgentThreadEntry::Elicitation(elicitation_id) => {
                let thread = self.thread.read(cx);
                if let Some((_, elicitation)) = thread.elicitation(elicitation_id)
                    && should_render_elicitation(elicitation)
                {
                    let elicitation = self.render_elicitation(entry_ix, elicitation, window, cx);

                    if let Some(handle) = self
                        .entry_view_state
                        .read(cx)
                        .entry(entry_ix)
                        .and_then(|entry| entry.focus_handle(cx))
                    {
                        elicitation.track_focus(&handle).into_any()
                    } else {
                        elicitation.into_any()
                    }
                } else {
                    Empty.into_any()
                }
            }
            // The plan has one surface, the working indicator; a finished plan
            // does not get a second card in the transcript.
            AgentThreadEntry::CompletedPlan(_) => Empty.into_any(),
            AgentThreadEntry::ContextCompaction(compaction) => {
                self.render_context_compaction(entry_ix, compaction, window, cx)
            }
        };

        let is_subagent_output = self.is_subagent()
            && matches!(entry, AgentThreadEntry::AssistantMessage(msg) if msg.is_subagent_output);

        let primary = if is_subagent_output {
            v_flex()
                .w_full()
                .child(
                    h_flex()
                        .id("subagent_output")
                        .px_5()
                        .py_1()
                        .gap_2()
                        .child(Divider::horizontal())
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::ForwardArrowUp)
                                        .color(Color::Muted)
                                        .size(IconSize::Small),
                                )
                                .child(
                                    Label::new("Subagent Output")
                                        .size(LabelSize::Custom(self.tool_name_font_size()))
                                        .color(Color::Muted),
                                ),
                        )
                        .child(Divider::horizontal())
                        .tooltip(Tooltip::text("Everything below this line was sent as output from this subagent to the main agent.")),
                )
                .child(primary)
                .into_any_element()
        } else {
            primary
        };

        let thread = self.thread.clone();

        let primary = if is_indented {
            let line_top = if is_first_indented {
                rems_from_px(-12.0_f32)
            } else {
                rems_from_px(0.0_f32)
            };

            div()
                .relative()
                .w_full()
                .pl_5()
                .bg(cx.theme().colors().panel_background.opacity(0.2))
                .child(
                    div()
                        .absolute()
                        .left(rems_from_px(18.0_f32))
                        .top(line_top)
                        .bottom_0()
                        .w_px()
                        .bg(cx.theme().colors().border.opacity(0.6)),
                )
                .child(primary)
                .into_any_element()
        } else {
            primary
        };

        let needs_confirmation = thread.read(cx).is_waiting_for_confirmation()
            || self.has_pending_request_elicitation(cx);

        let comments_editor = self.thread_feedback.comments_editor.clone();

        let primary = if entry_ix + 1 == total_entries {
            v_flex()
                .w_full()
                .child(primary)
                .when(!needs_confirmation, |this| {
                    this.child(self.render_thread_controls(&thread, cx))
                })
                .when_some(comments_editor, |this, editor| {
                    this.child(Self::render_feedback_feedback_editor(editor, cx))
                })
                .into_any_element()
        } else {
            primary
        };

        if let Some(editing_index) = self.editing_message
            && editing_index < entry_ix
        {
            let is_subagent = self.is_subagent();

            let backdrop = div()
                .id(("backdrop", entry_ix))
                .size_full()
                .absolute()
                .inset_0()
                .bg(cx.theme().colors().panel_background)
                .opacity(0.8)
                .block_mouse_except_scroll()
                .on_click(cx.listener(Self::cancel_editing));

            div()
                .relative()
                .child(primary)
                .when(!is_subagent, |this| this.child(backdrop))
                .into_any_element()
        } else {
            primary
        }
    }

    fn render_elicitation(
        &self,
        entry_ix: usize,
        elicitation: &Elicitation,
        _window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        ElicitationCard::new(
            entry_ix,
            elicitation,
            self.agent_display_name.clone(),
            self.elicitation_form_states.get(&elicitation.id),
            self.elicitation_card_handlers(cx),
        )
        .render(cx)
    }

    fn elicitation_card_handlers(&self, cx: &Context<Self>) -> ElicitationCardHandlers {
        let view = cx.entity().downgrade();

        ElicitationCardHandlers::new(
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.submit_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.decline_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.cancel_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.dismiss_url_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            move |_elicitation_id, url, _window, cx| cx.open_url(&url),
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                            form.set_boolean(&field_name, value);
                            cx.notify();
                        }
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                            form.set_single_select(&field_name, value);
                            cx.notify();
                        }
                    })
                    .log_err();
                }
            },
            move |elicitation_id, field_name, value, selected, cx| {
                view.update(cx, |this, cx| {
                    if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                        form.set_multi_select(&field_name, value, selected);
                        cx.notify();
                    }
                })
                .log_err();
            },
        )
    }

    fn render_feedback_feedback_editor(editor: Entity<Editor>, cx: &Context<Self>) -> Div {
        h_flex()
            .key_context("AgentFeedbackMessageEditor")
            .on_action(cx.listener(move |this, _: &menu::Cancel, _, cx| {
                this.thread_feedback.dismiss_comments();
                cx.notify();
            }))
            .on_action(cx.listener(move |this, _: &menu::Confirm, _window, cx| {
                this.submit_feedback_message(cx);
            }))
            .p_2()
            .mb_2()
            .mx_5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(div().w_full().child(editor))
            .child(
                h_flex()
                    .child(
                        IconButton::new("dismiss-feedback-message", IconName::Close)
                            .icon_color(Color::Error)
                            .icon_size(IconSize::XSmall)
                            .shape(ui::IconButtonShape::Square)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.thread_feedback.dismiss_comments();
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("submit-feedback-message", IconName::Return)
                            .icon_size(IconSize::XSmall)
                            .shape(ui::IconButtonShape::Square)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.submit_feedback_message(cx);
                            })),
                    ),
            )
    }

    fn render_thread_controls(
        &self,
        thread: &Entity<AcpThread>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_generating = matches!(thread.read(cx).status(), ThreadStatus::Generating);

        if is_generating {
            return Empty.into_any_element();
        }

        let last_response_index = thread
            .read(cx)
            .entries()
            .iter()
            .rposition(|entry| matches!(entry, AgentThreadEntry::AssistantMessage(_)));

        let copy_response_button = last_response_index.map(|response_index| {
            IconButton::new("copy_agent_response", IconName::Copy)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Copy This Agent Response"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let entries = this.thread.read(cx).entries();
                    if let Some(text) = Self::get_agent_message_content(entries, response_index, cx)
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }))
        });

        let show_stats = AgentSettings::get_global(cx).show_turn_stats;

        let last_turn_clock = show_stats
            .then(|| {
                self.turn_fields
                    .last_turn_duration
                    .filter(|&duration| duration > STOPWATCH_THRESHOLD)
                    .map(|duration| {
                        Label::new(duration_alt_display(duration))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })
            })
            .flatten();

        let last_turn_tokens_label = last_turn_clock
            .is_some()
            .then(|| {
                self.turn_fields
                    .last_turn_tokens
                    .filter(|&tokens| tokens > TOKEN_THRESHOLD)
                    .map(|tokens| {
                        Label::new(format!("{} tokens", crate::humanize_token_count(tokens)))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })
            })
            .flatten();

        let feedback_buttons = (self.is_subagent() && self.is_thread_feedback_enabled(cx)).then(
            || {
                let feedback = self.thread_feedback.feedback;
                let tooltip_meta =
                    "Rating sends all of your current conversation to the Zed team.";

                h_flex()
                    .child(
                        IconButton::new("feedback-thumbs-up", IconName::ThumbsUp)
                            .icon_size(IconSize::Small)
                            .icon_color(match feedback {
                                Some(ThreadFeedback::Positive) => Color::Accent,
                                _ => Color::Muted,
                            })
                            .tooltip(move |window, cx| match feedback {
                                Some(ThreadFeedback::Positive) => {
                                    Tooltip::text("Thanks for your feedback!")(window, cx)
                                }
                                _ => Tooltip::with_meta(
                                    "Helpful Response",
                                    None,
                                    tooltip_meta,
                                    cx,
                                ),
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.handle_feedback_click(ThreadFeedback::Positive, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("feedback-thumbs-down", IconName::ThumbsDown)
                            .icon_size(IconSize::Small)
                            .icon_color(match feedback {
                                Some(ThreadFeedback::Negative) => Color::Accent,
                                _ => Color::Muted,
                            })
                            .tooltip(move |window, cx| match feedback {
                                Some(ThreadFeedback::Negative) => Tooltip::text(
                                    "We appreciate your feedback and will use it to improve in the future.",
                                )(
                                    window, cx
                                ),
                                _ => Tooltip::with_meta(
                                    "Not Helpful Response",
                                    None,
                                    tooltip_meta,
                                    cx,
                                ),
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.handle_feedback_click(ThreadFeedback::Negative, window, cx);
                            })),
                    )
            },
        );

        let separator_dots = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .alpha(0.5)
        };

        h_flex()
            .w_full()
            .py_1p5()
            .px_4()
            .justify_end()
            .opacity(0.4)
            .hover(|s| s.opacity(1.))
            .when(
                last_turn_tokens_label.is_some() || last_turn_clock.is_some(),
                |this| {
                    this.child(
                        h_flex()
                            .px_1()
                            .gap_1()
                            .when_some(last_turn_tokens_label, |this, label| {
                                this.child(label).child(separator_dots())
                            })
                            .when_some(last_turn_clock, |this, label| {
                                this.child(label).child(separator_dots())
                            }),
                    )
                },
            )
            .when_some(feedback_buttons, |this, buttons| this.child(buttons))
            .when_some(copy_response_button, |this, button| this.child(button))
            .into_any_element()
    }

    fn is_thread_feedback_enabled(&self, cx: &App) -> bool {
        util::maybe!({
            let project = self.thread.read(cx).project().read(cx);
            let user_store = project.user_store();
            if let Some(configuration) = user_store.read(cx).current_organization_configuration() {
                if !configuration.is_agent_thread_feedback_enabled {
                    return false;
                }
            }

            AgentSettings::get_global(cx).enable_feedback
                && self.thread.read(cx).connection().telemetry().is_some()
        })
    }

    // The local slash commands the message editor should currently expose.
    // Kept in sync with the availability of the corresponding actions via
    // `sync_local_commands`.
    fn available_local_commands(&self, cx: &App) -> Vec<PromptLocalCommand> {
        let mut commands = Vec::new();

        if self.is_thread_feedback_enabled(cx) {
            commands.push(PromptLocalCommand::ThumbsUp);
            commands.push(PromptLocalCommand::ThumbsDown);
        }

        commands
    }

    // Pushes the current set of available local commands to the message
    // editor so they appear in its slash-command popup.
    pub(crate) fn sync_local_commands(&self, cx: &App) {
        let commands = self.available_local_commands(cx);
        self.message_editor.read(cx).set_local_commands(commands);
    }

    fn render_request_elicitations(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let server_view = self.server_view.clone();
        let handlers_view = server_view.clone();
        server_view
            .read_with(cx, |server_view, cx| {
                let Some(connection) = server_view.request_elicitation_connection() else {
                    return Vec::new();
                };
                server_view.render_request_elicitations(&connection, handlers_view, cx)
            })
            .unwrap_or_default()
    }

    // Retained for its test coverage; no longer surfaced as a button.
    #[cfg(test)]
    pub(crate) fn scroll_to_most_recent_user_prompt(&mut self, cx: &mut Context<Self>) {
        let entries = self.thread.read(cx).entries();
        if entries.is_empty() {
            return;
        }

        // Find the most recent user message and scroll it to the top of the viewport.
        // (Fallback: if no user message exists, scroll to the bottom.)
        if let Some(ix) = entries
            .iter()
            .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.0),
            });
            cx.notify();
        } else {
            self.scroll_to_end(cx);
        }
    }

    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        cx.notify();
    }

    fn handle_feedback_click(
        &mut self,
        feedback: ThreadFeedback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.thread_feedback
            .submit(self.thread.clone(), feedback, window, cx);
        cx.notify();
    }

    fn submit_feedback_message(&mut self, cx: &mut Context<Self>) {
        let thread = self.thread.clone();
        self.thread_feedback.submit_comments(thread, cx);
        cx.notify();
    }

    pub(crate) fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to(ListOffset::default());
        cx.notify();
    }

    fn scroll_output_page_up(
        &mut self,
        _: &ScrollOutputPageUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page_height = self.list_state.viewport_bounds().size.height;
        self.list_state.scroll_by(-page_height * 0.9);
        cx.notify();
    }

    fn scroll_output_page_down(
        &mut self,
        _: &ScrollOutputPageDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page_height = self.list_state.viewport_bounds().size.height;
        self.list_state.scroll_by(page_height * 0.9);
        cx.notify();
    }

    fn scroll_output_line_up(
        &mut self,
        _: &ScrollOutputLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_state.scroll_by(-window.line_height() * 3.);
        cx.notify();
    }

    fn scroll_output_line_down(
        &mut self,
        _: &ScrollOutputLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_state.scroll_by(window.line_height() * 3.);
        cx.notify();
    }

    fn scroll_output_to_top(
        &mut self,
        _: &ScrollOutputToTop,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to_top(cx);
    }

    fn scroll_output_to_bottom(
        &mut self,
        _: &ScrollOutputToBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to_end(cx);
    }

    fn scroll_output_to_previous_message(
        &mut self,
        _: &ScrollOutputToPreviousMessage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let current_ix = self.list_state.logical_scroll_top().item_ix;
        if let Some(target_ix) = (0..current_ix)
            .rev()
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: target_ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn scroll_output_to_next_message(
        &mut self,
        _: &ScrollOutputToNextMessage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let current_ix = self.list_state.logical_scroll_top().item_ix;
        if let Some(target_ix) = (current_ix + 1..entries.len())
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: target_ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn refresh_thread_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.thread_search_visible {
            return;
        }
        if let Some(bar) = self.thread_search_bar.clone() {
            bar.update(cx, |bar, cx| bar.update_matches(window, cx));
        }
    }

    /// Hides the thread search bar, clears its highlights, and returns focus to
    /// the message editor. Returns `true` if the search bar was visible.
    pub(crate) fn close_thread_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.thread_search_visible {
            return false;
        }

        if let Some(bar) = self.thread_search_bar.clone() {
            bar.update(cx, |bar, cx| bar.clear_highlights(cx));
        }

        self.thread_search_visible = false;
        self.message_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
        true
    }

    pub(crate) fn toggle_search(
        &mut self,
        _: &crate::ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.thread_search_bar.is_none() {
            let thread = self.thread.clone();
            let view = cx.entity().downgrade();
            let on_activate =
                Arc::new(move |entry_ix: usize, _window: &mut Window, cx: &mut App| {
                    // Avoid re-entering `ThreadView` when search navigation is forwarded
                    // from a `ThreadView` action handler.
                    let view = view.clone();
                    cx.defer(move |cx| {
                        view.update(cx, |this, cx| {
                            this.list_state.scroll_to(gpui::ListOffset {
                                item_ix: entry_ix,
                                offset_in_item: gpui::px(0.),
                            });
                            cx.notify();
                        })
                        .ok();
                    });
                });
            let search_bar = cx.new(|cx| {
                ThreadSearchBar::new(
                    thread,
                    self.entry_view_state.clone(),
                    on_activate,
                    window,
                    cx,
                )
            });
            self._subscriptions.push(cx.subscribe_in(
                &search_bar,
                window,
                |this, _bar, event, window, cx| {
                    if matches!(event, ThreadSearchBarEvent::Dismissed) {
                        this.thread_search_visible = false;
                        this.message_editor.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    }
                },
            ));
            self.thread_search_bar = Some(search_bar);
        }

        // Re-focus an open bar unless it already owns focus.
        let search_bar_focused = self
            .thread_search_bar
            .as_ref()
            .is_some_and(|bar| bar.focus_handle(cx).contains_focused(window, cx));

        if self.thread_search_visible && search_bar_focused {
            if let Some(bar) = &self.thread_search_bar {
                bar.update(cx, |bar, cx| bar.clear_highlights(cx));
            }
            self.thread_search_visible = false;
            self.message_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        } else {
            self.thread_search_visible = true;
            if let Some(bar) = self.thread_search_bar.clone() {
                bar.update(cx, |bar, cx| bar.focus_and_refresh(window, cx));
            }
            cx.notify();
        }
    }

    pub fn open_thread_as_markdown(
        &self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let thread = self.thread.read(cx);
        let thread_title = thread
            .title()
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into())
            .to_string();
        let markdown = thread.to_markdown(cx);

        open_markdown_in_workspace(thread_title, markdown, workspace, window, cx)
    }

    pub(crate) fn sync_editor_mode(&mut self, cx: &mut Context<Self>) {
        let has_messages = self.list_state.item_count() > 0;
        let v2_empty_state = !has_messages;

        if !has_messages {
            self.editor_expanded = false;
        }

        let mode = if self.editor_expanded {
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
            }
        } else if v2_empty_state {
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::Default,
            }
        } else {
            EditorMode::AutoHeight {
                min_lines: AgentSettings::get_global(cx).message_editor_min_lines,
                max_lines: Some(AgentSettings::get_global(cx).set_message_editor_max_lines()),
            }
        };
        self.message_editor.update(cx, |editor, cx| {
            editor.set_mode(mode, cx);
        });
    }

    /// Subagent tool calls of this thread that are still running. Surfaced as a
    /// chip next to the working indicator so a quiet turn that is really several
    /// subagents deep reads as such.
    pub(crate) fn running_subagent_count(&self, cx: &App) -> usize {
        self.thread
            .read(cx)
            .entries()
            .iter()
            .filter(|entry| match entry {
                AgentThreadEntry::ToolCall(tool_call) => {
                    tool_call.subagent_session_info.is_some()
                        && matches!(
                            tool_call.status,
                            ToolCallStatus::InProgress | ToolCallStatus::Pending
                        )
                }
                _ => false,
            })
            .count()
    }

    /// The review comments attached to a sent user message, rendered the same
    /// way the diff review overlay shows them: each comment's quoted code and
    /// text in a bordered, tinted block, so the message and the diff agree.
    fn render_sent_review_comments(
        &self,
        entry_ix: usize,
        message: &acp_thread::UserMessage,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let blocks = crate::diff_review::review_blocks(&message.chunks);
        if blocks.is_empty() {
            return None;
        }
        Some(self.render_review_comments(("sent-review", entry_ix), &blocks, cx))
    }

    /// The review-comment visual shared by a sent message's bubble and a queued
    /// message's row: one bordered, tinted block per comment, each showing the
    /// quoted code it anchors to and then the comment text, matching the diff
    /// review overlay's comment block.
    ///
    /// The blocks are parsed out of the composed review text (`review_blocks`,
    /// the only public accessor today). When `diff_review::review_comment_blocks`
    /// lands (a structured `Vec<ParsedReview { comments: Vec<{ path, range,
    /// quoted_code, comment }> }>`), this should read that instead of parsing.
    fn render_review_comments(
        &self,
        id_seed: impl Into<ElementId>,
        blocks: &[(usize, SharedString)],
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let comments: Vec<ParsedReviewComment> = blocks
            .iter()
            .flat_map(|(_, text)| parse_review_block(text))
            .collect();

        v_flex()
            .id(id_seed)
            .pt_1p5()
            .w_full()
            .gap_1p5()
            .children(comments.into_iter().map(|comment| {
                v_flex()
                    .w_full()
                    .gap_1()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface_background)
                    .child(
                        Label::new(comment.location)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .when_some(comment.code, |this, code| {
                        this.child(
                            v_flex()
                                .w_full()
                                .px_1p5()
                                .py_1()
                                .rounded_sm()
                                .bg(colors.editor_background)
                                .children(code.lines().map(|line| {
                                    Label::new(line.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx)
                                })),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text)
                            .child(comment.comment),
                    )
            }))
            .into_any_element()
    }

    /// The plan as one compact line inside the working indicator, and, while
    /// expanded, the full list under it. This is the thread's only plan
    /// surface: there is no separate task list.
    /// The plan as its own full-width block above the input. Collapsed it is
    /// the last completed item, the current one, and the next one; expanded it
    /// is every completed and upcoming item in those same rows, rather than a
    /// second list underneath.
    fn render_plan(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let plan = self.thread.read(cx).plan();
        if plan.is_empty() {
            return None;
        }
        let expanded = self.plan_expanded;

        let completed: Vec<&PlanEntry> = plan
            .entries
            .iter()
            .filter(|entry| matches!(entry.status, acp::PlanEntryStatus::Completed))
            .collect();
        let upcoming: Vec<&PlanEntry> = plan
            .entries
            .iter()
            .filter(|entry| matches!(entry.status, acp::PlanEntryStatus::Pending))
            .collect();
        let current = plan.stats().in_progress_entry;

        // Collapsed shows the tail of what is done and the head of what is not;
        // expanded shows all of both. A "+1" overflow row costs the same space
        // as the item it hides, so two items are always shown in full rather
        // than one item plus a "+1 completed" line.
        let collapsed_take = |len: usize| if len <= 2 { len } else { 1 };
        let shown_completed: Vec<&PlanEntry> = if expanded {
            completed.clone()
        } else {
            let take = collapsed_take(completed.len());
            completed.iter().rev().take(take).rev().copied().collect()
        };
        let shown_upcoming: Vec<&PlanEntry> = if expanded {
            upcoming.clone()
        } else {
            let take = collapsed_take(upcoming.len());
            upcoming.iter().take(take).copied().collect()
        };
        let completed_overflow = completed.len() - shown_completed.len();
        let upcoming_overflow = upcoming.len() - shown_upcoming.len();

        let plan_row = |glyph: AnyElement, text: SharedString, color: Color| {
            h_flex()
                .w_full()
                .gap_1p5()
                .min_w_0()
                .child(h_flex().w_2().flex_none().justify_center().child(glyph))
                // The label takes the row's free width and shrinks within it.
                // Without flex_1 here the text is laid out against a zero basis
                // and truncates to an ellipsis.
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(text)
                            .size(LabelSize::Small)
                            .color(color)
                            .truncate(),
                    ),
                )
        };

        // Overflow counts sit on their own line under the entries they stand
        // for, aligned to the text column, so they read as a continuation
        // rather than a suffix of one entry.
        let overflow_row = |text: String| {
            h_flex()
                .w_full()
                .gap_1p5()
                .min_w_0()
                .child(h_flex().w_2().flex_none())
                .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
        };

        let mut rows = v_flex().w_full().gap_0p5().min_w_0();
        if completed_overflow > 0 {
            rows = rows.child(overflow_row(format!("+{completed_overflow} completed")));
        }
        for entry in shown_completed.iter() {
            rows = rows.child(plan_row(
                Icon::new(IconName::TodoComplete)
                    .size(IconSize::XSmall)
                    .color(Color::Success)
                    .into_any_element(),
                plan_entry_text(entry, cx),
                Color::Muted,
            ));
        }
        if let Some(entry) = current {
            // The in-progress item speaks the same language as the turn itself:
            // the shared running glyph.
            rows = rows.child(plan_row(
                ui::agent_running_indicator().into_any_element(),
                plan_entry_text(entry, cx),
                Color::Default,
            ));
        }
        for entry in shown_upcoming.iter() {
            rows = rows.child(plan_row(
                Icon::new(IconName::TodoPending)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
                plan_entry_text(entry, cx),
                Color::Muted,
            ));
        }
        if upcoming_overflow > 0 {
            rows = rows.child(overflow_row(format!("+{upcoming_overflow} more")));
        }

        Some(
            v_flex()
                .id("plan-line")
                .w_full()
                .min_w_0()
                // Air between the transcript above and the plan.
                .mt_2()
                .mb_1()
                // Say that the block expands: pointer plus a quiet hover tint.
                .cursor_pointer()
                .rounded_md()
                .hover(|this| this.bg(cx.theme().colors().element_hover.opacity(0.5)))
                .child(rows)
                .tooltip(Tooltip::text(if expanded {
                    "Collapse Plan"
                } else {
                    "Expand Plan"
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.plan_expanded = !this.plan_expanded;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    fn render_generating(&self, confirmation: bool, cx: &Context<Self>) -> impl IntoElement {
        let running_subagents = self.running_subagent_count(cx);
        let show_stats = AgentSettings::get_global(cx).show_turn_stats;
        let elapsed_label = show_stats
            .then(|| {
                self.turn_fields.turn_started_at.and_then(|started_at| {
                    let elapsed = started_at.elapsed();
                    (elapsed > STOPWATCH_THRESHOLD).then(|| duration_alt_display(elapsed))
                })
            })
            .flatten();

        let is_blocked_on_terminal_command =
            !confirmation && self.is_blocked_on_terminal_command(cx);
        // The plan renders as a sibling row above this indicator; when its
        // in-progress entry is drawn it already carries the running glyph.
        let plan_carries_the_spinner = !confirmation
            && !self.thread.read(cx).plan().is_empty()
            && self
                .thread
                .read(cx)
                .plan()
                .stats()
                .in_progress_entry
                .is_some();
        let is_waiting = confirmation || self.thread.read(cx).has_in_progress_tool_calls();

        let turn_tokens_label = elapsed_label
            .is_some()
            .then(|| {
                self.turn_fields
                    .turn_tokens
                    .filter(|&tokens| tokens > TOKEN_THRESHOLD)
                    .map(|tokens| crate::humanize_token_count(tokens))
            })
            .flatten();

        let arrow_icon = if is_waiting {
            IconName::ArrowUp
        } else {
            IconName::ArrowDown
        };

        // The indicator shares the Stop button's row above the input, so it
        // takes the row's free width and carries no padding of its own.
        h_flex()
            .id("generating-spinner")
            .min_w_0()
            .flex_1()
            .gap_2()
            .map(|this| {
                if confirmation {
                    this.child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(GeneratingSpinnerElement::new(SpinnerVariant::Sand)),
                    )
                    .child(
                        div().min_w(rems(8.)).child(
                            LoadingLabel::new("Awaiting Confirmation")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                } else if is_blocked_on_terminal_command || plan_carries_the_spinner {
                    // The plan's in-progress row (rendered just above this) has
                    // the running glyph; a second one here would say no more.
                    this
                } else {
                    // The same running glyph as sidebar rows and thread tabs.
                    this.child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(ui::agent_running_indicator()),
                    )
                }
            })
            .when(running_subagents > 0, |this| {
                this.child(
                    h_flex()
                        .id("running-subagents")
                        .gap_1()
                        .px_1()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border.opacity(0.6))
                        .child(
                            Icon::new(IconName::Person)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!(
                                "{running_subagents} subagent{}",
                                if running_subagents == 1 { "" } else { "s" }
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text("Subagents running in this turn")),
                )
            })
            .when_some(elapsed_label, |this, elapsed| {
                this.child(
                    Label::new(elapsed)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(turn_tokens_label, |this, tokens| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(arrow_icon)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("{} tokens", tokens))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .into_any_element()
    }

    pub(crate) fn auto_expand_streaming_thought(&mut self, cx: &mut Context<Self>) {
        let thread = self.thread.clone();
        let changed = self.entry_view_state.update(cx, |state, cx| {
            let thread = thread.read(cx);
            if thread.status() != ThreadStatus::Generating {
                return false;
            }
            state.auto_expand_streaming_thought(thread, cx)
        });
        if changed {
            cx.notify();
        }
    }

    pub(crate) fn clear_auto_expand_tracking(&mut self, cx: &mut Context<Self>) {
        self.entry_view_state.update(cx, |state, _cx| {
            state.clear_auto_expand_tracking();
        });
    }

    fn render_message_context_menu(
        &self,
        entry_ix: usize,
        message_body: AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let workspace = self.workspace.clone();

        right_click_menu(format!("agent_context_menu-{}", entry_ix))
            .trigger(move |_, _, _| message_body)
            .menu(move |window, cx| {
                let focus = window.focused(cx);
                let entity = entity.clone();
                let workspace = workspace.clone();

                ContextMenu::build(window, cx, move |menu, _, cx| {
                    let this = entity.read(cx);
                    let is_at_top = this.list_state.logical_scroll_top().item_ix == 0;

                    let chunks =
                        this.thread.read(cx).entries().get(entry_ix).and_then(
                            |entry| match &entry {
                                AgentThreadEntry::AssistantMessage(msg) => Some(&msg.chunks),
                                _ => None,
                            },
                        );

                    let has_selection = chunks
                        .map(|chunks| {
                            chunks.iter().any(|chunk| {
                                let md = match chunk {
                                    AssistantMessageChunk::Message { block, .. } => {
                                        block.markdown()
                                    }
                                    AssistantMessageChunk::Thought { block, .. } => {
                                        block.markdown()
                                    }
                                };
                                md.map_or(false, |m| m.read(cx).has_selection())
                            })
                        })
                        .unwrap_or(false);

                    let context_menu_link = chunks.and_then(|chunks| {
                        chunks.iter().find_map(|chunk| {
                            let md = match chunk {
                                AssistantMessageChunk::Message { block, .. } => block.markdown(),
                                AssistantMessageChunk::Thought { block, .. } => block.markdown(),
                            };
                            md.and_then(|m| m.read(cx).context_menu_link().cloned())
                        })
                    });

                    let copy_this_agent_response =
                        ContextMenuEntry::new("Copy This Agent Response").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    let entries = this.thread.read(cx).entries();
                                    if let Some(text) =
                                        Self::get_agent_message_content(entries, entry_ix, cx)
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                    }
                                });
                            }
                        });

                    let scroll_item = if is_at_top {
                        ContextMenuEntry::new("Scroll to Bottom").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.scroll_to_end(cx);
                                });
                            }
                        })
                    } else {
                        ContextMenuEntry::new("Scroll to Top").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.scroll_to_top(cx);
                                });
                            }
                        })
                    };

                    let open_thread_as_markdown = ContextMenuEntry::new("Open Thread as Markdown")
                        .handler({
                            let entity = entity.clone();
                            let workspace = workspace.clone();
                            move |window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    entity
                                        .update(cx, |this, cx| {
                                            this.open_thread_as_markdown(workspace, window, cx)
                                        })
                                        .detach_and_log_err(cx);
                                }
                            }
                        });

                    menu.when_some(focus, |menu, focus| menu.context(focus))
                        .when_some(context_menu_link, |menu, url| {
                            menu.entry("Copy Link", None, move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()));
                            })
                            .separator()
                        })
                        .action_disabled_when(
                            !has_selection,
                            "Copy Selection",
                            Box::new(markdown::CopyAsMarkdown),
                        )
                        .item(copy_this_agent_response)
                        .separator()
                        .item(scroll_item)
                        .item(open_thread_as_markdown)
                })
            })
            .into_any_element()
    }

    fn get_agent_message_content(
        entries: &[AgentThreadEntry],
        entry_index: usize,
        cx: &App,
    ) -> Option<String> {
        let entry = entries.get(entry_index)?;
        if matches!(entry, AgentThreadEntry::UserMessage(_)) {
            return None;
        }

        let start_index = (0..entry_index)
            .rev()
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
            .map(|i| i + 1)
            .unwrap_or(0);

        let end_index = (entry_index + 1..entries.len())
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
            .map(|i| i - 1)
            .unwrap_or(entries.len() - 1);

        let parts: Vec<String> = (start_index..=end_index)
            .filter_map(|i| entries.get(i))
            .filter_map(|entry| {
                if let AgentThreadEntry::AssistantMessage(message) = entry {
                    let text: String = message
                        .chunks
                        .iter()
                        .filter_map(|chunk| match chunk {
                            AssistantMessageChunk::Message { block, .. } => {
                                let markdown = block.to_markdown(cx);
                                if markdown.trim().is_empty() {
                                    None
                                } else {
                                    Some(markdown.to_string())
                                }
                            }
                            AssistantMessageChunk::Thought { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");

                    if text.is_empty() { None } else { Some(text) }
                } else {
                    None
                }
            })
            .collect();

        let text = parts.join("\n\n");
        if text.is_empty() { None } else { Some(text) }
    }

    fn is_blocked_on_terminal_command(&self, cx: &App) -> bool {
        let thread = self.thread.read(cx);
        if !matches!(thread.status(), ThreadStatus::Generating) {
            return false;
        }

        let mut has_running_terminal_call = false;

        for entry in thread.entries().iter().rev() {
            match entry {
                AgentThreadEntry::UserMessage(_) => break,
                AgentThreadEntry::ToolCall(tool_call)
                    if matches!(
                        tool_call.status,
                        ToolCallStatus::InProgress | ToolCallStatus::Pending
                    ) =>
                {
                    if matches!(tool_call.kind, acp::ToolKind::Execute) {
                        has_running_terminal_call = true;
                    } else {
                        return false;
                    }
                }
                AgentThreadEntry::ToolCall(_)
                | AgentThreadEntry::Elicitation(_)
                | AgentThreadEntry::AssistantMessage(_)
                | AgentThreadEntry::CompletedPlan(_)
                | AgentThreadEntry::ContextCompaction(_) => {}
            }
        }

        has_running_terminal_call
    }

    fn render_collapsible_command(
        &self,
        group: SharedString,
        is_preview: bool,
        command: Entity<Markdown>,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        // The label's markdown source is a fenced code block; strip the fences
        // so the copy button yields just the command text.
        let command_source = command.read(cx).source();
        let command_text = strip_command_fences(&command_source).to_string();

        let mut style =
            MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_agent_buffer_font(cx);
        style.container_style.text.font_size = Some(rems_from_px(12_f32).into());
        style.container_style.text.line_height = Some(rems_from_px(17_f32).into());
        style.height_is_multiple_of_line_height = true;
        // Soft-wrap the command instead of horizontally scrolling it: the card is
        // narrow, and in scroll mode a long command wraps anyway but its wrapped
        // lines don't pick up the code block's left padding. Wrap mode lays the
        // text out as a normal block inside the padded content box, so every
        // line (wrapped or not) is padded consistently.
        style.code_block_overflow_x_scroll = false;

        let header_bg = self.tool_card_header_bg(cx);
        let run_command_label = if is_preview {
            Some(
                h_flex().h_6().child(
                    Label::new("Run Command")
                        .buffer_font(cx)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        } else {
            None
        };
        // Suppress the code block's built-in copy button so we don't stack two
        // copy buttons on top of each other; the outer button below is the one
        // we want, because it copies the unfenced command text.
        let markdown_element = self
            .render_markdown(command, style, cx)
            .code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::Hidden,
                wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                border: false,
            });
        let copy_button_id = SharedString::from(format!("{group}-copy-command"));
        let copy_button = CopyButton::new(copy_button_id, command_text)
            .tooltip_label("Copy Command")
            .visible_on_hover(group.clone());

        v_flex()
            .group(group)
            .relative()
            .p_1p5()
            .bg(header_bg)
            .when(is_preview, |this| this.pt_1().children(run_command_label))
            .child(markdown_element)
            .child(div().absolute().top_1().right_1().child(copy_button))
    }

    /// The file-type icon (the project panel's, via [`FileIcons`]) for a tool
    /// call that acts on exactly one file. `None` for terminals and for tool
    /// calls that touch several files or none, which keep their kind icon.
    fn tool_call_file_icon(tool_call: &ToolCall, cx: &App) -> Option<SharedString> {
        file_icon_for_locations(
            &tool_call.locations,
            tool_call.terminals().next().is_some(),
            cx,
        )
    }

    fn tool_kind_icon(kind: acp::ToolKind) -> IconName {
        match kind {
            acp::ToolKind::Read => IconName::ToolSearch,
            acp::ToolKind::Edit => IconName::ToolPencil,
            acp::ToolKind::Delete => IconName::ToolDeleteFile,
            acp::ToolKind::Move => IconName::ArrowRightLeft,
            acp::ToolKind::Search => IconName::ToolSearch,
            acp::ToolKind::Execute => IconName::ToolTerminal,
            acp::ToolKind::Think => IconName::ToolThink,
            acp::ToolKind::Fetch => IconName::ToolWeb,
            acp::ToolKind::SwitchMode => IconName::ArrowRightLeft,
            acp::ToolKind::Other | _ => IconName::ToolHammer,
        }
    }

    /// Whether an entry renders as compact action chips. Every tool call does,
    /// except permission prompts and subagent calls, which keep their full
    /// rendering (and so break a chip run). So does an assistant message that is
    /// nothing but thoughts: thinking is an agent action, and its chips belong
    /// in the same grid as the tool calls around it. An assistant message that
    /// says something (any non-blank prose chunk) is not a chip and ends the run.
    /// Whether an entry belongs to a run of chips. A thoughts-only message
    /// draws no chip of its own (thinking is shown beside the progress
    /// indicator), but it counts here so that it does not split the chips
    /// around it into separate groups.
    fn is_chip_entry(entry: &AgentThreadEntry, cx: &App) -> bool {
        match entry {
            AgentThreadEntry::ToolCall(tool_call) => {
                !matches!(
                    tool_call.status,
                    ToolCallStatus::WaitingForConfirmation { .. }
                ) && !tool_call.is_subagent()
                    // Compaction renders as the transcript-wide barrier, never
                    // as an action chip.
                    && !tool_call.is_compaction(cx)
            }
            AgentThreadEntry::AssistantMessage(_) => Self::is_thoughts_only_message(entry, cx),
            _ => false,
        }
    }

    /// A message that only thinks. Thinking is shown live beside the progress
    /// indicator and nowhere else, so the transcript skips these entirely.
    fn is_thoughts_only_message(entry: &AgentThreadEntry, cx: &App) -> bool {
        match entry {
            AgentThreadEntry::AssistantMessage(message) => {
                !message.indented
                    && !message.is_subagent_output
                    && !Self::has_prose(message, cx)
                    && Self::thought_chunks(message, cx).next().is_some()
            }
            _ => false,
        }
    }

    /// The distinct edited-file locations of an edit tool call, as indices into
    /// `locations`. Empty for non-edit calls; a call with fewer than two
    /// distinct files stays a single chip (there is no per-file breakdown to
    /// split).
    fn edited_files(tool_call: &ToolCall, cx: &App) -> Vec<EditedFile> {
        let is_edit =
            matches!(tool_call.kind, acp::ToolKind::Edit) || tool_call.diffs().next().is_some();
        if !is_edit {
            return Vec::new();
        }

        let mut files: Vec<EditedFile> = Vec::new();
        for (ix, location) in tool_call.locations.iter().enumerate() {
            if files.iter().any(|file| file.path == location.path) {
                continue;
            }
            files.push(EditedFile {
                path: location.path.to_path_buf(),
                location_ix: Some(ix),
            });
        }
        if !files.is_empty() {
            return files;
        }

        // An agent that sends a patch (Codex) reports the diffs but no
        // locations at all, which leaves the call its generic "editing files"
        // label. Its diffs still name the files, so they are the fallback: the
        // call is one chip per file either way.
        for diff in tool_call.diffs() {
            let Some(path) = diff.read(cx).file_path(cx) else {
                continue;
            };
            let path = std::path::PathBuf::from(path);
            if files.iter().any(|file| file.path == path) {
                continue;
            }
            files.push(EditedFile {
                path,
                location_ix: None,
            });
        }
        files
    }

    /// The non-blank thought chunks of an assistant message, as
    /// `(chunk_ix, markdown)`. Each is one chip: the agent's distinct thoughts
    /// arrive as distinct chunks (streaming deltas of one thought merge into a
    /// single chunk in [`AcpThread`]), so one chip per chunk is one chip per
    /// thought.
    fn thought_chunks<'a>(
        message: &'a AssistantMessage,
        cx: &'a App,
    ) -> impl Iterator<Item = (usize, Entity<Markdown>)> + 'a {
        message
            .chunks
            .iter()
            .enumerate()
            .filter_map(move |(chunk_ix, chunk)| match chunk {
                AssistantMessageChunk::Thought { block, .. } => {
                    let markdown = block.markdown()?;
                    (!markdown.read(cx).source().trim().is_empty())
                        .then(|| (chunk_ix, markdown.clone()))
                }
                AssistantMessageChunk::Message { .. } => None,
            })
    }

    fn has_prose(message: &AssistantMessage, cx: &App) -> bool {
        message.chunks.iter().any(|chunk| match chunk {
            AssistantMessageChunk::Message { block, .. } => block
                .markdown()
                .is_some_and(|markdown| !markdown.read(cx).source().trim().is_empty()),
            AssistantMessageChunk::Thought { .. } => false,
        })
    }

    /// The entry, if any, that is currently in progress and therefore belongs to
    /// the working indicator (the active area), not the transcript: a running
    /// terminal, or a still-streaming tail message that is nothing but thoughts,
    /// at the end of a `Generating` turn. Always the last entry, since that is
    /// the only one a turn is still producing.
    /// The thought the agent is having right now, which the active area shows
    /// beside the progress indicator.
    fn active_area_entry(
        entries: &[AgentThreadEntry],
        generating: bool,
        cx: &App,
    ) -> Option<usize> {
        if !generating {
            return None;
        }
        let ix = entries.len().checked_sub(1)?;
        Self::is_thoughts_only_message(&entries[ix], cx).then_some(ix)
    }

    /// The number of leading entries the transcript draws: every entry except
    /// the in-progress tail the active area shows.
    fn visible_entry_count(&self, cx: &App) -> usize {
        let thread = self.thread.read(cx);
        let entries = thread.entries();
        let generating = thread.status() == ThreadStatus::Generating;
        match Self::active_area_entry(entries, generating, cx) {
            Some(active_ix) => active_ix,
            None => entries.len(),
        }
    }

    /// The maximal run of consecutive chip-able entries containing `entry_ix`,
    /// as `(run_start, run_len)`. The run ends at the next assistant text
    /// message (or any other non-chip entry); a lone tool call is a run of
    /// length one. The in-progress tail is trimmed off first, so it is never
    /// grouped into a run.
    fn action_run_bounds(&self, entry_ix: usize, cx: &App) -> Option<(usize, usize)> {
        let entries = self.thread.read(cx).entries();
        let visible = self.visible_entry_count(cx);
        if entry_ix >= visible {
            return None;
        }
        let entries = &entries[..visible];
        // Every entry the list draws asks this, and answering means walking to
        // both ends of its run, so a long run tests the same entries once per
        // entry it contains. Within a frame the answers cannot change.
        let is_chip = |ix: usize| -> bool {
            let mut flags = self.chip_cache.frame_chip_entries.borrow_mut();
            if flags.len() < entries.len() {
                flags.resize(entries.len(), None);
            }
            match flags[ix] {
                Some(known) => known,
                None => {
                    let known = Self::is_chip_entry(&entries[ix], cx);
                    flags[ix] = Some(known);
                    known
                }
            }
        };

        if !is_chip(entry_ix) {
            return None;
        }
        let mut start = entry_ix;
        while start > 0 && is_chip(start - 1) {
            start -= 1;
        }
        let mut end = entry_ix;
        while end + 1 < entries.len() && is_chip(end + 1) {
            end += 1;
        }
        Some(Self::chunk_of_run(start, end, entry_ix))
    }

    /// Run bounds without a view to memoize through, for tests.
    #[cfg(test)]
    fn action_run_bounds_in(
        entries: &[AgentThreadEntry],
        entry_ix: usize,
        cx: &App,
    ) -> Option<(usize, usize)> {
        if !Self::is_chip_entry(entries.get(entry_ix)?, cx) {
            return None;
        }
        let mut start = entry_ix;
        while start > 0 && Self::is_chip_entry(&entries[start - 1], cx) {
            start -= 1;
        }
        let mut end = entry_ix;
        while end + 1 < entries.len() && Self::is_chip_entry(&entries[end + 1], cx) {
            end += 1;
        }
        Some(Self::chunk_of_run(start, end, entry_ix))
    }

    /// The part of a run that `entry_ix` is drawn in. A run's chips are drawn
    /// by its first entry as one block, which the list can only skip or draw
    /// whole; past a certain length that block is several screens tall and
    /// every frame pays for all of it. Splitting a long run into blocks lets
    /// the list leave the ones that are nowhere near the viewport alone. The
    /// answer is the same for every entry of a block, so each block is drawn
    /// once, by its own first entry.
    fn chunk_of_run(start: usize, end: usize, entry_ix: usize) -> (usize, usize) {
        const MAX_RUN: usize = 48;

        let chunk_start = start + (entry_ix - start) / MAX_RUN * MAX_RUN;
        (chunk_start, MAX_RUN.min(end + 1 - chunk_start))
    }

    /// The chips a run of entries renders as. One chip per tool call and one per
    /// thought, except that adjacent wait calls (agents that poll can emit long
    /// stretches of them) collapse into a single counted chip.
    fn action_chips(&self, run_start: usize, run_len: usize, cx: &App) -> Vec<ActionChip> {
        let entries = self.thread.read(cx).entries();
        let visible = self.visible_entry_count(cx);
        Self::action_chips_in_cached(
            &entries[..visible],
            run_start,
            run_len,
            Some(&self.chip_cache),
            cx,
        )
    }

    /// Opens this edited file's diff as its own tab: an editor over the
    /// call's single-file diff multibuffer, hunks expanded.
    /// Opens (or, when already open, closes) a diff tab for one edited file.
    /// The tab shows the file's real project buffer with the agent's changes
    /// as expanded diff hunks: the tab carries the file's name, line numbers
    /// are the file's own, and the buffer is editable in place. The tool
    /// call's detached ACP diff is only a fallback for buffers the action log
    /// no longer tracks.
    /// Opens a diff view for one edited file. The view is always the REAL
    /// project buffer (real path, real line numbers, LSP, editable) with the
    /// agent's pre-edit content as the diff base; the agent's own detached
    /// buffer is never shown, since it has no language server and its line
    /// numbers lead into a scratch file.
    ///
    /// Each chip owns its own view: two edits of one file open two diffs, each
    /// against the base that edit started from.
    /// Opens the edited file's REAL project editor (real path, real tab, LSP,
    /// editable) and shows the agent's change in its gutter by attaching a
    /// diff whose base is the agent's pre-edit content. This is the exact
    /// decoration Zed's own single-file review applies, done unconditionally
    /// for this one editor so it does not depend on the `single_file_review`
    /// setting. `workspace.open_path` is idempotent, so clicking again just
    /// re-activates the tab rather than stacking duplicates.
    /// Opens the real project file for one edited file and shows the agent's
    /// change in it: a normal editor (real tab title, real path, language
    /// server, editable), with the agent's pre-edit content attached as the
    /// diff base so the gutter shows exactly what changed. Never a detached
    /// scratch buffer.
    ///
    /// Opening the same file again just re-activates its tab and re-reveals the
    /// change, rather than stacking duplicates.
    /// Opens the place a tool complained about: the file it named, at the line
    /// it named. Paths come from compiler output, so they may be relative to
    /// the worktree or absolute.
    fn open_output_location(
        &mut self,
        location: &acp_thread::OutputLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let path = std::path::Path::new(&location.path);
        let project_path = project.read(cx).find_project_path(path, cx);
        let row = location.line.saturating_sub(1);
        let column = location.column.unwrap_or(1).saturating_sub(1);

        let open_task = self.workspace.update(cx, |workspace, cx| {
            if let Some(project_path) = project_path {
                workspace.open_path(project_path, None, true, window, cx)
            } else {
                workspace.open_abs_path(
                    path.to_path_buf(),
                    OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            }
        });
        let Ok(open_task) = open_task else {
            return;
        };
        window
            .spawn(cx, async move |cx| {
                let item = open_task.await?;
                let Some(editor) = item.downcast::<Editor>() else {
                    return anyhow::Ok(());
                };
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections
                            .select_ranges([Point::new(row, column)..Point::new(row, column)]);
                    });
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    fn open_edit_file_diff(
        &mut self,
        entry_ix: usize,
        file_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let entries = self.thread.read(cx).entries();
        let AgentThreadEntry::ToolCall(tool_call) = entries.get(entry_ix)? else {
            return None;
        };
        let file = Self::edited_files(tool_call, cx).get(file_ix).cloned()?;

        let project = self.project.upgrade()?;
        let base_text: Arc<str> = self
            .diff_for_edited_file(tool_call, &file, cx)
            .map(|diff| diff.read(cx).base_text().clone())?;

        // One call's edits to one file, as their own diff view: a multibuffer
        // of the real buffer against the text as the call found it.
        crate::tool_call_diff::open_tool_call_diff(
            crate::tool_call_diff::ToolCallDiffKey {
                tool_call_id: tool_call.id.clone(),
                path: file.path,
            },
            base_text,
            project,
            self.workspace.clone(),
            window,
            cx,
        )
        .detach();
        Some(())
    }

    /// The `+added -removed` readout on an edit chip: a chip of its own, so that
    /// it reads as the clickable thing it is (it opens the call's diff).
    fn render_diff_stat_chip(
        &self,
        element_id: impl Into<ElementId>,
        stats: action_log::DiffStats,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The chip's tint says which way the change went: green for growing,
        // red for shrinking, and grey when it neither grew nor shrank, which
        // is a rewrite rather than a lean either way. The counts keep their own
        // colours regardless.
        let tint = match stats.lines_added.cmp(&stats.lines_removed) {
            std::cmp::Ordering::Greater => cx.theme().status().success,
            std::cmp::Ordering::Less => cx.theme().status().error,
            std::cmp::Ordering::Equal => cx.theme().colors().text_muted,
        };
        h_flex()
            .id(element_id)
            .flex_none()
            .gap_0p5()
            .px_0p5()
            .rounded_sm()
            .border_1()
            .border_color(tint.opacity(0.35))
            .bg(tint.opacity(0.12))
            .cursor_pointer()
            .hover(|style| style.bg(tint.opacity(0.25)))
            .tooltip(Tooltip::text("Show Diff"))
            .on_click(cx.listener(move |this, _, window, cx| {
                // The parent chip has its own click; without this the click
                // would trigger both.
                cx.stop_propagation();
                on_click(this, window, cx);
            }))
            .when(stats.lines_added > 0, |this| {
                this.child(
                    Label::new(format!("+{}", stats.lines_added))
                        .size(LabelSize::XSmall)
                        .color(Color::Created)
                        .buffer_font(cx),
                )
            })
            .when(stats.lines_removed > 0, |this| {
                this.child(
                    Label::new(format!("-{}", stats.lines_removed))
                        .size(LabelSize::XSmall)
                        .color(Color::Deleted)
                        .buffer_font(cx),
                )
            })
            .into_any_element()
    }

    /// A thought's chip: the think icon plus a short summary of the thought.
    /// Thoughts do not expand; the full thought shows on hover, so a run of
    /// thinking reads as quiet chips rather than a wall of collapsible text.
    /// The live thought: quiet italic text with a slow shimmer, styled
    /// nothing like the action chips so it reads as the agent's voice rather
    /// than one of its actions. Hover shows the full thought.
    fn render_thought_chip(
        &self,
        key: (usize, usize),
        thought: &Entity<Markdown>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let summary = Self::thought_summary(&thought.read(cx).source());
        let markdown = thought.clone();
        let workspace = self.workspace.clone();
        let code_span_resolver = self.code_span_resolver.clone();

        h_flex()
            .id(SharedString::from(format!(
                "live-thought-{}-{}",
                key.0, key.1
            )))
            .min_w_0()
            .gap_1p5()
            .px_0p5()
            .child(
                Icon::new(IconName::ToolThink)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_w_0()
                    .italic()
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .with_animation(
                        SharedString::from(format!("live-thought-shimmer-{}-{}", key.0, key.1)),
                        Animation::new(Duration::from_secs(2))
                            .repeat()
                            .with_easing(pulsating_between(0.45, 1.0)),
                        |this, delta| this.opacity(delta),
                    ),
            )
            // The full thought is the hover card: there is no click-to-expand.
            // A long one scrolls in the card rather than being clipped.
            .hoverable_tooltip(chip_hover_card(move |window, cx| {
                let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                card_scroll_region("thought-hover-scroll", rems(24.), rems(24.))
                    .text_xs()
                    .child(render_agent_markdown(
                        markdown.clone(),
                        style,
                        &workspace,
                        &code_span_resolver,
                        cx,
                    ))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_terminal_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        terminal: &Entity<acp_thread::Terminal>,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let terminal_data = terminal.read(cx);
        let started_at = terminal_data.started_at();

        let tool_failed = matches!(
            &tool_call.status,
            ToolCallStatus::Rejected | ToolCallStatus::Canceled | ToolCallStatus::Failed
        );

        let confirmation_options = match &tool_call.status {
            ToolCallStatus::WaitingForConfirmation { options, .. } => Some(options),
            _ => None,
        };
        let needs_confirmation = confirmation_options.is_some();

        let output = terminal_data.output();
        let command_finished = output.is_some()
            && !matches!(
                tool_call.status,
                ToolCallStatus::InProgress | ToolCallStatus::Pending
            );
        let truncated_output =
            output.is_some_and(|output| output.original_content_len > output.content.len());
        let output_line_count = output.map(|output| output.content_line_count).unwrap_or(0);

        let command_failed = command_finished
            && output.is_some_and(|o| o.exit_status.is_some_and(|status| !status.success()));

        let time_elapsed = if let Some(output) = output {
            output.ended_at.duration_since(started_at)
        } else {
            started_at.elapsed()
        };

        let header_id =
            SharedString::from(format!("terminal-tool-header-{}", terminal.entity_id()));
        let header_group = SharedString::from(format!(
            "terminal-tool-header-group-{}",
            terminal.entity_id()
        ));
        let border_color = self.tool_card_border_color(cx);

        // One-line summary of the command, styled like the subtle Read/Search
        // rows. The label's markdown source is a fenced code block; strip the
        // fences and show the same interesting-part summary the chip does. The
        // full command surfaces in a tooltip whenever the summary hides part of
        // it (a long command, a chain, or a multi-line script).
        let command_text = strip_command_fences(&tool_call.label.read(cx).source()).to_string();
        let display_command: SharedString =
            acp_thread::command_display_prefix(&command_text, 100).into();
        let show_full_command = display_command.as_ref() != command_text.trim();

        // Bash-highlight the one-line command, reusing the language the label
        // markdown (a bash-tagged fenced code block) already resolved.
        let command_language = tool_call.label.read(cx).first_code_block_language();
        let command_element = {
            let markdown_style =
                MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_buffer_font(cx);
            let mut command_text_style = markdown_style.base_text_style.clone();
            command_text_style.font_size = rems_from_px(12_f32).into();
            command_text_style.color = cx.theme().colors().text_muted;
            let command_runs = highlight_code_runs(
                &display_command,
                command_language.as_ref(),
                command_text_style,
                &markdown_style,
            );
            StyledText::new(display_command).with_runs(command_runs)
        };

        let user_expanded = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);
        let user_collapsed = self
            .entry_view_state
            .read(cx)
            .is_tool_call_user_collapsed(&tool_call.id);
        // Failed commands open their output so problems are visible without a
        // click; an explicit collapse by the user wins.
        let auto_expanded = (tool_failed || command_failed) && !user_collapsed;
        let inert_header = layout == ToolCallLayout::ChipBody;
        let is_expanded = inert_header || needs_confirmation || user_expanded || auto_expanded;
        let now_expanded = user_expanded || auto_expanded;

        let header_element: Option<AnyElement> = (!inert_header).then(|| {
            let header = h_flex()
                .id(header_id)
                .group(&header_group)
                .w_full()
                .flex_none()
                .gap_1p5()
                // Matches the dimming of the other quiet one-line tool rows.
                .opacity(0.85)
                .px_1()
                .rounded(rems_from_px(3_f32))
                // Same hover affordance as the edit/read one-line rows; inside an
                // expanded chip the header is plain text.
                .when(!inert_header, |this| {
                    this.hover(|s| s.bg(cx.theme().colors().element_hover.opacity(0.5)))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let id = tool_call.id.clone();
                            move |this, _event, window, cx| {
                                this.entry_view_state.update(cx, |state, _cx| {
                                    state.set_tool_call_expanded(&id, !now_expanded);
                                });
                                this.refresh_thread_search(window, cx);
                                cx.notify();
                            }
                        }))
                })
                .child(
                    Icon::new(IconName::ToolTerminal)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .id(("terminal-tool-command", terminal.entity_id()))
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .line_clamp(1)
                        .text_ellipsis()
                        .text_xs()
                        .when(show_full_command, |this| {
                            this.tooltip(Tooltip::text(command_text.clone()))
                        })
                        .child(command_element),
                )
                .when(time_elapsed > Duration::from_secs(10), |header| {
                    header.child(
                        Label::new(format!("({})", duration_alt_display(time_elapsed)))
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    )
                })
                .when(!command_finished && !needs_confirmation, |header| {
                    header.child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                })
                .when(truncated_output, |header| {
                    let tooltip = if let Some(output) = output {
                        if output_line_count + 10 > terminal::MAX_SCROLL_HISTORY_LINES {
                            format!(
                                "Output exceeded terminal max lines and was \
                            truncated, the model received the first {}.",
                                format_file_size(output.content.len() as u64, true)
                            )
                        } else {
                            format!(
                                "Output is {} long, and to avoid unexpected token usage, \
                                only {} was sent back to the agent.",
                                format_file_size(output.original_content_len as u64, true),
                                format_file_size(output.content.len() as u64, true)
                            )
                        }
                    } else {
                        "Output was truncated".to_string()
                    };

                    header.child(
                        h_flex()
                            .id(("terminal-tool-truncated-label", terminal.entity_id()))
                            .gap_1()
                            .child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::XSmall)
                                    .color(Color::Ignored),
                            )
                            .child(
                                Label::new("Truncated")
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall),
                            )
                            .tooltip(Tooltip::text(tooltip)),
                    )
                })
                .when(tool_failed || command_failed, |header| {
                    header.child(
                        div()
                            .id(("terminal-tool-error-code-indicator", terminal.entity_id()))
                            .child(
                                Icon::new(IconName::Close)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .when_some(output.and_then(|o| o.exit_status), |this, status| {
                                this.tooltip(Tooltip::text(format!(
                                    "Exited with code {}",
                                    status.code().unwrap_or(-1),
                                )))
                            }),
                    )
                })
                .when(!inert_header, |this| {
                    this.child(
                        Disclosure::new(
                            SharedString::from(format!(
                                "terminal-tool-disclosure-{}",
                                terminal.entity_id()
                            )),
                            is_expanded,
                        )
                        .opened_icon(IconName::ChevronUp)
                        .closed_icon(IconName::ChevronDown)
                        .visible_on_hover(&header_group)
                        .on_click(cx.listener({
                            let id = tool_call.id.clone();
                            move |this, _event, window, cx| {
                                this.entry_view_state.update(cx, |state, _cx| {
                                    state.set_tool_call_expanded(&id, !now_expanded);
                                });
                                this.refresh_thread_search(window, cx);
                                cx.notify();
                            }
                        })),
                    )
                });
            header.into_any_element()
        });

        let terminal_view = self
            .entry_view_state
            .read(cx)
            .entry(entry_ix)
            .and_then(|entry| entry.terminal(terminal));

        v_flex()
            // Slightly deeper indent than prose and tighter vertical rhythm:
            // tool rows read as quiet annotations between messages. The
            // header's px_1 hover padding brings the icon back to the same
            // column as the other one-line rows.
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.my_0p5().ml_5().mr_5()
            })
            .children(header_element)
            .when(is_expanded, |this| {
                // The one-line row above already shows the command; the
                // expansion adds only the terminal output (no repeated
                // command block, no working-dir label).
                this.child(
                    v_flex()
                        .mt_1()
                        .ml(rems(0.4))
                        .pl_3p5()
                        .gap_1()
                        .border_l_1()
                        .when(tool_failed || command_failed, |this| this.border_dashed())
                        .border_color(border_color)
                        // The script the command carried, before its output:
                        // what ran reads before what it printed.
                        .children(self.render_command_scripts(tool_call, window, cx))
                        .when_some(tool_call.sandbox_not_applied.as_ref(), |this, reason| {
                            // Upstream renders this warning inside its terminal
                            // card header, which our one-line row replaces; the
                            // warning data is shared, the rendering is ours.
                            let TerminalSandboxWarning {
                                title,
                                detail,
                                docs_url,
                            } = self.sandbox_not_applied_warning(reason, cx);
                            this.child(
                                h_flex()
                                    .id(("sandbox-not-applied", entry_ix))
                                    .gap_1()
                                    .cursor_pointer()
                                    .child(
                                        Icon::new(IconName::LockOff)
                                            .size(IconSize::XSmall)
                                            .color(Color::Warning),
                                    )
                                    .child(
                                        Label::new(title.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .tooltip(move |_window, cx| {
                                        Tooltip::with_meta(
                                            title.clone(),
                                            None,
                                            format!(
                                                "{detail} Click to learn more about sandboxing."
                                            ),
                                            cx,
                                        )
                                    })
                                    .on_click(move |_, _, cx| cx.open_url(&docs_url)),
                            )
                        })
                        .children(terminal_view.map(|terminal_view| {
                            let element = if terminal_view
                                .read(cx)
                                .content_mode(window, cx)
                                .is_scrollable()
                            {
                                div().h_72().child(terminal_view).into_any_element()
                            } else {
                                // Static captured output: cap it and let long
                                // output scroll instead of growing the row
                                // without bound.
                                div()
                                    .id(("terminal-output-scroll", terminal.entity_id()))
                                    .max_h_96()
                                    .overflow_y_scroll()
                                    // Scrolling the output is about the output;
                                    // without this the thread list scrolls too.
                                    .occlude()
                                    .child(terminal_view)
                                    .into_any_element()
                            };

                            div()
                                .rounded_md()
                                .overflow_hidden()
                                .bg(cx.theme().colors().editor_background)
                                .text_ui_sm(cx)
                                .on_action(cx.listener(|_this, _: &NewTerminal, window, cx| {
                                    window.dispatch_action(NewThread.boxed_clone(), cx);
                                    cx.stop_propagation();
                                }))
                                .child(element)
                                .into_any_element()
                        })),
                )
            })
            .when_some(confirmation_options, |this, options| {
                let is_first = self.is_first_tool_call(active_session_id, &tool_call.id, cx);
                let allow_disabled = self.sandbox_confusables_block_allow(tool_call, cx);
                this.child(self.render_permission_buttons(
                    self.thread.read(cx).session_id().clone(),
                    is_first,
                    options,
                    entry_ix,
                    tool_call.id.clone(),
                    focus_handle,
                    allow_disabled,
                    cx,
                ))
            })
            .into_any()
    }

    /// Render the "ran without sandbox" warning shown on a terminal tool card,
    /// tailored to *why* the sandbox wasn't applied.
    fn sandbox_not_applied_warning(
        &self,
        reason: &SandboxNotAppliedReason,
        cx: &Context<Self>,
    ) -> TerminalSandboxWarning {
        // (title, detail line, docs section slug)
        let (title, detail, docs_section): (SharedString, SharedString, Option<&'static str>) =
            match reason {
                SandboxNotAppliedReason::ErrorLinuxWsl(error) => (
                    "Couldn't create a sandbox".into(),
                    error.user_facing_message().into(),
                    Some(error.docs_section()),
                ),
                SandboxNotAppliedReason::DisabledForThisThread => {
                    // The grant only exists because an earlier command failed to
                    // create a sandbox; surface that same explanation here.
                    let thread_error = self.find_thread_sandbox_error(cx);
                    let detail = thread_error
                        .as_ref()
                        .map(|error| {
                            SharedString::from(format!(
                                "Allowed for this conversation after the sandbox failed: {}",
                                error.user_facing_message()
                            ))
                        })
                        .unwrap_or_else(|| {
                            "Unsandboxed execution is allowed for the rest of this conversation."
                                .into()
                        });
                    let docs_section = thread_error.as_ref().map(|error| error.docs_section());
                    ("Ran without sandbox".into(), detail, docs_section)
                }
            };

        TerminalSandboxWarning {
            title,
            detail,
            docs_url: zed_urls::sandboxing_docs(docs_section, cx).into(),
        }
    }

    /// Find the first terminal tool call in the thread whose sandbox couldn't be
    /// created, so a later "disabled for this thread" warning can reuse the same
    /// explanation of *why* the sandbox failed.
    fn find_thread_sandbox_error(&self, cx: &App) -> Option<acp_thread::LinuxWslSandboxError> {
        self.thread.read(cx).entries().iter().find_map(|entry| {
            if let AgentThreadEntry::ToolCall(tool_call) = entry
                && let Some(SandboxNotAppliedReason::ErrorLinuxWsl(error)) =
                    &tool_call.sandbox_not_applied
            {
                return Some(error.clone());
            }
            None
        })
    }

    fn is_first_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        tool_call_id: &acp::ToolCallId,
        cx: &App,
    ) -> bool {
        self.conversation
            .read(cx)
            .pending_tool_call(active_session_id, cx)
            .map_or(false, |(pending_session_id, pending_tool_call_id, _)| {
                self.thread.read(cx).session_id() == &pending_session_id
                    && tool_call_id == &pending_tool_call_id
            })
    }

    fn render_any_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let has_terminals = tool_call.terminals().next().is_some();

        // Give every tool-call subtree a unique element-id prefix derived from
        // the globally-unique tool call id and the layout. This single wrapper
        // is what keeps all the `entry_ix`-keyed element ids inside the card
        // collision-free, even when the same tool call is rendered in multiple
        // places at once (inline list + floating awaiting-permission row) or
        // when subagent entries are inlined into the parent view's element tree.
        let container_id = ElementId::Name(SharedString::from(format!(
            "tool-call-{}-{}",
            tool_call.id.0,
            layout.id_str()
        )));

        div().w_full().id(container_id).map(|this| {
            if tool_call.is_subagent() {
                this.child(
                    self.render_subagent_tool_call(
                        active_session_id,
                        entry_ix,
                        tool_call,
                        tool_call
                            .subagent_session_info
                            .as_ref()
                            .map(|i| i.session_id.clone()),
                        focus_handle,
                        window,
                        cx,
                    ),
                )
            } else if has_terminals {
                this.children(tool_call.terminals().map(|terminal| {
                    self.render_terminal_tool_call(
                        active_session_id,
                        entry_ix,
                        terminal,
                        tool_call,
                        focus_handle,
                        layout,
                        window,
                        cx,
                    )
                }))
            } else {
                this.child(self.render_tool_call(
                    active_session_id,
                    entry_ix,
                    tool_call,
                    focus_handle,
                    layout,
                    window,
                    cx,
                ))
            }
        })
    }

    fn render_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let has_location = tool_call.locations.len() == 1;
        let card_header_id = SharedString::from(format!("inner-tool-call-header-{entry_ix}"));

        let failed_or_canceled = match &tool_call.status {
            ToolCallStatus::Rejected | ToolCallStatus::Canceled | ToolCallStatus::Failed => true,
            _ => false,
        };

        let needs_confirmation = matches!(
            tool_call.status,
            ToolCallStatus::WaitingForConfirmation { .. }
        );
        let is_terminal_tool = matches!(tool_call.kind, acp::ToolKind::Execute);

        let is_edit =
            matches!(tool_call.kind, acp::ToolKind::Edit) || tool_call.diffs().next().is_some();

        let is_cancelled_edit = is_edit && matches!(tool_call.status, ToolCallStatus::Canceled);
        let (has_revealed_diff, tool_call_output_focus, tool_call_output_focus_handle) = tool_call
            .diffs()
            .next()
            .and_then(|diff| {
                let editor = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.editor_for_diff(diff))?;
                let has_revealed_diff = diff.read(cx).has_revealed_range(cx);
                let has_focus = editor.read(cx).is_focused(window);
                let focus_handle = editor.focus_handle(cx);
                Some((has_revealed_diff, has_focus, focus_handle))
            })
            .unwrap_or_else(|| (false, false, focus_handle.clone()));

        // Edits render as a subtle one-line label (like Read/Search) instead of
        // a card; the diff is available behind the disclosure chevron.
        let use_card_layout = needs_confirmation || is_terminal_tool;

        let has_image_content = tool_call.content.iter().any(|c| c.image().is_some());

        let should_show_raw_input = !is_terminal_tool && !is_edit && !has_image_content;

        let has_content = !tool_call.content.is_empty()
            || (should_show_raw_input && tool_call.raw_input.is_some());

        let is_collapsible = has_content && !needs_confirmation;
        // One-line rows expand/collapse on click (like terminal rows).
        // Rows whose click opens a file keep that behavior, except edits,
        // whose go-to-file moves to a hover icon button so click can
        // toggle the diff.
        let click_toggles_expand = is_collapsible && (is_edit || !has_location);
        let show_goto_file_button = is_edit && has_location && is_collapsible;
        let mut is_open = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);

        is_open |= needs_confirmation;

        let input_output_header = |label: SharedString| {
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
        };

        let tool_output_display = if is_open {
            match &tool_call.status {
                ToolCallStatus::WaitingForConfirmation { .. } => {
                    let confirmation_content = v_flex()
                        .w_full()
                        .children(tool_call.content.iter().enumerate().map(
                            |(content_ix, content)| {
                                div()
                                    .child(self.render_tool_call_content(
                                        active_session_id,
                                        entry_ix,
                                        content,
                                        content_ix,
                                        tool_call,
                                        use_card_layout,
                                        failed_or_canceled,
                                        focus_handle,
                                        window,
                                        cx,
                                    ))
                                    .into_any_element()
                            },
                        ))
                        .when_some(
                            tool_call.sandbox_authorization_details.as_ref(),
                            |this, details| {
                                this.child(self.render_sandbox_authorization_details(
                                    entry_ix,
                                    &tool_call.id,
                                    details,
                                    window,
                                    cx,
                                ))
                            },
                        )
                        .when_some(
                            tool_call.sandbox_fallback_authorization_details.as_ref(),
                            |this, details| {
                                this.child(
                                    self.render_sandbox_fallback_authorization_details(details, cx),
                                )
                            },
                        )
                        .when(should_show_raw_input, |this| {
                            let is_raw_input_expanded =
                                self.expanded_tool_call_raw_inputs.contains(&tool_call.id);

                            let input_header = if is_raw_input_expanded {
                                "Raw Input:"
                            } else {
                                "View Raw Input"
                            };

                            this.child(
                                v_flex()
                                    .p_2()
                                    .gap_1()
                                    .border_t_1()
                                    .border_color(self.tool_card_border_color(cx))
                                    .child(
                                        h_flex()
                                            .id("disclosure_container")
                                            .pl_0p5()
                                            .gap_1()
                                            .justify_between()
                                            .rounded_xs()
                                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                                            .child(input_output_header(input_header.into()))
                                            .child(
                                                Disclosure::new(
                                                    ("raw-input-disclosure", entry_ix),
                                                    is_raw_input_expanded,
                                                )
                                                .opened_icon(IconName::ChevronUp)
                                                .closed_icon(IconName::ChevronDown),
                                            )
                                            .on_click(cx.listener({
                                                let id = tool_call.id.clone();

                                                move |this: &mut Self, _, _, cx| {
                                                    if this
                                                        .expanded_tool_call_raw_inputs
                                                        .contains(&id)
                                                    {
                                                        this.expanded_tool_call_raw_inputs
                                                            .remove(&id);
                                                    } else {
                                                        this.expanded_tool_call_raw_inputs
                                                            .insert(id.clone());
                                                    }
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    .when(is_raw_input_expanded, |this| {
                                        this.children(tool_call.raw_input_markdown.clone().map(
                                            |input| {
                                                self.render_markdown(
                                                    input,
                                                    MarkdownStyle::themed(
                                                        MarkdownFont::Agent,
                                                        window,
                                                        cx,
                                                    ),
                                                    cx,
                                                )
                                            },
                                        ))
                                    }),
                            )
                        });

                    confirmation_content.into_any()
                }
                ToolCallStatus::Pending | ToolCallStatus::InProgress
                    if is_edit
                        && tool_call.content.is_empty()
                        && self.as_native_connection(cx).is_some() =>
                {
                    self.render_diff_loading(cx)
                }
                ToolCallStatus::Pending
                | ToolCallStatus::InProgress
                | ToolCallStatus::Completed
                | ToolCallStatus::Failed
                | ToolCallStatus::Canceled => v_flex()
                    .when(should_show_raw_input, |this| {
                        this.mt_1p5().w_full().child(
                            v_flex()
                                .ml(rems(0.4))
                                .px_3p5()
                                .pb_1()
                                .gap_1()
                                .border_l_1()
                                .border_color(self.tool_card_border_color(cx))
                                .child(input_output_header("Raw Input:".into()))
                                .children(tool_call.raw_input_markdown.clone().map(|input| {
                                    div().id(("tool-call-raw-input-markdown", entry_ix)).child(
                                        self.render_markdown(
                                            input,
                                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                            cx,
                                        ),
                                    )
                                }))
                                .child(input_output_header("Output:".into())),
                        )
                    })
                    .children(
                        tool_call
                            .content
                            .iter()
                            .enumerate()
                            .map(|(content_ix, content)| {
                                let output_id = SharedString::from(format!(
                                    "tool-call-output-{entry_ix}-{content_ix}"
                                ));
                                div()
                                    .id(output_id.clone())
                                    .debug_selector(move || output_id.to_string())
                                    .child(self.render_tool_call_content(
                                        active_session_id,
                                        entry_ix,
                                        content,
                                        content_ix,
                                        tool_call,
                                        use_card_layout,
                                        failed_or_canceled,
                                        focus_handle,
                                        window,
                                        cx,
                                    ))
                            }),
                    )
                    .into_any(),
                ToolCallStatus::Rejected => Empty.into_any(),
            }
            .into()
        } else {
            None
        };

        let permission_buttons =
            if let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status {
                Some(self.render_permission_buttons(
                    self.thread.read(cx).session_id().clone(),
                    self.is_first_tool_call(active_session_id, &tool_call.id, cx),
                    options,
                    entry_ix,
                    tool_call.id.clone(),
                    focus_handle,
                    self.sandbox_confusables_block_allow(tool_call, cx),
                    cx,
                ))
            } else {
                None
            };

        let body = v_flex()
            .map(|this| {
                if matches!(
                    layout,
                    ToolCallLayout::Embedded | ToolCallLayout::Floating
                ) {
                    this
                } else if use_card_layout {
                    this.my_1p5()
                        .rounded_md()
                        .border_1()
                        .when(failed_or_canceled, |this| this.border_dashed())
                        .border_color(self.tool_card_border_color(cx))
                        .bg(cx.theme().colors().editor_background)
                        .overflow_hidden()
                } else {
                    // Tighter vertical rhythm and dimmed further than the
                    // muted text color alone: quiet one-line rows.
                    this.my_0p5().opacity(0.85)
                }
            })
            .when(layout == ToolCallLayout::Standalone, |this| {
                // One-line rows sit slightly deeper than prose (px_5), so they
                // read as quiet annotations; cards keep the prose margin.
                this.map(|this| {
                    if use_card_layout {
                        this.ml_5()
                    } else if has_location {
                        this.ml_5()
                    } else {
                        this.ml_6()
                    }
                })
                .mr_5()
            })
            .map(|this| {
                if is_terminal_tool {
                    this.child(self.render_collapsible_command(
                        card_header_id.clone(),
                        true,
                        tool_call.label.clone(),
                        window,
                        cx,
                    ))
                } else {
                    this.child(
                        h_flex()
                            .group(&card_header_id)
                            .relative()
                            .w_full()
                            .justify_between()
                            .when(use_card_layout, |this| {
                                this.p_0p5()
                                    .rounded_t(rems_from_px(5_f32))
                                    .bg(self.tool_card_header_bg(cx))
                            })
                            .child(self.render_tool_call_label(
                                entry_ix,
                                tool_call,
                                is_edit,
                                is_cancelled_edit,
                                has_revealed_diff,
                                use_card_layout,
                                click_toggles_expand,
                                window,
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .when(show_goto_file_button, |this| {
                                        this.child(
                                            IconButton::new(
                                                ("goto-tool-call-file", entry_ix),
                                                IconName::ArrowUpRight,
                                            )
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .visible_on_hover(&card_header_id)
                                            .tooltip(Tooltip::text("Go to File"))
                                            .on_click(cx.listener(
                                                move |this, _, window, cx| {
                                                    this.open_tool_call_location(
                                                        entry_ix, 0, window, cx,
                                                    );
                                                },
                                            )),
                                        )
                                    })
                                    .when(is_collapsible || failed_or_canceled, |this| {
                                        let diff_for_discard = if has_revealed_diff
                                            && is_cancelled_edit
                                        {
                                            tool_call.diffs().next().cloned()
                                        } else {
                                            None
                                        };

                                        this.child(
                                            h_flex()
                                                .pr_0p5()
                                                .gap_1()
                                                .when(is_collapsible, |this| {
                                                    this.child(
                                                        Disclosure::new(
                                                            ("expand-output", entry_ix),
                                                            is_open,
                                                        )
                                                        .opened_icon(IconName::ChevronUp)
                                                        .closed_icon(IconName::ChevronDown)
                                                        .visible_on_hover(&card_header_id)
                                                        .on_click(cx.listener({
                                                            let id = tool_call.id.clone();
                                                            move |this: &mut Self,
                                                                  _,
                                                                  window,
                                                                  cx: &mut Context<Self>| {
                                                                this.entry_view_state.update(
                                                                    cx,
                                                                    |state, _cx| {
                                                                        state
                                                                            .toggle_tool_call_expansion(
                                                                                &id,
                                                                            );
                                                                    },
                                                                );
                                                                this.refresh_thread_search(window, cx);
                                                                cx.notify();
                                                            }
                                                        })),
                                                    )
                                                })
                                                .when(failed_or_canceled, |this| {
                                                    if is_cancelled_edit && !has_revealed_diff {
                                                        this.child(
                                                            div()
                                                                .id(entry_ix)
                                                                .tooltip(Tooltip::text(
                                                                    "Interrupted Edit",
                                                                ))
                                                                .child(
                                                                    Icon::new(IconName::XCircle)
                                                                        .color(Color::Muted)
                                                                        .size(IconSize::Small),
                                                                ),
                                                        )
                                                    } else if is_cancelled_edit {
                                                        this
                                                    } else {
                                                        this.child(
                                                            Icon::new(IconName::Close)
                                                                .color(Color::Error)
                                                                .size(IconSize::Small),
                                                        )
                                                    }
                                                })
                                                .when_some(diff_for_discard, |this, diff| {
                                                    let tool_call_id = tool_call.id.clone();
                                                    let is_discarded = self
                                                        .discarded_partial_edits
                                                        .contains(&tool_call_id);

                                                    this.when(!is_discarded, |this| {
                                                        this.child(
                                                            IconButton::new(
                                                                ("discard-partial-edit", entry_ix),
                                                                IconName::Undo,
                                                            )
                                                            .icon_size(IconSize::Small)
                                                            .tooltip(move |_, cx| {
                                                                Tooltip::with_meta(
                                                                    "Discard Interrupted Edit",
                                                                    None,
                                                                    "You can discard this interrupted partial edit and restore the original file content.",
                                                                    cx,
                                                                )
                                                            })
                                                            .on_click(cx.listener({
                                                                let tool_call_id =
                                                                    tool_call_id.clone();
                                                                move |this, _, _window, cx| {
                                                                    let diff_data = diff.read(cx);
                                                                    let base_text = diff_data
                                                                        .base_text()
                                                                        .clone();
                                                                    let buffer =
                                                                        diff_data.buffer().clone();
                                                                    buffer.update(
                                                                        cx,
                                                                        |buffer, cx| {
                                                                            buffer.set_text(
                                                                                base_text.as_ref(),
                                                                                cx,
                                                                            );
                                                                        },
                                                                    );
                                                                    this.discarded_partial_edits
                                                                        .insert(
                                                                            tool_call_id.clone(),
                                                                        );
                                                                    cx.notify();
                                                                }
                                                            })),
                                                        )
                                                    })
                                                }),
                                        )
                                    })
                                    .when(tool_call_output_focus, |this| {
                                        this.child(
                                            Button::new("open-file-button", "Open File")
                                                .style(ButtonStyle::Outlined)
                                                .label_size(LabelSize::Small)
                                                .key_binding(
                                                    KeyBinding::for_action_in(&OpenExcerpts, &tool_call_output_focus_handle, cx)
                                                        .map(|s| s.size(rems_from_px(12_f32))),
                                                )
                                                .on_click(|_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(OpenExcerpts),
                                                        cx,
                                                    )
                                                }),
                                        )
                                    }),
                            )

                    )
                }
            })
            .children(tool_output_display);

        v_flex()
            .map(|this| {
                if matches!(layout, ToolCallLayout::Embedded | ToolCallLayout::Floating) {
                    this
                } else if use_card_layout {
                    this.my_1p5()
                        .rounded_md()
                        .border_1()
                        .when(failed_or_canceled, |this| this.border_dashed())
                        .border_color(self.tool_card_border_color(cx))
                        .bg(cx.theme().colors().editor_background)
                        .overflow_hidden()
                } else {
                    this.my_1()
                }
            })
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.map(|this| {
                    if has_location && !use_card_layout {
                        this.ml_4()
                    } else {
                        this.ml_5()
                    }
                })
                .mr_5()
            })
            .map(|this| {
                if layout == ToolCallLayout::Floating {
                    this.child(
                        div()
                            .id(("floating-tool-call-body", entry_ix))
                            .max_h_40()
                            .overflow_y_scroll()
                            .child(body),
                    )
                } else {
                    this.child(body)
                }
            })
            .children(permission_buttons)
    }

    /// A small "Learn more" link to the sandboxing docs, deep-linked to
    /// `section` when provided. Shared by the sandbox warning and the two
    /// sandbox approval prompts so the user can always reach an explanation of
    /// what they're being asked about.
    fn render_sandbox_docs_link(
        &self,
        id: &'static str,
        section: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let url = zed_urls::sandboxing_docs(section, cx);

        Button::new(id, "View Sandboxing Docs")
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .end_icon(
                Icon::new(IconName::ArrowUpRight)
                    .color(Color::Muted)
                    .size(IconSize::XSmall),
            )
            .tooltip({
                let url = url.clone();
                move |_, cx| Tooltip::with_meta("Open Docs", None, url.clone(), cx)
            })
            .on_click(move |_, _, cx| cx.open_url(&url))
            .into_any_element()
    }

    fn render_sandbox_authorization_details(
        &self,
        entry_ix: usize,
        tool_call_id: &acp::ToolCallId,
        details: &SandboxAuthorizationDetails,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let has_network = details.network_all_hosts || !details.network_hosts.is_empty();
        let has_write = details.allow_fs_write_all || !details.write_paths.is_empty();
        // The dedicated Windows-drive warning prompt is only ever sent while the
        // warning is enabled, so key the banner on the prompt itself. Keeping it
        // visible even after the "Don't show again" checkbox flips the setting
        // avoids the card disappearing out from under the user mid-decision.
        let has_windows_fs_warning = details.warn_windows_fs;
        if !has_network
            && !has_write
            && !details.unsandboxed
            && details.reason.is_empty()
            && !has_windows_fs_warning
        {
            return Empty.into_any_element();
        }

        let confusable_findings = if Self::confusable_warning_enabled(cx) {
            Self::sandbox_confusable_findings(details)
        } else {
            Vec::new()
        };

        let network_section = has_network.then(|| {
            let summary = if details.network_all_hosts {
                "any host".to_string()
            } else {
                format!(
                    "{} {}",
                    details.network_hosts.len(),
                    if details.network_hosts.len() == 1 {
                        "host"
                    } else {
                        "hosts"
                    }
                )
            };
            let has_host_list = !details.network_all_hosts && !details.network_hosts.is_empty();
            let is_open = !self
                .collapsed_sandbox_network_details
                .contains(tool_call_id);
            let mut hosts = details.network_hosts.clone();
            hosts.sort();

            v_flex()
                .child(
                    h_flex()
                        .id(("sandbox-network-details-header", entry_ix))
                        // Align text with the allow/deny button icons below,
                        // which sit at p_1 (container) + Base04 (button) ≈ px_2.
                        .px_2()
                        .py_1()
                        .justify_between()
                        .when(has_host_list, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .on_click(cx.listener({
                                    let tool_call_id = tool_call_id.clone();
                                    move |this, _event, _window, cx| {
                                        if this
                                            .collapsed_sandbox_network_details
                                            .remove(&tool_call_id)
                                        {
                                            cx.notify();
                                            return;
                                        }

                                        this.collapsed_sandbox_network_details
                                            .insert(tool_call_id.clone());
                                        cx.notify();
                                    }
                                }))
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("Network access")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                )
                                .child(
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .when(has_host_list, |this| {
                            this.child(
                                Disclosure::new(("sandbox-network-details", entry_ix), is_open)
                                    .opened_icon(IconName::ChevronUp)
                                    .closed_icon(IconName::ChevronDown),
                            )
                        }),
                )
                .when(has_host_list && is_open, |this| {
                    this.child(v_flex().children(hosts.iter().enumerate().map(
                        |(host_ix, host)| {
                            h_flex()
                                .min_w_0()
                                .px_2()
                                .py_1p5()
                                .bg(cx.theme().colors().editor_background)
                                .when(host_ix < hosts.len() - 1, |this| {
                                    this.border_b_1().border_color(cx.theme().colors().border)
                                })
                                .child(
                                    Label::new(host.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx),
                                )
                        },
                    )))
                })
        });

        let write_section = has_write.then(|| {
            let summary = if details.allow_fs_write_all {
                "unrestricted except Git metadata".to_string()
            } else {
                format!(
                    "{} {}",
                    details.write_paths.len(),
                    if details.write_paths.len() == 1 {
                        "path"
                    } else {
                        "paths"
                    }
                )
            };
            let has_path_list = !details.allow_fs_write_all && !details.write_paths.is_empty();
            let is_open = !self
                .collapsed_sandbox_authorization_details
                .contains(tool_call_id);
            let mut paths = details.write_paths.clone();
            // Sort by the path that is actually granted (the resolved canonical
            // when present, else the requested path).
            paths.sort_by(|a, b| a.canonical_or_requested().cmp(b.canonical_or_requested()));

            v_flex()
                .child(
                    h_flex()
                        .id(("sandbox-authorization-details-header", entry_ix))
                        .px_2()
                        .py_1()
                        .justify_between()
                        .when(has_path_list, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .on_click(cx.listener({
                                    let tool_call_id = tool_call_id.clone();
                                    move |this, _event, _window, cx| {
                                        if this
                                            .collapsed_sandbox_authorization_details
                                            .remove(&tool_call_id)
                                        {
                                            cx.notify();
                                            return;
                                        }

                                        this.collapsed_sandbox_authorization_details
                                            .insert(tool_call_id.clone());
                                        cx.notify();
                                    }
                                }))
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("Write Access")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                )
                                .child(
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .when(has_path_list, |this| {
                            this.child(
                                Disclosure::new(
                                    ("sandbox-authorization-details", entry_ix),
                                    is_open,
                                )
                                .opened_icon(IconName::ChevronUp)
                                .closed_icon(IconName::ChevronDown),
                            )
                        }),
                )
                .when(has_path_list && is_open, |this| {
                    this.child(v_flex().children(paths.iter().enumerate().map(
                        |(path_ix, path)| {
                            self.render_sandbox_authorization_path_row(entry_ix, path_ix, path, cx)
                        },
                    )))
                })
        });

        let unsandboxed_section = details.unsandboxed.then(|| {
            h_flex()
                .px_2()
                .py_1()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Warning)
                        .color(Color::Warning)
                        .size(IconSize::Small),
                )
                .child(
                    Label::new("Runs without the OS sandbox")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        });

        let reason_section = (!details.reason.is_empty()).then(|| {
            v_flex()
                .px_2()
                .py_1()
                .gap_0p5()
                .child(
                    Label::new("Reason")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(details.reason.clone()).size(LabelSize::Small))
        });

        // The command stays in the tool-call title above; here we show what the
        // command is asking for (paths / domains) and the agent's reason.
        v_flex()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .when(has_windows_fs_warning, |this| {
                this.child(self.render_sandbox_windows_fs_warning(cx))
            })
            .when(!confusable_findings.is_empty(), |this| {
                this.child(self.render_sandbox_confusable_warning(
                    tool_call_id,
                    &confusable_findings,
                    window,
                    cx,
                ))
            })
            .children(network_section)
            .children(write_section)
            .children(unsandboxed_section)
            .children(reason_section)
            .when(!has_windows_fs_warning, |this| {
                // The Windows-drive warning banner carries its own docs link, so
                // skip the default one that every other sandbox prompt appends.
                this.child(
                    h_flex()
                        .px_1()
                        .py_0p5()
                        .child(self.render_sandbox_docs_link(
                            "sandbox-authorization-docs-link",
                            None,
                            cx,
                        )),
                )
            })
            .into_any_element()
    }

    /// Scan the hosts and paths in a sandbox escalation request for surprising
    /// Unicode characters (homoglyphs, invisible characters, bidi overrides).
    /// Returns, for each offending value, the display string shown to the user
    /// and the distinct suspicious characters it contains. Hosts are decoded from
    /// Punycode first, so the display string is the Unicode form the user should
    /// scrutinize. Empty when nothing is surprising.
    fn sandbox_confusable_findings(
        details: &SandboxAuthorizationDetails,
    ) -> Vec<(String, Vec<unicode_confusables::SuspiciousChar>)> {
        let mut findings = Vec::new();
        for host in &details.network_hosts {
            let (decoded, suspicious) = unicode_confusables::scan_host(host);
            if !suspicious.is_empty() {
                findings.push((decoded, suspicious));
            }
        }
        for granted in &details.write_paths {
            // Scan both the requested path and the resolved target (when they
            // differ), so a confusable in either the shown request or the real
            // grant destination is surfaced.
            let requested = granted.requested.display().to_string();
            let resolved = granted.canonical_or_requested().display().to_string();
            for display in [requested, resolved]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
            {
                let suspicious = unicode_confusables::scan(&display);
                if !suspicious.is_empty() {
                    findings.push((display, suspicious));
                }
            }
        }
        findings
    }

    /// Whether the surprising-Unicode warning is enabled in settings (on by
    /// default). When off, prompts neither show the banner nor gate their allow
    /// buttons on it.
    fn confusable_warning_enabled(cx: &App) -> bool {
        AgentSettings::get_global(cx)
            .sandbox_permissions
            .warn_confusable_unicode
    }

    /// Whether this tool call's sandbox escalation shows surprising Unicode that
    /// the user hasn't acknowledged yet. While true, the prompt's allow buttons
    /// stay disabled so the user can't grant access to a lookalike target
    /// without first ticking the acknowledgement checkbox.
    fn sandbox_confusables_block_allow(&self, tool_call: &ToolCall, cx: &App) -> bool {
        if !Self::confusable_warning_enabled(cx) {
            return false;
        }
        let Some(details) = tool_call.sandbox_authorization_details.as_ref() else {
            return false;
        };
        if self
            .acknowledged_confusable_warnings
            .contains(&tool_call.id)
        {
            return false;
        }
        !Self::sandbox_confusable_findings(details).is_empty()
    }

    /// Red banner warning that a requested domain or path contains surprising
    /// Unicode characters, with a checkbox the user must tick to unlock the
    /// allow buttons. See [`Self::sandbox_confusables_block_allow`].
    fn render_sandbox_confusable_warning(
        &self,
        tool_call_id: &acp::ToolCallId,
        findings: &[(String, Vec<unicode_confusables::SuspiciousChar>)],
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let acknowledged = self.acknowledged_confusable_warnings.contains(tool_call_id);
        let line_height = window.line_height();

        v_flex()
            .w_full()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().status().error_border)
            .bg(cx.theme().status().error_background.opacity(0.15))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        h_flex()
                            .h(line_height)
                            .flex_none()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            ),
                    )
                    .child(
                        v_flex().min_w_0().flex_1().gap_1().children(findings.iter().map(
                            |(value, suspicious)| {
                                v_flex()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        Label::new(format!(
                                            "“{value}” contains potentially surprising Unicode characters"
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                    )
                                    .child(v_flex().min_w_0().pl_2().children(
                                        suspicious.iter().map(|character| {
                                            Label::new(format!("• {}", character.description()))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .buffer_font(cx)
                                        }),
                                    ))
                            },
                        )),
                    )
                    .child(
                        IconButton::new("configure-confusable-warning", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Configure unicode confusables warning"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                        target: None,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .child(
                Checkbox::new(
                    SharedString::from(format!("confusable-ack-{}", tool_call_id.0)),
                    if acknowledged {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("I understand and wish to proceed")
                .label_size(LabelSize::Small)
                .on_click(cx.listener({
                    let tool_call_id = tool_call_id.clone();
                    move |this, state: &ToggleState, _window, cx| {
                        if *state == ToggleState::Selected {
                            this.acknowledged_confusable_warnings
                                .insert(tool_call_id.clone());
                        } else {
                            this.acknowledged_confusable_warnings.remove(&tool_call_id);
                        }
                        cx.notify();
                    }
                })),
            )
            .into_any_element()
    }

    /// Whether the Windows-drive (DrvFs) weaker-guarantee warning is enabled in
    /// settings (on by default). Windows-only in effect: `warn_windows_fs` is
    /// never set on other platforms.
    fn ntfs_warning_enabled(cx: &App) -> bool {
        AgentSettings::get_global(cx)
            .sandbox_permissions
            .warn_ntfs_grants
    }

    /// Informational banner shown on a sandbox approval prompt when the command
    /// will write to a file on a Windows drive (reached inside WSL via DrvFs),
    /// whose sandbox-integrity guarantees are weaker than the distro's native
    /// filesystem. Unlike the confusable-Unicode banner this does not gate the
    /// allow buttons: the approval itself is the acknowledgement. A settings gear
    /// links to where the warning can be suppressed.
    fn render_sandbox_windows_fs_warning(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .p_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().status().warning_border)
            .bg(cx.theme().status().warning_background.opacity(0.15))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .gap_0p5()
                            .child(
                                Label::new("This command can write to a file on a Windows drive")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(
                                    "Sandboxes with write access to a location on a Windows \
                                     drive may not provide full filesystem isolation.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(h_flex().child(self.render_sandbox_docs_link(
                                "sandbox-windows-fs-docs-link",
                                Some("windows"),
                                cx,
                            ))),
                    )
                    .child(
                        IconButton::new("configure-ntfs-warning", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Configure Windows-drive warning"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                        target: None,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .child(
                Checkbox::new(
                    "sandbox-windows-fs-dont-warn",
                    if Self::ntfs_warning_enabled(cx) {
                        ToggleState::Unselected
                    } else {
                        ToggleState::Selected
                    },
                )
                .label("Don't show this warning again")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                    let disable = *state == ToggleState::Selected;
                    let fs = this.thread.read(cx).project().read(cx).fs().clone();
                    update_settings_file(fs, cx, move |settings, _| {
                        settings
                            .agent
                            .get_or_insert_default()
                            .sandbox_permissions
                            .get_or_insert_default()
                            .warn_ntfs_grants = Some(!disable);
                    });
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    fn render_sandbox_fallback_authorization_details(
        &self,
        details: &SandboxFallbackAuthorizationDetails,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The command itself is shown in the tool-call header (a collapsible
        // command), so here we only explain *why* the sandbox couldn't be
        // created — the user needs both to decide whether to run unsandboxed.
        if details.reason.is_empty() {
            return Empty.into_any_element();
        }

        h_flex()
            .p_1p5()
            .gap_1p5()
            .items_start()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Icon::new(IconName::Warning)
                    .color(Color::Warning)
                    .size(IconSize::Small),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new("Couldn't create a sandbox")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(details.reason.clone()).size(LabelSize::Small))
                    .child(self.render_sandbox_docs_link(
                        "sandbox-fallback-docs-link",
                        details.docs_section.as_deref(),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_sandbox_authorization_path_row(
        &self,
        entry_ix: usize,
        path_ix: usize,
        granted: &settings::GrantedWritePath,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        // The path that is actually granted is the resolved canonical target.
        // When the request went through a symlink to a *different* target, both
        // paths are shown, each explicitly captioned, so it's unmistakable which
        // string was requested and which location write access is really granted
        // to.
        let granted_path = granted.canonical_or_requested();
        let requested_path = granted.requested.clone();
        // Grants are stored in the request's own namespace (a Windows path stays
        // `C:\...`, a WSL path stays `/...`), so a genuine symlink/junction
        // redirect is just a plain inequality between the request and its
        // resolved canonical.
        let is_redirected = granted
            .resolved
            .as_deref()
            .is_some_and(|resolved| resolved != requested_path.as_path());

        let granted_display = granted_path.display().to_string();
        let requested_display = requested_path.display().to_string();

        let captioned_path = |caption: SharedString, path: String, cx: &Context<Self>| {
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(caption)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(path).size(LabelSize::Small).buffer_font(cx))
        };

        v_flex()
            .id(format!("sandbox-authorization-path-{entry_ix}-{path_ix}"))
            .min_w_0()
            .gap_1()
            .px_2()
            .py_1p5()
            .bg(cx.theme().colors().editor_background)
            .map(|this| {
                if is_redirected {
                    this.child(captioned_path("Source".into(), requested_display, cx))
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .color(Color::Muted)
                                .size(IconSize::Small),
                        )
                        .child(captioned_path("Target".into(), granted_display, cx))
                } else {
                    // Not a genuine redirect: show what the user asked for (e.g.
                    // the `C:\...` path), not the internal Linux canonical.
                    this.child(captioned_path("Write Path".into(), requested_display, cx))
                }
            })
            .child(Divider::horizontal())
    }

    fn render_permission_buttons(
        &self,
        session_id: acp::SessionId,
        is_first: bool,
        options: &PermissionOptions,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        // When true, the "allow" choices are disabled (e.g. an unacknowledged
        // surprising-Unicode warning is showing). "Deny"/"Retry" stay enabled.
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        match options {
            PermissionOptions::Flat(options) => self.render_permission_buttons_flat(
                session_id,
                is_first,
                options,
                entry_ix,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
            PermissionOptions::Dropdown(choices) => self.render_permission_buttons_with_dropdown(
                is_first,
                choices,
                None,
                entry_ix,
                session_id,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
            PermissionOptions::DropdownWithPatterns {
                choices,
                patterns,
                tool_name,
            } => self.render_permission_buttons_with_dropdown(
                is_first,
                choices,
                Some((patterns, tool_name)),
                entry_ix,
                session_id,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
        }
    }

    fn render_permission_buttons_with_dropdown(
        &self,
        is_first: bool,
        choices: &[PermissionOptionChoice],
        patterns: Option<(&[PermissionPattern], &str)>,
        entry_ix: usize,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        let selection = self.permission_selections.get(&tool_call_id);

        let selected_index = selection
            .and_then(|s| s.choice_index())
            .unwrap_or_else(|| choices.len().saturating_sub(1));

        let dropdown_label: SharedString =
            if matches!(selection, Some(PermissionSelection::SelectedPatterns(_))) {
                "Always for selected commands".into()
            } else {
                choices
                    .get(selected_index)
                    .or(choices.last())
                    .map(|choice| choice.label())
                    .unwrap_or_else(|| "Only this time".into())
            };

        // The granularity dropdown is deliberately not rendered: permission
        // prompts are the exception (sessions default to an auto-approving
        // mode), so when one appears it is a plain Allow/Deny decision.
        let _dropdown = if let Some((pattern_list, tool_name)) = patterns {
            self.render_permission_granularity_dropdown_with_patterns(
                choices,
                pattern_list,
                tool_name,
                dropdown_label,
                entry_ix,
                tool_call_id.clone(),
                is_first,
                cx,
            )
        } else {
            self.render_permission_granularity_dropdown(
                choices,
                dropdown_label,
                entry_ix,
                tool_call_id.clone(),
                selected_index,
                is_first,
                cx,
            )
        };

        h_flex()
            .w_full()
            .p_1()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new(("allow-btn", entry_ix), "Allow")
                            .disabled(allow_disabled)
                            .start_icon(
                                Icon::new(IconName::Check)
                                    .size(IconSize::XSmall)
                                    .color(Color::Success),
                            )
                            .label_size(LabelSize::Small)
                            .when(is_first && !allow_disabled, |this| {
                                this.key_binding(
                                    KeyBinding::for_action_in(
                                        &AllowOnce as &dyn Action,
                                        focus_handle,
                                        cx,
                                    )
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                                )
                            })
                            .on_click(cx.listener({
                                let session_id = session_id.clone();
                                let tool_call_id = tool_call_id.clone();
                                move |this, _, window, cx| {
                                    this.authorize_with_granularity(
                                        session_id.clone(),
                                        tool_call_id.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new(("deny-btn", entry_ix), "Deny")
                            .start_icon(
                                Icon::new(IconName::Close)
                                    .size(IconSize::XSmall)
                                    .color(Color::Error),
                            )
                            .label_size(LabelSize::Small)
                            .when(is_first, |this| {
                                this.key_binding(
                                    KeyBinding::for_action_in(
                                        &RejectOnce as &dyn Action,
                                        focus_handle,
                                        cx,
                                    )
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                                )
                            })
                            .on_click(cx.listener({
                                move |this, _, window, cx| {
                                    this.authorize_with_granularity(
                                        session_id.clone(),
                                        tool_call_id.clone(),
                                        false,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
    }

    fn render_permission_granularity_dropdown(
        &self,
        choices: &[PermissionOptionChoice],
        current_label: SharedString,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        selected_index: usize,
        is_first: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let menu_options: Vec<(usize, SharedString)> = choices
            .iter()
            .enumerate()
            .map(|(i, choice)| (i, choice.label()))
            .collect();

        let permission_dropdown_handle = self.permission_dropdown_handle.clone();

        PopoverMenu::new(("permission-granularity", entry_ix))
            .with_handle(permission_dropdown_handle)
            .trigger(
                Button::new(("granularity-trigger", entry_ix), current_label)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .label_size(LabelSize::Small)
                    .when(is_first, |this| {
                        this.key_binding(
                            KeyBinding::for_action_in(
                                &crate::OpenPermissionDropdown as &dyn Action,
                                &self.focus_handle(cx),
                                cx,
                            )
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    }),
            )
            .menu(move |window, cx| {
                let tool_call_id = tool_call_id.clone();
                let options = menu_options.clone();

                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (index, display_name) in options.iter() {
                        let display_name = display_name.clone();
                        let index = *index;
                        let tool_call_id_for_entry = tool_call_id.clone();
                        let is_selected = index == selected_index;
                        menu = menu.toggleable_entry(
                            display_name,
                            is_selected,
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                window.dispatch_action(
                                    SelectPermissionGranularity {
                                        tool_call_id: tool_call_id_for_entry.0.to_string(),
                                        index,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                            },
                        );
                    }

                    menu
                }))
            })
            .into_any_element()
    }

    fn render_permission_granularity_dropdown_with_patterns(
        &self,
        choices: &[PermissionOptionChoice],
        patterns: &[PermissionPattern],
        _tool_name: &str,
        current_label: SharedString,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        is_first: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let default_choice_index = choices.len().saturating_sub(1);
        let menu_options: Vec<(usize, SharedString)> = choices
            .iter()
            .enumerate()
            .map(|(i, choice)| (i, choice.label()))
            .collect();

        let pattern_options: Vec<(usize, SharedString)> = patterns
            .iter()
            .enumerate()
            .map(|(i, cp)| {
                (
                    i,
                    SharedString::from(format!("Always for `{}` commands", cp.display_name)),
                )
            })
            .collect();

        let pattern_count = patterns.len();
        let permission_dropdown_handle = self.permission_dropdown_handle.clone();
        let view = cx.entity().downgrade();

        PopoverMenu::new(("permission-granularity", entry_ix))
            .with_handle(permission_dropdown_handle.clone())
            .anchor(gpui::Anchor::TopRight)
            .attach(gpui::Anchor::BottomRight)
            .trigger(
                Button::new(("granularity-trigger", entry_ix), current_label)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .label_size(LabelSize::Small)
                    .when(is_first, |this| {
                        this.key_binding(
                            KeyBinding::for_action_in(
                                &crate::OpenPermissionDropdown as &dyn Action,
                                &self.focus_handle(cx),
                                cx,
                            )
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    }),
            )
            .menu(move |window, cx| {
                let tool_call_id = tool_call_id.clone();
                let options = menu_options.clone();
                let patterns = pattern_options.clone();
                let view = view.clone();
                let dropdown_handle = permission_dropdown_handle.clone();

                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |menu, _window, cx| {
                        let mut menu = menu;

                        // Read fresh selection state from the view on each rebuild.
                        let selection: Option<PermissionSelection> = view.upgrade().and_then(|v| {
                            let view = v.read(cx);
                            view.permission_selections.get(&tool_call_id).cloned()
                        });

                        let is_pattern_mode =
                            matches!(selection, Some(PermissionSelection::SelectedPatterns(_)));

                        // Granularity choices: "Always for terminal", "Only this time"
                        for (index, display_name) in options.iter() {
                            let display_name = display_name.clone();
                            let index = *index;
                            let tool_call_id_for_entry = tool_call_id.clone();
                            let is_selected = !is_pattern_mode
                                && selection
                                    .as_ref()
                                    .and_then(|s| s.choice_index())
                                    .map_or(index == default_choice_index, |ci| ci == index);

                            let view = view.clone();
                            menu = menu.toggleable_entry(
                                display_name,
                                is_selected,
                                IconPosition::End,
                                None,
                                move |_window, cx| {
                                    view.update(cx, |this, cx| {
                                        this.permission_selections.insert(
                                            tool_call_id_for_entry.clone(),
                                            PermissionSelection::Choice(index),
                                        );
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }

                        menu = menu.separator().header("Select Options…");

                        for (pattern_index, label) in patterns.iter() {
                            let label = label.clone();
                            let pattern_index = *pattern_index;
                            let tool_call_id_for_pattern = tool_call_id.clone();
                            let is_checked = selection
                                .as_ref()
                                .is_some_and(|s| s.is_pattern_checked(pattern_index));

                            let view = view.clone();
                            menu = menu.toggleable_entry(
                                label,
                                is_checked,
                                IconPosition::End,
                                None,
                                move |_window, cx| {
                                    view.update(cx, |this, cx| {
                                        let selection = this
                                            .permission_selections
                                            .get_mut(&tool_call_id_for_pattern);

                                        match selection {
                                            Some(PermissionSelection::SelectedPatterns(_)) => {
                                                // Already in pattern mode — toggle.
                                                this.permission_selections
                                                    .get_mut(&tool_call_id_for_pattern)
                                                    .expect("just matched above")
                                                    .toggle_pattern(pattern_index);
                                            }
                                            _ => {
                                                // First click: activate pattern mode
                                                // with all patterns checked.
                                                this.permission_selections.insert(
                                                    tool_call_id_for_pattern.clone(),
                                                    PermissionSelection::SelectedPatterns(
                                                        (0..pattern_count).collect(),
                                                    ),
                                                );
                                            }
                                        }
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }

                        let any_patterns_checked = selection
                            .as_ref()
                            .is_some_and(|s| s.has_any_checked_patterns());
                        let dropdown_handle = dropdown_handle.clone();
                        menu = menu.custom_row(move |_window, _cx| {
                            div()
                                .py_1()
                                .w_full()
                                .child(
                                    Button::new("apply-patterns", "Apply")
                                        .full_width()
                                        .style(ButtonStyle::Outlined)
                                        .label_size(LabelSize::Small)
                                        .disabled(!any_patterns_checked)
                                        .on_click({
                                            let dropdown_handle = dropdown_handle.clone();
                                            move |_event, _window, cx| {
                                                dropdown_handle.hide(cx);
                                            }
                                        }),
                                )
                                .into_any_element()
                        });

                        menu
                    },
                ))
            })
            .into_any_element()
    }

    fn render_permission_buttons_flat(
        &self,
        session_id: acp::SessionId,
        is_first: bool,
        options: &[acp::PermissionOption],
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        let mut seen_kinds: ArrayVec<acp::PermissionOptionKind, 3, u8> = ArrayVec::new();

        div()
            .p_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .w_full()
            .v_flex()
            .gap_0p5()
            .children(options.iter().map(move |option| {
                let option_id = SharedString::from(option.option_id.0.clone());
                Button::new((option_id, entry_ix), option.name.clone())
                    .map(|this| {
                        // The sandbox-fallback prompt offers a "Retry" option
                        // that re-attempts creating the sandbox; it isn't an
                        // allow/deny choice, so give it its own icon and no
                        // keybinding.
                        let is_retry = option.option_id.0.as_ref()
                            == acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID;
                        let (icon, action) = if is_retry {
                            (
                                Icon::new(IconName::RotateCcw)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                                None,
                            )
                        } else {
                            match option.kind {
                                acp::PermissionOptionKind::AllowOnce => (
                                    Icon::new(IconName::Check)
                                        .size(IconSize::XSmall)
                                        .color(Color::Success),
                                    Some(&AllowOnce as &dyn Action),
                                ),
                                acp::PermissionOptionKind::AllowAlways => (
                                    Icon::new(IconName::CheckDouble)
                                        .size(IconSize::XSmall)
                                        .color(Color::Success),
                                    if option.option_id.0.as_ref()
                                        == acp_thread::SandboxPermission::AllowThread.as_id()
                                    {
                                        None
                                    } else {
                                        Some(&AllowAlways as &dyn Action)
                                    },
                                ),
                                acp::PermissionOptionKind::RejectOnce => (
                                    Icon::new(IconName::Close)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                    Some(&RejectOnce as &dyn Action),
                                ),
                                acp::PermissionOptionKind::RejectAlways | _ => (
                                    Icon::new(IconName::Close)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                    None,
                                ),
                            }
                        };

                        // An "allow" choice is disabled while a surprising-Unicode
                        // warning is unacknowledged; "deny"/"retry" stay enabled.
                        let is_allow = matches!(
                            option.kind,
                            acp::PermissionOptionKind::AllowOnce
                                | acp::PermissionOptionKind::AllowAlways
                        ) && !is_retry;
                        let disabled = allow_disabled && is_allow;

                        let this = this.start_icon(icon).disabled(disabled);

                        let Some(action) = action else {
                            return this;
                        };

                        if !is_first || disabled || seen_kinds.contains(&option.kind) {
                            return this;
                        }

                        seen_kinds.push(option.kind).unwrap();

                        this.key_binding(
                            KeyBinding::for_action_in(action, focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    })
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener({
                        let tool_call_id = tool_call_id.clone();
                        let option_id = option.option_id.clone();
                        let option_kind = option.kind;
                        let session_id = session_id.clone();
                        move |this, _, window, cx| {
                            this.authorize_tool_call(
                                session_id.clone(),
                                tool_call_id.clone(),
                                SelectedPermissionOutcome::new(option_id.clone(), option_kind),
                                window,
                                cx,
                            );
                        }
                    }))
            }))
    }

    fn render_diff_loading(&self, cx: &Context<Self>) -> AnyElement {
        let bar = |n: u64, width_class: &str| {
            let bg_color = cx.theme().colors().element_active;
            let base = h_flex().h_1().rounded_full();

            let modified = match width_class {
                "w_4_5" => base.w_3_4(),
                "w_1_4" => base.w_1_4(),
                "w_2_4" => base.w_2_4(),
                "w_3_5" => base.w_3_5(),
                "w_2_5" => base.w_2_5(),
                _ => base.w_1_2(),
            };

            modified.with_animation(
                ElementId::Integer(n),
                Animation::new(Duration::from_secs(2)).repeat(),
                move |tab, delta| {
                    let delta = (delta - 0.15 * n as f32) / 0.7;
                    let delta = 1.0 - (0.5 - delta).abs() * 2.;
                    let delta = ease_in_out(delta.clamp(0., 1.));
                    let delta = 0.1 + 0.9 * delta;

                    tab.bg(bg_color.opacity(delta))
                },
            )
        };

        v_flex()
            .p_3()
            .gap_1()
            .rounded_b_md()
            .bg(cx.theme().colors().editor_background)
            .child(bar(0, "w_4_5"))
            .child(bar(1, "w_1_4"))
            .child(bar(2, "w_2_4"))
            .child(bar(3, "w_3_5"))
            .child(bar(4, "w_2_5"))
            .into_any_element()
    }

    fn render_tool_call_label(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        is_edit: bool,
        has_failed: bool,
        has_revealed_diff: bool,
        use_card_layout: bool,
        click_toggles_expand: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let has_location = tool_call.locations.len() == 1;
        let is_file = tool_call.kind == acp::ToolKind::Edit && has_location;
        let is_subagent_tool_call = tool_call.is_subagent();

        let file_icon = if has_location {
            FileIcons::get_icon(&tool_call.locations[0].path, cx)
                .map(|from_path| Icon::from_path(from_path).color(Color::Muted))
                .unwrap_or(Icon::new(IconName::ToolPencil).color(Color::Muted))
        } else {
            Icon::new(IconName::ToolPencil).color(Color::Muted)
        };

        let tool_icon = if is_file && has_failed && has_revealed_diff {
            div()
                .id(entry_ix)
                .tooltip(Tooltip::text("Interrupted Edit"))
                .child(DecoratedIcon::new(
                    file_icon,
                    Some(
                        IconDecoration::new(
                            IconDecorationKind::Triangle,
                            self.tool_card_header_bg(cx),
                            cx,
                        )
                        .color(cx.theme().status().warning)
                        .position(gpui::Point {
                            x: px(-2.),
                            y: px(-2.),
                        }),
                    ),
                ))
                .into_any_element()
        } else if is_file {
            div().child(file_icon).into_any_element()
        } else if is_subagent_tool_call {
            Icon::new(self.agent_icon)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else {
            Icon::new(match tool_call.kind {
                acp::ToolKind::Read => IconName::ToolSearch,
                acp::ToolKind::Edit => IconName::ToolPencil,
                acp::ToolKind::Delete => IconName::ToolDeleteFile,
                acp::ToolKind::Move => IconName::ArrowRightLeft,
                acp::ToolKind::Search => IconName::ToolSearch,
                acp::ToolKind::Execute => IconName::ToolTerminal,
                acp::ToolKind::Think => IconName::ToolThink,
                acp::ToolKind::Fetch => IconName::ToolWeb,
                acp::ToolKind::SwitchMode => IconName::ArrowRightLeft,
                acp::ToolKind::Other | _ => IconName::ToolHammer,
            })
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element()
        };

        // Quiet +added -removed line counts for edit one-liners, derived
        // from the tool call's diffs.
        let edit_stats_element = is_edit
            .then(|| {
                let mut stats = action_log::DiffStats::default();
                for diff in tool_call.diffs() {
                    if let Some((_buffer, buffer_diff)) = diff.read(cx).buffer_and_diff(cx) {
                        let file_stats = action_log::DiffStats::single_file(buffer_diff.read(cx));
                        stats.lines_added += file_stats.lines_added;
                        stats.lines_removed += file_stats.lines_removed;
                    }
                }
                stats
            })
            .filter(|stats| stats.lines_added > 0 || stats.lines_removed > 0)
            .map(|stats| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .ml_1()
                    .when(stats.lines_added > 0, |this| {
                        this.child(
                            Label::new(format!("+{}", stats.lines_added))
                                .size(LabelSize::XSmall)
                                .color(Color::Created)
                                .buffer_font(cx),
                        )
                    })
                    .when(stats.lines_removed > 0, |this| {
                        this.child(
                            Label::new(format!("-{}", stats.lines_removed))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted)
                                .buffer_font(cx),
                        )
                    })
            });

        let gradient_overlay = {
            div()
                .absolute()
                .top_0()
                .right_0()
                .w_12()
                .h_full()
                .map(|this| {
                    if use_card_layout {
                        this.bg(linear_gradient(
                            90.,
                            linear_color_stop(self.tool_card_header_bg(cx), 1.),
                            linear_color_stop(self.tool_card_header_bg(cx).opacity(0.2), 0.),
                        ))
                    } else {
                        this.bg(linear_gradient(
                            90.,
                            linear_color_stop(cx.theme().colors().panel_background, 1.),
                            linear_color_stop(
                                cx.theme().colors().panel_background.opacity(0.2),
                                0.,
                            ),
                        ))
                    }
                })
        };

        h_flex()
            .relative()
            .w_full()
            .h(window.line_height() - px(2.))
            .text_size(self.tool_name_font_size())
            .gap_1p5()
            .when(has_location || use_card_layout, |this| this.px_1())
            .when(has_location || click_toggles_expand, |this| {
                this.cursor(CursorStyle::PointingHand)
                    .rounded(rems_from_px(3_f32)) // Concentric border radius
                    .hover(|s| s.bg(cx.theme().colors().element_hover.opacity(0.5)))
            })
            .overflow_hidden()
            .child(tool_icon)
            .child(if has_location {
                h_flex()
                    .id(("open-tool-call-location", entry_ix))
                    .w_full()
                    .map(|this| {
                        if use_card_layout {
                            this.text_color(cx.theme().colors().text)
                        } else {
                            this.text_color(cx.theme().colors().text_muted)
                        }
                    })
                    .child(
                        self.render_markdown(
                            tool_call.label.clone(),
                            MarkdownStyle {
                                prevent_mouse_interaction: true,
                                ..MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
                                    .with_muted_text(cx)
                            },
                            cx,
                        ),
                    )
                    .children(edit_stats_element)
                    .map(|this| {
                        if click_toggles_expand {
                            // Edit rows expand/collapse on click, like
                            // terminal rows; go-to-file moves to the hover
                            // icon button on the right.
                            let id = tool_call.id.clone();
                            this.on_click(cx.listener(move |this, _, window, cx| {
                                this.entry_view_state.update(cx, |state, _cx| {
                                    state.toggle_tool_call_expansion(&id);
                                });
                                this.refresh_thread_search(window, cx);
                                cx.notify();
                            }))
                        } else {
                            this.tooltip(Tooltip::text("Go to File"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_tool_call_location(entry_ix, 0, window, cx);
                                }))
                        }
                    })
                    .into_any_element()
            } else if click_toggles_expand {
                let id = tool_call.id.clone();
                h_flex()
                    .id(("toggle-tool-call-output", entry_ix))
                    .w_full()
                    .child(
                        self.render_markdown(
                            tool_call.label.clone(),
                            MarkdownStyle {
                                prevent_mouse_interaction: true,
                                ..MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
                                    .with_muted_text(cx)
                            },
                            cx,
                        ),
                    )
                    .children(edit_stats_element)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.entry_view_state.update(cx, |state, _cx| {
                            state.toggle_tool_call_expansion(&id);
                        });
                        this.refresh_thread_search(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                h_flex()
                    .w_full()
                    .child(self.render_markdown(
                        tool_call.label.clone(),
                        MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_muted_text(cx),
                        cx,
                    ))
                    .into_any()
            })
            .when(!is_edit, |this| this.child(gradient_overlay))
    }

    /// The image a tool call is about, if any: the picture is the point of such
    /// a call, so the chip shows it rather than describing it.
    ///
    /// An agent delivers one either way: as a file it read, or as image data on
    /// the call itself. A screenshot usually lands outside the project, so a
    /// path that no worktree claims still counts.
    fn tool_call_image(&self, tool_call: &ToolCall, cx: &App) -> Option<ChipImage> {
        if let Some((image, dimensions)) =
            tool_call.content.iter().find_map(|content| content.image())
        {
            return Some(ChipImage::Data {
                image: image.clone(),
                dimensions,
            });
        }

        let location = tool_call.locations.first()?;
        if !Self::path_is_image(&location.path) {
            return None;
        }
        let path = self
            .project
            .upgrade()
            .and_then(|project| {
                let project_path = project.read(cx).find_project_path(&location.path, cx)?;
                project.read(cx).absolute_path(&project_path, cx)
            })
            .or_else(|| location.path.is_absolute().then(|| location.path.clone()))?;
        Some(ChipImage::File(path))
    }

    /// Whether a call is about a picture, without resolving where it lives.
    /// Chip grouping runs without a project handle, and a call that carries an
    /// image must not be folded into a summary either way.
    fn tool_call_has_image(tool_call: &ToolCall, _cx: &App) -> bool {
        tool_call
            .content
            .iter()
            .any(|content| content.image().is_some())
            || tool_call
                .locations
                .first()
                .is_some_and(|location| Self::path_is_image(&location.path))
    }

    /// Whether a path names an image we can draw. SVG is excluded: the image
    /// element rasterizes bitmaps, and Zed opens vector files as text.
    fn path_is_image(path: &std::path::Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
            .is_some_and(|extension| {
                gpui::Img::extensions().contains(&extension.as_str()) && extension != "svg"
            })
    }

    fn open_tool_call_location(
        &self,
        entry_ix: usize,
        location_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let (tool_call_location, agent_location) = self
            .thread
            .read(cx)
            .entries()
            .get(entry_ix)?
            .location(location_ix)?;

        let project_path = self
            .project
            .upgrade()?
            .read(cx)
            .find_project_path(&tool_call_location.path, cx);

        let open_task = self
            .workspace
            .update(cx, |workspace, cx| {
                if let Some(project_path) = project_path {
                    workspace.open_path(project_path, None, true, window, cx)
                } else {
                    workspace.open_abs_path(
                        tool_call_location.path.clone(),
                        OpenOptions {
                            focus: Some(true),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                }
            })
            .log_err()?;
        window
            .spawn(cx, async move |cx| {
                let item = open_task.await?;

                let Some(active_editor) = item.downcast::<Editor>() else {
                    return anyhow::Ok(());
                };

                active_editor.update_in(cx, |editor, window, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    if snapshot.as_singleton().is_some()
                        && let Some(anchor) = snapshot.anchor_in_excerpt(agent_location.position)
                    {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        })
                    } else {
                        let row = tool_call_location.line.unwrap_or_default();
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
                        })
                    }
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);

        None
    }

    fn render_tool_call_content(
        &self,
        session_id: &acp::SessionId,
        entry_ix: usize,
        content: &ToolCallContent,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        has_failed: bool,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        match content {
            ToolCallContent::ContentBlock(content) => {
                if let Some((resource, markdown)) = content.embedded_resource() {
                    self.render_embedded_resource_output(
                        resource,
                        markdown.cloned(),
                        entry_ix,
                        context_ix,
                        tool_call,
                        card_layout,
                        window,
                        cx,
                    )
                } else if let Some(resource_link) = content.resource_link() {
                    self.render_resource_link(resource_link, cx)
                } else if let Some(markdown) = content.markdown() {
                    self.render_markdown_output(
                        markdown.clone(),
                        entry_ix,
                        context_ix,
                        tool_call,
                        card_layout,
                        window,
                        cx,
                    )
                } else if let Some((image, dimensions)) = content.image() {
                    let location = tool_call.locations.first().cloned();
                    self.render_image_output(
                        entry_ix,
                        image.clone(),
                        dimensions,
                        location,
                        card_layout,
                        cx,
                    )
                } else {
                    Empty.into_any_element()
                }
            }
            ToolCallContent::Diff(diff) => {
                self.render_diff_editor(entry_ix, diff, tool_call, has_failed, cx)
            }
            ToolCallContent::Terminal(terminal) => self.render_terminal_tool_call(
                session_id,
                entry_ix,
                terminal,
                tool_call,
                focus_handle,
                ToolCallLayout::Standalone,
                window,
                cx,
            ),
        }
    }

    fn render_embedded_resource_output(
        &self,
        resource: &acp::EmbeddedResource,
        markdown: Option<Entity<Markdown>>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Some(markdown) = markdown {
            return self.render_markdown_output(
                markdown,
                entry_ix,
                context_ix,
                tool_call,
                card_layout,
                window,
                cx,
            );
        }

        let uri = match &resource.resource {
            acp::EmbeddedResourceResource::BlobResourceContents(blob) => blob.uri.as_str(),
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.uri.as_str(),
            _ => "",
        };

        v_flex()
            .gap_1()
            .map(|this| {
                if card_layout {
                    this.p_2().when(context_ix > 0, |this| {
                        this.border_t_1()
                            .border_color(self.tool_card_border_color(cx))
                    })
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .when(!uri.is_empty(), |this| {
                this.child(
                    Label::new(uri.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_resource_link(
        &self,
        resource_link: &acp::ResourceLink,
        cx: &Context<Self>,
    ) -> AnyElement {
        let uri: SharedString = resource_link.uri.clone().into();
        let is_file = resource_link.uri.strip_prefix("file://");

        let Some(project) = self.project.upgrade() else {
            return Empty.into_any_element();
        };

        let label: SharedString = if let Some(abs_path) = is_file {
            // Split off an optional `#L<line>` fragment so the path still resolves.
            let (abs_path, fragment) = abs_path
                .split_once('#')
                .map_or((abs_path, None), |(path, fragment)| (path, Some(fragment)));

            let path_label = if let Some(project_path) = project
                .read(cx)
                .project_path_for_absolute_path(&Path::new(abs_path), cx)
                && let Some(worktree) = project
                    .read(cx)
                    .worktree_for_id(project_path.worktree_id, cx)
            {
                worktree
                    .read(cx)
                    .full_path(&project_path.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                abs_path.to_string()
            };

            match fragment {
                Some(fragment) => format!("{path_label}#{fragment}").into(),
                None => path_label.into(),
            }
        } else {
            uri.clone()
        };

        let button_id = SharedString::from(format!("item-{}", uri));

        div()
            .ml(rems(0.4))
            .pl_2p5()
            .border_l_1()
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                Button::new(button_id, label)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(true)
                    .when(is_file.is_none(), |this| {
                        this.end_icon(
                            Icon::new(IconName::ArrowUpRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .on_click(cx.listener({
                        let workspace = self.workspace.clone();
                        move |_, _, window, cx: &mut Context<Self>| {
                            open_link(uri.clone(), &workspace, window, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_diff_editor(
        &self,
        entry_ix: usize,
        diff: &Entity<acp_thread::Diff>,
        tool_call: &ToolCall,
        has_failed: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tool_progress = matches!(
            &tool_call.status,
            ToolCallStatus::InProgress | ToolCallStatus::Pending
        );

        let revealed_diff_editor = if let Some(entry) =
            self.entry_view_state.read(cx).entry(entry_ix)
            && let Some(editor) = entry.editor_for_diff(diff)
            && diff.read(cx).has_revealed_range(cx)
        {
            Some(editor)
        } else {
            None
        };

        let show_top_border = !has_failed || revealed_diff_editor.is_some();

        v_flex()
            .h_full()
            .when(show_top_border, |this| {
                this.border_t_1()
                    .when(has_failed, |this| this.border_dashed())
                    .border_color(self.tool_card_border_color(cx))
            })
            .child(if let Some(editor) = revealed_diff_editor {
                editor.into_any_element()
            } else if tool_progress && self.as_native_connection(cx).is_some() {
                self.render_diff_loading(cx)
            } else {
                Empty.into_any()
            })
            .into_any()
    }

    fn render_markdown_output(
        &self,
        markdown: Entity<Markdown>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let markdown_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let output = self
            .render_numbered_read_file_output(
                markdown.clone(),
                entry_ix,
                context_ix,
                tool_call,
                markdown_style.clone(),
                cx,
            )
            .unwrap_or_else(|| {
                self.render_markdown(markdown, markdown_style, cx)
                    .into_any()
            });

        v_flex()
            .gap_2()
            .map(|this| {
                if card_layout {
                    this.p_2().when(context_ix > 0, |this| {
                        this.border_t_1()
                            .border_color(self.tool_card_border_color(cx))
                    })
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .text_xs()
            .text_color(cx.theme().colors().text_muted)
            .child(output)
            .into_any_element()
    }

    fn render_numbered_read_file_output(
        &self,
        markdown: Entity<Markdown>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        markdown_style: MarkdownStyle,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let is_read_file = tool_call
            .tool_name
            .as_ref()
            .is_some_and(|tool_name| tool_name.as_ref() == "read_file");
        if !is_read_file {
            return None;
        }

        let markdown = markdown.read(cx);
        let parsed = parse_cat_numbered_markdown_code_block(markdown.source())?;
        let language = markdown.first_code_block_language();
        Some(render_cat_numbered_code_block(
            parsed,
            language,
            markdown_style,
            format!("copy-read-file-output-{entry_ix}-{context_ix}"),
            cx,
        ))
    }

    fn render_image_output(
        &self,
        entry_ix: usize,
        image: Arc<gpui::Image>,
        dimensions: Option<gpui::Size<u32>>,
        location: Option<acp::ToolCallLocation>,
        card_layout: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_2()
            .map(|this| {
                if card_layout {
                    this
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .when_some(location, |this, _loc| {
                this.child(
                    h_flex().w_full().justify_end().child(
                        Button::new(("go-to-file", entry_ix), "Go to File")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_tool_call_location(entry_ix, 0, window, cx);
                            })),
                    ),
                )
            })
            .child(
                // Same definite box as an inline image chip, sized to the
                // picture's own shape so it is fitted into the box without
                // being letterboxed inside it or running past its bottom.
                div()
                    .w(IMAGE_CHIP_WIDTH)
                    .h(image_box_height(dimensions, IMAGE_CHIP_WIDTH))
                    .child(img(image).size_full().object_fit(ObjectFit::Contain)),
            )
            .into_any_element()
    }

    fn render_subagent_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        subagent_session_id: Option<acp::SessionId>,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let subagent_thread_view = subagent_session_id.and_then(|session_id| {
            self.server_view
                .upgrade()
                .and_then(|server_view| server_view.read(cx).as_connected())
                .and_then(|connected| connected.threads.get(&session_id))
        });

        let content = self.render_subagent_card(
            active_session_id,
            entry_ix,
            subagent_thread_view,
            tool_call,
            focus_handle,
            window,
            cx,
        );

        v_flex().mx_5().my_1p5().gap_3().child(content)
    }

    fn render_subagent_card(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        thread_view: Option<&Entity<ThreadView>>,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let thread = thread_view
            .as_ref()
            .map(|view| view.read(cx).thread.clone());
        let subagent_session_id = thread
            .as_ref()
            .map(|thread| thread.read(cx).session_id().clone());
        let action_log = thread.as_ref().map(|thread| thread.read(cx).action_log());
        let changed_buffers = action_log
            .map(|log| log.read(cx).changed_buffers(cx).collect::<Vec<_>>())
            .unwrap_or_default();

        let is_pending_tool_call = thread_view
            .as_ref()
            .and_then(|tv| {
                let sid = tv.read(cx).thread.read(cx).session_id();
                self.conversation.read(cx).pending_tool_call(sid, cx)
            })
            .is_some();

        let is_expanded = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);
        let files_changed = changed_buffers.len();
        let diff_stats = DiffStats::all_files(changed_buffers, cx);

        let is_running = matches!(
            tool_call.status,
            ToolCallStatus::Pending
                | ToolCallStatus::InProgress
                | ToolCallStatus::WaitingForConfirmation { .. }
        );

        let is_failed = matches!(
            tool_call.status,
            ToolCallStatus::Failed | ToolCallStatus::Rejected
        );

        let is_cancelled = matches!(tool_call.status, ToolCallStatus::Canceled)
            || tool_call.content.iter().any(|c| match c {
                ToolCallContent::ContentBlock(block) => {
                    block.text_content(cx) == Some("User canceled")
                }
                _ => false,
            });

        let thread_title = thread
            .as_ref()
            .and_then(|t| t.read(cx).title())
            .filter(|t| !t.is_empty());
        let tool_call_label = tool_call.label.read(cx).source().to_string();
        let has_tool_call_label = !tool_call_label.is_empty();

        let has_title = thread_title.is_some() || has_tool_call_label;
        let has_no_title_or_canceled = !has_title || is_failed || is_cancelled;

        let title: SharedString = if let Some(thread_title) = thread_title {
            thread_title
        } else if !tool_call_label.is_empty() {
            tool_call_label.into()
        } else if is_cancelled {
            "Subagent Canceled".into()
        } else if is_failed {
            "Subagent Failed".into()
        } else {
            "Spawning Agent…".into()
        };

        let card_header_id = format!("subagent-header-{}", entry_ix);
        let status_icon = format!("status-icon-{}", entry_ix);
        let diff_stat_id = format!("subagent-diff-{}", entry_ix);

        let icon = h_flex().w_4().justify_center().child(if is_running {
            SpinnerLabel::new()
                .size(LabelSize::Small)
                .into_any_element()
        } else if is_cancelled {
            div()
                .id(status_icon)
                .child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::Small)
                        .color(Color::Custom(
                            cx.theme().colors().icon_disabled.opacity(0.5),
                        )),
                )
                .tooltip(Tooltip::text("Subagent Cancelled"))
                .into_any_element()
        } else if is_failed {
            div()
                .id(status_icon)
                .child(
                    Icon::new(IconName::Close)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .tooltip(Tooltip::text("Subagent Failed"))
                .into_any_element()
        } else {
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
                .into_any_element()
        });

        let has_expandable_content = thread
            .as_ref()
            .map_or(false, |thread| !thread.read(cx).entries().is_empty());

        let tooltip_meta_description = if is_expanded {
            "Click to Collapse"
        } else {
            "Click to Preview"
        };

        let error_message = self.subagent_error_message(&tool_call.status, tool_call, cx);

        v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .when(has_no_title_or_canceled, |this| this.border_dashed())
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                h_flex()
                    .group(&card_header_id)
                    .h_8()
                    .p_1()
                    .w_full()
                    .justify_between()
                    .when(!has_no_title_or_canceled, |this| {
                        this.bg(self.tool_card_header_bg(cx))
                    })
                    .child(
                        h_flex()
                            .id(format!("subagent-title-{}", entry_ix))
                            .px_1()
                            .min_w_0()
                            .size_full()
                            .gap_2()
                            .justify_between()
                            .rounded_sm()
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .w_full()
                                    .gap_1p5()
                                    .child(icon)
                                    .child(
                                        Label::new(title.to_string())
                                            .size(LabelSize::Custom(self.tool_name_font_size()))
                                            .truncate(),
                                    )
                                    .when(files_changed > 0, |this| {
                                        this.child(
                                            Label::new(format!(
                                                "— {} {} changed",
                                                files_changed,
                                                if files_changed == 1 { "file" } else { "files" }
                                            ))
                                            .size(LabelSize::Custom(self.tool_name_font_size()))
                                            .color(Color::Muted),
                                        )
                                        .child(
                                            DiffStat::new(
                                                diff_stat_id.clone(),
                                                diff_stats.lines_added as usize,
                                                diff_stats.lines_removed as usize,
                                            )
                                            .label_size(LabelSize::Custom(
                                                self.tool_name_font_size(),
                                            )),
                                        )
                                    }),
                            )
                            .when(!has_no_title_or_canceled && !is_pending_tool_call, |this| {
                                this.tooltip(move |_, cx| {
                                    Tooltip::with_meta(
                                        title.to_string(),
                                        None,
                                        tooltip_meta_description,
                                        cx,
                                    )
                                })
                            })
                            .when(has_expandable_content && !is_pending_tool_call, |this| {
                                this.cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                    .child(
                                        div().visible_on_hover(card_header_id).child(
                                            Icon::new(if is_expanded {
                                                IconName::ChevronUp
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .color(Color::Muted)
                                            .size(IconSize::Small),
                                        ),
                                    )
                                    .on_click(cx.listener({
                                        let tool_call_id = tool_call.id.clone();
                                        move |this, _, window, cx| {
                                            let expanded =
                                                this.entry_view_state.update(cx, |state, _cx| {
                                                    state.toggle_tool_call_expansion(&tool_call_id);
                                                    state.is_tool_call_expanded(&tool_call_id)
                                                });
                                            this.refresh_thread_search(window, cx);
                                            telemetry::event!("Subagent Toggled", expanded);
                                            cx.notify();
                                        }
                                    }))
                            }),
                    )
                    .when(is_running && subagent_session_id.is_some(), |buttons| {
                        buttons.child(
                            IconButton::new(format!("stop-subagent-{}", entry_ix), IconName::Stop)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop Subagent"))
                                .when_some(
                                    thread_view
                                        .as_ref()
                                        .map(|view| view.read(cx).thread.clone()),
                                    |this, thread| {
                                        this.on_click(cx.listener(
                                            move |_this, _event, _window, cx| {
                                                telemetry::event!("Subagent Stopped");
                                                thread.update(cx, |thread, cx| {
                                                    thread.cancel(cx).detach();
                                                });
                                            },
                                        ))
                                    },
                                ),
                        )
                    }),
            )
            .when_some(thread_view, |this, thread_view| {
                let thread = &thread_view.read(cx).thread;
                let tv_session_id = thread.read(cx).session_id();
                let pending_tool_call = self
                    .conversation
                    .read(cx)
                    .pending_tool_call(tv_session_id, cx);

                let nav_session_id = tv_session_id.clone();

                let fullscreen_toggle = h_flex()
                    .id(entry_ix)
                    .py_1()
                    .w_full()
                    .justify_center()
                    .border_t_1()
                    .when(is_failed, |this| this.border_dashed())
                    .border_color(self.tool_card_border_color(cx))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(IconName::Maximize)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .tooltip(Tooltip::text("Make Subagent Full Screen"))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        telemetry::event!("Subagent Maximized");
                        this.server_view
                            .update(cx, |this, cx| {
                                this.navigate_to_thread(nav_session_id.clone(), window, cx);
                            })
                            .ok();
                    }));

                if is_running && let Some((_, subagent_tool_call_id, _)) = pending_tool_call {
                    if let Some((entry_ix, tool_call)) =
                        thread.read(cx).tool_call(&subagent_tool_call_id)
                    {
                        this.child(Divider::horizontal().color(DividerColor::Border))
                            .child(thread_view.read(cx).render_any_tool_call(
                                active_session_id,
                                entry_ix,
                                tool_call,
                                focus_handle,
                                ToolCallLayout::Embedded,
                                window,
                                cx,
                            ))
                            .child(fullscreen_toggle)
                    } else {
                        this
                    }
                } else {
                    this.when(is_expanded, |this| {
                        this.child(self.render_subagent_expanded_content(
                            thread_view,
                            tool_call,
                            window,
                            cx,
                        ))
                        .when_some(error_message, |this, message| {
                            this.child(
                                Callout::new()
                                    .severity(Severity::Error)
                                    .icon(IconName::XCircle)
                                    .title(message),
                            )
                        })
                        .child(fullscreen_toggle)
                    })
                }
            })
            .into_any_element()
    }

    fn render_subagent_expanded_content(
        &self,
        thread_view: &Entity<ThreadView>,
        tool_call: &ToolCall,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        const MAX_PREVIEW_ENTRIES: usize = 8;

        let subagent_view = thread_view.read(cx);
        let session_id = subagent_view.thread.read(cx).session_id().clone();

        let is_canceled_or_failed = matches!(
            tool_call.status,
            ToolCallStatus::Canceled | ToolCallStatus::Failed | ToolCallStatus::Rejected
        );

        let editor_bg = cx.theme().colors().editor_background;
        let overlay = {
            div()
                .absolute()
                .inset_0()
                .size_full()
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(editor_bg.opacity(0.5), 0.),
                    linear_color_stop(editor_bg.opacity(0.), 0.1),
                ))
                .block_mouse_except_scroll()
        };

        let entries = subagent_view.thread.read(cx).entries();
        let total_entries = entries.len();
        let mut entry_range = if let Some(info) = tool_call.subagent_session_info.as_ref() {
            info.message_start_index
                ..info
                    .message_end_index
                    .map(|i| (i + 1).min(total_entries))
                    .unwrap_or(total_entries)
        } else {
            0..total_entries
        };
        entry_range.start = entry_range
            .end
            .saturating_sub(MAX_PREVIEW_ENTRIES)
            .max(entry_range.start);
        let start_ix = entry_range.start;

        let scroll_handle = self
            .subagent_scroll_handles
            .borrow_mut()
            .entry(subagent_view.session_id.clone())
            .or_default()
            .clone();

        scroll_handle.scroll_to_bottom();

        let rendered_entries: Vec<AnyElement> = entries
            .get(entry_range)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let actual_ix = start_ix + i;
                subagent_view.render_entry(actual_ix, total_entries, entry, window, cx)
            })
            .collect();

        v_flex()
            .w_full()
            .border_t_1()
            .when(is_canceled_or_failed, |this| this.border_dashed())
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                div()
                    .pb_1()
                    .min_h_0()
                    // Include the tool call id so the same subagent session
                    // rendered in multiple parent cards gets distinct element
                    // ids for its inlined entries (avoids duplicate a11y ids).
                    .id(format!(
                        "subagent-entries-{}-{}",
                        session_id, tool_call.id.0
                    ))
                    .track_scroll(&scroll_handle)
                    .children(rendered_entries),
            )
            .h_56()
            .child(overlay)
            .into_any_element()
    }

    fn subagent_error_message(
        &self,
        status: &ToolCallStatus,
        tool_call: &ToolCall,
        cx: &App,
    ) -> Option<SharedString> {
        if matches!(status, ToolCallStatus::Failed) {
            tool_call.content.iter().find_map(|content| {
                if let ToolCallContent::ContentBlock(block) = content {
                    if let Some(source) = block.text_content(cx).filter(|source| !source.is_empty())
                    {
                        if source == "User canceled" {
                            return None;
                        } else {
                            return Some(SharedString::from(source));
                        }
                    }
                }
                None
            })
        } else {
            None
        }
    }

    fn tool_card_header_bg(&self, cx: &Context<Self>) -> Hsla {
        cx.theme()
            .colors()
            .element_background
            .blend(cx.theme().colors().editor_foreground.opacity(0.025))
    }

    fn tool_card_border_color(&self, cx: &Context<Self>) -> Hsla {
        cx.theme().colors().border.opacity(0.8)
    }

    fn tool_name_font_size(&self) -> Rems {
        rems_from_px(13_f32)
    }

    fn provider_by_name(name: &SharedString, cx: &App) -> Option<Arc<dyn LanguageModelProvider>> {
        LanguageModelRegistry::read_global(cx)
            .providers()
            .into_iter()
            .find(|provider| provider.name().0 == *name)
    }

    pub(crate) fn render_thread_error(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let callout = match self.thread_error.as_ref()? {
            ThreadError::Other { message, .. } => {
                self.render_any_thread_error(message.clone(), window, cx)
            }
            ThreadError::Refusal => self.render_refusal_error(cx),
            ThreadError::DataRetentionConsentRequired => {
                self.render_data_retention_consent_error(cx)
            }
            ThreadError::AuthenticationRequired(error) => {
                self.render_authentication_required_error(error.clone(), cx)
            }
            ThreadError::PaymentRequired => self.render_payment_required_error(cx),
            ThreadError::RateLimitExceeded { provider } => self.render_error_callout(
                "Rate Limit Reached",
                format!(
                    "{provider}'s rate limit was reached. Zed will retry automatically. \
                    You can also wait a moment and try again."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::ServerOverloaded { provider } => self.render_error_callout(
                "Provider Unavailable",
                format!(
                    "{provider}'s servers are temporarily unavailable. Zed will retry \
                    automatically. If the problem persists, check the provider's status page."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::PromptTooLarge => self.render_prompt_too_large_error(cx),
            ThreadError::NoCredentials { provider } => {
                let message = Self::provider_by_name(provider, cx)
                    .map(|provider| provider.missing_credentials_error_message())
                    .unwrap_or_else(|| {
                        format!("No credentials are configured for {provider}.").into()
                    });
                self.render_error_callout("Credentials Missing", message, false, true, cx)
            }
            ThreadError::StreamError { provider } => self.render_error_callout(
                "Connection Interrupted",
                format!(
                    "The connection to {provider}'s API was interrupted. Zed will retry \
                    automatically. If the problem persists, check your network connection."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::AuthenticationFailed { provider } => {
                let message = Self::provider_by_name(provider, cx)
                    .map(|provider| provider.authentication_error_message())
                    .unwrap_or_else(|| format!("Could not authenticate with {provider}.").into());
                self.render_error_callout("Authentication Failed", message, false, false, cx)
            }
            ThreadError::PermissionDenied { provider, message } => {
                let message: SharedString = message.clone().unwrap_or_else(|| {
                    format!("{provider} rejected the request due to insufficient permissions.")
                        .into()
                });

                self.render_error_callout("Permission Denied", message, false, false, cx)
            }
            ThreadError::ProviderRejection { message } => {
                self.render_error_callout("Request Failed", message.clone(), true, false, cx)
            }
            ThreadError::MaxOutputTokens => self.render_error_callout(
                "Output Limit Reached",
                "The model stopped because it reached its maximum output length. \
                You can ask it to continue where it left off."
                    .into(),
                false,
                false,
                cx,
            ),
            ThreadError::NoModelSelected => self
                .render_model_not_available_error(cx)
                .unwrap_or_else(|| {
                    self.render_error_callout(
                        "No Model Selected",
                        "Select a model from the model picker below to get started.".into(),
                        false,
                        false,
                        cx,
                    )
                }),
            ThreadError::ApiError { provider } => self.render_error_callout(
                "API Error",
                format!(
                    "{provider}'s API returned an unexpected error. \
                    If the problem persists, try switching models or restarting Zed."
                )
                .into(),
                true,
                true,
                cx,
            ),
        };

        Some(div().child(callout.border_position(self.callout_border_position())))
    }

    fn render_refusal_error(&self, cx: &mut Context<'_, Self>) -> Callout {
        let model_or_agent_name = self.current_model_name(cx);
        let refusal_message = format!(
            "{} refused to respond to this prompt. \
            This can happen when a model believes the prompt violates its content policy \
            or safety guidelines, so rephrasing it can sometimes address the issue.",
            model_or_agent_name
        );

        Callout::new()
            .severity(Severity::Error)
            .title("Request Refused")
            .icon(IconName::XCircle)
            .description(refusal_message.clone())
            .actions_slot(self.create_copy_button(&refusal_message))
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_authentication_required_error(
        &self,
        error: SharedString,
        cx: &mut Context<Self>,
    ) -> Callout {
        Callout::new()
            .severity(Severity::Error)
            .title("Authentication Required")
            .icon(IconName::XCircle)
            .description(error.clone())
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.authenticate_button(cx))
                    .child(self.create_copy_button(error)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_payment_required_error(&self, cx: &mut Context<Self>) -> Callout {
        const ERROR_MESSAGE: &str =
            "You reached your free usage limit. Upgrade to Zed Pro for more prompts.";

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("Free Usage Exceeded")
            .description(ERROR_MESSAGE)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.upgrade_button(cx))
                    .child(self.create_copy_button(ERROR_MESSAGE)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_error_callout(
        &self,
        title: &'static str,
        message: SharedString,
        show_retry: bool,
        show_copy: bool,
        cx: &mut Context<Self>,
    ) -> Callout {
        let can_resume = show_retry && self.thread.read(cx).can_retry(cx);
        let show_actions = can_resume || show_copy;

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title(title)
            .description(message.clone())
            .when(show_actions, |callout| {
                callout.actions_slot(
                    h_flex()
                        .gap_0p5()
                        .when(can_resume, |this| this.child(self.retry_button(cx)))
                        .when(show_copy, |this| {
                            this.child(self.create_copy_button(message.clone()))
                        }),
                )
            })
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_model_not_available_error(&self, cx: &mut Context<Self>) -> Option<Callout> {
        let thread = self.as_native_thread(cx)?;

        let has_authenticated_provider =
            LanguageModelRegistry::read_global(cx).has_authenticated_provider(cx);

        let (title, description): (SharedString, SharedString) =
            match thread.read(cx).thread_model() {
                agent::ThreadModel::Ready(_) => return None,
                agent::ThreadModel::Unresolved(selected_model) => {
                    if let Some(provider) = LanguageModelRegistry::global(cx)
                        .read(cx)
                        .provider(&&selected_model.provider)
                    {
                        if !provider.is_authenticated(cx) {
                            (
                                format!("Failed to authenticate with {} provider", provider.name())
                                    .into(),
                                "Open the settings to configure the selected provider".into(),
                            )
                        } else {
                            (
                                format!("Model {} was not found", selected_model.model.0).into(),
                                "You may need to reconfigure authentication for this provider"
                                    .into(),
                            )
                        }
                    } else {
                        (
                            format!("Provider {} was not found", selected_model.provider).into(),
                            "Open the settings to configure providers".into(),
                        )
                    }
                }
                agent::ThreadModel::Unset => {
                    if has_authenticated_provider {
                        (
                            "No model selected".into(),
                            "Choose a different model or configure other providers to get started"
                                .into(),
                        )
                    } else {
                        (
                            "No model selected".into(),
                            "Configure a provider to get started".into(),
                        )
                    }
                }
            };

        let callout = Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title(title)
            .description(description)
            .actions_slot(
                h_flex()
                    .gap_1()
                    .child(self.open_llm_providers_settings_button(cx))
                    .when(has_authenticated_provider, |this| {
                        this.child(self.open_model_selector_button(cx))
                    }),
            )
            .dismiss_action(self.dismiss_error_button(cx));

        Some(callout)
    }

    fn open_llm_providers_settings_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("configure-llm-provider", "Configure Provider")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                window.dispatch_action(
                    Box::new(zed_actions::OpenSettingsAt {
                        path: "llm_providers".to_string(),
                        target: None,
                    }),
                    cx,
                );
            }))
    }

    fn open_model_selector_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("open-model-selector", "Select Model")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .key_binding(KeyBinding::for_action(&ToggleModelSelector, cx))
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                window.dispatch_action(ToggleModelSelector.boxed_clone(), cx);
            }))
    }

    fn render_prompt_too_large_error(&self, cx: &mut Context<Self>) -> Callout {
        const MESSAGE: &str = "This conversation is too long for the model's context window. \
            Start a new thread or remove some attached files to continue.";

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("Context Too Large")
            .description(MESSAGE)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.new_thread_button(cx))
                    .child(self.create_copy_button(MESSAGE)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn retry_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("retry", "Retry")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, _, cx| {
                this.retry_generation(cx);
            }))
    }

    fn new_thread_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("new_thread", "New Agent")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                // The errored thread is still a live tab; a plain NewThread
                // would just re-focus it.
                window.dispatch_action(crate::NewAdditionalThread.boxed_clone(), cx);
            }))
    }

    fn upgrade_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("upgrade", "Upgrade")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .on_click(cx.listener({
                move |this, _, _, cx| {
                    this.clear_thread_error(cx);
                    cx.open_url(&zed_urls::upgrade_to_zed_pro_url(cx));
                }
            }))
    }

    fn authenticate_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("authenticate", "Authenticate")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener({
                move |this, _, window, cx| {
                    let server_view = this.server_view.clone();

                    this.clear_thread_error(cx);
                    if let Some(message) = this.in_flight_prompt.take() {
                        this.message_editor.update(cx, |editor, cx| {
                            editor.set_message(message, window, cx);
                        });
                    }
                    let connection = this.thread.read(cx).connection().clone();
                    window.defer(cx, |window, cx| {
                        ConversationView::handle_auth_required(
                            server_view,
                            AuthRequired::new(),
                            connection,
                            window,
                            cx,
                        );
                    })
                }
            }))
    }

    fn current_model_name(&self, cx: &App) -> SharedString {
        // For native agent (Zed Agent), use the specific model name (e.g., "Claude 3.5 Sonnet")
        // For ACP agents, use the agent name (e.g., "Claude Agent", "Gemini CLI")
        // This provides better clarity about what refused the request
        if self.as_native_connection(cx).is_some() {
            self.model_selector
                .clone()
                .and_then(|selector| selector.read(cx).active_model(cx))
                .map(|model| model.name.clone())
                .unwrap_or_else(|| SharedString::from("The model"))
        } else {
            // ACP agent - use the agent name (e.g., "Claude Agent", "Gemini CLI")
            self.agent_id.0.clone()
        }
    }

    /// Agent errors whose payload is JSON (a usage limit, a quota, a provider
    /// error) render as the payload's human-readable message, with its links
    /// clickable and its error code as a quiet secondary label. Anything else
    /// renders raw.
    fn render_any_thread_error(
        &mut self,
        error: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Callout {
        let can_resume = self.thread.read(cx).can_retry(cx);

        let payload = acp_thread::parse_agent_error_payload(&error);
        let source: SharedString = match &payload {
            Some(payload) => acp_thread::linkify_urls(&payload.message).into(),
            None => error.clone(),
        };

        let markdown = if let Some(markdown) = &self.thread_error_markdown {
            markdown.clone()
        } else {
            let markdown = cx.new(|cx| Markdown::new(source, None, None, cx));
            self.thread_error_markdown = Some(markdown.clone());
            markdown
        };

        let markdown_style =
            MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_muted_text(cx);
        let message = self
            .render_markdown(markdown, markdown_style, cx)
            .into_any_element();

        let error_code = payload.as_ref().and_then(|payload| payload.code.clone());
        let description = v_flex()
            .gap_1()
            .child(message)
            .when_some(error_code, |this, code| {
                this.child(
                    Label::new(code)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
            })
            .into_any_element();

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("An Error Happened")
            .description_slot(description)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .when(can_resume, |this| {
                        this.child(
                            IconButton::new("retry", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Retry Generation"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.retry_generation(cx);
                                })),
                        )
                    })
                    .child(self.create_copy_button(error.to_string())),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    /// The scripts a command carried, shown as highlighted code when its chip
    /// is expanded. ssh is already unwrapped by the parser, so a script that
    /// ran on another machine appears the same way a local one does.
    /// Builds the highlighted code for whatever scripts a command carried, so
    /// expanding its chip can show them. Done on expansion because rendering
    /// cannot create entities.
    fn prepare_command_scripts(&mut self, tool_call_id: &acp::ToolCallId, cx: &mut Context<Self>) {
        if self
            .command_script_markdown
            .borrow()
            .contains_key(tool_call_id)
        {
            return;
        }
        let entries = self.thread.read(cx).entries();
        let Some(AgentThreadEntry::ToolCall(tool_call)) = entries.iter().find(
            |entry| matches!(entry, AgentThreadEntry::ToolCall(call) if &call.id == tool_call_id),
        ) else {
            return;
        };
        let scripts = acp_thread::command_scripts(&acp_thread::parse_command(
            &strip_command_fences(&tool_call.label.read(cx).source()),
        ));
        if scripts.is_empty() {
            return;
        }
        let built: Vec<(SharedString, Entity<Markdown>)> = scripts
            .iter()
            .map(|script| {
                let language = script.language.clone().unwrap_or_default();
                let source: SharedString =
                    format!("```{language}\n{}\n```", script.code.trim_end()).into();
                let markdown = cx.new(|cx| Markdown::new(source, None, None, cx));
                (SharedString::from(script.label.clone()), markdown)
            })
            .collect();
        self.command_script_markdown
            .borrow_mut()
            .insert(tool_call_id.clone(), built);
    }

    fn render_command_scripts(
        &self,
        tool_call: &ToolCall,
        window: &Window,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(entries) = self
            .command_script_markdown
            .borrow()
            .get(&tool_call.id)
            .cloned()
        else {
            return Vec::new();
        };
        entries
            .into_iter()
            .map(|(label, markdown)| {
                let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(self.render_markdown(markdown, style, cx))
                    .into_any_element()
            })
            .collect()
    }

    fn render_markdown(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &App,
    ) -> MarkdownElement {
        let list_state = self.list_state.clone();
        render_agent_markdown(
            markdown,
            style,
            &self.workspace,
            &self.code_span_resolver,
            cx,
        )
        // Zooming a diagram grows/shrinks its block; pause tail-following so the
        // viewport stays put instead of snapping back to the bottom. The list
        // resumes following on its own once the content returns to the bottom.
        .on_mermaid_zoom(move |_window, _cx| {
            list_state.pause_following_tail();
        })
    }

    fn create_copy_button(&self, message: impl Into<String>) -> impl IntoElement {
        let message = message.into();

        CopyButton::new("copy-error-message", message).tooltip_label("Copy Error Message")
    }

    fn dismiss_error_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("dismiss", IconName::Close)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Dismiss"))
            .on_click(cx.listener({
                move |this, _, _, cx| {
                    this.clear_thread_error(cx);
                    cx.notify();
                }
            }))
    }

    fn render_resume_notice(_cx: &Context<Self>) -> AnyElement {
        let description = "This agent does not support viewing previous messages. However, your session will still continue from where you last left off.";

        Callout::new()
            .border_position(CalloutBorderPosition::Bottom)
            .severity(Severity::Info)
            .icon(IconName::Info)
            .title("Resumed Session")
            .description(description)
            .into_any_element()
    }

    fn render_codex_windows_warning(&self, cx: &mut Context<Self>) -> Callout {
        Callout::new()
            .border_position(self.callout_border_position())
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title("Codex on Windows")
            .description("For best performance, run Codex in Windows Subsystem for Linux (WSL2)")
            .actions_slot(
                Button::new("open-wsl-modal", "Open in WSL").on_click(cx.listener({
                    move |_, _, _window, cx| {
                        #[cfg(windows)]
                        _window.dispatch_action(
                            zed_actions::wsl_actions::OpenWsl::default().boxed_clone(),
                            cx,
                        );
                        cx.notify();
                    }
                })),
            )
            .dismiss_action(
                IconButton::new("dismiss", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Dismiss Warning"))
                    .on_click(cx.listener({
                        move |this, _, _, cx| {
                            this.show_codex_windows_warning = false;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_skill_loading_issues(&self, cx: &mut Context<Self>) -> Vec<Callout> {
        let border_position = self.callout_border_position();

        let description_warnings = self
            .skill_loading_issues
            .iter()
            .filter(|issue| issue.kind == SkillLoadingIssueKind::DescriptionTooLong)
            .cloned()
            .collect::<Vec<_>>();

        let long_description_warning =
            self.render_skill_description_warnings(description_warnings, cx);

        let other_warnings = self
            .skill_loading_issues
            .iter()
            .filter(|issue| issue.kind != SkillLoadingIssueKind::DescriptionTooLong)
            .enumerate()
            .map(|(index, issue)| {
                let abs_path = issue.path.clone();
                let workspace = self.workspace.clone();
                let path_label = issue.path.display().to_string();
                let target = issue.clone();

                let title = match issue.kind {
                    SkillLoadingIssueKind::LoadFailed => "Skill Failed to Load",
                    SkillLoadingIssueKind::DescriptionTooLong => unreachable!(),
                    SkillLoadingIssueKind::CatalogBudgetExceeded => {
                        "Skill Omitted from Model Catalog"
                    }
                };

                Callout::new()
                    .icon(IconName::Warning)
                    .severity(Severity::Warning)
                    .title(title)
                    .description(format!("{}\n{path_label}", issue.message))
                    .actions_slot(
                        Button::new(("open-skill-file", index), "Open Skill")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let abs_path = abs_path.clone();
                                workspace
                                    .update(cx, |workspace, cx| {
                                        workspace
                                            .open_abs_path(
                                                abs_path,
                                                workspace::OpenOptions::default(),
                                                window,
                                                cx,
                                            )
                                            .detach_and_log_err(cx);
                                    })
                                    .ok();
                            })),
                    )
                    .dismiss_action(
                        IconButton::new(("dismiss-skill-issue", index), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.skill_loading_issues.retain(|issue| *issue != target);
                                this.dismissed_skill_loading_issues.insert(target.clone());
                                cx.notify();
                            })),
                    )
            })
            .collect::<Vec<_>>();

        long_description_warning
            .into_iter()
            .chain(other_warnings)
            .map(|callout| callout.border_position(border_position))
            .collect()
    }

    fn render_skill_description_warnings(
        &self,
        description_warnings: Vec<SkillLoadingIssue>,
        cx: &mut Context<Self>,
    ) -> Option<Callout> {
        if description_warnings.is_empty() {
            return None;
        }

        let warning_count = description_warnings.len();
        let title = if warning_count == 1 {
            "1 Skill Loaded with a Long Description".to_string()
        } else {
            format!("{warning_count} Skills Loaded with Long Descriptions")
        };

        let rows = description_warnings
            .iter()
            .enumerate()
            .map(|(index, issue)| {
                let abs_path = issue.path.clone();
                let workspace = self.workspace.clone();
                let full_path = issue.path.display().to_string();
                let file_label = skill_issue_file_label(&issue.path);

                ButtonLike::new(("skill-description-warning-file", index))
                    .full_width()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Dash)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(file_label).size(LabelSize::Small)),
                    )
                    .tooltip(move |_, cx| {
                        Tooltip::with_meta("Open Skill", None, full_path.clone(), cx)
                    })
                    .on_click(cx.listener(move |_, _, window, cx| {
                        let abs_path = abs_path.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace
                                    .open_abs_path(
                                        abs_path,
                                        workspace::OpenOptions::default(),
                                        window,
                                        cx,
                                    )
                                    .detach_and_log_err(cx);
                            })
                            .ok();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let callout = Callout::new()
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title(title)
            .description_slot(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(format!(
                            "Ensure skill descriptions are at most {MAX_SKILL_DESCRIPTION_LEN} bytes; longer ones may consume more model-context tokens."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .children(rows),
            );

        let targets = description_warnings;

        Some(
            callout.dismiss_action(
                IconButton::new("dismiss-skill-description-warnings", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dismiss"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.skill_loading_issues
                            .retain(|issue| !targets.contains(issue));
                        for target in &targets {
                            this.dismissed_skill_loading_issues.insert(target.clone());
                        }
                        cx.notify();
                    })),
            ),
        )
    }

    fn render_external_source_prompt_warning(&self, cx: &mut Context<Self>) -> Callout {
        Callout::new()
            .border_position(self.callout_border_position())
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title("Review Before Sending")
            .description("This prompt was pre-filled by an external link. Read it carefully before you submit it to the model.")
            .dismiss_action(
                IconButton::new("dismiss-external-source-prompt-warning", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dismiss Warning"))
                    .on_click(cx.listener({
                        move |this, _, _, cx| {
                            this.show_external_source_prompt_warning = false;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_multi_root_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        if self.multi_root_callout_dismissed {
            return None;
        }

        if self.as_native_connection(cx).is_some() {
            return None;
        }

        if self
            .thread
            .read(cx)
            .connection()
            .supports_session_additional_directories()
        {
            return None;
        }

        let project = self.project.upgrade()?;
        let worktree_count = project.read(cx).visible_worktrees(cx).count();
        if worktree_count <= 1 {
            return None;
        }

        let work_dirs = self.thread.read(cx).work_dirs()?;
        let active_dir = work_dirs
            .ordered_paths()
            .next()
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "one folder".to_string());

        Some(
            Callout::new()
                .severity(Severity::Warning)
                .icon(IconName::Warning)
                .title("This agent doesn't currently support multi-root workspaces")
                .description(format!(
                    "It currently only operates by default on \"{}\".",
                    active_dir
                ))
                .border_position(self.callout_border_position())
                .dismiss_action(
                    IconButton::new("dismiss-multi-root-callout", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Dismiss"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.multi_root_callout_dismissed = true;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_new_version_callout(&self, version: &SharedString, cx: &mut Context<Self>) -> Div {
        let server_view = self.server_view.clone();
        let has_version = !version.is_empty();
        let title = if has_version {
            "New Version Available"
        } else {
            "Agent Update Available"
        };
        let button_label = if has_version {
            format!("Update to v{}", version)
        } else {
            "Reconnect".to_string()
        };

        v_flex().w_full().justify_end().child(
            h_flex()
                .p_2()
                .pr_3()
                .w_full()
                .gap_1p5()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().element_background)
                .child(
                    h_flex()
                        .flex_1()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Download)
                                .color(Color::Accent)
                                .size(IconSize::Small),
                        )
                        .child(Label::new(title).size(LabelSize::Small)),
                )
                .child(
                    Button::new("update-button", button_label)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(move |_, window, cx| {
                            server_view
                                .update(cx, |view, cx| view.reset(window, cx))
                                .ok();
                        }),
                ),
        )
    }

    fn render_token_limit_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        if self.token_limit_callout_dismissed || self.as_native_thread(cx).is_none() {
            return None;
        }

        let token_usage = self.thread.read(cx).token_usage()?;

        // When auto-compaction is available (the model's context window is large
        // enough), the thread is compacted automatically before it reaches the
        // limit, so there's no need to warn the user. Models with a context
        // window that's too small can't be auto-compacted, so we fall back to
        // the normal warning.
        if token_usage.max_tokens >= agent::MIN_COMPACTION_CONTEXT_WINDOW {
            return None;
        }

        let ratio = token_usage.ratio();

        let (severity, icon, title) = match ratio {
            acp_thread::TokenUsageRatio::Normal => return None,
            acp_thread::TokenUsageRatio::Warning => (
                Severity::Warning,
                IconName::Warning,
                "Conversation reaching the token limit soon",
            ),
            acp_thread::TokenUsageRatio::Exceeded => (
                Severity::Error,
                IconName::XCircle,
                "Conversation reached the token limit",
            ),
        };

        let description =
            "To continue, run /compact or start a new conversation and @-mention this one";

        Some(
            Callout::new()
                .border_position(self.callout_border_position())
                .severity(severity)
                .icon(icon)
                .title(title)
                .description(description)
                .actions_slot(
                    h_flex().gap_0p5().child(
                        Button::new("start-new-thread", "Start New Agent")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let session_id = this.thread.read(cx).session_id().clone();
                                window.dispatch_action(
                                    crate::NewNativeAgentThreadFromSummary {
                                        from_session_id: session_id,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                            })),
                    ),
                )
                .dismiss_action(self.dismiss_error_button(cx)),
        )
    }

    /// Returns the model to offer as a downgrade target when the current model
    /// requires data retention consent (e.g. Opus 4.8 for Fable).
    fn data_retention_fallback_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let thread = self.as_native_thread(cx)?;
        let model = thread.read(cx).model()?.clone();
        let fallback_id = model.refusal_fallback_model_id()?;
        LanguageModelRegistry::read_global(cx)
            .available_models(cx)
            .find(|fallback| {
                fallback.provider_id() == model.provider_id()
                    && fallback.id().0.as_ref() == fallback_id
            })
    }

    fn render_data_retention_consent_error(&self, cx: &mut Context<Self>) -> Callout {
        let fallback_model = self.data_retention_fallback_model(cx);

        Callout::new()
            .severity(Severity::Warning)
            .icon(IconName::Warning)
            .title(format!(
                "Note: {} cannot be offered with Zero Data Retention.",
                self.current_model_name(cx)
            ))
            .description_slot(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("Anthropic will retain inference logs.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("data-retention-learn-more", "Learn More")
                            .label_size(LabelSize::Small)
                            .on_click(|_, _, cx| {
                                cx.open_url(DATA_RETENTION_LEARN_MORE_URL);
                            }),
                    ),
            )
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .when_some(fallback_model, |this, fallback| {
                        this.child(
                            Button::new(
                                "switch-data-retention-fallback",
                                format!("Switch to {}", fallback.name().0),
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_to_data_retention_fallback_and_resend(cx);
                            })),
                        )
                    })
                    .child(
                        Button::new("accept-data-retention", "Accept")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Warning))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.accept_data_retention_and_resend(cx);
                            })),
                    ),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn accept_data_retention_and_resend(&mut self, cx: &mut Context<Self>) {
        let fs = self.thread.read(cx).project().read(cx).fs().clone();
        // Resume the failed turn only once the in-memory settings reflect
        // consent, otherwise the resent request would be rejected again.
        let completion = update_settings_file_with_completion(fs, cx, |settings, _| {
            settings
                .telemetry
                .get_or_insert_default()
                .anthropic_retention = Some(true);
        });
        cx.spawn(async move |this, cx| {
            completion.await??;
            this.update(cx, |this, cx| this.retry_generation(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn switch_to_data_retention_fallback_and_resend(&mut self, cx: &mut Context<Self>) {
        let Some(fallback) = self.data_retention_fallback_model(cx) else {
            return;
        };
        let model_id = acp_thread::AgentModelId::new(format!(
            "{}/{}",
            fallback.provider_id().0,
            fallback.id().0
        ));
        let session_id = self.thread.read(cx).session_id().clone();
        let Some(selector) = self
            .thread
            .read(cx)
            .connection()
            .model_selector(&session_id)
        else {
            return;
        };
        let select = selector.select_model(model_id, cx);
        cx.spawn(async move |this, cx| {
            select.await?;
            this.update(cx, |this, cx| this.retry_generation(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_permission_dropdown(
        &mut self,
        _: &crate::OpenPermissionDropdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu_handle = self.permission_dropdown_handle.clone();
        window.defer(cx, move |window, cx| {
            menu_handle.toggle(window, cx);
        });
    }

    fn open_add_context_menu(
        &mut self,
        _action: &OpenAddContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu_handle = self.add_context_menu_handle.clone();
        window.defer(cx, move |window, cx| {
            menu_handle.toggle(window, cx);
        });
    }

    fn toggle_fast_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.fast_mode_available(cx) {
            return;
        }

        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };

        let current_speed = thread.read(cx).speed().unwrap_or_default();
        let new_speed = current_speed.toggle();

        // Enabling with a pending provider warning goes through the agent
        // settings popover, which shows the warning inline.
        if new_speed == Speed::Fast && self.pending_fast_mode_confirmation(cx).is_some() {
            if let Some(model_selector) = self.model_selector.clone() {
                window.defer(cx, move |window, cx| {
                    model_selector.update(cx, |selector, cx| selector.toggle(window, cx));
                });
            }
            return;
        }

        self.apply_fast_mode_speed(new_speed, cx);
    }

    fn apply_fast_mode_speed(&mut self, new_speed: Speed, cx: &mut Context<Self>) {
        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };
        thread.update(cx, |thread, cx| {
            thread.set_speed(new_speed, cx);

            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.speed = Some(new_speed);
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.speed = Some(new_speed)
                        });
                    }
                }
            });
        });
    }

    fn cycle_native_agent_thinking_effort(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };

        let (effort_levels, current_effort) = {
            let thread_ref = thread.read(cx);
            let Some(model) = thread_ref.model() else {
                return;
            };
            if !model.supports_thinking() || !thread_ref.thinking_enabled() {
                return;
            }
            let effort_levels = model.supported_effort_levels();
            if effort_levels.is_empty() {
                return;
            }
            let current_effort = thread_ref.thinking_effort().cloned();
            (effort_levels, current_effort)
        };

        let current_index = current_effort.and_then(|current| {
            effort_levels
                .iter()
                .position(|level| level.value == current)
        });
        let next_index = match current_index {
            Some(index) => (index + 1) % effort_levels.len(),
            None => 0,
        };
        let next_effort = effort_levels[next_index].value.to_string();

        thread.update(cx, |thread, cx| {
            thread.set_thinking_effort(Some(next_effort.clone()), cx);

            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.effort = Some(next_effort.clone());
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.effort = Some(next_effort)
                        });
                    }
                }
            });
        });
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep the message editor's local slash commands in sync with the
        // current availability of feedback/sharing, which can change between
        // renders (settings, connection state, feature flags).
        self.sync_local_commands(cx);

        // The list renders its entries while this frame's tree is laid out, so
        // anything the chips work out along the way holds until the next frame
        // starts here.
        self.chip_cache.begin_frame();

        let has_messages = self.list_state.item_count() > 0;
        let list_state = self.list_state.clone();

        let conversation = v_flex()
            .when(self.resumed_without_history, |this| {
                this.child(Self::render_resume_notice(cx))
            })
            .map(|this| {
                if has_messages {
                    this.relative()
                        .flex_1()
                        .size_full()
                        .child(self.render_entries(cx))
                        .vertical_scrollbar_for(&list_state, window, cx)
                        .into_any()
                } else {
                    this.into_any()
                }
            });

        v_flex()
            .key_context("AcpThread")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                if this.parent_session_id.is_none() {
                    this.cancel_generation(cx);
                }
            }))
            .on_action(cx.listener(
                |this, _: &super::thread_search_bar::DismissThreadSearch, window, cx| {
                    this.close_thread_search(window, cx);
                },
            ))
            // Esc can arrive as `editor::Cancel` from the query editor.
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    if !this.close_thread_search(window, cx) {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(
                |this, action: &super::thread_search_bar::SelectNextThreadMatch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.select_next_match(action, window, cx));
                    }
                },
            ))
            .on_action(cx.listener(
                |this, action: &super::thread_search_bar::SelectPreviousThreadMatch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.select_prev_match(action, window, cx));
                    }
                },
            ))
            .on_action(
                cx.listener(|this, _: &search::ToggleCaseSensitive, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| {
                            bar.toggle_case_sensitive(&search::ToggleCaseSensitive, window, cx)
                        });
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &search::ToggleWholeWord, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| {
                            bar.toggle_whole_word(&search::ToggleWholeWord, window, cx)
                        });
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &search::ToggleRegex, window, cx| {
                if !this.thread_search_visible {
                    cx.propagate();
                    return;
                }
                if let Some(bar) = this.thread_search_bar.clone() {
                    bar.update(cx, |bar, cx| {
                        bar.toggle_regex(&search::ToggleRegex, window, cx)
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action: &search::FocusSearch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.focus_search(action, window, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &workspace::GoBack, window, cx| {
                if let Some(parent_session_id) = this.thread.read(cx).parent_session_id().cloned() {
                    this.server_view
                        .update(cx, |view, cx| {
                            view.navigate_to_thread(parent_session_id, window, cx);
                        })
                        .ok();
                }
            }))
            .on_action(cx.listener(Self::keep_all))
            .on_action(cx.listener(Self::reject_all))
            .on_action(cx.listener(Self::undo_last_reject))
            .on_action(cx.listener(Self::allow_always))
            .on_action(cx.listener(Self::allow_once))
            .on_action(cx.listener(Self::reject_once))
            .on_action(cx.listener(Self::handle_authorize_tool_call))
            .on_action(cx.listener(Self::handle_select_permission_granularity))
            .on_action(cx.listener(Self::handle_toggle_command_pattern))
            .on_action(cx.listener(Self::open_permission_dropdown))
            .on_action(cx.listener(Self::open_add_context_menu))
            .on_action(cx.listener(Self::scroll_output_page_up))
            .on_action(cx.listener(Self::scroll_output_page_down))
            .on_action(cx.listener(Self::scroll_output_line_up))
            .on_action(cx.listener(Self::scroll_output_line_down))
            .on_action(cx.listener(Self::scroll_output_to_top))
            .on_action(cx.listener(Self::scroll_output_to_bottom))
            .on_action(cx.listener(Self::scroll_output_to_previous_message))
            .on_action(cx.listener(Self::scroll_output_to_next_message))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(|this, _: &ToggleFastMode, window, cx| {
                this.toggle_fast_mode(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleThinkingMode, _window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(thread) = this.as_native_thread(cx) {
                    thread.update(cx, |thread, cx| {
                        let model_allows_disabling = thread
                            .model()
                            .is_none_or(|model| model.supports_disabling_thinking());
                        if model_allows_disabling {
                            thread.set_thinking_enabled(!thread.thinking_enabled(), cx);
                        }
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &CycleThinkingEffort, _window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::ThoughtLevel,
                            false,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }
                this.cycle_native_agent_thinking_effort(cx);
            }))
            .on_action(
                cx.listener(|this, _: &ToggleThinkingEffortMenu, window, cx| {
                    if this.thread.read(cx).status() != ThreadStatus::Idle {
                        return;
                    }
                    if let Some(config_options_view) = this.config_options_view.clone() {
                        let handled = config_options_view.update(cx, |view, cx| {
                            view.toggle_category_picker(
                                acp::SessionConfigOptionCategory::ThoughtLevel,
                                window,
                                cx,
                            )
                        });
                        if handled {
                            return;
                        }
                    }
                    let menu_handle = this.thinking_effort_menu_handle.clone();
                    window.defer(cx, move |window, cx| {
                        menu_handle.toggle(window, cx);
                    });
                }),
            )
            .on_action(cx.listener(|this, _: &SendNextQueuedMessage, window, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.send_queued_message_now(id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RemoveFirstQueuedMessage, _, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.remove_from_queue(id, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &EditFirstQueuedMessage, window, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.move_queued_message_to_main_editor(id, None, None, window, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSteerFirstQueuedMessage, _, cx| {
                    if this.as_native_thread(cx).is_none() {
                        return;
                    }
                    if let Some(id) = this.message_queue.first_id() {
                        this.toggle_queue_entry_steer(id, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &ClearMessageQueue, _, cx| {
                this.clear_queue(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleProfileSelector, window, cx| {
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.toggle_category_picker(
                            acp::SessionConfigOptionCategory::Mode,
                            window,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(profile_selector) = this.profile_selector.clone() {
                    profile_selector.read(cx).menu_handle().toggle(window, cx);
                } else if let Some(mode_selector) = this.mode_selector.clone() {
                    mode_selector.read(cx).menu_handle().toggle(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CycleModeSelector, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::Mode,
                            false,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(profile_selector) = this.profile_selector.clone() {
                    profile_selector.update(cx, |profile_selector, cx| {
                        profile_selector.cycle_profile(cx);
                    });
                } else if let Some(mode_selector) = this.mode_selector.clone() {
                    mode_selector.update(cx, |mode_selector, cx| {
                        mode_selector.cycle_mode(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleModelSelector, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.toggle_category_picker(
                            acp::SessionConfigOptionCategory::Model,
                            window,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(model_selector) = this.model_selector.clone() {
                    model_selector
                        .update(cx, |model_selector, cx| model_selector.toggle(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &CycleFavoriteModels, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::Model,
                            true,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(model_selector) = this.model_selector.clone() {
                    model_selector.update(cx, |model_selector, cx| {
                        model_selector.cycle_favorite_models(window, cx);
                    });
                }
            }))
            .size_full()
            .children(self.render_subagent_titlebar(cx))
            .when_some(
                self.thread_search_visible
                    .then(|| self.thread_search_bar.clone())
                    .flatten(),
                |this, bar| this.child(bar),
            )
            .child(conversation)
            .children(self.render_multi_root_callout(cx))
            .children(self.render_active_area_row(window, cx))
            .children(self.render_activity_bar(window, cx))
            .when(self.show_external_source_prompt_warning, |this| {
                this.child(self.render_external_source_prompt_warning(cx))
            })
            .when(self.show_codex_windows_warning, |this| {
                this.child(self.render_codex_windows_warning(cx))
            })
            .children(self.render_skill_loading_issues(cx))
            .children(self.render_thread_retry_status_callout(cx))
            .children(self.render_thread_error(window, cx))
            .when_some(
                match has_messages {
                    true => None,
                    false => self.new_server_version_available.clone(),
                },
                |this, version| this.child(self.render_new_version_callout(&version, cx)),
            )
            .children(self.render_token_limit_callout(cx))
            .children(self.render_request_elicitations(cx))
            .child(self.render_message_editor(window, cx))
    }
}

pub(crate) fn open_link(
    url: SharedString,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        cx.open_url(&url);
        return;
    };

    let path_style = workspace.read(cx).path_style(cx);
    let (relative_path, fragment) = split_local_url_fragment(&url);
    if let Some(fragment) = fragment
        && !relative_path.is_empty()
        && !path_style.is_absolute(relative_path)
    {
        let project = workspace.read(cx).project().clone();
        let decoded_path = decode_path_escapes(relative_path);
        let abs_path = project.update(cx, |project, cx| {
            let resolve_path = |path: &str| {
                let project_path = project.find_project_path(path, cx)?;
                project.entry_for_path(&project_path, cx)?;
                project.absolute_path(&project_path, cx)
            };
            resolve_path(&decoded_path).or_else(|| resolve_path(relative_path))
        });
        if let Some(abs_path) = abs_path {
            let point = fragment
                .strip_prefix('L')
                .and_then(source_position_from_fragment)
                .map(|(row, _)| Point::new(row, 0));
            workspace.update(cx, |workspace, cx| {
                open_abs_path_at_point(workspace, abs_path, point, window, cx);
            });
            return;
        }
    }

    if let Some(mention) = MentionUri::parse_hyperlink(&url, path_style).log_err() {
        // Percent escapes in bare paths are ambiguous: prefer the decoded
        // interpretation, falling back to the literal one (e.g. a file
        // actually named `a%20b.rs`) only when the decoded path doesn't
        // resolve in the project but the literal one does.
        let resolves_in_project = |mention: &MentionUri, cx: &App| {
            mention.abs_path().is_some_and(|abs_path| {
                let project = workspace.read(cx).project().read(cx);
                project
                    .find_project_path(abs_path, cx)
                    .is_some_and(|path| project.entry_for_path(&path, cx).is_some())
            })
        };
        let mention = match MentionUri::parse_hyperlink_literal(&url, path_style) {
            Some(literal)
                if !resolves_in_project(&mention, cx) && resolves_in_project(&literal, cx) =>
            {
                literal
            }
            _ => mention,
        };
        workspace.update(cx, |workspace, cx| match mention {
            MentionUri::File { abs_path } => {
                open_abs_path_at_point(workspace, abs_path, None, window, cx);
            }
            MentionUri::PastedImage { .. } => {}
            MentionUri::Directory { abs_path } => {
                let project = workspace.project();
                let Some(entry_id) = project.update(cx, |project, cx| {
                    let path = project.find_project_path(abs_path, cx)?;
                    project.entry_for_path(&path, cx).map(|entry| entry.id)
                }) else {
                    return;
                };

                project.update(cx, |_, cx| {
                    cx.emit(project::Event::RevealInProjectPanel(entry_id));
                });
            }
            MentionUri::Symbol {
                abs_path: path,
                line_range,
                ..
            } => {
                open_abs_path_at_point(
                    workspace,
                    path,
                    Some(Point::new(*line_range.start(), 0)),
                    window,
                    cx,
                );
            }
            MentionUri::Selection {
                abs_path: Some(path),
                line_range,
                column,
            } => {
                open_abs_path_at_point(
                    workspace,
                    path,
                    Some(Point::new(*line_range.start(), column.unwrap_or(0))),
                    window,
                    cx,
                );
            }
            MentionUri::Selection { abs_path: None, .. } => {}
            MentionUri::Thread { id, name } => {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.open_thread(id, None, Some(name.into()), window, cx)
                    });
                }
            }
            MentionUri::Fetch { url } => {
                cx.open_url(url.as_str());
            }
            MentionUri::Diagnostics { .. } => {}
            MentionUri::TerminalSelection { .. } => {}
            MentionUri::GitDiff { .. } => {}
            MentionUri::MergeConflict { .. } => {}
            MentionUri::Rule { name, .. } => {
                crate::ui::open_migrated_rule(workspace, &name, window, cx);
            }
            MentionUri::Skill {
                skill_file_path, ..
            } => {
                workspace
                    .open_abs_path(
                        skill_file_path,
                        workspace::OpenOptions {
                            focus: Some(true),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            }
        })
    } else {
        workspace.update(cx, |workspace, cx| {
            workspace.open_url_or_file(&url, None, window, cx);
        });
    }
}

/// Returns the name of the leading built-in (native-category) slash command —
/// e.g. `compact` for `/compact` or `/compact summarize the API work` — whether
/// or not the user typed any trailing text after it. Built-in commands ignore
/// trailing arguments, so the caller sends the bare command and queues any
/// remainder rather than discarding it. Commands from MCP servers and ACP
/// agents are excluded: their trailing text is a real argument the agent
/// consumes.
///
/// Native commands run a turn that produces its own thread entry, so the typed
/// command is never echoed as a user message (see `send_command_queueing_remainder`).
fn leading_native_command(
    text: &str,
    available_commands: &[acp::AvailableCommand],
) -> Option<String> {
    let rest = text.trim_start().strip_prefix('/')?;
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    let is_native = available_commands.iter().any(|command| {
        command.name == name
            && acp_thread::command_category_from_meta(&command.meta)
                == Some(acp_thread::CommandCategory::Native)
    });
    is_native.then(|| name.to_string())
}

/// Removes a leading `/command_name` token from `text`, returning the trimmed
/// remainder. Falls back to the trimmed input if the prefix isn't present.
fn strip_leading_command(text: &str, command_name: &str) -> String {
    let trimmed = text.trim_start();
    trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.strip_prefix(command_name))
        .map(|rest| rest.trim_start().to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

/// One comment parsed out of a composed review block: where it anchors, the
/// quoted code it points at, and the comment text.
struct ParsedReviewComment {
    location: SharedString,
    code: Option<SharedString>,
    comment: SharedString,
}

/// Parses a composed review block (`diff_review::compose_review_message`'s
/// output) back into its comments. The block is a header line, then per comment
/// a `` `path` line N: `` header, an optional fenced code quote, and the comment
/// text. This is a thin reader over the only public accessor
/// (`review_blocks`); it should be replaced by `diff_review::review_comment_blocks`
/// once that structured API is available.
fn parse_review_block(text: &str) -> Vec<ParsedReviewComment> {
    fn is_location_line(line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.starts_with('`') && trimmed.ends_with(':')
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut comments = Vec::new();
    let mut index = 0;

    // Skip the header and anything before the first comment.
    while index < lines.len() && !is_location_line(lines[index]) {
        index += 1;
    }

    while index < lines.len() {
        let location = lines[index].trim().trim_end_matches(':').replace('`', "");
        index += 1;

        // Optional fenced code quote.
        let mut code: Option<String> = None;
        if lines.get(index).map(|line| line.trim()) == Some("```") {
            index += 1;
            let mut body: Vec<&str> = Vec::new();
            while index < lines.len() && lines[index].trim() != "```" {
                body.push(lines[index]);
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            code = Some(body.join("\n"));
        }

        // The comment text runs until the next comment's location line.
        let mut body: Vec<&str> = Vec::new();
        while index < lines.len() && !is_location_line(lines[index]) {
            body.push(lines[index]);
            index += 1;
        }

        comments.push(ParsedReviewComment {
            location: location.trim().to_string().into(),
            code: code.map(Into::into),
            comment: body.join("\n").trim().to_string().into(),
        });
    }

    comments
}

/// The project panel's file-type icon for a tool call's locations: only when
/// the call is about exactly one file with an extension to key on.
fn file_icon_for_locations(
    locations: &[acp::ToolCallLocation],
    has_terminal: bool,
    cx: &App,
) -> Option<SharedString> {
    if has_terminal || locations.len() != 1 {
        return None;
    }
    let path = &locations.first()?.path;
    path.extension()?;
    FileIcons::get_icon(path, cx)
}

fn plan_entry_text(entry: &PlanEntry, cx: &App) -> SharedString {
    entry.content.read(cx).source().to_string().into()
}

/// Resolves each of a thread's work dirs to the worktree that hosts it, and
/// that worktree's branch. The most specific (longest) worktree path containing
/// a work dir wins, so a thread working in a linked worktree resolves to that
/// worktree's branch and never to the main checkout's or a sibling worktree's.
fn branches_for_thread_paths(
    thread_paths: &[PathBuf],
    worktree_branches: &[(PathBuf, String)],
) -> Vec<(PathBuf, String)> {
    let mut resolved: Vec<(PathBuf, String)> = Vec::new();
    for thread_path in thread_paths {
        let host = worktree_branches
            .iter()
            .filter(|(worktree_path, _)| thread_path.starts_with(worktree_path))
            .max_by_key(|(worktree_path, _)| worktree_path.components().count());
        if let Some((worktree_path, branch)) = host
            && !resolved
                .iter()
                .any(|(path, existing)| path == worktree_path && existing == branch)
        {
            resolved.push((worktree_path.clone(), branch.clone()));
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    fn box_height(width: u32, height: u32) -> f32 {
        image_box_height(Some(gpui::size(width, height)), IMAGE_CHIP_WIDTH).0
    }

    #[test]
    fn a_box_is_as_tall_as_its_picture_needs() {
        // A 16:9 screenshot at 24rem wide needs 13.5rem, not the 20rem the
        // fixed box used to reserve.
        assert_eq!(box_height(1920, 1080), 13.5);
        // A square one needs its full width back.
        assert_eq!(box_height(600, 600), 24.);
        // Twice as tall as wide is a phone screenshot, and still fits.
        assert_eq!(box_height(500, 1000), 32.);
    }

    #[test]
    fn a_very_tall_picture_is_capped_rather_than_endless() {
        // A full-page capture would want 240rem; it letterboxes instead.
        assert_eq!(box_height(400, 4000), 32.);
    }

    #[test]
    fn a_wide_strip_still_gets_a_row() {
        assert_eq!(box_height(4000, 100), 4.);
    }

    #[test]
    fn an_unmeasurable_picture_keeps_the_fixed_box() {
        assert_eq!(image_box_height(None, IMAGE_CHIP_WIDTH).0, IMAGE_CHIP_HEIGHT.0);
        // A zero dimension would divide by zero rather than describe a shape.
        assert_eq!(
            image_box_height(Some(gpui::size(0, 100)), IMAGE_CHIP_WIDTH).0,
            IMAGE_CHIP_HEIGHT.0
        );
    }

    #[track_caller]
    fn collapsed_label(command: &str, outcome: Option<&str>) -> CommandChipLabel {
        match ThreadView::collapsed_command(&CommandFacts::for_command(command), outcome) {
            CollapsedCommand::Label(label) => label,
            CollapsedCommand::Pieces(_) => panic!("expected one label for {command}"),
        }
    }

    #[track_caller]
    fn collapsed_pieces(command: &str) -> Vec<CommandChipPiece> {
        match ThreadView::collapsed_command(&CommandFacts::for_command(command), None) {
            CollapsedCommand::Pieces(pieces) => pieces,
            CollapsedCommand::Label(label) => {
                panic!("expected pieces for {command}, got {}", label.text)
            }
        }
    }

    #[test]
    fn a_long_run_of_actions_is_drawn_in_blocks() {
        // A run the list can afford to draw at once stays whole.
        assert_eq!(ThreadView::chunk_of_run(3, 10, 3), (3, 8));
        assert_eq!(ThreadView::chunk_of_run(3, 10, 10), (3, 8));

        // A longer one is split, and every entry of a block agrees on which
        // block it is in, so each block is drawn exactly once by its own first
        // entry.
        assert_eq!(ThreadView::chunk_of_run(0, 99, 0), (0, 48));
        assert_eq!(ThreadView::chunk_of_run(0, 99, 47), (0, 48));
        assert_eq!(ThreadView::chunk_of_run(0, 99, 48), (48, 48));
        assert_eq!(ThreadView::chunk_of_run(0, 99, 95), (48, 48));
        assert_eq!(ThreadView::chunk_of_run(0, 99, 96), (96, 4));

        // A run that does not start at the first entry splits from where it
        // starts, not from the top of the thread.
        assert_eq!(ThreadView::chunk_of_run(10, 109, 57), (10, 48));
        assert_eq!(ThreadView::chunk_of_run(10, 109, 58), (58, 48));
    }

    #[test]
    fn a_half_devshell_line_marks_the_acts_that_ran_in_it() {
        // The badge names mapper once; without the marks the reader cannot tell
        // which half of the line had that toolchain.
        let pieces = collapsed_pieces(
            "cd .. && nix develop .#mapper --command bash -c \
            'cd arcade && cargo fmt && cargo check' ; echo RUST_CLEAN; \
            cd portico && pnpm typecheck && pnpm lint",
        );
        assert_eq!(
            pieces
                .iter()
                .map(|piece| (piece.label.text.as_str(), piece.in_environment))
                .collect::<Vec<_>>(),
            [
                ("cargo fmt", true),
                ("cargo check", true),
                ("pnpm typecheck", false),
                ("pnpm lint", false),
            ]
        );

        // A line wholly inside one is not mixed, so nothing is marked: the
        // badge alone already covers every act.
        let whole = collapsed_pieces(
            "nix develop .#mapper --command bash -c 'cargo fmt && cargo check && cargo test'",
        );
        assert!(whole.iter().all(|piece| !piece.in_environment));
    }

    #[test]
    fn unsummarizable_chains_show_a_piece_per_act() {
        // Nothing to summarize: five unrelated builds.
        let pieces = collapsed_pieces(
            "cargo build -p one && cargo build -p two && cargo build -p three \
            && cargo build -p four && cargo build -p five",
        );
        let texts: Vec<&str> = pieces
            .iter()
            .map(|piece| piece.label.text.as_str())
            .collect();
        // Each act also names the crate it was pointed at (`cargo_names_the_
        // crates_it_was_pointed_at`), so the five pieces are distinguishable
        // by more than position.
        assert_eq!(
            texts,
            [
                "cargo build one",
                "cargo build two",
                "cargo build three",
                "cargo build four",
                "cargo build five",
            ],
            "every act, not the first few and a count of the rest"
        );
        // Every piece is a command in its own right, wearing Rust's own icon.
        for piece in &pieces {
            assert_eq!(piece.glyph, ChipGlyph::Language("rust"));
            assert_eq!(piece.label.commands, vec![0..piece.label.text.len()]);
        }

        // A chain of different acts: each keeps its own glyph.
        let mixed = collapsed_pieces("python3 gen.py && cargo test && rg TODO src");
        assert_eq!(
            mixed
                .iter()
                .map(|piece| piece.glyph.clone())
                .collect::<Vec<_>>(),
            [
                ChipGlyph::Language("python"),
                ChipGlyph::Language("rust"),
                ChipGlyph::Icon(IconName::MagnifyingGlass),
            ]
        );
        assert_eq!(mixed[0].label.text, "python3 gen.py");

        // A summary that quotes the command is shell; the outcome after it is
        // not, so it stays out of the highlighted range.
        let summarized = collapsed_label("pnpm lint", Some("3 errors"));
        assert_eq!(summarized.text, "pnpm lint · 3 errors");
        assert_eq!(summarized.commands, vec![0.."pnpm lint".len()]);

        // A summary that describes the line is prose: none of it is shell.
        let described = collapsed_label(
            "git show HEAD:a.ts | sed -n '1,20p'; git show HEAD:b.ts | sed -n '1,20p'",
            None,
        );
        assert_eq!(described.text, "Read 2 files at HEAD");
        assert!(described.commands.is_empty());

        // A single command keeps the full width, and all of it is shell.
        let single = collapsed_label("cargo build --release", None);
        assert_eq!(single.text, "cargo build --release");
        assert_eq!(single.commands, vec![0..single.text.len()]);
    }

    fn test_tool_call(
        id: &str,
        title: &str,
        kind: acp::ToolKind,
        tool_name: Option<&str>,
        cx: &mut App,
    ) -> AgentThreadEntry {
        AgentThreadEntry::ToolCall(ToolCall {
            id: acp::ToolCallId::new(id),
            label: cx.new(|cx| Markdown::new(title.to_string().into(), None, None, cx)),
            kind,
            content: Vec::new(),
            status: ToolCallStatus::Completed,
            locations: Vec::new(),
            resolved_locations: Vec::new(),
            raw_input: None,
            raw_input_markdown: None,
            raw_output: None,
            tool_name: tool_name.map(Into::into),
            subagent_session_info: None,
            sandbox_authorization_details: None,
            sandbox_fallback_authorization_details: None,
            sandbox_not_applied: None,
        })
    }

    #[gpui::test]
    fn chip_facts_are_computed_once_per_command(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let AgentThreadEntry::ToolCall(mut tool_call) = test_tool_call(
                "1",
                "```bash\nrm -rf build\n```",
                acp::ToolKind::Execute,
                None,
                cx,
            ) else {
                unreachable!()
            };
            // Only a call with a terminal is a command; give it one the way an
            // agent does, so the chip path treats it as such.
            let cache = ChipCache::default();

            let facts = cache.command(&tool_call, cx);
            assert!(facts.destructive, "rm -rf is destructive");
            assert_eq!(facts.command, "rm -rf build");

            // Asking again reuses the same answer rather than reparsing: this
            // is the whole point, since rendering asks on every frame.
            assert!(Rc::ptr_eq(&facts, &cache.command(&tool_call, cx)));

            // A command that changes is parsed again.
            tool_call.label = cx.new(|cx| {
                Markdown::new(
                    "```bash\ncargo build\n```".to_string().into(),
                    None,
                    None,
                    cx,
                )
            });
            let rebuilt = cache.command(&tool_call, cx);
            assert!(!Rc::ptr_eq(&facts, &rebuilt));
            assert!(!rebuilt.destructive);
            assert_eq!(rebuilt.command, "cargo build");
        });
    }

    #[gpui::test]
    fn images_keep_their_own_chip(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let read = |id: &str, path: &str, cx: &mut App| {
                let AgentThreadEntry::ToolCall(mut tool_call) =
                    test_tool_call(id, path, acp::ToolKind::Read, Some("Read"), cx)
                else {
                    unreachable!()
                };
                tool_call.locations = vec![acp::ToolCallLocation::new(PathBuf::from(path))];
                AgentThreadEntry::ToolCall(tool_call)
            };

            // A screenshot outside the project, amid reads that would
            // otherwise all fold into one summary chip.
            let entries = vec![
                read("1", "src/main.rs", cx),
                read("2", "src/lib.rs", cx),
                read("3", "/tmp/screenshot.png", cx),
                read("4", "src/other.rs", cx),
                read("5", "src/more.rs", cx),
            ];

            let AgentThreadEntry::ToolCall(image_call) = &entries[2] else {
                unreachable!()
            };
            assert!(
                ThreadView::tool_call_has_image(image_call, cx),
                "a png a call read is a picture wherever it lives"
            );

            // The image keeps a chip of its own, and the reads around it
            // collapse with each other rather than swallowing it.
            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 5, cx),
                vec![
                    ActionChip::Collapsed {
                        entry_ixs: vec![0, 1]
                    },
                    ActionChip::ToolCall { entry_ix: 2 },
                    ActionChip::Collapsed {
                        entry_ixs: vec![3, 4]
                    },
                ]
            );
        });
    }

    /// An edit tool call whose locations name the given files (no diffs, so the
    /// chip split is exercised from locations alone).
    /// An edit call the way a patch-sending agent (Codex) reports it: diffs
    /// that name the files, and no locations at all.
    fn test_patch_tool_call(id: &str, files: &[&str], cx: &mut App) -> AgentThreadEntry {
        let AgentThreadEntry::ToolCall(mut tool_call) =
            test_tool_call(id, "editing files", acp::ToolKind::Edit, None, cx)
        else {
            unreachable!()
        };
        let language_registry = Arc::new(language::LanguageRegistry::test(
            cx.background_executor().clone(),
        ));
        tool_call.content = files
            .iter()
            .map(|path| {
                acp_thread::ToolCallContent::Diff(cx.new(|cx| {
                    acp_thread::Diff::finalized(
                        path.to_string(),
                        Some("old\n".to_string()),
                        "new\n".to_string(),
                        language_registry.clone(),
                        cx,
                    )
                }))
            })
            .collect();
        AgentThreadEntry::ToolCall(tool_call)
    }

    fn test_edit_tool_call(id: &str, files: &[&str], cx: &mut App) -> AgentThreadEntry {
        let AgentThreadEntry::ToolCall(mut tool_call) =
            test_tool_call(id, "editing files", acp::ToolKind::Edit, None, cx)
        else {
            unreachable!()
        };
        tool_call.locations = files
            .iter()
            .map(|path| acp::ToolCallLocation::new(PathBuf::from(path)))
            .collect();
        AgentThreadEntry::ToolCall(tool_call)
    }

    /// An assistant message of the given chunks, `(text, is_thought)`.
    fn test_assistant_message(chunks: &[(&str, bool)], cx: &mut App) -> AgentThreadEntry {
        let chunks = chunks
            .iter()
            .map(|(text, is_thought)| {
                let block = acp_thread::ContentBlock::Markdown {
                    markdown: cx.new(|cx| Markdown::new(text.to_string().into(), None, None, cx)),
                };
                if *is_thought {
                    AssistantMessageChunk::Thought { id: None, block }
                } else {
                    AssistantMessageChunk::Message { id: None, block }
                }
            })
            .collect();

        AgentThreadEntry::AssistantMessage(AssistantMessage {
            chunks,
            indented: false,
            is_subagent_output: false,
        })
    }

    #[gpui::test]
    fn thinking_is_never_a_transcript_chip(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let entries = vec![
                test_tool_call("1", "Read foo.rs", acp::ToolKind::Read, None, cx),
                test_assistant_message(&[("First thought.", true), ("Second thought.", true)], cx),
                test_tool_call("2", "Edited foo.rs", acp::ToolKind::Edit, None, cx),
                // Prose ends the run, thoughts of its own or not.
                test_assistant_message(&[("A thought.", true), ("The answer.", false)], cx),
                test_tool_call("3", "Read bar.rs", acp::ToolKind::Read, None, cx),
            ];

            // Thinking is shown beside the progress indicator while it happens,
            // and nowhere else: it draws no chip and is not transcript content.
            // It stays part of the run so it does not split the chips around it.
            assert!(ThreadView::is_thoughts_only_message(&entries[1], cx));
            assert!(
                !ThreadView::is_thoughts_only_message(&entries[3], cx),
                "a message that says something is transcript prose, thoughts or not"
            );

            // A thoughts-only message does not end the run of chips around it.
            assert_eq!(
                ThreadView::action_run_bounds_in(&entries, 0, cx),
                Some((0, 3))
            );
            assert_eq!(ThreadView::action_run_bounds_in(&entries, 3, cx), None);

            // Only the live thought is shown, and only while generating.
            assert_eq!(
                ThreadView::active_area_entry(&entries[..2], true, cx),
                Some(1)
            );
            assert_eq!(
                ThreadView::active_area_entry(&entries[..2], false, cx),
                None
            );

            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 3, cx),
                vec![
                    // A lone read keeps its own chip (two or more collapse into
                    // a summary); the fileless edit says nothing worth a chip
                    // and draws none until its per-file chips arrive.
                    ActionChip::ToolCall { entry_ix: 0 },
                ]
            );
        });
    }

    #[test]
    fn review_blocks_are_detected_and_parsed_for_display() {
        let text = "I reviewed the changes and left 2 review comments below. Please address them.\n\
             \n\
             `src/main.rs` lines 10-12:\n\
             ```\n\
             fn main() {\n    todo!()\n}\n\
             ```\n\
             This needs an implementation.\n\
             \n\
             `src/lib.rs` line 3:\n\
             ```\n\
             pub struct Foo;\n\
             ```\n\
             Rename this.\n";

        // The detector the sent-message bubble AND the queued-message row share:
        // both call `review_blocks` on their content, so both recognize this.
        let block = acp::ContentBlock::Text(acp::TextContent::new(text.to_string()));
        let content = vec![block];
        let detected = crate::diff_review::review_blocks(&content);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].0, 2, "the block reports its comment count");

        // A review-only message has no typed text left once the review block is
        // dropped: that is what makes the queued row show the visual, not text.
        assert!(crate::diff_review::without_review_blocks(content).is_empty());

        // The composed text parses back into per-comment blocks for display.
        let comments = parse_review_block(text);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].location.as_ref(), "src/main.rs lines 10-12");
        assert_eq!(
            comments[0].code.as_deref(),
            Some("fn main() {\n    todo!()\n}")
        );
        assert_eq!(
            comments[0].comment.as_ref(),
            "This needs an implementation."
        );
        assert_eq!(comments[1].location.as_ref(), "src/lib.rs line 3");
        assert_eq!(comments[1].code.as_deref(), Some("pub struct Foo;"));
        assert_eq!(comments[1].comment.as_ref(), "Rename this.");
    }

    #[gpui::test]
    fn multi_file_edits_split_into_one_chip_per_file(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let entries = vec![
                test_edit_tool_call("1", &["/project/src/main.rs"], cx),
                // Three locations, two distinct files: one chip per distinct file.
                test_edit_tool_call(
                    "2",
                    &["/project/a.rs", "/project/b.rs", "/project/a.rs"],
                    cx,
                ),
            ];

            // A single-file edit is one chip, named after its file rather than
            // after the call's (possibly generic) title.
            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 1, cx),
                vec![ActionChip::EditFile {
                    entry_ix: 0,
                    file_ix: 0
                }]
            );

            // A multi-file edit splits into one chip per distinct file.
            assert_eq!(
                ThreadView::action_chips_in(&entries, 1, 1, cx),
                vec![
                    ActionChip::EditFile {
                        entry_ix: 1,
                        file_ix: 0
                    },
                    ActionChip::EditFile {
                        entry_ix: 1,
                        file_ix: 1
                    },
                ]
            );

            let AgentThreadEntry::ToolCall(multi) = &entries[1] else {
                unreachable!()
            };
            // The two chips map to the first occurrence of each distinct file.
            assert_eq!(
                ThreadView::edited_files(multi, cx)
                    .into_iter()
                    .map(|file| file.location_ix)
                    .collect::<Vec<_>>(),
                vec![Some(0), Some(1)]
            );
        });
    }

    #[gpui::test]
    fn patch_edits_split_by_their_diffs_when_no_locations_are_reported(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            // Codex sends a patch: diffs name the files, locations are empty.
            // Keying the split on locations alone left this one "files" chip,
            // which is the shape that kept surviving.
            let entries = vec![test_patch_tool_call(
                "1",
                &["/project/a.rs", "/project/b.rs"],
                cx,
            )];
            let AgentThreadEntry::ToolCall(patch) = &entries[0] else {
                unreachable!()
            };
            assert!(
                patch.locations.is_empty(),
                "this is the no-locations shape the fix is about"
            );

            let files = ThreadView::edited_files(patch, cx);
            assert_eq!(
                files
                    .iter()
                    .map(|file| file.path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                vec!["/project/a.rs".to_string(), "/project/b.rs".to_string()],
                "the files come from the diffs when the call reports no locations"
            );
            assert!(files.iter().all(|file| file.location_ix.is_none()));

            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 1, cx),
                vec![
                    ActionChip::EditFile {
                        entry_ix: 0,
                        file_ix: 0
                    },
                    ActionChip::EditFile {
                        entry_ix: 0,
                        file_ix: 1
                    },
                ]
            );
        });
    }

    #[gpui::test]
    fn in_progress_tail_is_left_to_the_active_area(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            // A still-streaming thoughts-only tail belongs to the active area
            // while the turn is generating, and to the transcript once it ends.
            let entries = vec![
                test_tool_call("1", "Read foo.rs", acp::ToolKind::Read, None, cx),
                test_assistant_message(&[("Still thinking.", true)], cx),
            ];
            assert_eq!(ThreadView::active_area_entry(&entries, true, cx), Some(1));
            assert_eq!(ThreadView::active_area_entry(&entries, false, cx), None);

            // A thought that is not the tail has settled: never skipped.
            let entries = vec![
                test_assistant_message(&[("A thought.", true)], cx),
                test_tool_call("1", "Read foo.rs", acp::ToolKind::Read, None, cx),
            ];
            assert_eq!(ThreadView::active_area_entry(&entries, true, cx), None);

            // A tail that says something is real transcript, not in progress.
            let entries = vec![test_assistant_message(&[("The answer.", false)], cx)];
            assert_eq!(ThreadView::active_area_entry(&entries, true, cx), None);
        });
    }

    #[gpui::test]
    fn blank_thoughts_are_not_chips(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let entries = vec![test_assistant_message(&[("  \n", true)], cx)];
            assert!(!ThreadView::is_chip_entry(&entries[0], cx));
            assert_eq!(ThreadView::action_run_bounds_in(&entries, 0, cx), None);
        });
    }

    #[gpui::test]
    fn wait_calls_are_not_chips(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        cx.update(|cx| {
            let wait = |id: &str, cx: &mut App| {
                test_tool_call(id, "Waiting", acp::ToolKind::Other, Some("wait"), cx)
            };
            let entries = vec![
                wait("1", cx),
                wait("2", cx),
                wait("3", cx),
                test_tool_call("4", "Read foo.rs", acp::ToolKind::Read, None, cx),
                wait("5", cx),
                test_tool_call("6", "Read bar.rs", acp::ToolKind::Read, None, cx),
            ];

            // Waiting is not an action worth reporting, so it draws nothing at
            // all; the calls around it stay in one run rather than being split
            // into separate groups by the hidden waits.
            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 6, cx),
                // The two reads fold into one summary chip.
                vec![ActionChip::Collapsed {
                    entry_ixs: vec![3, 5]
                }]
            );

            // Reads separated by a real command do not fold across it: only
            // consecutive stretches collapse.
            let entries = vec![
                test_tool_call("1", "Read foo.rs", acp::ToolKind::Read, None, cx),
                test_tool_call("2", "Read baz.rs", acp::ToolKind::Read, None, cx),
                test_tool_call("3", "Ran a build", acp::ToolKind::Execute, None, cx),
                test_tool_call("4", "Read bar.rs", acp::ToolKind::Read, None, cx),
            ];
            assert_eq!(
                ThreadView::action_chips_in(&entries, 0, 4, cx),
                vec![
                    ActionChip::Collapsed {
                        entry_ixs: vec![0, 1]
                    },
                    ActionChip::ToolCall { entry_ix: 2 },
                    ActionChip::ToolCall { entry_ix: 3 },
                ]
            );

            // A run of nothing but waits produces no chips.
            let entries = vec![wait("1", cx), wait("2", cx)];
            assert_eq!(ThreadView::action_chips_in(&entries, 0, 2, cx), vec![]);
        });
    }

    #[test]
    fn thought_summary_says_what_the_agent_thought() {
        assert_eq!(
            ThreadView::thought_summary("The user wants a chip. Then they want a grid."),
            "The user wants a chip",
            "the chip shows one sentence, not the paragraph"
        );
        assert_eq!(
            ThreadView::thought_summary("**Planning the fix**\n\nMore detail here."),
            "Planning the fix",
            "markdown decoration is not part of the summary"
        );
        assert_eq!(
            ThreadView::thought_summary("\n\n# Reading the code\nmore"),
            "Reading the code"
        );
        assert_eq!(ThreadView::thought_summary("   \n "), "Thinking");

        let long = "a".repeat(40) + " " + &"b".repeat(40);
        let summary = ThreadView::thought_summary(&long);
        assert!(
            summary.ends_with('…'),
            "a long thought truncates: {summary}"
        );
        assert!(summary.chars().count() <= 65);
    }

    #[gpui::test]
    fn action_chips_use_file_type_icons(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let location = |path: &str| acp::ToolCallLocation::new(PathBuf::from(path));

        cx.update(|cx| {
            let rust = file_icon_for_locations(&[location("/project/src/main.rs")], false, cx)
                .expect("a Rust file has a file-type icon");
            let markdown = file_icon_for_locations(&[location("/project/README.md")], false, cx)
                .expect("a markdown file has a file-type icon");
            assert_ne!(
                rust, markdown,
                "the icon is keyed on the path's extension, like the project panel"
            );

            // Terminals, multi-file calls, and calls with no location keep the
            // tool-kind icon.
            assert_eq!(
                file_icon_for_locations(&[location("/project/src/main.rs")], true, cx),
                None
            );
            assert_eq!(
                file_icon_for_locations(
                    &[location("/project/src/main.rs"), location("/project/b.rs")],
                    false,
                    cx
                ),
                None
            );
            assert_eq!(file_icon_for_locations(&[], false, cx), None);
            assert_eq!(
                file_icon_for_locations(&[location("/project/Makefile")], false, cx),
                None,
                "a path with no extension has nothing to key on"
            );
        });
    }

    #[test]
    fn thread_branches_are_scoped_to_the_threads_own_worktree() {
        let worktree_branches = vec![
            (PathBuf::from("/repo"), "main".to_string()),
            (PathBuf::from("/repo/wt/mine"), "mine".to_string()),
            (PathBuf::from("/repo/wt/other"), "other".to_string()),
        ];

        // A thread in a linked worktree resolves only to that worktree's
        // branch, even though the main checkout is an ancestor of it and the
        // sibling worktree is a peer.
        assert_eq!(
            branches_for_thread_paths(&[PathBuf::from("/repo/wt/mine")], &worktree_branches),
            vec![(PathBuf::from("/repo/wt/mine"), "mine".to_string())]
        );

        // A subdirectory of a worktree still resolves to that worktree.
        assert_eq!(
            branches_for_thread_paths(
                &[PathBuf::from("/repo/wt/mine/crates/agent_ui")],
                &worktree_branches
            ),
            vec![(PathBuf::from("/repo/wt/mine"), "mine".to_string())]
        );

        // A thread in the main checkout gets the main branch only.
        assert_eq!(
            branches_for_thread_paths(&[PathBuf::from("/repo")], &worktree_branches),
            vec![(PathBuf::from("/repo"), "main".to_string())]
        );

        // A path outside every known worktree resolves to nothing.
        assert!(
            branches_for_thread_paths(&[PathBuf::from("/elsewhere")], &worktree_branches)
                .is_empty()
        );
    }

    #[test]
    fn thread_branches_dedupe_repeated_work_dirs() {
        let worktree_branches = vec![(PathBuf::from("/repo"), "main".to_string())];
        assert_eq!(
            branches_for_thread_paths(
                &[
                    PathBuf::from("/repo"),
                    PathBuf::from("/repo/crates"),
                    PathBuf::from("/repo/assets"),
                ],
                &worktree_branches
            ),
            vec![(PathBuf::from("/repo"), "main".to_string())]
        );
    }

    fn native_command(name: &str) -> acp::AvailableCommand {
        acp::AvailableCommand::new(name, "").meta(acp_thread::meta_with_command_category(
            acp_thread::CommandCategory::Native,
        ))
    }

    fn mcp_command(name: &str) -> acp::AvailableCommand {
        acp::AvailableCommand::new(name, "").meta(acp_thread::meta_with_command_category(
            acp_thread::CommandCategory::Mcp,
        ))
    }

    #[test]
    fn test_leading_native_command_matches_bare_and_with_remainder() {
        let commands = [native_command("compact"), mcp_command("deploy")];

        // Native command with trailing text.
        assert_eq!(
            leading_native_command("/compact summarize the API work", &commands),
            Some("compact".to_string())
        );
        // Leading/trailing whitespace is tolerated.
        assert_eq!(
            leading_native_command("  /compact   do x  ", &commands),
            Some("compact".to_string())
        );

        // Bare native command (no remainder) is still recognized, so it runs as
        // a command turn (without echoing a user message) rather than being sent
        // to the model as a normal prompt.
        assert_eq!(
            leading_native_command("/compact", &commands),
            Some("compact".to_string())
        );
        assert_eq!(
            leading_native_command("/compact   ", &commands),
            Some("compact".to_string())
        );

        // MCP/ACP commands are not native: their trailing text is a real
        // argument the agent consumes, and they echo as normal user messages.
        assert_eq!(leading_native_command("/deploy prod", &commands), None);
        assert_eq!(leading_native_command("/deploy", &commands), None);

        // Unknown command, or not a slash command at all.
        assert_eq!(leading_native_command("/unknown foo", &commands), None);
        assert_eq!(leading_native_command("just a message", &commands), None);
    }

    #[test]
    fn test_strip_leading_command() {
        assert_eq!(strip_leading_command("/compact do x", "compact"), "do x");
        assert_eq!(
            strip_leading_command("  /compact  do x ", "compact"),
            "do x "
        );
        // No matching prefix: returns the trimmed input unchanged.
        assert_eq!(strip_leading_command("hello", "compact"), "hello");
    }

    #[test]
    fn test_strip_edit_verb() {
        assert_eq!(
            ThreadView::strip_edit_verb("Edit crates/ui/src/label.rs"),
            "crates/ui/src/label.rs"
        );
        assert_eq!(
            ThreadView::strip_edit_verb("Wrote src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(
            ThreadView::strip_edit_verb("Created src/main.rs"),
            "src/main.rs"
        );
        // Nothing to strip: the path is the whole label.
        assert_eq!(ThreadView::strip_edit_verb("src/main.rs"), "src/main.rs");
        // A path that merely starts with the letters of a verb is untouched.
        assert_eq!(ThreadView::strip_edit_verb("editor.rs"), "editor.rs");
    }

    #[gpui::test]
    async fn test_tool_call_diff_opens_with_the_calls_hunks(cx: &mut gpui::TestAppContext) {
        use crate::tool_call_diff::{ToolCallDiff, ToolCallDiffKey, open_tool_call_diff};

        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({"src": {"main.rs": "first\nCHANGED\nthird\n"}}),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let key = ToolCallDiffKey {
            tool_call_id: acp::ToolCallId::new("call-1"),
            path: path!("/project/src/main.rs").into(),
        };
        multi_workspace.update_in(cx, |_, window, cx| {
            open_tool_call_diff(
                key.clone(),
                // What the file said before the call edited it.
                "first\nsecond\nthird\n".into(),
                project.clone(),
                workspace.downgrade(),
                window,
                cx,
            )
            .detach();
        });
        cx.run_until_parked();

        let diff = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ToolCallDiff>(cx)
                .next()
                .expect("clicking a file chip opens its diff")
        });
        diff.read_with(cx, |diff, cx| {
            assert!(
                !diff.multibuffer().read(cx).is_empty(),
                "the diff shows the call's change, not an empty view"
            );
            let hunks = diff
                .multibuffer()
                .read(cx)
                .snapshot(cx)
                .diff_hunks()
                .count();
            assert_eq!(hunks, 1, "one changed line is one hunk");
        });

        // Opening the same chip again activates the same tab rather than
        // stacking another copy of it.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_tool_call_diff(
                key,
                "first\nsecond\nthird\n".into(),
                project.clone(),
                workspace.downgrade(),
                window,
                cx,
            )
            .detach();
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ToolCallDiff>(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn test_open_link_bare_path(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({"src": {"main.rs": "first\nsecond\nthird\n"}}),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        // Relative path — call from multi_workspace so the inner workspace entity is not locked
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link("src/main.rs".into(), &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("file should be open");
            assert!(*active.path == *"src/main.rs");
        });

        multi_workspace.update_in(cx, |_, window, cx| {
            open_link("src/main.rs#L2".into(), &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("file should be open in an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });

        // Absolute path
        let abs_path: SharedString = path!("/project/src/main.rs").to_string().into();
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(abs_path, &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("file should be open");
            assert!(*active.path == *"src/main.rs");
        });
    }

    #[gpui::test]
    async fn test_open_link_html_file_opens_in_the_browser(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({"report.html": "<h1>hi</h1>", "notes.md": "one"}),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                path!("/project/report.html").to_string().into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            cx.opened_url().as_deref(),
            Some(
                url::Url::from_file_path(path!("/project/report.html"))
                    .unwrap()
                    .as_str()
            ),
            "an HTML report should open in the browser"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.active_item(cx).is_none(),
                "an HTML report should not open as a buffer"
            );
        });

        // Every other file still opens in the editor.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                path!("/project/notes.md").to_string().into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("a markdown file should open in the editor");
            assert!(*active.path == *"notes.md");
        });
    }

    #[gpui::test]
    async fn test_open_link_percent_escape_disambiguation(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "a%20b.rs": "literal\nsecond\n",
                "a b.rs": "decoded\nsecond\n",
                "c d.rs": "first\nsecond\n",
                "e%20f.rs": "first\nsecond\n",
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let open_link_and_active_path = |url: String, cx: &mut gpui::VisualTestContext| {
            multi_workspace.update_in(cx, |_, window, cx| {
                open_link(url.into(), &workspace_weak, window, cx);
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .expect("file should be open")
                    .path
            })
        };

        // Both interpretations exist: the decoded one wins.
        let path = open_link_and_active_path(path!("/project/a%20b.rs").to_string(), cx);
        assert_eq!(*path, *"a b.rs");

        // Only the decoded file exists.
        let path = open_link_and_active_path(path!("/project/c%20d.rs").to_string(), cx);
        assert_eq!(*path, *"c d.rs");

        // Only the literally-named file exists: fall back to it.
        let path = open_link_and_active_path(path!("/project/e%20f.rs").to_string(), cx);
        assert_eq!(*path, *"e%20f.rs");

        let path = open_link_and_active_path("a%20b.rs#L2".to_string(), cx);
        assert_eq!(*path, *"a b.rs");

        let path = open_link_and_active_path("c%20d.rs#L2".to_string(), cx);
        assert_eq!(*path, *"c d.rs");

        let path = open_link_and_active_path("e%20f.rs#L2".to_string(), cx);
        assert_eq!(*path, *"e%20f.rs");
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("file should be open in an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });
    }

    #[gpui::test]
    async fn test_open_link_out_of_project_path(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({"src": {"main.rs": ""}}))
            .await;
        fs.insert_tree(path!("/outside"), json!({"notes.md": "one\ntwo\nthree\n"}))
            .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        // A nonexistent out-of-project path opens nothing, not even an
        // empty buffer.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                path!("/outside/missing.md").to_string().into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.active_item(cx).is_none(),
                "nothing should open for a nonexistent path"
            );
        });

        // An existing out-of-project file opens at the linked line.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                format!("{}:2", path!("/outside/notes.md")).into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            let item = workspace.active_item(cx).expect("file should be open");
            let project_path = item.project_path(cx).expect("item should have a path");
            let abs_path = workspace
                .project()
                .read(cx)
                .absolute_path(&project_path, cx);
            assert_eq!(
                abs_path.as_deref(),
                Some(Path::new(path!("/outside/notes.md")))
            );
            item.downcast::<Editor>().expect("should be an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });
    }
}

const FAST_MODE_WARNING_NAMESPACE: &str = "fast-mode-warning-dismissed";

fn fast_mode_warning_id(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
) -> String {
    format!("{}:{}", provider_id.0, model_id.0)
}

fn fast_mode_warning_dismissed(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
    cx: &App,
) -> bool {
    KeyValueStore::global(cx)
        .scoped(FAST_MODE_WARNING_NAMESPACE)
        .read(&fast_mode_warning_id(provider_id, model_id))
        .log_err()
        .flatten()
        .is_some()
}

fn set_fast_mode_warning_dismissed(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
    cx: &mut App,
) {
    let key = fast_mode_warning_id(provider_id, model_id);
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(FAST_MODE_WARNING_NAMESPACE)
            .write(key, "1".to_string())
            .await
            .log_err();
    })
    .detach();
}

pub(crate) fn reset_fast_mode_warnings(cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(FAST_MODE_WARNING_NAMESPACE)
            .delete_all()
            .await
            .log_err();
    })
    .detach();
}
