use std::{ops::Range, sync::Arc};

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk, ToolCall};
use agent::ThreadStore;
use agent_client_protocol::schema::v1 as acp_v1;
use agent_settings::AgentSettings;
use collections::{HashMap, HashSet};
use editor::{
    Editor, EditorEvent, EditorMode, HiddenUnstagedDiffHunkRenderer, MinimapVisibility,
    SizingBehavior,
};
use gpui::{
    AnyEntity, App, AppContext as _, Corners, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, ScrollHandle, TextStyleRefinement, WeakEntity, Window,
};
use language::language_settings::SoftWrap;
use multi_buffer::MultiBuffer;
use project::{AgentId, Project, project_settings::DiagnosticSeverity};
use rope::Point;
use settings::{Settings as _, ThinkingBlockDisplay};
use terminal_view::TerminalView;
use theme_settings::ThemeSettings;
use ui::{Context, TextSize};
use workspace::Workspace;

use crate::message_editor::{MessageEditor, MessageEditorEvent, SharedSessionCapabilities};

/// Maps an entry index through the removal of `removed` (a contiguous range of
/// entries), returning `None` if the index referred to a removed entry.
fn reindex_after_removal(index: usize, removed: &Range<usize>) -> Option<usize> {
    if index < removed.start {
        Some(index)
    } else if index < removed.end {
        None
    } else {
        Some(index - removed.len())
    }
}

pub struct EntryViewState {
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    thread_store: Option<Entity<ThreadStore>>,
    entries: Vec<Entry>,
    session_capabilities: SharedSessionCapabilities,
    agent_id: AgentId,
    expanded_thinking_blocks: HashSet<(usize, usize)>,
    auto_expanded_thinking_block: Option<(usize, usize)>,
    user_toggled_thinking_blocks: HashSet<(usize, usize)>,
    expanded_compactions: HashSet<usize>,
    expanded_tool_calls: HashSet<acp_v1::ToolCallId>,
    user_collapsed_tool_calls: HashSet<acp_v1::ToolCallId>,
    /// Whether `entries` holds this thread's views. A thread that has been off
    /// screen long enough drops them (see `ThreadView::drop_entry_views`) and
    /// builds them again from the same entries when it comes back, so the
    /// threads left open all day cost their view tree only while someone is
    /// looking at one. Everything else here is keyed by entry index or tool
    /// call id rather than by view, so it survives the drop and the thread
    /// returns with its expansions intact.
    views_built: bool,
}

impl EntryViewState {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
        thread_store: Option<Entity<ThreadStore>>,
        session_capabilities: SharedSessionCapabilities,
        agent_id: AgentId,
    ) -> Self {
        Self {
            workspace,
            project,
            thread_store,
            entries: Vec::new(),
            session_capabilities,
            agent_id,
            expanded_thinking_blocks: HashSet::default(),
            auto_expanded_thinking_block: None,
            user_toggled_thinking_blocks: HashSet::default(),
            expanded_compactions: HashSet::default(),
            expanded_tool_calls: HashSet::default(),
            user_collapsed_tool_calls: HashSet::default(),
            views_built: true,
        }
    }

    pub fn views_are_built(&self) -> bool {
        self.views_built
    }

    /// Drops every per-entry view. The caller is responsible for the list state
    /// that holds their focus handles.
    pub fn drop_views(&mut self) {
        self.entries.clear();
        self.views_built = false;
    }

    /// Opens the gate `sync_entry` closes while the views are dropped. Called
    /// once by the rebuild, immediately before it syncs every entry.
    pub fn mark_views_built(&mut self) {
        self.views_built = true;
    }

    pub(crate) fn is_tool_call_expanded(&self, tool_call_id: &acp_v1::ToolCallId) -> bool {
        self.expanded_tool_calls.contains(tool_call_id)
    }

    pub(crate) fn is_tool_call_content_visible(&self, tool_call: &ToolCall) -> bool {
        self.is_tool_call_expanded(&tool_call.id) || tool_call.authorization_id().is_some()
    }

    pub(crate) fn expand_tool_call(&mut self, tool_call_id: acp_v1::ToolCallId) {
        self.expanded_tool_calls.insert(tool_call_id);
    }

    pub(crate) fn collapse_tool_call(&mut self, tool_call_id: &acp_v1::ToolCallId) {
        self.expanded_tool_calls.remove(tool_call_id);
    }

    pub(crate) fn toggle_tool_call_expansion(&mut self, tool_call_id: &acp_v1::ToolCallId) {
        if !self.expanded_tool_calls.remove(tool_call_id) {
            self.expanded_tool_calls.insert(tool_call_id.clone());
        }
    }

    /// Whether the user explicitly collapsed this tool call, overriding any
    /// auto-expansion (e.g. a failed terminal command opening its output).
    pub(crate) fn is_tool_call_user_collapsed(&self, tool_call_id: &acp_v1::ToolCallId) -> bool {
        self.user_collapsed_tool_calls.contains(tool_call_id)
    }

    pub(crate) fn set_tool_call_expanded(
        &mut self,
        tool_call_id: &acp_v1::ToolCallId,
        expanded: bool,
    ) {
        if expanded {
            self.expanded_tool_calls.insert(tool_call_id.clone());
            self.user_collapsed_tool_calls.remove(tool_call_id);
        } else {
            self.expanded_tool_calls.remove(tool_call_id);
            self.user_collapsed_tool_calls.insert(tool_call_id.clone());
        }
    }

    pub(crate) fn is_compaction_expanded(&self, entry_ix: usize) -> bool {
        self.expanded_compactions.contains(&entry_ix)
    }

    pub(crate) fn collapse_compaction(&mut self, entry_ix: usize) {
        self.expanded_compactions.remove(&entry_ix);
    }

    pub(crate) fn toggle_compaction_expansion(&mut self, entry_ix: usize) {
        if !self.expanded_compactions.remove(&entry_ix) {
            self.expanded_compactions.insert(entry_ix);
        }
    }

    pub(crate) fn clear_auto_expand_tracking(&mut self) {
        self.auto_expanded_thinking_block = None;
    }

    pub(crate) fn auto_expand_streaming_thought(&mut self, thread: &AcpThread, cx: &App) -> bool {
        let thinking_display = AgentSettings::get_global(cx).thinking_display;

        if !matches!(
            thinking_display,
            ThinkingBlockDisplay::Auto | ThinkingBlockDisplay::Preview
        ) {
            return false;
        }

        let last_ix = thread.entries().len().saturating_sub(1);
        let key = match thread.entries().get(last_ix) {
            Some(AgentThreadEntry::AssistantMessage(message)) => match message.chunks.last() {
                Some(AssistantMessageChunk::Thought { .. }) => {
                    Some((last_ix, message.chunks.len() - 1))
                }
                _ => None,
            },
            _ => None,
        };

        if let Some(key) = key {
            if self.auto_expanded_thinking_block != Some(key) {
                self.auto_expanded_thinking_block = Some(key);
                self.expanded_thinking_blocks.insert(key);
                return true;
            }
        } else if self.auto_expanded_thinking_block.is_some() {
            if thinking_display == ThinkingBlockDisplay::Auto
                && let Some(key) = self.auto_expanded_thinking_block
                && !self.user_toggled_thinking_blocks.contains(&key)
            {
                self.expanded_thinking_blocks.remove(&key);
            }
            self.auto_expanded_thinking_block = None;
            return true;
        }

        false
    }

    // Thoughts no longer expand in the transcript (their full text is a hover
    // card), so nothing in production toggles them; the thread-search test still
    // drives this to assert expanded thinking content is searchable.
    #[cfg(test)]
    pub(crate) fn toggle_thinking_block_expansion(&mut self, key: (usize, usize), cx: &App) {
        match AgentSettings::get_global(cx).thinking_display {
            ThinkingBlockDisplay::Auto => {
                let is_open = self.expanded_thinking_blocks.contains(&key)
                    || self.user_toggled_thinking_blocks.contains(&key);

                if is_open {
                    self.expanded_thinking_blocks.remove(&key);
                    self.user_toggled_thinking_blocks.remove(&key);
                } else {
                    self.expanded_thinking_blocks.insert(key);
                    self.user_toggled_thinking_blocks.insert(key);
                }
            }
            ThinkingBlockDisplay::Preview => {
                let is_user_expanded = self.user_toggled_thinking_blocks.contains(&key);
                let is_in_expanded_set = self.expanded_thinking_blocks.contains(&key);

                if is_user_expanded {
                    self.user_toggled_thinking_blocks.remove(&key);
                    self.expanded_thinking_blocks.remove(&key);
                } else if is_in_expanded_set {
                    self.user_toggled_thinking_blocks.insert(key);
                } else {
                    self.expanded_thinking_blocks.insert(key);
                    self.user_toggled_thinking_blocks.insert(key);
                }
            }
            ThinkingBlockDisplay::AlwaysExpanded => {
                if self.user_toggled_thinking_blocks.contains(&key) {
                    self.user_toggled_thinking_blocks.remove(&key);
                } else {
                    self.user_toggled_thinking_blocks.insert(key);
                }
            }
            ThinkingBlockDisplay::AlwaysCollapsed => {
                if self.user_toggled_thinking_blocks.contains(&key) {
                    self.user_toggled_thinking_blocks.remove(&key);
                    self.expanded_thinking_blocks.remove(&key);
                } else {
                    self.expanded_thinking_blocks.insert(key);
                    self.user_toggled_thinking_blocks.insert(key);
                }
            }
        }
    }

    pub(crate) fn thinking_block_state(&self, key: (usize, usize), cx: &App) -> (bool, bool) {
        let is_user_toggled = self.user_toggled_thinking_blocks.contains(&key);
        let is_in_expanded_set = self.expanded_thinking_blocks.contains(&key);

        match AgentSettings::get_global(cx).thinking_display {
            ThinkingBlockDisplay::Auto => {
                let is_open = is_user_toggled || is_in_expanded_set;
                (is_open, false)
            }
            ThinkingBlockDisplay::Preview => {
                let is_open = is_user_toggled || is_in_expanded_set;
                let is_constrained = is_in_expanded_set && !is_user_toggled;
                (is_open, is_constrained)
            }
            ThinkingBlockDisplay::AlwaysExpanded => (!is_user_toggled, false),
            ThinkingBlockDisplay::AlwaysCollapsed => (is_user_toggled, false),
        }
    }

    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    pub fn sync_entry(
        &mut self,
        index: usize,
        thread: &Entity<AcpThread>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A thread whose views are dropped has no `entries` to sync into, and
        // the rebuild syncs every entry anyway. Both of the callers that reach
        // here while the agent works on an off-screen thread (a new entry, an
        // updated one) are answered by that rebuild.
        if !self.views_built {
            return;
        }
        let Some(thread_entry) = thread.read(cx).entries().get(index) else {
            return;
        };

        match thread_entry {
            AgentThreadEntry::UserMessage(message) => {
                let can_rewind = thread.read(cx).supports_truncate(cx);
                let has_client_id = message.client_id.is_some();
                let is_subagent = thread.read(cx).parent_session_id().is_some();
                // Attached review comments render as a chip next to the
                // message, not as the wall of quoted diff they compose into.
                let source_blocks = crate::diff_review::without_review_blocks(
                    message.content.source_blocks().to_vec(),
                );
                let source_version = message.content.source_version();
                let source_is_representable = source_blocks
                    .iter()
                    .all(acp_thread::content::can_convert_to_v1);
                let is_editable =
                    can_rewind && has_client_id && !is_subagent && source_is_representable;
                if let Some(Entry::UserMessage {
                    editor,
                    synced_source_version,
                }) = self.entries.get_mut(index)
                {
                    // Read-only messages cannot hold drafts, so focus must not
                    // block new content, nor should unchanged content reset their selection.
                    let was_read_only = editor.read(cx).editor().read(cx).read_only(cx);
                    let refreshed_source = ((!was_read_only
                        || *synced_source_version != source_version)
                        && (!is_editable || !editor.focus_handle(cx).is_focused(window)))
                    .then(|| source_blocks.clone());
                    editor.update(cx, |editor, cx| editor.set_read_only(!is_editable, cx));
                    if let Some(source_blocks) = refreshed_source {
                        editor.update(cx, |editor, cx| {
                            editor.set_source_message(source_blocks, window, cx);
                        });
                        *synced_source_version = source_version;
                    }
                } else {
                    let message_editor = cx.new(|cx| {
                        let mut editor = MessageEditor::new(
                            self.workspace.clone(),
                            self.project.clone(),
                            self.thread_store.clone(),
                            self.session_capabilities.clone(),
                            self.agent_id.clone(),
                            "Edit message － @ to include context",
                            editor::EditorMode::AutoHeight {
                                min_lines: 1,
                                max_lines: None,
                            },
                            window,
                            cx,
                        );
                        if !is_editable {
                            editor.set_read_only(true, cx);
                        }
                        // The editor sits inside the accent-tinted message
                        // bubble; its own background would paint over the
                        // tint.
                        editor.set_transparent_background(true, cx);
                        editor.set_source_message(source_blocks, window, cx);
                        editor
                    });
                    cx.subscribe(&message_editor, move |_, editor, event, cx| {
                        cx.emit(EntryViewEvent {
                            entry_index: index,
                            view_event: ViewEvent::MessageEditorEvent(editor, event.clone()),
                        })
                    })
                    .detach();
                    self.set_entry(
                        index,
                        Entry::UserMessage {
                            editor: message_editor,
                            synced_source_version: source_version,
                        },
                    );
                }
            }
            AgentThreadEntry::ToolCall(tool_call) => {
                let id = tool_call.id.clone();
                let terminals = tool_call.terminals().cloned().collect::<Vec<_>>();
                let diffs = tool_call.diffs().cloned().collect::<Vec<_>>();
                let patch_hunk_buffers = tool_call
                    .content()
                    .iter()
                    .filter_map(|content| match content {
                        acp_thread::ToolCallContent::DiffPatch { render, .. } => {
                            Some(&render.files)
                        }
                        _ => None,
                    })
                    .flat_map(|files| files.iter().flat_map(|file| &file.hunks))
                    .map(|hunk| hunk.buffer.clone())
                    .collect::<Vec<_>>();
                let patch_hunk_ids: HashSet<_> = patch_hunk_buffers
                    .iter()
                    .map(|buffer| buffer.entity_id())
                    .collect();

                let is_tool_call_completed =
                    matches!(tool_call.status(), acp_thread::ToolCallStatus::Completed);
                // Decided before the borrow below, which takes `self.entries`.
                let terminal_output_wanted = self.is_tool_call_content_visible(tool_call);
                let workspace = self.workspace.clone();
                let project = self.project.clone();

                let tool_call_entry = if let Some(Entry::ToolCall(tool_call)) =
                    self.entries.get_mut(index)
                {
                    tool_call
                } else {
                    self.set_entry(
                        index,
                        Entry::ToolCall(ToolCallEntry {
                            content: HashMap::default(),
                            patch_hunk_ids: HashSet::default(),
                            terminals_seen: HashSet::default(),
                            focus_handle: cx.focus_handle(),
                        }),
                    );
                    let Some(Entry::ToolCall(tool_call)) = self.entries.get_mut(index) else {
                        unreachable!()
                    };
                    tool_call
                };
                let ToolCallEntry {
                    content: views,
                    terminals_seen,
                    ..
                } = tool_call_entry;

                for terminal in terminals {
                    let terminal_id = terminal.entity_id();
                    if terminals_seen.insert(terminal_id) {
                        // A command that has already exited the first time this
                        // entry is synced is history being replayed, not a
                        // command that just started, and `expand_terminal_card`
                        // is about the latter. Opening every command in a
                        // restored thread was both wrong (the fork's card is a
                        // quiet one-line chip) and the reason a loaded thread
                        // built a terminal view per command in its history. A
                        // failure still opens itself, through `auto_expanded`.
                        let already_finished =
                            is_tool_call_completed && terminal.read(cx).output().is_some();
                        if !already_finished {
                            cx.emit(EntryViewEvent {
                                entry_index: index,
                                view_event: ViewEvent::NewTerminal(id.clone()),
                            });
                        }
                    } else {
                        let terminal = terminal.read(cx);
                        if is_tool_call_completed
                            && terminal.is_process_backed()
                            && terminal.output().is_none()
                        {
                            cx.emit(EntryViewEvent {
                                entry_index: index,
                                view_event: ViewEvent::TerminalMovedToBackground(id.clone()),
                            });
                        }
                    }

                    // The output is only drawn while the call is open, and in a
                    // long thread almost none of them are. A `TerminalView` and
                    // the `BlinkManager` behind it per command, for every
                    // command the thread ever ran, is what the thread being
                    // read was holding — and the off-screen sweep cannot help
                    // the one thread that is on screen. Built when something
                    // asks to see it; the toggles that open a call sync the
                    // entry again so the view is there for the next frame.
                    if terminal_output_wanted {
                        views.entry(terminal_id).or_insert_with(|| {
                            create_terminal(
                                workspace.clone(),
                                project.clone(),
                                terminal.clone(),
                                window,
                                cx,
                            )
                            .into_any()
                        });
                    } else {
                        views.remove(&terminal_id);
                    }
                }

                for diff in diffs {
                    views.entry(diff.entity_id()).or_insert_with(|| {
                        let editor = create_editor_diff(diff.clone(), window, cx);
                        cx.subscribe(&editor, {
                            let diff = diff.clone();
                            let entry_index = index;
                            move |_this, _editor, event: &EditorEvent, cx| {
                                if let EditorEvent::OpenExcerptsRequested {
                                    selections_by_buffer,
                                    split,
                                } = event
                                {
                                    let multibuffer = diff.read(cx).multibuffer();
                                    if let Some((buffer_id, (ranges, _))) =
                                        selections_by_buffer.iter().next()
                                    {
                                        if let Some(buffer) =
                                            multibuffer.read(cx).buffer(*buffer_id)
                                        {
                                            if let Some(range) = ranges.first() {
                                                let point =
                                                    buffer.read(cx).offset_to_point(range.start.0);
                                                if let Some(path) = diff.read(cx).file_path(cx) {
                                                    cx.emit(EntryViewEvent {
                                                        entry_index,
                                                        view_event: ViewEvent::OpenDiffLocation {
                                                            path,
                                                            position: point,
                                                            split: *split,
                                                        },
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        })
                        .detach();
                        cx.emit(EntryViewEvent {
                            entry_index: index,
                            view_event: ViewEvent::NewDiff(id.clone()),
                        });
                        editor.into_any()
                    });
                }
                for buffer in patch_hunk_buffers {
                    views.entry(buffer.entity_id()).or_insert_with(|| {
                        let editor = create_multibuffer_diff_editor(buffer, window, cx);
                        cx.emit(EntryViewEvent {
                            entry_index: index,
                            view_event: ViewEvent::NewDiff(id.clone()),
                        });
                        editor.into_any()
                    });
                }
                if let Some(Entry::ToolCall(entry)) = self.entries.get_mut(index) {
                    for stale_id in entry.patch_hunk_ids.difference(&patch_hunk_ids) {
                        entry.content.remove(stale_id);
                    }
                    entry.patch_hunk_ids = patch_hunk_ids;
                }
            }
            AgentThreadEntry::Elicitation(_) => {
                if !matches!(self.entries.get(index), Some(Entry::Elicitation { .. })) {
                    self.set_entry(
                        index,
                        Entry::Elicitation {
                            focus_handle: cx.focus_handle(),
                        },
                    );
                }
            }
            AgentThreadEntry::AssistantMessage(message) => {
                let entry = if let Some(Entry::AssistantMessage(entry)) =
                    self.entries.get_mut(index)
                {
                    entry
                } else {
                    self.set_entry(
                        index,
                        Entry::AssistantMessage(AssistantMessageEntry {
                            scroll_handles_by_chunk_index: HashMap::default(),
                            last_thought_source_version: None,
                            focus_handle: cx.focus_handle(),
                        }),
                    );
                    let Some(Entry::AssistantMessage(entry)) = self.entries.get_mut(index) else {
                        unreachable!()
                    };
                    entry
                };
                entry.sync(message);
            }
            AgentThreadEntry::ContextCompaction(_) => {
                if !matches!(self.entries.get(index), Some(Entry::ContextCompaction)) {
                    self.set_entry(index, Entry::ContextCompaction);
                }
            }
        };
    }

    fn set_entry(&mut self, index: usize, entry: Entry) {
        if index == self.entries.len() {
            self.entries.push(entry);
        } else {
            self.entries[index] = entry;
        }
    }

    pub fn remove(&mut self, range: Range<usize>) {
        // The reindexing below still has to happen with the views dropped: a
        // thread truncated while off screen comes back with its expansions
        // pointing at the entries that are left.
        if self.views_built {
            self.entries.drain(range.clone());
        }

        self.expanded_compactions = self
            .expanded_compactions
            .iter()
            .filter_map(|&entry_ix| reindex_after_removal(entry_ix, &range))
            .collect();
        self.expanded_thinking_blocks = self
            .expanded_thinking_blocks
            .iter()
            .filter_map(|&(entry_ix, chunk_ix)| {
                reindex_after_removal(entry_ix, &range).map(|entry_ix| (entry_ix, chunk_ix))
            })
            .collect();
        self.user_toggled_thinking_blocks = self
            .user_toggled_thinking_blocks
            .iter()
            .filter_map(|&(entry_ix, chunk_ix)| {
                reindex_after_removal(entry_ix, &range).map(|entry_ix| (entry_ix, chunk_ix))
            })
            .collect();
        self.auto_expanded_thinking_block =
            self.auto_expanded_thinking_block
                .and_then(|(entry_ix, chunk_ix)| {
                    reindex_after_removal(entry_ix, &range).map(|entry_ix| (entry_ix, chunk_ix))
                });
    }

    pub fn agent_ui_font_size_changed(&mut self, cx: &mut App) {
        for entry in self.entries.iter() {
            match entry {
                Entry::UserMessage { .. }
                | Entry::AssistantMessage { .. }
                | Entry::Elicitation { .. }
                | Entry::ContextCompaction => {}
                Entry::ToolCall(ToolCallEntry { content, .. }) => {
                    for view in content.values() {
                        if let Ok(diff_editor) = view.clone().downcast::<Editor>() {
                            diff_editor.update(cx, |diff_editor, cx| {
                                diff_editor.set_text_style_refinement(
                                    diff_editor_text_style_refinement(cx),
                                );
                                cx.notify();
                            })
                        }
                    }
                }
            }
        }
    }
}

impl EventEmitter<EntryViewEvent> for EntryViewState {}

pub struct EntryViewEvent {
    pub entry_index: usize,
    pub view_event: ViewEvent,
}

pub enum ViewEvent {
    NewDiff(acp_v1::ToolCallId),
    NewTerminal(acp_v1::ToolCallId),
    TerminalMovedToBackground(acp_v1::ToolCallId),
    MessageEditorEvent(Entity<MessageEditor>, MessageEditorEvent),
    OpenDiffLocation {
        path: String,
        position: Point,
        split: bool,
    },
}

#[derive(Debug)]
pub struct AssistantMessageEntry {
    scroll_handles_by_chunk_index: HashMap<usize, ScrollHandle>,
    last_thought_source_version: Option<acp_thread::MessageContentVersion>,
    focus_handle: FocusHandle,
}

impl AssistantMessageEntry {
    pub fn scroll_handle_for_chunk(&self, ix: usize) -> Option<ScrollHandle> {
        self.scroll_handles_by_chunk_index.get(&ix).cloned()
    }

    pub fn sync(&mut self, message: &acp_thread::AssistantMessage) {
        if let Some(acp_thread::AssistantMessageChunk::Thought { block, .. }) =
            message.chunks.last()
            && self.last_thought_source_version != Some(block.source_version())
        {
            let ix = message.chunks.len() - 1;
            let handle = self.scroll_handles_by_chunk_index.entry(ix).or_default();
            handle.scroll_to_bottom();
            self.last_thought_source_version = Some(block.source_version());
        }
    }
}

#[derive(Debug)]
pub struct ToolCallEntry {
    content: HashMap<EntityId, AnyEntity>,
    patch_hunk_ids: HashSet<EntityId>,
    /// The terminals this call has reported, whether or not a view was built
    /// for one. A terminal's view is only built once something asks to see it,
    /// so `content` can no longer stand in for "have we met this terminal
    /// before" — which is what decides whether the card auto-expands and when
    /// a command has carried on past its turn.
    terminals_seen: HashSet<EntityId>,
    focus_handle: FocusHandle,
}

#[derive(Debug)]
pub enum Entry {
    UserMessage {
        editor: Entity<MessageEditor>,
        synced_source_version: acp_thread::MessageContentVersion,
    },
    AssistantMessage(AssistantMessageEntry),
    ToolCall(ToolCallEntry),
    Elicitation {
        focus_handle: FocusHandle,
    },
    ContextCompaction,
}

impl Entry {
    pub fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match self {
            Self::UserMessage { editor, .. } => Some(editor.read(cx).focus_handle(cx)),
            Self::AssistantMessage(message) => Some(message.focus_handle.clone()),
            Self::ToolCall(tool_call) => Some(tool_call.focus_handle.clone()),
            Self::Elicitation { focus_handle } => Some(focus_handle.clone()),
            Self::ContextCompaction => None,
        }
    }

    pub fn message_editor(&self) -> Option<&Entity<MessageEditor>> {
        match self {
            Self::UserMessage { editor, .. } => Some(editor),
            Self::AssistantMessage(_)
            | Self::ToolCall(_)
            | Self::Elicitation { .. }
            | Self::ContextCompaction => None,
        }
    }

    pub fn editor_for_diff(&self, diff: &Entity<acp_thread::Diff>) -> Option<Entity<Editor>> {
        self.content_map()?
            .get(&diff.entity_id())
            .cloned()
            .and_then(|entity| entity.downcast::<Editor>().ok())
    }

    pub fn editor_for_patch_hunk(&self, buffer: &Entity<MultiBuffer>) -> Option<Entity<Editor>> {
        self.content_map()?
            .get(&buffer.entity_id())
            .cloned()
            .and_then(|entity| entity.downcast::<Editor>().ok())
    }

    pub fn terminal(
        &self,
        terminal: &Entity<acp_thread::Terminal>,
    ) -> Option<Entity<TerminalView>> {
        self.content_map()?
            .get(&terminal.entity_id())
            .cloned()
            .and_then(|entity| entity.downcast::<TerminalView>().ok())
    }

    pub fn scroll_handle_for_assistant_message_chunk(
        &self,
        chunk_ix: usize,
    ) -> Option<ScrollHandle> {
        match self {
            Self::AssistantMessage(message) => message.scroll_handle_for_chunk(chunk_ix),
            Self::UserMessage { .. }
            | Self::ToolCall(_)
            | Self::Elicitation { .. }
            | Self::ContextCompaction => None,
        }
    }

    fn content_map(&self) -> Option<&HashMap<EntityId, AnyEntity>> {
        match self {
            Self::ToolCall(ToolCallEntry { content, .. }) => Some(content),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn has_content(&self) -> bool {
        match self {
            Self::ToolCall(ToolCallEntry { content, .. }) => !content.is_empty(),
            Self::UserMessage { .. }
            | Self::AssistantMessage(_)
            | Self::Elicitation { .. }
            | Self::ContextCompaction => false,
        }
    }
}

impl Focusable for ToolCallEntry {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Focusable for Entry {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::UserMessage { editor, .. } => editor.read(cx).focus_handle(cx),
            Self::AssistantMessage(message) => message.focus_handle.clone(),
            Self::ToolCall(tool_call) => tool_call.focus_handle.clone(),
            Self::Elicitation { focus_handle } => focus_handle.clone(),
            Self::ContextCompaction => cx.focus_handle(),
        }
    }
}

fn create_terminal(
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    terminal: Entity<acp_thread::Terminal>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TerminalView> {
    cx.new(|cx| {
        let read_only = !terminal.read(cx).is_process_backed();
        let mut view = TerminalView::new(
            terminal.read(cx).inner().clone(),
            workspace,
            None,
            project,
            window,
            cx,
        )
        .with_read_only(read_only);

        // GPUI can't clip children to rounded corners, so the terminal has to
        // round its own background to avoid painting over the corners of the
        // tool card it sits in.
        // This matches the `rounded_md`/`rounded_b_md` on that card, which GPUI
        // doesn't expose as a value, so if the card's corner radii ever change,
        // this also needs to be updated.
        view.set_background_corner_radii(
            Some(Corners {
                bottom_left: gpui::rems(0.375),
                bottom_right: gpui::rems(0.375),
                ..Default::default()
            }),
            cx,
        );

        view.set_embedded_mode(Some(1000), cx);
        view
    })
}

fn create_editor_diff(
    diff: Entity<acp_thread::Diff>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    create_multibuffer_diff_editor(diff.read(cx).multibuffer().clone(), window, cx)
}

fn create_multibuffer_diff_editor(
    multibuffer: Entity<MultiBuffer>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::new(
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::SizeByContent,
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
        editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
        editor.set_soft_wrap_mode(SoftWrap::None, cx);
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

pub(crate) fn diff_editor_text_style_refinement(cx: &mut App) -> TextStyleRefinement {
    TextStyleRefinement {
        font_size: Some(
            TextSize::Small
                .rems(cx)
                .to_pixels(ThemeSettings::get_global(cx).agent_ui_font_size(cx))
                .into(),
        ),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;

    use acp_thread::{AgentConnection, StubAgentConnection};
    use agent_client_protocol::schema::v1 as acp_v1;
    use buffer_diff::{DiffHunkStatus, DiffHunkStatusKind};
    use editor::RowInfo;
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use parking_lot::RwLock;

    use crate::entry_view_state::{Entry, EntryViewState};
    use crate::message_editor::SessionCapabilities;
    use multi_buffer::MultiBufferRow;
    use pretty_assertions::assert_matches;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::{MultiWorkspace, PathList};

    #[test]
    fn test_reindex_after_removal() {
        use super::reindex_after_removal;

        // Entries before the removed range keep their index.
        assert_eq!(reindex_after_removal(0, &(2..4)), Some(0));
        assert_eq!(reindex_after_removal(1, &(2..4)), Some(1));
        // Entries inside the removed range are dropped.
        assert_eq!(reindex_after_removal(2, &(2..4)), None);
        assert_eq!(reindex_after_removal(3, &(2..4)), None);
        // Entries after the removed range slide down by its length.
        assert_eq!(reindex_after_removal(4, &(2..4)), Some(2));
        assert_eq!(reindex_after_removal(5, &(2..4)), Some(3));
        // An empty removal range leaves indices untouched.
        assert_eq!(reindex_after_removal(3, &(2..2)), Some(3));
    }

    #[gpui::test]
    async fn test_diff_sync(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "hello.txt": "hi world"
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let tool_call = acp_v1::ToolCall::new("tool", "Tool call")
            .status(acp_v1::ToolCallStatus::InProgress)
            .content(vec![acp_v1::ToolCallContent::Diff(
                acp_v1::Diff::new("/project/hello.txt", "hello world").old_text("hi world"),
            )]);
        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|_, cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new(path!("/project"))]),
                    cx,
                )
            })
            .await
            .unwrap();
        let session_id = thread.update(cx, |thread, _| thread.session_id().clone());

        cx.update(|_, cx| {
            connection.send_update(session_id, acp_v1::SessionUpdate::ToolCall(tool_call), cx)
        });

        let thread_store = None;

        let view_state = cx.new(|_cx| {
            EntryViewState::new(
                workspace.downgrade(),
                project.downgrade(),
                thread_store,
                Arc::new(RwLock::new(SessionCapabilities::default())),
                "Test Agent".into(),
            )
        });

        view_state.update_in(cx, |view_state, window, cx| {
            view_state.sync_entry(0, &thread, window, cx)
        });

        let diff = thread.read_with(cx, |thread, _| {
            thread
                .entries()
                .get(0)
                .unwrap()
                .diffs()
                .next()
                .unwrap()
                .clone()
        });

        cx.run_until_parked();

        let diff_editor = view_state.read_with(cx, |view_state, _cx| {
            view_state.entry(0).unwrap().editor_for_diff(&diff).unwrap()
        });
        assert_eq!(
            diff_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "hi world\nhello world"
        );
        let row_infos = diff_editor.read_with(cx, |editor, cx| {
            let multibuffer = editor.buffer().read(cx);
            multibuffer
                .snapshot(cx)
                .row_infos(MultiBufferRow(0))
                .collect::<Vec<_>>()
        });
        assert_matches!(
            row_infos.as_slice(),
            [
                RowInfo {
                    multibuffer_row: Some(MultiBufferRow(0)),
                    diff_status: Some(DiffHunkStatus {
                        kind: DiffHunkStatusKind::Deleted,
                        ..
                    }),
                    ..
                },
                RowInfo {
                    multibuffer_row: Some(MultiBufferRow(1)),
                    diff_status: Some(DiffHunkStatus {
                        kind: DiffHunkStatusKind::Added,
                        ..
                    }),
                    ..
                }
            ]
        );
    }

    #[gpui::test]
    async fn test_elicitation_preserves_entry_index(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|_, cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new(path!("/project"))]),
                    cx,
                )
            })
            .await
            .unwrap();
        let session_id = thread.update(cx, |thread, _| thread.session_id().clone());

        let _response_task = thread.update(cx, |thread, cx| {
            thread
                .request_elicitation(
                    acp_v1::CreateElicitationRequest::new(
                        acp_v1::ElicitationFormMode::new(
                            acp_v1::ElicitationSessionScope::new(session_id.clone()),
                            acp_v1::ElicitationSchema::new().string("name", true),
                        ),
                        "Provide a name",
                    ),
                    cx,
                )
                .unwrap()
        });
        cx.update(|_, cx| {
            connection.send_update(
                session_id,
                acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                    acp_v1::ContentBlock::Text(acp_v1::TextContent::new("hello")),
                )),
                cx,
            );
        });

        let view_state = cx.new(|_cx| {
            EntryViewState::new(
                workspace.downgrade(),
                project.downgrade(),
                None,
                Arc::new(RwLock::new(SessionCapabilities::default())),
                "Test Agent".into(),
            )
        });

        view_state.update_in(cx, |view_state, window, cx| {
            view_state.sync_entry(0, &thread, window, cx);
            view_state.sync_entry(1, &thread, window, cx);
        });

        view_state.read_with(cx, |view_state, _cx| {
            assert!(matches!(
                view_state.entry(0),
                Some(Entry::Elicitation { .. })
            ));
            assert!(matches!(
                view_state.entry(1),
                Some(Entry::AssistantMessage(_))
            ));
        });
    }

    /// A thread that goes off screen drops its views, keeps taking entries while
    /// it is away, and comes back with a view for every one of them — including
    /// the ones that arrived while it had none.
    #[gpui::test]
    async fn test_dropped_views_are_rebuilt_with_the_entries_that_arrived_meanwhile(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|_, cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new(path!("/project"))]),
                    cx,
                )
            })
            .await
            .unwrap();
        let session_id = thread.update(cx, |thread, _| thread.session_id().clone());

        let view_state = cx.new(|_cx| {
            EntryViewState::new(
                workspace.downgrade(),
                project.downgrade(),
                None,
                Arc::new(RwLock::new(SessionCapabilities::default())),
                "Test Agent".into(),
            )
        });

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                    acp_v1::ContentBlock::Text(acp_v1::TextContent::new("first")),
                )),
                cx,
            );
        });
        view_state.update_in(cx, |view_state, window, cx| {
            view_state.sync_entry(0, &thread, window, cx);
        });
        view_state.read_with(cx, |view_state, _cx| {
            assert!(view_state.views_are_built());
            assert!(matches!(
                view_state.entry(0),
                Some(Entry::AssistantMessage(_))
            ));
        });

        // Off screen: the views go.
        view_state.update(cx, |view_state, _cx| view_state.drop_views());
        view_state.read_with(cx, |view_state, _cx| {
            assert!(!view_state.views_are_built());
            assert!(view_state.entry(0).is_none());
        });

        // The agent keeps working. Syncing an entry while the views are dropped
        // is the no-op that keeps this from indexing past the end of an empty
        // list, which is what a running thread would otherwise do here.
        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new("tool", "Tool call")
                        .status(acp_v1::ToolCallStatus::InProgress),
                ),
                cx,
            );
        });
        view_state.update_in(cx, |view_state, window, cx| {
            view_state.sync_entry(1, &thread, window, cx);
        });
        view_state.read_with(cx, |view_state, _cx| {
            assert!(!view_state.views_are_built());
            assert!(view_state.entry(0).is_none());
            assert!(view_state.entry(1).is_none());
        });

        // Back on screen: a view per entry, the one from while it was away
        // included.
        let count = thread.read_with(cx, |thread, _cx| thread.entries().len());
        assert_eq!(count, 2);
        view_state.update_in(cx, |view_state, window, cx| {
            view_state.mark_views_built();
            for ix in 0..count {
                view_state.sync_entry(ix, &thread, window, cx);
            }
        });
        view_state.read_with(cx, |view_state, _cx| {
            assert!(view_state.views_are_built());
            assert!(matches!(
                view_state.entry(0),
                Some(Entry::AssistantMessage(_))
            ));
            assert!(matches!(view_state.entry(1), Some(Entry::ToolCall(_))));
        });
    }

    /// Truncating a thread while its views are dropped still moves the
    /// expansions the remaining entries carry, so the thread does not come back
    /// with a compaction expanded that belongs to an entry that is gone.
    #[gpui::test]
    async fn test_truncation_while_dropped_still_reindexes_expansions(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let view_state = cx.new(|_cx| {
            EntryViewState::new(
                workspace.downgrade(),
                project.downgrade(),
                None,
                Arc::new(RwLock::new(SessionCapabilities::default())),
                "Test Agent".into(),
            )
        });

        view_state.update(cx, |view_state, _cx| {
            view_state.toggle_compaction_expansion(3);
            assert!(view_state.is_compaction_expanded(3));
            view_state.drop_views();
            // The drain is skipped (there is nothing to drain), the reindexing
            // is not.
            view_state.remove(0..2);
            assert!(!view_state.is_compaction_expanded(3));
            assert!(view_state.is_compaction_expanded(1));
        });
    }

    /// A command that had already finished the first time its entry was synced
    /// — every command in a thread restored from history — gets no terminal
    /// view, because nothing is drawing its output. Opening the call is what
    /// builds one.
    #[gpui::test]
    async fn test_a_finished_commands_output_has_no_view_until_the_call_is_opened(
        cx: &mut TestAppContext,
    ) {
        use agent_client_protocol::schema::{MaybeUndefined, v2 as acp_v2};

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|_, cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new(path!("/project"))]),
                    cx,
                )
            })
            .await
            .unwrap();

        let view_state = cx.new(|_cx| {
            EntryViewState::new(
                workspace.downgrade(),
                project.downgrade(),
                None,
                Arc::new(RwLock::new(SessionCapabilities::default())),
                "Test Agent".into(),
            )
        });

        // A finished command, exit status and all, before anything draws it.
        thread.update(cx, |thread, cx| {
            thread
                .upsert_tool_call_patch(
                    acp_v2::ToolCallUpdate::new("tool")
                        .title("Run command")
                        .kind(acp_v2::ToolKind::Execute)
                        .status(acp_v2::ToolCallStatus::Completed)
                        .content(vec![acp_v2::ToolCallContent::Terminal(
                            acp_v2::Terminal::new("display"),
                        )]),
                    cx,
                )
                .expect("a tool call carrying a terminal");
            thread
                .upsert_display_terminal(
                    "display".into(),
                    acp_thread::DisplayTerminalPatch {
                        command: MaybeUndefined::Value("cargo test".into()),
                        output: MaybeUndefined::Value(acp_thread::DisplayTerminalOutput {
                            data: b"ok".to_vec(),
                            meta: None,
                        }),
                        exit_status: MaybeUndefined::Value(
                            acp_v2::TerminalExitStatus::new().exit_code(0),
                        ),
                        ..Default::default()
                    },
                    cx,
                )
                .expect("the command's captured output");
        });
        cx.run_until_parked();

        let terminal = thread.read_with(cx, |thread, _| {
            thread
                .terminal(acp_v1::TerminalId::new("display"))
                .expect("the terminal the tool call named")
        });

        view_state.update_in(cx, |view_state, window, cx| {
            view_state.sync_entry(0, &thread, window, cx);
        });
        view_state.read_with(cx, |view_state, _cx| {
            assert!(
                view_state
                    .entry(0)
                    .expect("the tool call entry")
                    .terminal(&terminal)
                    .is_none(),
                "a command nobody has opened should not hold a terminal view"
            );
        });

        view_state.update_in(cx, |view_state, window, cx| {
            view_state.expand_tool_call(acp_v1::ToolCallId::new("tool"));
            view_state.sync_entry(0, &thread, window, cx);
        });
        view_state.read_with(cx, |view_state, _cx| {
            assert!(
                view_state
                    .entry(0)
                    .expect("the tool call entry")
                    .terminal(&terminal)
                    .is_some(),
                "opening the call should build the view its output is drawn with"
            );
        });
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings_store = SettingsStore::test(cx);
            settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
    }
}
