mod thread_switcher;

use acp_thread::ThreadStatus;
use action_log::DiffStats;
use agent::{ThreadStore, ZED_AGENT_ID};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use agent_ui::terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore, terminal_title_prefix,
};
use agent_ui::thread_metadata_store::{
    ThreadMetadata, ThreadMetadataStore, ThreadPrSnapshot, WorktreePaths,
    worktree_info_from_thread_paths,
};
use agent_ui::threads_archive_view::{format_history_entry_timestamp, fuzzy_match_positions};
use agent_ui::{
    AcpThreadImportOnboarding, Agent, AgentPanel, AgentPanelEvent, AgentThreadSource,
    ArchiveSelectedThread, CrossChannelImportOnboarding, DEFAULT_THREAD_TITLE, NewTerminalThread,
    NewThread, RemoveSelectedThread, RenameSelectedThread, TerminalId, ThreadId, ThreadImportModal,
    ThreadTitleRegenerationResult, channels_with_threads, import_threads_from_other_channels,
};
use agent_ui::{MessageEditorEvent, StateChange, thread_worktree_archive};
use chrono::{DateTime, Utc};
use editor::Editor;
use gh_status::GhStatusStore;
use gpui::{
    Action as _, AnyElement, App, ClickEvent, Context, Decorations, Entity, EntityId, FocusHandle,
    Focusable, KeyContext, ListState, Pixels, Render, SharedString, Task, TaskExt, WeakEntity,
    Window, WindowHandle, linear_color_stop, linear_gradient, list, prelude::*, px,
};
use itertools::Itertools;
use language_model::LanguageModelRegistry;
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use notifications::status_toast::StatusToast;
use project::{AgentId, AgentRegistryStore, Event as ProjectEvent, WorktreeId};
use recent_projects::sidebar_recent_projects::SidebarRecentProjects;
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use ui::utils::platform_title_bar_height;

use serde::{Deserialize, Serialize};
use settings::Settings as _;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use ui::{
    AgentThreadStatus, ContextMenu, Disclosure, Divider, KeyBinding, PopoverMenuHandle,
    ProjectEmptyState, ScrollAxes, Scrollbars, ThreadItem, ThreadItemPrChip,
    ThreadItemWorktreeInfo, Tooltip, WithScrollbar, prelude::*, right_click_menu,
};
use unicode_segmentation::UnicodeSegmentation as _;
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{
    CloseWindow, MultiWorkspace, MultiWorkspaceEvent, NextProject, NextThread, Open, OpenMode,
    PreviousProject, PreviousThread, ProjectGroupKey, RemovalIntent, SaveIntent,
    Sidebar as WorkspaceSidebar, SidebarSide, Toast, Workspace, notifications::NotificationId,
};

use zed_actions::OpenRecent;
use zed_actions::editor::{MoveDown, MoveUp};

use zed_actions::agents_sidebar::{FocusSidebarFilter, ToggleThreadSwitcher};

use crate::thread_switcher::{
    ThreadSwitcher, ThreadSwitcherEntry, ThreadSwitcherEvent, ThreadSwitcherSelection,
    ThreadSwitcherTerminalEntry, ThreadSwitcherThreadEntry,
};

#[cfg(test)]
mod sidebar_tests;

gpui::actions!(
    agents_sidebar,
    [
        /// Creates a new thread in the active project group.
        NewThreadInGroup,
    ]
);

gpui::actions!(
    dev,
    [
        /// Dumps multi-workspace state (projects, worktrees, active threads) into a new buffer.
        DumpWorkspaceInfo,
    ]
);

const DEFAULT_WIDTH: Pixels = px(300.0);
const MIN_WIDTH: Pixels = px(200.0);
const MAX_WIDTH: Pixels = px(800.0);

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebar {
    #[serde(default)]
    width: Option<f32>,
    #[serde(default)]
    collapsed_sections: Vec<SidebarSection>,
}

enum ArchiveWorktreeOutcome {
    Success,
    Cancelled,
}

#[derive(Clone, Debug)]
enum ActiveEntry {
    Thread {
        thread_id: agent_ui::ThreadId,
        /// Stable remote identifier, used for matching when thread_id
        /// differs (e.g. after cross-window activation creates a new
        /// local ThreadId).
        session_id: Option<acp::SessionId>,
        workspace: Entity<Workspace>,
    },
    Terminal {
        terminal_id: TerminalId,
        workspace: Entity<Workspace>,
    },
}

impl ActiveEntry {
    fn workspace(&self) -> &Entity<Workspace> {
        match self {
            ActiveEntry::Thread { workspace, .. } | ActiveEntry::Terminal { workspace, .. } => {
                workspace
            }
        }
    }

    fn is_active_thread(&self, thread_id: &agent_ui::ThreadId) -> bool {
        matches!(self, ActiveEntry::Thread { thread_id: active_thread_id, .. } if active_thread_id == thread_id)
    }

    fn is_active_terminal(&self, terminal_id: TerminalId) -> bool {
        matches!(self, ActiveEntry::Terminal { terminal_id: active_terminal_id, .. } if *active_terminal_id == terminal_id)
    }

    fn matches_entry(&self, entry: &ListEntry) -> bool {
        match (self, entry) {
            (
                ActiveEntry::Thread {
                    thread_id,
                    session_id,
                    ..
                },
                ListEntry::Thread(thread),
            ) => {
                *thread_id == thread.metadata.thread_id
                    || session_id
                        .as_ref()
                        .zip(thread.metadata.session_id.as_ref())
                        .is_some_and(|(a, b)| a == b)
            }
            (ActiveEntry::Terminal { terminal_id, .. }, ListEntry::Terminal(terminal)) => {
                *terminal_id == terminal.metadata.terminal_id
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveThreadInfo {
    session_id: acp::SessionId,
    title: SharedString,
    status: AgentThreadStatus,
    icon: IconName,
    icon_from_external_svg: Option<SharedString>,
    is_title_generating: bool,
    diff_stats: DiffStats,
}

#[derive(Clone)]
enum ThreadEntryWorkspace {
    Open(Entity<Workspace>),
    Closed {
        /// The paths this entry uses (may point to linked worktrees).
        folder_paths: PathList,
        /// The project group this entry belongs to.
        project_group_key: ProjectGroupKey,
    },
}

impl ThreadEntryWorkspace {
    fn is_remote(&self, cx: &App) -> bool {
        match self {
            ThreadEntryWorkspace::Open(workspace) => {
                !workspace.read(cx).project().read(cx).is_local()
            }
            ThreadEntryWorkspace::Closed {
                project_group_key, ..
            } => project_group_key.host().is_some(),
        }
    }
}

/// If the title begins with a decorative prefix (such as a leading emoji,
/// spinner glyph, or symbol the agent prefixed the title with), splits that
/// prefix off so a single representative glyph can be displayed in place of the
/// entry's icon.
fn split_leading_icon_char(
    title: &SharedString,
    highlight_positions: &[usize],
) -> Option<(SharedString, SharedString, Vec<usize>)> {
    let prefix = terminal_title_prefix(title)?;
    let icon_char = pick_icon_glyph(prefix)?;

    let stripped_len = prefix.len();
    let trimmed_title = &title[stripped_len..];
    if trimmed_title.is_empty() {
        return None;
    }

    let adjusted_positions = highlight_positions
        .iter()
        .filter(|&&position| position >= stripped_len)
        .map(|&position| position - stripped_len)
        .collect();

    Some((
        icon_char,
        trimmed_title.to_string().into(),
        adjusted_positions,
    ))
}

/// Picks a single glyph to render as the icon from a detected title prefix.
///
/// We only ever show one glyph, so this makes a best effort to choose a
/// meaningful one by glancing at the leading characters of the prefix:
/// runs of `.` are condensed into a single ellipsis, surrounding ASCII brackets
/// are stripped (so `[!]` yields `!`), and a leading run of the same character
/// is collapsed (so `>>>` yields `>`). The result is the first grapheme cluster
/// of whatever remains, keeping multi-codepoint emoji intact.
fn pick_icon_glyph(prefix: &str) -> Option<SharedString> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return None;
    }

    // Strip a single pair of surrounding ASCII brackets, e.g. `[!]` -> `!`.
    let unwrapped = match prefix.chars().next() {
        Some('[') => prefix.strip_prefix('[').and_then(|s| s.strip_suffix(']')),
        Some('(') => prefix.strip_prefix('(').and_then(|s| s.strip_suffix(')')),
        Some('{') => prefix.strip_prefix('{').and_then(|s| s.strip_suffix('}')),
        Some('<') => prefix.strip_prefix('<').and_then(|s| s.strip_suffix('>')),
        _ => None,
    };
    let prefix = unwrapped
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(prefix);

    // Condense a leading run of dots (`...`) into a single ellipsis.
    if prefix.starts_with("..") {
        return Some("\u{2026}".into());
    }

    // Take the first grapheme cluster so multi-codepoint emoji stay intact.
    let first_grapheme = prefix.graphemes(true).next()?;
    if first_grapheme.trim().is_empty() {
        return None;
    }

    Some(first_grapheme.to_string().into())
}

fn draft_display_label_for_thread_metadata(
    metadata: &ThreadMetadata,
    workspace: &ThreadEntryWorkspace,
    cx: &App,
) -> Option<(SharedString, DraftKind)> {
    let workspace = match workspace {
        ThreadEntryWorkspace::Open(workspace) => Some(workspace),
        ThreadEntryWorkspace::Closed { .. } => None,
    };

    if let Some(label) =
        agent_ui::draft_prompt_store::display_label_for_draft(workspace, metadata.thread_id, cx)
    {
        return Some((label, DraftKind::WithContent));
    }

    let placeholder = agent_ui::draft_prompt_store::empty_draft_placeholder_label();
    Some((placeholder, DraftKind::Empty))
}

/// Whether any folder-path basename of the thread fuzzy-matches the query.
fn folder_path_basename_matches(query: &str, metadata: &ThreadMetadata) -> bool {
    metadata.folder_paths().paths().iter().any(|p| {
        p.as_path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| fuzzy_match_positions(query, name).is_some())
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DraftKind {
    WithContent,
    Empty,
}

#[derive(Clone)]
struct ThreadEntry {
    metadata: ThreadMetadata,
    icon: IconName,
    icon_from_external_svg: Option<SharedString>,
    status: AgentThreadStatus,
    workspace: ThreadEntryWorkspace,
    is_live: bool,
    is_title_generating: bool,
    draft: Option<DraftKind>,
    /// A draft that will start a new worktree on send does not belong to the
    /// workspace it is merely composed in, so it stays out of its group.
    draft_leaves_workspace: bool,
    highlight_positions: Vec<usize>,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    diff_stats: DiffStats,
    /// Set when this thread is the only one in its worktree, in which case it
    /// is drawn as the worktree: the header would have said the same thing
    /// twice, so the row wears what the header carried instead.
    solo_worktree: Option<SoloWorktree>,
    /// Set when this thread is one of several under a worktree header, which is
    /// what the row is indented by.
    under_worktree_header: bool,
}

/// What a worktree header carries that a thread row does not, for a worktree
/// whose single thread stands in for it.
#[derive(Clone)]
struct SoloWorktree {
    /// The worktree's workspace when open, so the row's + can start a second
    /// thread in it. Doing so gives the worktree a header again.
    workspace: Option<Entity<Workspace>>,
    /// Whether archiving this thread takes a worktree with it, which is what
    /// the archive button says it will do.
    is_linked_worktree: bool,
}

/// What a selection points at, stable across list rebuilds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryIdentity {
    Thread(crate::ThreadId),
    Terminal(TerminalId),
}

fn entry_identity(entry: &ListEntry) -> Option<EntryIdentity> {
    match entry {
        ListEntry::Thread(thread) => Some(EntryIdentity::Thread(thread.metadata.thread_id)),
        ListEntry::Terminal(terminal) => {
            Some(EntryIdentity::Terminal(terminal.metadata.terminal_id))
        }
        ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => None,
    }
}

#[derive(Clone)]
struct TerminalEntry {
    metadata: TerminalThreadMetadata,
    workspace: ThreadEntryWorkspace,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    has_notification: bool,
    highlight_positions: Vec<usize>,
}

impl ThreadEntry {
    /// Updates this thread entry with active thread information.
    ///
    /// The existing [`ThreadEntry`] was likely deserialized from the database
    /// but if we have a correspond thread already loaded we want to apply the
    /// live information.
    fn apply_active_info(&mut self, info: &ActiveThreadInfo) {
        self.metadata.title = Some(info.title.clone());
        self.status = info.status;
        self.icon = info.icon;
        self.icon_from_external_svg = info.icon_from_external_svg.clone();
        self.is_live = true;
        self.is_title_generating = info.is_title_generating;
        self.diff_stats = info.diff_stats;
    }
}

/// The three top-level sections of the sidebar list: threads that are currently
/// open in Zed (a tab in any workspace's panel; tabs are the definition of
/// open), the flat history of everything else, and the archived threads. Each
/// is collapsible, and the collapsed set is persisted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SidebarSection {
    OpenInZed,
    AllThreads,
    Archived,
}

impl SidebarSection {
    fn label(self) -> &'static str {
        match self {
            SidebarSection::OpenInZed => "Active",
            SidebarSection::AllThreads => "All Threads",
            SidebarSection::Archived => "Archived",
        }
    }
}

#[derive(Clone)]
enum ListEntry {
    SectionHeader(SidebarSection),
    /// A quiet grouping row above the Active-section threads of one
    /// workspace, shown once a workspace holds more than one row: several
    /// agents can share a worktree, and the rows say which.
    WorkspaceHeader(Arc<WorkspaceHeaderEntry>),
    Thread(Arc<ThreadEntry>),
    Terminal(TerminalEntry),
}

#[derive(Clone)]
struct WorkspaceHeaderEntry {
    label: SharedString,
    /// The group's newest thread, whose entry supplies the header's PR chips.
    lead_thread: Option<Arc<ThreadEntry>>,
    /// The group's workspace when open, so the header's + can start a thread
    /// in this worktree.
    workspace: Option<Entity<Workspace>>,
    /// Session ids of the group's unarchived threads, newest first. The
    /// header's hover archive button archives them all; the last one's archival
    /// tears the linked worktree down. Empty once there is nothing left to
    /// archive, which is what keeps the button off an archived group.
    member_sessions: Vec<acp::SessionId>,
    /// Only linked worktrees archive; the main project's header offers no
    /// archive button.
    is_linked_worktree: bool,
    /// The worktree's root on disk, for measuring what it costs to keep.
    path: Option<PathBuf>,
    /// The group's own key, which is what a collapsed group is remembered by:
    /// the rows come and go as threads are opened and archived, and the header
    /// itself is rebuilt every update.
    key: String,
    /// How many rows the group holds, so a collapsed one can say what it is
    /// hiding.
    member_count: usize,
}

#[derive(Clone)]
enum ActivatableEntry {
    Thread {
        metadata: ThreadMetadata,
    },
    Terminal {
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
    },
}

impl ActivatableEntry {
    fn from_list_entry(entry: &ListEntry) -> Option<Self> {
        match entry {
            ListEntry::Thread(thread) => Some(Self::Thread {
                metadata: thread.metadata.clone(),
            }),
            ListEntry::Terminal(terminal) => Some(Self::Terminal {
                metadata: terminal.metadata.clone(),
                workspace: terminal.workspace.clone(),
            }),
            ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => None,
        }
    }
}

#[cfg(test)]
impl ListEntry {
    fn session_id(&self) -> Option<&acp::SessionId> {
        match self {
            ListEntry::Thread(thread_entry) => thread_entry.metadata.session_id.as_ref(),
            ListEntry::Terminal(_)
            | ListEntry::SectionHeader(_)
            | ListEntry::WorkspaceHeader(_) => None,
        }
    }

    fn reachable_workspaces<'a>(
        &'a self,
        _multi_workspace: &'a workspace::MultiWorkspace,
        _cx: &'a App,
    ) -> Vec<Entity<Workspace>> {
        match self {
            ListEntry::Thread(thread) => match &thread.workspace {
                ThreadEntryWorkspace::Open(ws) => vec![ws.clone()],
                ThreadEntryWorkspace::Closed { .. } => Vec::new(),
            },
            ListEntry::Terminal(terminal) => match &terminal.workspace {
                ThreadEntryWorkspace::Open(workspace) => vec![workspace.clone()],
                ThreadEntryWorkspace::Closed { .. } => Vec::new(),
            },
            ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => Vec::new(),
        }
    }
}

impl From<ThreadEntry> for ListEntry {
    fn from(thread: ThreadEntry) -> Self {
        ListEntry::Thread(Arc::new(thread))
    }
}

impl From<TerminalEntry> for ListEntry {
    fn from(terminal: TerminalEntry) -> Self {
        ListEntry::Terminal(terminal)
    }
}

#[derive(Default)]
struct SidebarContents {
    /// The rendered list: [`Self::all_entries`] minus the rows of collapsed
    /// sections. Selection, activation and neighbor lookup all index into
    /// this, so a collapsed section's rows are skipped by keyboard navigation
    /// and by neighbor activation for free.
    entries: Vec<ListEntry>,
    /// Every row, collapsed or not. Passes that must not depend on what is
    /// currently drawn (gh watches, PR snapshots, draft tracking, the thread
    /// switcher) read this.
    all_entries: Vec<ListEntry>,
    notified_threads: HashSet<agent_ui::ThreadId>,
    notified_terminals: HashSet<TerminalId>,
    /// Threads hosted by a tab somewhere, so a rebuild can tell which ones the
    /// user closed since the last one. Narrower than the Active section, which
    /// also counts a panel's current view (a draft has no tab yet).
    tabbed_threads: HashSet<agent_ui::ThreadId>,
    has_open_projects: bool,
}

/// Identity-and-layout key for a [`ListEntry`] used to preserve measured list items
/// across rebuilds. Equal shapes must render to the same height; add any new
/// height-affecting state here.
#[derive(Debug, PartialEq, Eq)]
enum EntryShape {
    SectionHeader(SidebarSection),
    WorkspaceHeader(SharedString),
    Thread(ThreadId),
    Terminal(TerminalId),
}

impl SidebarContents {
    fn is_thread_notified(&self, thread_id: &agent_ui::ThreadId) -> bool {
        self.notified_threads.contains(thread_id)
    }

    fn is_terminal_notified(&self, terminal_id: TerminalId) -> bool {
        self.notified_terminals.contains(&terminal_id)
    }
}

/// A directory's size on disk, via `du`, which walks in C rather than in a
/// future and is the fastest thing available without an index. `None` when the
/// path is gone or `du` could not read it.
#[cfg(not(test))]
async fn directory_size(path: PathBuf) -> Option<u64> {
    let output = util::command::new_command("du")
        .args(["-sk", "--"])
        .arg(&path)
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let kilobytes: u64 = text.split_whitespace().next()?.parse().ok()?;
    Some(kilobytes * 1024)
}

/// `du` is a real subprocess with its own OS thread for I/O, which the test
/// scheduler cannot make deterministic (`Detected activity on thread
/// "async-process" ... Your test is not deterministic`): any sidebar test
/// that rebuilds entries with a worktree header on screen — nearly all of
/// them — spawned one and broke every test after it in the same binary. The
/// size is a display-only hint (see `worktree_size_label`) that no test
/// asserts on, so tests skip the real measurement entirely.
#[cfg(test)]
async fn directory_size(_path: PathBuf) -> Option<u64> {
    None
}

/// A size worth reading at a glance: whole gigabytes, and nothing below one.
/// A worktree's own source is megabytes; what makes it expensive is build
/// output, and that is always gigabytes.
fn worktree_size_label(bytes: u64) -> Option<SharedString> {
    const GIGABYTE: u64 = 1024 * 1024 * 1024;
    (bytes >= GIGABYTE).then(|| format!("{} GB", bytes / GIGABYTE).into())
}

fn workspace_path_list(workspace: &Entity<Workspace>, cx: &App) -> PathList {
    PathList::new(&workspace.read(cx).root_paths(cx))
}

fn workspace_has_terminal_metadata_except(
    workspace: &Entity<Workspace>,
    except_terminal_id: Option<TerminalId>,
    cx: &App,
) -> bool {
    let Some(store) = TerminalThreadMetadataStore::try_global(cx) else {
        return false;
    };
    let path_list = workspace_path_list(workspace, cx);
    let remote_connection = workspace
        .read(cx)
        .project()
        .read(cx)
        .remote_connection_options(cx);
    store
        .read(cx)
        .entries_for_path(&path_list, remote_connection.as_ref())
        .any(|terminal| except_terminal_id != Some(terminal.terminal_id))
}

/// Shows a [`RemoteConnectionModal`] on the given workspace and establishes
/// an SSH connection. Suitable for passing to
/// [`MultiWorkspace::find_or_create_workspace`] as the `connect_remote`
/// argument.
fn connect_remote(
    modal_workspace: Entity<Workspace>,
    connection_options: RemoteConnectionOptions,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> gpui::Task<anyhow::Result<Option<Entity<remote::RemoteClient>>>> {
    remote_connection::connect_with_modal(&modal_workspace, connection_options, window, cx)
}

// Per-project-group cache of the remote default branch, used to populate the
// "Create New Worktree" submenu without doing git I/O while the menu is open.

#[cfg(test)]
// Mirrors the behavior of the worktree picker's "Create new worktree" entries.
/// Starts a fresh agent draft in the current workspace, recording that sending
/// it should create a new git worktree off `branch_target`. Nothing is created
/// until the user sends: the worktree, its workspace, and the thread are all
/// created on first send (see `AgentPanel::create_worktree_and_send`), so there
/// is no dummy thread and no flash.
fn create_worktree_thread(
    workspace: &Entity<Workspace>,
    branch_target: zed_actions::NewWorktreeBranchTarget,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        // A new worktree starts as a draft in the panel: the worktree and its
        // agent are created when the draft is sent, not before.
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.new_worktree_draft(branch_target, AgentThreadSource::Sidebar, window, cx);
        });
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
}

/// The sidebar re-derives its entire entry list from scratch on every
/// change via `update_entries` → `rebuild_contents`. Avoid adding
/// incremental or inter-event coordination state — if something can
/// be computed from the current world state, compute it in the rebuild.
pub struct Sidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    width: Pixels,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    thread_rename_editor: Entity<Editor>,
    list_state: ListState,
    contents: SidebarContents,
    /// Sections the user has collapsed. Persisted through the sidebar's
    /// serialized state.
    collapsed_sections: HashSet<SidebarSection>,
    /// The index of the list item that currently has the keyboard focus
    ///
    /// Note: This is NOT the same as the active item.
    selection: Option<usize>,
    /// Tracks which sidebar entry is currently active (highlighted).
    active_entry: Option<ActiveEntry>,
    hovered_thread_index: Option<usize>,
    renaming_thread_id: Option<ThreadId>,
    /// Threads in the database-backed regeneration path need their own loading
    /// state because they do not have a live `agent::Thread` to report it.
    regenerating_titles: HashSet<ThreadId>,
    /// Updated only in response to explicit user actions (clicking a
    /// thread, confirming in the thread switcher, etc.) — never from
    /// background data changes. Used to sort the thread switcher popup.
    thread_last_accessed: HashMap<ThreadId, DateTime<Utc>>,
    terminal_last_accessed: HashMap<TerminalId, DateTime<Utc>>,
    thread_switcher: Option<Entity<ThreadSwitcher>>,
    _thread_switcher_subscriptions: Vec<gpui::Subscription>,
    pending_thread_activation: Option<agent_ui::ThreadId>,
    /// Workspace where a new thread was requested before its agent panel
    /// finished loading; fulfilled from the `PanelAdded` handler.
    pending_new_thread_workspace: Option<WeakEntity<Workspace>>,
    /// Remembers whether each draft last rendered as empty or with content so
    /// that when a draft that was empty gains content again, we refresh
    /// its interaction time.
    draft_kinds: HashMap<ThreadId, DraftKind>,
    /// Debounces the rebuild that typing into a draft triggers.
    draft_typing_task: Option<Task<()>>,
    restoring_tasks: HashMap<agent_ui::ThreadId, Task<()>>,
    recent_projects_popover_handle: PopoverMenuHandle<SidebarRecentProjects>,
    /// Branches currently watched in the [`GhStatusStore`], keyed by
    /// (repo path, branch). Kept in sync with the visible thread entries.
    gh_watched_branches: HashSet<(PathBuf, String)>,
    /// Worktree groups the user has folded shut, by group key. A worktree with
    /// a dozen threads is a wall of rows between you and the next worktree,
    /// and most of the time only its newest thread is interesting.
    collapsed_worktrees: HashSet<String>,
    /// What each worktree costs on disk, measured once per session.
    ///
    /// A worktree of a Rust project is mostly build output: one `target` is
    /// six figures of files, and a dozen worktrees is most of a disk. Nothing
    /// else in the app knows that, so the number is worth showing next to the
    /// thing that would delete it. Measuring means walking the tree, which is
    /// exactly the work the file scanner was taught to avoid, so it happens
    /// once per worktree per session, one at a time, in the background.
    worktree_sizes: HashMap<PathBuf, u64>,
    worktree_size_task: Option<Task<()>>,
    worktree_sizes_pending: Vec<PathBuf>,
    _subscriptions: Vec<gpui::Subscription>,
    _draft_editor_observations: Vec<gpui::Subscription>,
    update_task: Option<Task<()>>,
    /// For the thread import banners, if there is just one we show "Import
    /// Threads" but if we are showing both the external agents and other
    /// channels import banners then we change the text to disambiguate the
    /// buttons. This field tracks whether we were using verbose labels so they
    /// can stay stable after dismissing one of the banners.
    import_banners_use_verbose_labels: Option<bool>,
    /// Display names of other release channels that have threads available to
    /// import.
    cross_channel_import_channels: Vec<SharedString>,
}

impl Sidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus_in(&focus_handle, window, Self::focus_in)
            .detach();

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search worktrees…", window, cx);
            editor
        });
        let thread_rename_editor = cx.new(|cx| Editor::single_line(window, cx));

        cx.subscribe_in(
            &multi_workspace,
            window,
            |this, _multi_workspace, event: &MultiWorkspaceEvent, window, cx| match event {
                MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => {
                    this.sync_active_entry_from_active_workspace(cx);
                    this.replace_archived_panel_thread(window, cx);
                    this.schedule_update_entries(false, cx);
                }
                MultiWorkspaceEvent::WorkspaceAdded(workspace) => {
                    this.subscribe_to_workspace(workspace, window, cx);
                    this.schedule_update_entries(false, cx);
                }
                MultiWorkspaceEvent::WorkspaceRemoved(_)
                | MultiWorkspaceEvent::ProjectGroupsChanged => {
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();

        cx.subscribe(&filter_editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                let query = this.filter_editor.read(cx).text(cx);
                if !query.is_empty() {
                    this.selection.take();
                }
                this.schedule_update_entries(!query.is_empty(), cx);
            }
        })
        .detach();

        cx.subscribe_in(
            &thread_rename_editor,
            window,
            |this, title_editor, event, window, cx| {
                this.handle_thread_rename_editor_event(title_editor, event, window, cx);
            },
        )
        .detach();

        cx.observe(&ThreadMetadataStore::global(cx), |this, _store, cx| {
            this.schedule_update_entries(false, cx);
        })
        .detach();

        cx.observe(
            &TerminalThreadMetadataStore::global(cx),
            |this, _store, cx| {
                this.schedule_update_entries(false, cx);
            },
        )
        .detach();

        // Unread markers live in the shared read state; rebuild rows when it
        // changes so the sidebar and thread tabs stay in sync.
        let thread_read_state = agent_ui::ThreadReadState::global(cx);
        cx.observe(&thread_read_state, |this, _state, cx| {
            this.schedule_update_entries(false, cx);
        })
        .detach();

        // PR data is read from the store at render time, so a re-render is
        // enough when it changes; the snapshot each live thread persists is
        // refreshed at the same time.
        if let Some(gh_store) = GhStatusStore::try_global(cx) {
            cx.observe(&gh_store, |this, _store, cx| {
                this.persist_pr_snapshots(cx);
                cx.notify();
            })
            .detach();
        }

        let channels_with_threads = channels_with_threads(cx);
        cx.spawn(async move |this, cx| {
            let channels = channels_with_threads.await;
            this.update(cx, |this, cx| {
                this.cross_channel_import_channels = channels;
                cx.notify();
            })
            .ok();
        })
        .detach();

        let deferred_multi_workspace = multi_workspace.downgrade();
        cx.defer_in(window, move |this, window, cx| {
            if let Some(multi_workspace) = deferred_multi_workspace.upgrade() {
                let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
                for workspace in &workspaces {
                    this.subscribe_to_workspace(workspace, window, cx);
                }
            }
            this.schedule_update_entries(false, cx);
        });

        Self {
            multi_workspace: multi_workspace.downgrade(),
            width: DEFAULT_WIDTH,
            focus_handle,
            filter_editor,
            thread_rename_editor,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
            contents: SidebarContents::default(),
            collapsed_sections: HashSet::new(),
            selection: None,
            active_entry: None,
            hovered_thread_index: None,
            renaming_thread_id: None,
            regenerating_titles: HashSet::new(),

            thread_last_accessed: HashMap::new(),
            terminal_last_accessed: HashMap::new(),
            thread_switcher: None,
            _thread_switcher_subscriptions: Vec::new(),
            pending_thread_activation: None,
            pending_new_thread_workspace: None,
            draft_kinds: HashMap::new(),
            draft_typing_task: None,
            restoring_tasks: HashMap::new(),
            recent_projects_popover_handle: PopoverMenuHandle::default(),
            gh_watched_branches: HashSet::new(),
            collapsed_worktrees: HashSet::new(),
            worktree_sizes: HashMap::new(),
            worktree_size_task: None,
            worktree_sizes_pending: Vec::new(),
            _subscriptions: Vec::new(),
            _draft_editor_observations: Vec::new(),
            update_task: None,
            import_banners_use_verbose_labels: None,
            cross_channel_import_channels: Vec::new(),
        }
    }

    fn is_active_workspace(&self, workspace: &Entity<Workspace>, cx: &App) -> bool {
        self.multi_workspace
            .upgrade()
            .map_or(false, |mw| mw.read(cx).workspace() == workspace)
    }

    fn subscribe_to_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = workspace.read(cx).project().clone();
        if project.read(cx).is_via_collab() {
            return;
        }

        cx.subscribe_in(
            &project,
            window,
            |this, project, event, _window, cx| match event {
                ProjectEvent::WorktreeAdded(_)
                | ProjectEvent::WorktreeRemoved(_)
                | ProjectEvent::WorktreeOrderChanged => {
                    this.schedule_update_entries(false, cx);
                }
                ProjectEvent::WorktreePathsChanged { old_worktree_paths } => {
                    this.move_entry_paths(project, old_worktree_paths, cx);
                    this.schedule_update_entries(false, cx);
                }
                _ => {}
            },
        )
        .detach();

        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        cx.subscribe_in(
            &git_store,
            window,
            |this, _, event: &project::git_store::GitStoreEvent, _window, cx| {
                if matches!(
                    event,
                    project::git_store::GitStoreEvent::RepositoryUpdated(
                        _,
                        project::git_store::RepositoryEvent::GitWorktreeListChanged
                            | project::git_store::RepositoryEvent::HeadChanged,
                        _,
                    )
                ) {
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();

        cx.subscribe_in(
            workspace,
            window,
            move |this, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::PanelAdded(view) = event {
                    if let Ok(agent_panel) = view.clone().downcast::<AgentPanel>() {
                        this.subscribe_to_agent_panel(workspace, &agent_panel, window, cx);
                        this.schedule_update_entries(false, cx);

                        // Fulfill a thread creation that was requested
                        // before this workspace's panel finished loading.
                        let pending = this
                            .pending_new_thread_workspace
                            .as_ref()
                            .and_then(|pending| pending.upgrade())
                            .is_some_and(|pending| &pending == workspace);
                        if pending {
                            this.pending_new_thread_workspace = None;
                            this.create_new_thread(&workspace.clone(), window, cx);
                        }
                    }
                }
            },
        )
        .detach();

        self.observe_docks(workspace, cx);

        if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            self.subscribe_to_agent_panel(workspace, &agent_panel, window, cx);
        }
    }

    fn move_entry_paths(
        &mut self,
        project: &Entity<project::Project>,
        old_paths: &WorktreePaths,
        cx: &mut Context<Self>,
    ) {
        if project.read(cx).is_via_collab() {
            return;
        }

        let new_paths = project.read(cx).worktree_paths(cx);
        let old_folder_paths = old_paths.folder_path_list().clone();

        let added_pairs: Vec<_> = new_paths
            .ordered_pairs()
            .filter(|(main, folder)| {
                !old_paths
                    .ordered_pairs()
                    .any(|(old_main, old_folder)| old_main == *main && old_folder == *folder)
            })
            .map(|(m, f)| (m.clone(), f.clone()))
            .collect();

        let new_folder_paths = new_paths.folder_path_list();
        let removed_folder_paths: Vec<PathBuf> = old_folder_paths
            .paths()
            .iter()
            .filter(|p| !new_folder_paths.paths().contains(p))
            .cloned()
            .collect();

        if added_pairs.is_empty() && removed_folder_paths.is_empty() {
            return;
        }

        let remote_connection = project.read(cx).remote_connection_options(cx);
        let apply_path_changes = |paths: &mut WorktreePaths| {
            for (main_path, folder_path) in &added_pairs {
                paths.add_path(main_path, folder_path);
            }
            for path in &removed_folder_paths {
                paths.remove_folder_path(path);
            }
        };
        ThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            store.change_worktree_paths(
                &old_folder_paths,
                remote_connection.as_ref(),
                &apply_path_changes,
                store_cx,
            );
        });
        TerminalThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            store.change_worktree_paths(
                &old_folder_paths,
                remote_connection.as_ref(),
                &apply_path_changes,
                store_cx,
            );
        });
    }

    fn subscribe_to_agent_panel(
        &mut self,
        workspace: &Entity<Workspace>,
        agent_panel: &Entity<AgentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = workspace.downgrade();
        cx.subscribe_in(
            agent_panel,
            window,
            move |this, agent_panel, event: &AgentPanelEvent, window, cx| match event {
                AgentPanelEvent::ActiveViewChanged
                | AgentPanelEvent::ActiveViewFocused
                | AgentPanelEvent::EntryChanged => {
                    this.sync_active_entry_from_panel(agent_panel, cx);
                    this.schedule_update_entries(false, cx);
                }
                AgentPanelEvent::TerminalCloseRequested { metadata } => {
                    if let Some(workspace) = workspace.upgrade() {
                        let workspace = ThreadEntryWorkspace::Open(workspace);
                        this.close_terminal(metadata, &workspace, window, cx);
                    }
                }
                AgentPanelEvent::ThreadInteracted { thread_id } => {
                    this.record_thread_interacted(thread_id, cx);
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();
    }

    fn sync_active_entry_from_active_workspace(&mut self, cx: &App) {
        let panel = self
            .active_workspace(cx)
            .and_then(|ws| ws.read(cx).panel::<AgentPanel>(cx));
        if let Some(panel) = panel {
            self.sync_active_entry_from_panel(&panel, cx);
        }
    }

    /// When switching workspaces, the active panel may still be showing
    /// a thread that was archived from a different workspace. In that
    /// case, create a fresh draft so the panel has valid content and
    /// `active_entry` can point at it.
    fn replace_archived_panel_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        let Some(thread_id) = panel.read(cx).active_thread_id(cx) else {
            return;
        };
        let is_archived = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .is_some_and(|m| m.archived);
        if is_archived {
            self.create_new_thread(&workspace, window, cx);
        }
    }

    /// Syncs `active_entry` from the agent panel's current state.
    /// Called from `ActiveViewChanged` — the panel has settled into its
    /// new view, so we can safely read it without race conditions.
    ///
    /// Also resolves `pending_thread_activation` when the panel's
    /// active thread matches the pending activation.
    fn sync_active_entry_from_panel(&mut self, agent_panel: &Entity<AgentPanel>, cx: &App) -> bool {
        let Some(active_workspace) = self.active_workspace(cx) else {
            return false;
        };

        // Only sync when the event comes from the active workspace's panel.
        let is_active_panel = active_workspace
            .read(cx)
            .panel::<AgentPanel>(cx)
            .is_some_and(|p| p == *agent_panel);
        if !is_active_panel {
            return false;
        }

        let panel = agent_panel.read(cx);

        if let Some(pending_thread_id) = self.pending_thread_activation {
            let panel_thread_id = panel
                .active_conversation_view()
                .map(|cv| cv.read(cx).parent_id());

            if panel_thread_id == Some(pending_thread_id) {
                let session_id = panel
                    .active_agent_thread(cx)
                    .map(|thread| thread.read(cx).session_id().clone());
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: pending_thread_id,
                    session_id,
                    workspace: active_workspace,
                });
                self.pending_thread_activation = None;
                return true;
            }
            // Pending activation not yet resolved — keep current active_entry.
            return false;
        }

        if let Some(terminal_id) = panel.active_terminal_id() {
            self.active_entry = Some(ActiveEntry::Terminal {
                terminal_id,
                workspace: active_workspace,
            });
        } else if let Some(thread_id) = panel.active_thread_id(cx) {
            let is_archived = ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .is_some_and(|m| m.archived);
            if !is_archived {
                let session_id = panel
                    .active_agent_thread(cx)
                    .map(|thread| thread.read(cx).session_id().clone());
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id,
                    session_id,
                    workspace: active_workspace,
                });
            }
        }

        false
    }

    fn observe_docks(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let docks: Vec<_> = workspace
            .read(cx)
            .all_docks()
            .into_iter()
            .cloned()
            .collect();
        let workspace = workspace.downgrade();
        for dock in docks {
            let workspace = workspace.clone();
            cx.observe(&dock, move |this, _dock, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                if !this.is_active_workspace(&workspace, cx) {
                    return;
                }

                cx.notify();
            })
            .detach();
        }
    }

    /// Opens a new workspace for a group that has no open workspaces.
    fn open_workspace_for_group(
        &mut self,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let path_list = project_group_key.path_list().clone();
        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |_this, cx| {
            let result = task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            result?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Rebuilds the sidebar contents from current workspace and thread state.
    ///
    /// Iterates [`MultiWorkspace::project_group_keys`] to determine project
    /// groups, then populates thread entries from the metadata store and
    /// merges live thread info from active agent panels.
    ///
    /// Aim for a single forward pass over workspaces and threads plus an
    /// O(T log T) sort. Avoid adding extra scans over the data.
    ///
    /// Properties:
    ///
    /// - Should always show every workspace in the multiworkspace
    ///     - If you have no threads, and two workspaces for the worktree and the main workspace, make sure at least one is shown
    /// - Should always show every thread, associated with each workspace in the multiworkspace
    /// - After every build_contents, our "active" state should exactly match the current workspace's, current agent panel's current thread.
    fn rebuild_contents(&mut self, cx: &App) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let mw = multi_workspace.read(cx);
        let workspaces: Vec<_> = mw.workspaces().cloned().collect();
        let active_workspace = Some(mw.workspace().clone());

        let agent_server_store = workspaces
            .first()
            .map(|ws| ws.read(cx).project().read(cx).agent_server_store().clone());

        let query = self.filter_editor.read(cx).text(cx);

        self.contents = SidebarContents::default();

        // Unread markers come from the shared read state (written by the
        // conversation views when a turn completes unviewed, cleared when the
        // thread's tab is viewed), so tabs and sidebar rows always agree.
        let mut notified_threads: HashSet<agent_ui::ThreadId> =
            agent_ui::ThreadReadState::try_global(cx)
                .map(|state| state.read(cx).unread_threads().iter().copied().collect())
                .unwrap_or_default();
        let mut notified_terminals: HashSet<TerminalId> = HashSet::new();
        let mut current_session_ids: HashSet<acp::SessionId> = HashSet::new();
        let mut current_thread_ids: HashSet<agent_ui::ThreadId> = HashSet::new();
        let mut current_terminal_ids: HashSet<TerminalId> = HashSet::new();

        let has_open_projects = workspaces
            .iter()
            .any(|ws| !workspace_path_list(ws, cx).paths().is_empty());

        let resolve_agent_icon = |agent_id: &AgentId| -> (IconName, Option<SharedString>) {
            let agent = Agent::from(agent_id.clone());
            let icon = agent.logo();
            let icon_from_external_svg = agent_server_store
                .as_ref()
                .and_then(|store| store.read(cx).agent_icon(&agent_id));
            (icon, icon_from_external_svg)
        };

        let mut live_notified_terminal_ids: HashSet<TerminalId> = HashSet::new();
        for workspace in &workspaces {
            if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                live_notified_terminal_ids.extend(
                    agent_panel
                        .read(cx)
                        .terminals(cx)
                        .into_iter()
                        .filter_map(|terminal| terminal.has_notification.then_some(terminal.id)),
                );
            }
        }

        let mut branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();
        for ws in &workspaces {
            let project = ws.read(cx).project().read(cx);
            for repo in project.repositories(cx).values() {
                let snapshot = repo.read(cx).snapshot();
                if let Some(branch) = &snapshot.branch {
                    branch_by_path.insert(
                        snapshot.work_directory_abs_path.to_path_buf(),
                        SharedString::from(Arc::<str>::from(branch.name())),
                    );
                }
                for linked_wt in snapshot.linked_worktrees() {
                    if let Some(branch) = linked_wt.branch_name() {
                        branch_by_path.insert(
                            linked_wt.path.clone(),
                            SharedString::from(Arc::<str>::from(branch)),
                        );
                    }
                }
            }
        }

        // Workspace resolution across all open workspaces, matching both the
        // stored path list and the remote identity.
        let open_workspace_locations: Vec<(
            PathList,
            Option<RemoteConnectionOptions>,
            Entity<Workspace>,
        )> = workspaces
            .iter()
            .map(|ws| {
                (
                    workspace_path_list(ws, cx),
                    ws.read(cx).project().read(cx).remote_connection_options(cx),
                    ws.clone(),
                )
            })
            .collect();
        let resolve_workspace = |worktree_paths: &WorktreePaths,
                                 remote_connection: Option<&RemoteConnectionOptions>|
         -> ThreadEntryWorkspace {
            let folder_paths = worktree_paths.folder_path_list();
            open_workspace_locations
                .iter()
                .find(|(paths, ws_remote, _)| {
                    paths == folder_paths
                        && same_remote_connection_identity(ws_remote.as_ref(), remote_connection)
                })
                .map(|(_, _, ws)| ThreadEntryWorkspace::Open(ws.clone()))
                .unwrap_or_else(|| ThreadEntryWorkspace::Closed {
                    folder_paths: folder_paths.clone(),
                    project_group_key: ProjectGroupKey::from_worktree_paths(
                        worktree_paths,
                        remote_connection.cloned(),
                    ),
                })
        };

        // All stored terminal threads, regardless of project.
        let terminal_store = TerminalThreadMetadataStore::global(cx);
        let mut terminals: Vec<TerminalEntry> = terminal_store
            .read(cx)
            .entries()
            .cloned()
            .map(|metadata| {
                let workspace = resolve_workspace(
                    &metadata.worktree_paths,
                    metadata.remote_connection.as_ref(),
                );
                let worktrees =
                    worktree_info_from_thread_paths(&metadata.worktree_paths, &branch_by_path);
                let has_notification = live_notified_terminal_ids.contains(&metadata.terminal_id);
                TerminalEntry {
                    metadata,
                    workspace,
                    worktrees,
                    has_notification,
                    highlight_positions: Vec::new(),
                }
            })
            .collect();
        current_terminal_ids.extend(
            terminals
                .iter()
                .map(|terminal| terminal.metadata.terminal_id),
        );
        notified_terminals.extend(terminals.iter().filter_map(|terminal| {
            terminal
                .has_notification
                .then_some(terminal.metadata.terminal_id)
        }));

        // All stored threads, archived included: the sidebar is the single
        // history surface.
        let thread_store = ThreadMetadataStore::global(cx);
        let mut threads: Vec<Arc<ThreadEntry>> = thread_store
            .read(cx)
            .entries()
            .cloned()
            .map(|row| {
                let (icon, icon_from_external_svg) = resolve_agent_icon(&row.agent_id);
                let workspace =
                    resolve_workspace(&row.worktree_paths, row.remote_connection.as_ref());
                let worktrees =
                    worktree_info_from_thread_paths(&row.worktree_paths, &branch_by_path);
                // Start drafts as `WithContent`; the post-processing pass
                // below downgrades them to `Empty` if no draft label can be
                // derived.
                let draft = row.is_draft().then_some(DraftKind::WithContent);
                Arc::new(ThreadEntry {
                    metadata: row,
                    icon,
                    icon_from_external_svg,
                    status: AgentThreadStatus::default(),
                    workspace,
                    is_live: false,
                    is_title_generating: false,
                    draft,
                    draft_leaves_workspace: false,
                    highlight_positions: Vec::new(),
                    worktrees,
                    diff_stats: DiffStats::default(),
                    solo_worktree: None,
                    under_worktree_header: false,
                })
            })
            .collect();

        for thread in &mut threads {
            if thread.draft.is_none() {
                continue;
            }
            if let Some((label, kind)) =
                draft_display_label_for_thread_metadata(&thread.metadata, &thread.workspace, cx)
            {
                let thread = Arc::make_mut(thread);
                thread.metadata.title = Some(label);
                thread.draft = Some(kind);
            }
            let leaves_workspace = match &thread.workspace {
                ThreadEntryWorkspace::Open(workspace) => workspace
                    .read(cx)
                    .panel::<AgentPanel>(cx)
                    .and_then(|panel| {
                        panel
                            .read(cx)
                            .conversation_view_for_id(&thread.metadata.thread_id, cx)
                    })
                    .is_some_and(|conversation_view| {
                        matches!(
                            conversation_view.read(cx).draft_worktree_choice(),
                            agent_ui::DraftWorktreeChoice::NewWorktree(_)
                                | agent_ui::DraftWorktreeChoice::NewWorktreeDefault
                        )
                    }),
                ThreadEntryWorkspace::Closed { .. } => false,
            };
            if leaves_workspace {
                Arc::make_mut(thread).draft_leaves_workspace = true;
            }
        }
        threads.retain(|thread| thread.draft.is_none() || thread.metadata.title.is_some());

        // Keep empty drafts only while their thread is active; preserve
        // drafts with content because they hold user-typed state.
        let pending_activation = self.pending_thread_activation;
        let active_panel_thread_id = active_workspace
            .as_ref()
            .and_then(|ws| ws.read(cx).panel::<AgentPanel>(cx))
            .and_then(|panel| panel.read(cx).active_thread_id(cx));
        threads.retain(|thread| {
            if thread.draft != Some(DraftKind::Empty) {
                return true;
            }
            if pending_activation.is_some() {
                return false;
            }
            Some(thread.metadata.thread_id) == active_panel_thread_id
        });

        // Build a lookup from live thread infos across all open workspaces.
        let mut live_info_by_session: HashMap<acp::SessionId, ActiveThreadInfo> = HashMap::new();
        for workspace in &workspaces {
            for info in all_thread_infos_for_workspace(workspace, cx) {
                live_info_by_session.insert(info.session_id.clone(), info);
            }
        }

        // Every thread open as a tab in some workspace's panel, plus whatever
        // that panel is currently showing, belongs in the Active section.
        // Live-session matching alone misses drafts, which have no session id
        // yet.
        //
        // A panel's view of a closed tab can outlive the tab (it is dropped
        // when the last handle goes, not when the tab closes), so open tabs are
        // asked for by name rather than inferred from which views are still
        // alive. Otherwise closing a thread leaves it sitting in Active.
        let mut open_thread_ids: HashSet<agent_ui::ThreadId> = HashSet::new();
        let mut tabbed_threads: HashSet<agent_ui::ThreadId> = HashSet::new();
        for workspace in &workspaces {
            if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                let agent_panel = agent_panel.read(cx);
                open_thread_ids.extend(
                    agent_panel
                        .active_conversation_view()
                        .map(|conversation_view| conversation_view.read(cx).parent_id()),
                );
                let tabs = agent_panel.open_thread_tab_ids(cx);
                open_thread_ids.extend(tabs.iter().copied());
                tabbed_threads.extend(tabs);
            }
        }

        // Merge live info into threads and mask the unread marker on the
        // thread the user is actively viewing.
        for thread in &mut threads {
            if let Some(session_id) = thread.metadata.session_id.clone() {
                if let Some(info) = live_info_by_session.get(&session_id) {
                    Arc::make_mut(thread).apply_active_info(info);
                }
            }

            let is_active_thread = self.active_entry.as_ref().is_some_and(|entry| {
                entry.is_active_thread(&thread.metadata.thread_id)
                    && active_workspace
                        .as_ref()
                        .is_some_and(|active| active == entry.workspace())
            });

            if is_active_thread {
                notified_threads.remove(&thread.metadata.thread_id);
            }
        }

        if !query.is_empty() {
            // A thread matches on its title, its worktree names, or the
            // basename of any of its folder paths (so typing a project name
            // surfaces its threads).
            let mut matched_threads: Vec<Arc<ThreadEntry>> = Vec::new();
            for mut thread in threads {
                let mut worktree_matched = false;
                {
                    let thread = Arc::make_mut(&mut thread);
                    let title = thread.metadata.display_title();
                    if let Some(positions) = fuzzy_match_positions(&query, title.as_ref()) {
                        thread.highlight_positions = positions;
                    }
                    for worktree in &mut thread.worktrees {
                        let Some(name) = worktree.worktree_name.as_ref() else {
                            continue;
                        };
                        if let Some(positions) = fuzzy_match_positions(&query, name) {
                            worktree.highlight_positions = positions;
                            worktree_matched = true;
                        }
                    }
                }
                let project_matched = folder_path_basename_matches(&query, &thread.metadata);
                if !thread.highlight_positions.is_empty() || worktree_matched || project_matched {
                    matched_threads.push(thread);
                }
            }
            threads = matched_threads;

            let mut matched_terminals: Vec<TerminalEntry> = Vec::new();
            for mut terminal in terminals {
                let mut terminal_matched = false;
                let terminal_title = terminal.metadata.display_title();
                if let Some(positions) = fuzzy_match_positions(&query, terminal_title.as_ref()) {
                    terminal.highlight_positions = positions;
                    terminal_matched = true;
                }
                let mut worktree_matched = false;
                for worktree in &mut terminal.worktrees {
                    let Some(name) = worktree.worktree_name.as_ref() else {
                        continue;
                    };
                    if let Some(positions) = fuzzy_match_positions(&query, name) {
                        worktree.highlight_positions = positions;
                        worktree_matched = true;
                    }
                }
                let project_matched = terminal.metadata.folder_paths().paths().iter().any(|p| {
                    p.as_path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| fuzzy_match_positions(&query, name).is_some())
                });
                if terminal_matched || worktree_matched || project_matched {
                    matched_terminals.push(terminal);
                }
            }
            terminals = matched_terminals;
        }

        let all_entries = Self::sectioned_entries(
            terminals,
            threads,
            &open_thread_ids,
            &mut current_session_ids,
            &mut current_thread_ids,
        );
        let entries = Self::visible_entries(
            &all_entries,
            &self.collapsed_sections,
            &self.collapsed_worktrees,
        );

        notified_threads.retain(|id| current_thread_ids.contains(id));

        self.thread_last_accessed
            .retain(|id, _| current_thread_ids.contains(id));
        self.terminal_last_accessed
            .retain(|id, _| current_terminal_ids.contains(id));

        self.contents = SidebarContents {
            entries,
            all_entries,
            notified_threads,
            notified_terminals,
            tabbed_threads,
            has_open_projects,
        };
    }

    /// Drops the rows of collapsed sections; headers always render, since they
    /// carry the disclosure that expands the section again.
    fn visible_entries(
        entries: &[ListEntry],
        collapsed_sections: &HashSet<SidebarSection>,
        collapsed_worktrees: &HashSet<String>,
    ) -> Vec<ListEntry> {
        let mut visible = Vec::with_capacity(entries.len());
        let mut hiding_section = false;
        // A worktree's rows run until the next header of either kind, so the
        // group's own header is what turns the hiding off again.
        let mut hiding_worktree = false;
        for entry in entries {
            match entry {
                ListEntry::SectionHeader(section) => {
                    hiding_section = collapsed_sections.contains(section);
                    hiding_worktree = false;
                    visible.push(entry.clone());
                }
                ListEntry::WorkspaceHeader(header) => {
                    hiding_worktree = collapsed_worktrees.contains(&header.key);
                    if !hiding_section {
                        visible.push(entry.clone());
                    }
                }
                // A worktree with one thread has no header, so the row is
                // both the group and its own end: a collapsed group above it
                // stops here rather than swallowing it.
                ListEntry::Thread(thread) if thread.solo_worktree.is_some() => {
                    hiding_worktree = false;
                    if !hiding_section {
                        visible.push(entry.clone());
                    }
                }
                ListEntry::Thread(_) | ListEntry::Terminal(_) => {
                    if !hiding_section && !hiding_worktree {
                        visible.push(entry.clone());
                    }
                }
            }
        }
        visible
    }

    /// Which workspace a thread belongs to, for grouping. `None` for a thread
    /// with no fixed home: a new-worktree draft is only visiting the workspace
    /// it was started from.
    fn thread_workspace_key(thread: &ThreadEntry) -> Option<String> {
        if thread.draft.is_some() && thread.draft_leaves_workspace {
            return None;
        }
        Some(match &thread.workspace {
            ThreadEntryWorkspace::Open(workspace) => {
                format!("open-{:?}", workspace.entity_id())
            }
            ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                format!("closed-{folder_paths:?}")
            }
        })
    }

    /// Clusters a section's rows by workspace, inserting a header above each
    /// workspace's rows. Rows arrive sorted newest-first; clusters keep the
    /// order of their newest row.
    fn group_rows_by_workspace(rows: Vec<ListEntry>) -> Vec<ListEntry> {
        fn workspace_key(entry: &ListEntry) -> Option<String> {
            let ListEntry::Thread(thread) = entry else {
                return None;
            };
            Sidebar::thread_workspace_key(thread)
        }
        // (Terminal rows keep their flat placement: they are transient and
        // carry their own worktree chip.)

        let mut out: Vec<ListEntry> = Vec::with_capacity(rows.len() + 4);
        let mut emitted: HashSet<String> = HashSet::new();
        for row in &rows {
            let Some(key) = workspace_key(row) else {
                out.push(row.clone());
                continue;
            };
            if !emitted.insert(key.clone()) {
                continue;
            }
            let members: Vec<ListEntry> = rows
                .iter()
                .filter(|candidate| workspace_key(candidate).as_ref() == Some(&key))
                .cloned()
                .collect();
            let lead_thread = members.iter().find_map(|member| match member {
                ListEntry::Thread(thread) => Some(thread.clone()),
                _ => None,
            });
            let info = lead_thread
                .as_ref()
                .and_then(|thread| thread.worktrees.first());
            let label = info
                .and_then(|info| info.worktree_name.clone())
                .unwrap_or_else(|| SharedString::from("Workspace"));
            let is_linked_worktree = info.is_some_and(|info| info.kind == ui::WorktreeKind::Linked);
            // What the header's archive would take. An already-archived thread
            // is not part of that, which is what keeps the button off a group
            // with nothing left to archive.
            let member_sessions: Vec<acp::SessionId> = members
                .iter()
                .filter_map(|member| match member {
                    ListEntry::Thread(thread) if !thread.metadata.archived => {
                        thread.metadata.session_id.clone()
                    }
                    _ => None,
                })
                .collect();
            let workspace = lead_thread
                .as_ref()
                .and_then(|thread| match &thread.workspace {
                    ThreadEntryWorkspace::Open(workspace) => Some(workspace.clone()),
                    ThreadEntryWorkspace::Closed { .. } => None,
                });
            // A worktree with one thread in it is one row. The header would
            // only repeat what the row already says (a thread's title is what
            // its worktree is for), so the row takes the header's chrome: the
            // PR chips, the + that starts a second thread here, and the archive
            // that takes the worktree with it.
            if let [ListEntry::Thread(thread)] = members.as_slice() {
                let mut solo = (**thread).clone();
                solo.solo_worktree = Some(SoloWorktree {
                    workspace,
                    is_linked_worktree,
                });
                out.push(ListEntry::Thread(Arc::new(solo)));
                continue;
            }

            out.push(ListEntry::WorkspaceHeader(Arc::new(WorkspaceHeaderEntry {
                label,
                lead_thread: lead_thread.clone(),
                workspace,
                member_sessions,
                is_linked_worktree,
                path: info.map(|info| PathBuf::from(info.full_path.as_ref())),
                key: key.clone(),
                member_count: members.len(),
            })));
            // A row under a header is indented, so the group reads as a group
            // rather than as a header that happens to sit above some threads.
            out.extend(members.into_iter().map(|member| match member {
                ListEntry::Thread(thread) => {
                    let mut grouped = (*thread).clone();
                    grouped.under_worktree_header = true;
                    ListEntry::Thread(Arc::new(grouped))
                }
                member => member,
            }));
        }
        out
    }

    /// Lays the list out as three sections: "Active" (threads that are open
    /// right now — a tab, or the thread a panel is currently showing, or a
    /// still-running session) on top, then "All Threads", the flat history
    /// of everything unarchived, then "Archived". All Threads is not what's
    /// left over after Active: it lists every unarchived thread regardless
    /// of whether it is also open, so an open thread appears in both
    /// sections. Each section is one flat list sorted most-recent-first
    /// (title breaks ties); empty drafts pin to the top. A section with rows
    /// always gets its header, which is what the user collapses it by.
    fn sectioned_entries(
        terminals: Vec<TerminalEntry>,
        threads: Vec<Arc<ThreadEntry>>,
        open_thread_ids: &HashSet<agent_ui::ThreadId>,
        current_session_ids: &mut HashSet<acp::SessionId>,
        current_thread_ids: &mut HashSet<agent_ui::ThreadId>,
    ) -> Vec<ListEntry> {
        fn display_time(entry: &ListEntry) -> DateTime<Utc> {
            match entry {
                ListEntry::Thread(thread) if thread.draft == Some(DraftKind::Empty) => {
                    DateTime::<Utc>::MAX_UTC
                }
                ListEntry::Thread(thread) => Sidebar::thread_display_time(&thread.metadata),
                ListEntry::Terminal(terminal) => terminal.metadata.created_at,
                ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => unreachable!(),
            }
        }

        // Title as tiebreaker keeps the order deterministic for equal
        // timestamps (store iteration order is not).
        fn title(entry: &ListEntry) -> SharedString {
            match entry {
                ListEntry::Thread(thread) => thread.metadata.display_title(),
                ListEntry::Terminal(terminal) => terminal.metadata.display_title(),
                ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => unreachable!(),
            }
        }

        fn record_ids(
            entry: &ListEntry,
            current_session_ids: &mut HashSet<acp::SessionId>,
            current_thread_ids: &mut HashSet<agent_ui::ThreadId>,
        ) {
            if let ListEntry::Thread(thread) = entry {
                if let Some(session_id) = &thread.metadata.session_id {
                    current_session_ids.insert(session_id.clone());
                }
                current_thread_ids.insert(thread.metadata.thread_id);
            }
        }

        let (archived_threads, unarchived_threads): (Vec<_>, Vec<_>) = threads
            .into_iter()
            .partition(|thread| thread.metadata.archived);

        // Active is what is open right now: a thread with a tab, or one whose
        // session is still running. Nothing else qualifies. A thread does not
        // become active by sharing a worktree with one that is, and closing a
        // thread takes it out of here even though its neighbours stay.
        let open_threads: Vec<_> = unarchived_threads
            .iter()
            .filter(|thread| thread.is_live || open_thread_ids.contains(&thread.metadata.thread_id))
            .cloned()
            .collect();

        // All threads is the whole history, not the leftovers: a thread being
        // open does not remove it from the list of threads. Only archiving
        // does, which is what the archived section is for.
        let history_threads = unarchived_threads;

        let sort = |rows: Vec<ListEntry>| {
            rows.into_iter()
                .sorted_by_key(|entry| (std::cmp::Reverse(display_time(entry)), title(entry)))
                .collect::<Vec<_>>()
        };

        let sections = [
            (
                SidebarSection::OpenInZed,
                Self::group_rows_by_workspace(sort(
                    open_threads.into_iter().map(ListEntry::Thread).collect(),
                )),
            ),
            (
                SidebarSection::AllThreads,
                Self::group_rows_by_workspace(sort(
                    terminals
                        .into_iter()
                        .map(ListEntry::Terminal)
                        .chain(history_threads.into_iter().map(ListEntry::Thread))
                        .collect(),
                )),
            ),
            (
                SidebarSection::Archived,
                Self::group_rows_by_workspace(sort(
                    archived_threads
                        .into_iter()
                        .map(ListEntry::Thread)
                        .collect(),
                )),
            ),
        ];

        let mut entries: Vec<ListEntry> = Vec::new();
        for (section, rows) in sections {
            // The Active header always renders: it carries the new-thread
            // button, which must exist even (especially) with nothing active.
            if rows.is_empty() && !matches!(section, SidebarSection::OpenInZed) {
                continue;
            }
            entries.push(ListEntry::SectionHeader(section));
            for entry in rows {
                record_ids(&entry, current_session_ids, current_thread_ids);
                entries.push(entry);
            }
        }

        entries
    }

    /// A draft's row shows the text being typed into it, so typing does change
    /// the list. It does not need to change it once per keystroke: rebuilding
    /// Queues the worktrees now on screen for measurement, skipping the ones
    /// already measured or already queued.
    fn measure_worktree_sizes(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::WorkspaceHeader(header) => header.path.clone(),
                _ => None,
            })
            .filter(|path| {
                !self.worktree_sizes.contains_key(path)
                    && !self.worktree_sizes_pending.contains(path)
            })
            .collect();
        if paths.is_empty() {
            return;
        }
        self.worktree_sizes_pending.extend(paths);
        self.measure_next_worktree(cx);
    }

    /// Measures one worktree, then the next. Sequential on purpose: a dozen
    /// concurrent walks of a build directory is the kind of IO storm that made
    /// the whole app feel slow before `file_scan_exclusions` learned about
    /// `target`.
    fn measure_next_worktree(&mut self, cx: &mut Context<Self>) {
        if self.worktree_size_task.is_some() {
            return;
        }
        let Some(path) = self.worktree_sizes_pending.first().cloned() else {
            return;
        };
        self.worktree_size_task = Some(cx.spawn(async move |this, cx| {
            let size = cx.background_spawn(directory_size(path.clone())).await;
            this.update(cx, |this, cx| {
                this.worktree_sizes_pending.retain(|queued| queued != &path);
                this.worktree_size_task = None;
                if let Some(size) = size {
                    this.worktree_sizes.insert(path, size);
                    cx.notify();
                }
                this.measure_next_worktree(cx);
            })
            .ok();
        }));
    }

    /// walks every stored thread and every open workspace, which is a lot of
    /// work to redo between two characters.
    fn rebuild_after_typing(&mut self, cx: &mut Context<Self>) {
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(200);
        self.draft_typing_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |this, cx| this.update_entries(cx)).ok();
        }));
    }

    fn schedule_update_entries(&mut self, select_first_after_update: bool, cx: &mut Context<Self>) {
        if self.update_task.is_some() && !select_first_after_update {
            return;
        }

        self.update_task = Some(cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.update_task = None;
                this.update_entries(cx);
                if select_first_after_update {
                    this.select_first_entry();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Rebuilds the sidebar's visible entries from already-cached state.
    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        if !multi_workspace.read(cx).multi_workspace_enabled(cx) {
            return;
        }

        let had_notifications = self.has_notifications(cx);
        let previous_shapes: Vec<EntryShape> = self.entry_shapes().collect();
        // Selection is index-based, and a rebuild reshuffles indices (a
        // confirmed thread moves to Active, rows appear and vanish). Remember
        // WHICH row is selected so it can be re-anchored afterwards; a stale
        // index silently selects a different row.
        let selected_identity = self
            .selection
            .and_then(|ix| self.contents.entries.get(ix))
            .and_then(entry_identity);
        let previously_tabbed = std::mem::take(&mut self.contents.tabbed_threads);

        self.rebuild_contents(cx);
        self.measure_worktree_sizes(cx);
        self.sync_gh_watches(cx);
        self.persist_pr_snapshots(cx);
        self.refresh_refilled_draft_times(cx);
        self.refresh_draft_editor_observations(cx);

        if let Some(identity) = selected_identity {
            // Selecting a row is how the user says which thread they are
            // looking at. Closing that thread answers the question, so the
            // selection goes with it rather than pointing at a shut tab.
            let was_closed = matches!(identity, EntryIdentity::Thread(thread_id)
                if previously_tabbed.contains(&thread_id)
                    && !self.contents.tabbed_threads.contains(&thread_id));
            self.selection = (!was_closed)
                .then(|| {
                    self.contents
                        .entries
                        .iter()
                        .position(|entry| entry_identity(entry) == Some(identity))
                })
                .flatten();
        }
        // Clamp a selection that points past the rebuilt list (or whose row is
        // gone entirely).
        if let Some(ix) = self.selection
            && ix >= self.contents.entries.len()
        {
            self.selection = self
                .contents
                .entries
                .len()
                .checked_sub(1)
                .and_then(|last| self.previous_selectable(last));
        }

        // Preserve measurements for unchanged entries.
        self.apply_list_state_diff(&previous_shapes);

        if had_notifications != self.has_notifications(cx) {
            multi_workspace.update(cx, |_, cx| {
                cx.notify();
            });
        }

        cx.notify();
    }

    /// Keeps the [`GhStatusStore`] watch set in sync with the branches of the
    /// listed thread entries (collapsed rows included: the thread view's PR
    /// badges read the store the sidebar keeps watched). Watches are
    /// refcounted, so each branch is watched exactly once by the sidebar and
    /// released when its thread disappears from the list.
    fn sync_gh_watches(&mut self, cx: &mut Context<Self>) {
        let Some(store) = GhStatusStore::try_global(cx) else {
            return;
        };
        let desired: HashSet<(PathBuf, String)> = self
            .contents
            .all_entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(thread) if !thread.metadata.archived => Some(thread),
                _ => None,
            })
            .flat_map(|thread| {
                thread.worktrees.iter().filter_map(|worktree| {
                    let branch = worktree.branch_name.as_ref()?;
                    Some((
                        PathBuf::from(worktree.full_path.as_ref()),
                        branch.to_string(),
                    ))
                })
            })
            .collect();
        if desired == self.gh_watched_branches {
            return;
        }
        store.update(cx, |store, cx| {
            for (repo_path, branch) in desired.difference(&self.gh_watched_branches) {
                store.watch(repo_path.clone(), branch.clone(), cx);
            }
            for (repo_path, branch) in self.gh_watched_branches.difference(&desired) {
                store.unwatch(repo_path, branch, cx);
            }
        });
        self.gh_watched_branches = desired;
    }

    /// The section a rendered row belongs to, found by the nearest preceding
    /// section header. Rows in [`SidebarSection::OpenInZed`] are open as tabs,
    /// so only they expose a close-the-tab affordance.
    fn section_of_entry(&self, ix: usize) -> Option<SidebarSection> {
        self.contents
            .entries
            .iter()
            .take(ix.saturating_add(1))
            .rev()
            .find_map(|entry| match entry {
                ListEntry::SectionHeader(section) => Some(*section),
                _ => None,
            })
    }

    /// Closes a thread's open tab in its workspace's panel, the same effect as
    /// closing the tab in the thread pane, without archiving or deleting it.
    fn close_thread_tab(
        &mut self,
        thread_id: agent_ui::ThreadId,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        let pane = panel.read(cx).thread_pane().clone();
        let item_id = pane.read(cx).items().find_map(|item| {
            let tab = item.downcast::<agent_ui::thread_tab::ThreadTab>()?;
            (tab.read(cx).thread_id(cx) == thread_id).then(|| item.item_id())
        });
        if let Some(item_id) = item_id {
            pane.update(cx, |pane, cx| {
                pane.remove_item(item_id, false, false, window, cx);
            });
        }
    }

    /// The branches of a thread's worktrees, as `(repo path, branch)`.
    fn thread_branches(thread: &ThreadEntry) -> Vec<(&Path, &str)> {
        // A draft has no worktree of its own yet; its paths still resolve to the
        // project's current branch, so treating it as branchless keeps the
        // project branch's PRs (and gh watches, and snapshots) off the draft row.
        if thread.draft.is_some() {
            return Vec::new();
        }
        thread
            .worktrees
            .iter()
            .filter_map(|worktree| {
                let branch = worktree.branch_name.as_ref()?;
                Some((Path::new(worktree.full_path.as_ref()), branch.as_ref()))
            })
            .collect()
    }

    /// PR chips for a thread row: the union of the PRs of all its worktree
    /// branches, deduplicated by URL. Rows with a branch but no PR get a
    /// muted "no PR" indicator so PR state is always visible.
    fn thread_pr_chips(thread: &ThreadEntry, cx: &App) -> Vec<ThreadItemPrChip> {
        let store = GhStatusStore::try_global(cx);
        let store = store.as_ref().map(|store| store.read(cx));
        let branches = Self::thread_branches(thread);
        let mut chips = gh_status::pr_chips_for_branches(branches, store);

        // An archived thread's worktree is gone from disk, so its branch cannot
        // be resolved and gh has nothing to query. Fall back to the PR state
        // persisted while the thread was live, so a merged PR still reads as
        // merged. This also covers a live thread whose first fetch is pending.
        if chips.iter().all(|chip| chip.url.is_none()) {
            let snapshot = ThreadMetadataStore::try_global(cx).and_then(|store| {
                store
                    .read(cx)
                    .pr_snapshot(thread.metadata.thread_id)
                    .cloned()
            });
            if let Some(snapshot) = snapshot.filter(|snapshot| !snapshot.prs.is_empty()) {
                return gh_status::pr_chips_for_prs(snapshot.prs.iter());
            }
        }

        // Absent PR state is still state: a row without a branch (so without a
        // PR) says so rather than dropping the badge and changing shape once
        // the branch and PR arrive.
        if chips.is_empty() {
            chips.push(gh_status::no_pr_chip());
        }
        chips
    }

    /// Persists the PR state of every live thread whose branches gh has
    /// answered for, so the badge survives archiving (which deletes the
    /// worktree, and with it the branch the PR was queried by).
    fn persist_pr_snapshots(&mut self, cx: &mut Context<Self>) {
        let (Some(gh_store), Some(metadata_store)) = (
            GhStatusStore::try_global(cx),
            ThreadMetadataStore::try_global(cx),
        ) else {
            return;
        };

        let mut snapshots: Vec<(ThreadId, ThreadPrSnapshot)> = Vec::new();
        gh_store.read_with(cx, |gh_store, _cx| {
            for entry in &self.contents.all_entries {
                let ListEntry::Thread(thread) = entry else {
                    continue;
                };
                if thread.metadata.archived {
                    continue;
                }
                let branches = Self::thread_branches(thread);
                if branches.is_empty() {
                    continue;
                }
                let branch_names = branches
                    .iter()
                    .map(|(_, branch)| SharedString::from(branch.to_string()))
                    .collect();
                let Some(prs) = gh_status::fetched_prs_for_branches(branches, gh_store) else {
                    continue;
                };
                snapshots.push((
                    thread.metadata.thread_id,
                    ThreadPrSnapshot {
                        branches: branch_names,
                        prs,
                    },
                ));
            }
        });

        if snapshots.is_empty() {
            return;
        }
        metadata_store.update(cx, |store, cx| {
            for (thread_id, snapshot) in snapshots {
                store.set_pr_snapshot(thread_id, snapshot, cx);
            }
        });
    }

    /// Splices only the changed entry range, leaving unchanged item measurements intact.
    fn apply_list_state_diff(&self, previous_shapes: &[EntryShape]) {
        let mut new_iter = self.entry_shapes();
        let mut prefix_len = 0;
        let leading_new = loop {
            match (previous_shapes.get(prefix_len), new_iter.next()) {
                (Some(prev), Some(next)) if *prev == next => prefix_len += 1,
                (None, None) => return,
                (_, leading) => break leading,
            }
        };

        let new_tail: Vec<EntryShape> = leading_new.into_iter().chain(new_iter).collect();
        let prev_tail = &previous_shapes[prefix_len..];
        let suffix_len = prev_tail
            .iter()
            .rev()
            .zip(new_tail.iter().rev())
            .take_while(|(prev, next)| prev == next)
            .count();

        let old_changed = prefix_len..previous_shapes.len() - suffix_len;
        let new_changed_count = new_tail.len() - suffix_len;
        self.list_state.splice(old_changed, new_changed_count);
    }

    fn entry_shapes<'a>(&'a self) -> impl Iterator<Item = EntryShape> + 'a {
        self.contents.entries.iter().map(|entry| match entry {
            ListEntry::SectionHeader(section) => EntryShape::SectionHeader(*section),
            ListEntry::WorkspaceHeader(header) => EntryShape::WorkspaceHeader(header.label.clone()),
            ListEntry::Thread(thread) => EntryShape::Thread(thread.metadata.thread_id),
            ListEntry::Terminal(terminal) => EntryShape::Terminal(terminal.metadata.terminal_id),
        })
    }

    /// Detects drafts that just went from empty back to having content and
    /// refreshes their interaction time to now, so a re-filled draft sorts to
    /// the top of the list instead of falling back to its original creation time.
    fn refresh_refilled_draft_times(&mut self, cx: &mut Context<Self>) {
        let mut new_kinds: HashMap<ThreadId, DraftKind> = HashMap::new();
        let mut refilled: Vec<ThreadId> = Vec::new();

        for entry in &self.contents.all_entries {
            let ListEntry::Thread(thread) = entry else {
                continue;
            };
            let Some(kind) = thread.draft else {
                continue;
            };
            let thread_id = thread.metadata.thread_id;

            if kind == DraftKind::WithContent
                && self.draft_kinds.get(&thread_id) == Some(&DraftKind::Empty)
            {
                refilled.push(thread_id);
            }
            new_kinds.insert(thread_id, kind);
        }
        self.draft_kinds = new_kinds;

        if refilled.is_empty() {
            return;
        }

        let now = Utc::now();

        ThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            for thread_id in refilled {
                store.update_interacted_at(&thread_id, now, store_cx);
            }
        });
    }

    /// Re-establishes subscriptions to each visible draft's message editor
    /// so we rebuild entries (and their displayed titles) as the user types.
    fn refresh_draft_editor_observations(&mut self, cx: &mut Context<Self>) {
        self._draft_editor_observations.clear();
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let draft_conversation_views: Vec<Entity<agent_ui::ConversationView>> = multi_workspace
            .read(cx)
            .workspaces()
            .filter_map(|ws| ws.read(cx).panel::<AgentPanel>(cx))
            .flat_map(|panel| panel.read(cx).conversation_views())
            .collect();

        for cv in draft_conversation_views {
            if let Some(thread_view) = cv.read(cx).active_thread() {
                let editor = thread_view.read(cx).message_editor.clone();
                self._draft_editor_observations.push(cx.subscribe(
                    &editor,
                    |this, _editor, event, cx| match event {
                        MessageEditorEvent::Edited => this.rebuild_after_typing(cx),
                        _ => (),
                    },
                ));
            }
            // Also subscribe to the ConversationView itself so that editor
            // replacements during lifecycle transitions (Loading →
            // Connected) re-wire the editor observation above.
            self._draft_editor_observations.push(cx.subscribe(
                &cv,
                |this, _cv, _event: &StateChange, cx| {
                    this.schedule_update_entries(false, cx);
                },
            ));
        }
    }

    fn select_first_entry(&mut self) {
        self.selection = self
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_) | ListEntry::Terminal(_)))
            .or_else(|| {
                if self.contents.entries.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
    }

    fn render_list_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.contents.entries.get(ix) else {
            return div().into_any_element();
        };
        let is_focused = self.focus_handle.is_focused(window);
        // is_selected means the keyboard selector is here.
        let is_selected = is_focused && self.selection == Some(ix);

        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|active| active.matches_entry(entry));

        match entry {
            ListEntry::SectionHeader(section) => self.render_section_header(*section, ix, cx),
            ListEntry::WorkspaceHeader(header) => self.render_workspace_header(ix, header, cx),
            ListEntry::Thread(thread) => self.render_thread(ix, thread, is_active, is_selected, cx),
            ListEntry::Terminal(terminal) => {
                self.render_terminal(ix, terminal, is_active, is_selected, cx)
            }
        }
    }

    /// A quiet worktree label above one workspace's rows, presentation only.
    /// The extra top padding is the gap between worktrees; rows within a
    /// worktree sit tighter than that.
    fn render_workspace_header(
        &self,
        ix: usize,
        header: &WorkspaceHeaderEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = SharedString::from(format!("workspace-header-{ix}"));
        let is_collapsed = self.collapsed_worktrees.contains(&header.key);
        // PR state belongs to the worktree (PR == branch == worktree); its rows
        // carry the chip themselves, and while they are visible the header's
        // copy would only stack a second identical chip on top. Once the group
        // folds those rows away the header carries the chip again, so the fold
        // is not also a way of losing sight of a failing PR.
        let pr_chips = if is_collapsed {
            header
                .lead_thread
                .as_ref()
                .map(|thread| Self::thread_pr_chips(thread, cx))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let member_sessions = header.member_sessions.clone();

        h_flex()
            .id(("workspace-header", ix))
            .group(group.clone())
            .w_full()
            .px_2p5()
            .pt_3()
            .pb_0p5()
            .gap_1()
            .items_center()
            // The whole header folds its group: a worktree with a dozen
            // threads is otherwise a wall of rows between you and the next
            // one. The chevron replaces the branch glyph rather than crowding
            // it, both because a folded group has to say so at a glance and
            // because the glyph was decoration on a row that is now a control.
            .cursor_pointer()
            .tooltip(Tooltip::text(if is_collapsed {
                "Show This Worktree's Threads"
            } else {
                "Hide This Worktree's Threads"
            }))
            .on_click(cx.listener({
                let key = header.key.clone();
                move |this, _, _window, cx| {
                    if !this.collapsed_worktrees.remove(&key) {
                        this.collapsed_worktrees.insert(key.clone());
                    }
                    this.update_entries(cx);
                    cx.notify();
                }
            }))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(header.label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Default)
                    .truncate(),
            )
            // A folded group says how much it is holding, so the fold is not
            // a way of losing threads.
            .when(is_collapsed, |this| {
                this.child(
                    Label::new(format!(
                        "{} thread{}",
                        header.member_count,
                        if header.member_count == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
            // What this worktree costs to keep. Only shown once measured, and
            // only when it is enough to matter.
            .children(
                header
                    .path
                    .as_ref()
                    .and_then(|path| self.worktree_sizes.get(path))
                    .copied()
                    .and_then(worktree_size_label)
                    .map(|size| {
                        Label::new(size)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element()
                    }),
            )
            .child(h_flex().flex_1())
            .children(
                pr_chips
                    .into_iter()
                    .enumerate()
                    .map(|(chip_ix, chip)| ui::PrChip::new(("workspace-header-pr", chip_ix), chip)),
            )
            // Start a new thread in THIS worktree (not a new one).
            .when_some(header.workspace.clone(), |this, workspace| {
                this.child(
                    IconButton::new(("new-thread-in-worktree", ix), IconName::Plus)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .visible_on_hover(group.clone())
                        .tooltip(Tooltip::text("New Thread in This Worktree"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.new_thread_in_worktree(&workspace, window, cx);
                        })),
                )
            })
            // Archiving belongs to the worktree, not its threads: the hover
            // button archives every thread in the group, and the last one's
            // archival tears the linked worktree down.
            .when(
                header.is_linked_worktree && !member_sessions.is_empty(),
                |this| {
                    this.child(
                        IconButton::new(("archive-workspace", ix), IconName::Archive)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .visible_on_hover(group)
                            .tooltip(Tooltip::text("Archive Worktree"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                for session_id in member_sessions.clone() {
                                    this.archive_thread(&session_id, window, cx);
                                }
                            })),
                    )
                },
            )
            .into_any_element()
    }

    fn render_section_header(
        &self,
        section: SidebarSection,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The history section sits below the Active group; a full-width top
        // divider plus extra spacing sets the two apart while staying quiet.
        let is_history = matches!(
            section,
            SidebarSection::AllThreads | SidebarSection::Archived
        );
        let is_open = !self.collapsed_sections.contains(&section);
        h_flex()
            .id(("section-header", ix))
            .w_full()
            .px_2p5()
            .gap_1()
            .cursor_pointer()
            .map(|this| if ix > 0 { this.pt_5() } else { this.pt_2() })
            .when(is_history, |this| {
                this.mt_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
            })
            .pb_2()
            .child(
                Disclosure::new(("section-disclosure", ix), is_open).on_click(cx.listener(
                    move |this, _, _window, cx| {
                        // The whole header row toggles too; without this the
                        // click would bubble and toggle a second time.
                        cx.stop_propagation();
                        this.toggle_section(section, cx);
                    },
                )),
            )
            .child(
                Label::new(section.label())
                    .size(LabelSize::Default)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Default),
            )
            .when(matches!(section, SidebarSection::OpenInZed), |this| {
                this.child(div().flex_1())
                    .child(self.render_new_thread_button(cx))
            })
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_section(section, cx);
            }))
            .into_any_element()
    }

    fn toggle_section(&mut self, section: SidebarSection, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(&section) {
            self.collapsed_sections.insert(section);
        }
        self.update_entries(cx);
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    /// The sidebar's plus button. One click opens a draft; the draft screen
    /// is where the worktree, agent, and model are chosen.
    fn render_new_thread_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus_handle = self.focus_handle.clone();
        // The top plus starts a thread in a brand-new worktree, created right
        // away. A thread in an existing worktree comes from that worktree
        // header's own plus.
        IconButton::new("sidebar-new-thread", IconName::Plus)
            .icon_size(IconSize::Small)
            .tooltip(move |_, cx| {
                Tooltip::for_action_in("New Thread in New Worktree", &NewThread, &focus_handle, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selection = None;
                let Some(workspace) = this.active_workspace(cx) else {
                    return;
                };
                if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.create_new_worktree_thread(window, cx);
                    });
                    workspace.update(cx, |workspace, cx| {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    });
                }
            }))
            .into_any_element()
    }

    fn dispatch_context(&self, window: &Window, cx: &Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("ThreadsSidebar");
        dispatch_context.add("menu");

        let is_renaming_thread = self
            .thread_rename_editor
            .focus_handle(cx)
            .is_focused(window);

        let identifier = if self.filter_editor.focus_handle(cx).is_focused(window) {
            "searching"
        } else if is_renaming_thread {
            "editing"
        } else {
            "not_searching"
        };

        dispatch_context.add(identifier);
        dispatch_context
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }

        if self.selection.is_none() {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming_thread_id.is_some() {
            self.cancel_thread_rename(window, cx);
            return;
        }

        if self.filter_editor.read(cx).is_focused(window) {
            if self.reset_filter_editor_text(window, cx) {
                self.selection = None;
                self.update_entries(cx);
                return;
            }

            if self.selection.is_none() {
                self.select_first_entry();
            }
            if self.selection.is_some() {
                self.focus_handle.focus(window, cx);
                cx.notify();
            }
            return;
        }

        if self.reset_filter_editor_text(window, cx) {
            self.update_entries(cx);
        } else {
            self.selection = None;
            self.filter_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn focus_sidebar_filter(
        &mut self,
        _: &FocusSidebarFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = None;
        self.filter_editor.focus_handle(cx).focus(window, cx);

        cx.notify();
    }

    fn reset_filter_editor_text(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.filter_editor.update(cx, |editor, cx| {
            if editor.buffer().read(cx).len(cx).0 > 0 {
                editor.set_text("", window, cx);
                true
            } else {
                false
            }
        })
    }

    fn has_filter_query(&self, cx: &App) -> bool {
        !self.filter_editor.read(cx).text(cx).is_empty()
    }

    fn start_renaming_thread(
        &mut self,
        ix: usize,
        thread_id: ThreadId,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming_thread_id.is_some() && self.renaming_thread_id != Some(thread_id) {
            self.finish_thread_rename(window, cx);
        }

        self.selection = Some(ix);
        self.renaming_thread_id = Some(thread_id);
        self.list_state.scroll_to_reveal_item(ix);
        self.thread_rename_editor.update(cx, |editor, cx| {
            editor.set_text(title, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn handle_thread_rename_editor_event(
        &mut self,
        title_editor: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = title_editor;
        // Only the end of a rename is a rename. Applying every keystroke wrote
        // a title per character, and each write rebuilt the sidebar's entries
        // underneath the editor receiving them, which is how a rename ended
        // itself on its first letter and left the focus in the search field.
        if matches!(event, editor::EditorEvent::Blurred) {
            self.finish_thread_rename(window, cx);
        }
    }

    fn apply_thread_rename(
        &mut self,
        thread_id: ThreadId,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut found = false;
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
            for workspace in workspaces {
                if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    if let Some(view) = agent_panel
                        .read(cx)
                        .conversation_view_for_id(&thread_id, cx)
                        && let Some(thread_view) = view.read(cx).root_thread_view()
                    {
                        thread_view.update(cx, |thread_view, cx| {
                            thread_view.rename(title.clone(), window, cx);
                        });
                        found = true;
                    }
                }
            }
        }

        if !found {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.set_title_override(thread_id, title, cx);
            });
        }
    }

    /// Ends a rename, keeping what was typed. Nothing has been applied until
    /// now: the title is written once, here.
    fn finish_thread_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(thread_id) = self.renaming_thread_id.take() else {
            return false;
        };
        let title = self.thread_rename_editor.read(cx).text(cx);
        let title = title.trim();
        // An empty title is not a rename, it is a mistake; the thread keeps the
        // name it had.
        if !title.is_empty() {
            self.apply_thread_rename(thread_id, SharedString::from(title.to_string()), window, cx);
        }
        self.focus_handle.focus(window, cx);
        self.update_entries(cx);
        true
    }

    /// Ends a rename, discarding what was typed.
    fn cancel_thread_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.renaming_thread_id.take().is_none() {
            return false;
        }
        self.focus_handle.focus(window, cx);
        self.update_entries(cx);
        true
    }

    fn editor_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select_next(&SelectNext, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select_previous(&SelectPrevious, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_none() {
            self.select_next(&SelectNext, window, cx);
        }
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Bucket headers are presentation-only; keyboard selection skips them.
    fn is_selectable_entry(&self, ix: usize) -> bool {
        matches!(
            self.contents.entries.get(ix),
            Some(ListEntry::Thread(_) | ListEntry::Terminal(_))
        )
    }

    fn next_selectable(&self, start: usize) -> Option<usize> {
        (start..self.contents.entries.len()).find(|&ix| self.is_selectable_entry(ix))
    }

    fn previous_selectable(&self, start: usize) -> Option<usize> {
        (0..=start).rev().find(|&ix| self.is_selectable_entry(ix))
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let next = match self.selection {
            Some(ix) => self
                .next_selectable(ix + 1)
                .or_else(|| self.next_selectable(0)),
            None => self.next_selectable(0),
        };
        if let Some(next) = next {
            self.selection = Some(next);
            self.list_state.scroll_to_reveal_item(next);
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection {
            Some(ix) => {
                if let Some(prev) = ix
                    .checked_sub(1)
                    .and_then(|start| self.previous_selectable(start))
                {
                    self.selection = Some(prev);
                    self.list_state.scroll_to_reveal_item(prev);
                } else {
                    self.selection = None;
                    self.filter_editor.focus_handle(cx).focus(window, cx);
                }
                cx.notify();
            }
            None => {
                if let Some(last) = self
                    .contents
                    .entries
                    .len()
                    .checked_sub(1)
                    .and_then(|last| self.previous_selectable(last))
                {
                    self.selection = Some(last);
                    self.list_state.scroll_to_reveal_item(last);
                    cx.notify();
                }
            }
        }
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(first) = self.next_selectable(0) {
            self.selection = Some(first);
            self.list_state.scroll_to_reveal_item(first);
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self
            .contents
            .entries
            .len()
            .checked_sub(1)
            .and_then(|last| self.previous_selectable(last))
        {
            self.selection = Some(last);
            self.list_state.scroll_to_reveal_item(last);
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.finish_thread_rename(window, cx) {
            return;
        }

        let Some(ix) = self.selection else { return };
        let Some(entry) = self.contents.entries.get(ix) else {
            return;
        };

        match entry {
            ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => {}
            ListEntry::Thread(thread) => {
                let metadata = thread.metadata.clone();
                match &thread.workspace {
                    ThreadEntryWorkspace::Open(workspace) => {
                        let workspace = workspace.clone();
                        self.activate_thread(metadata, &workspace, false, window, cx);
                    }
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        let folder_paths = folder_paths.clone();
                        let project_group_key = project_group_key.clone();
                        self.open_workspace_and_activate_thread(
                            metadata,
                            folder_paths,
                            &project_group_key,
                            window,
                            cx,
                        );
                    }
                }
            }
            ListEntry::Terminal(terminal) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.activate_terminal_entry(metadata, workspace, false, window, cx);
            }
        }
    }

    fn find_workspace_across_windows(
        &self,
        cx: &App,
        predicate: impl Fn(&Entity<Workspace>, &App) -> bool,
    ) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
        cx.windows()
            .into_iter()
            .filter_map(|window| window.downcast::<MultiWorkspace>())
            .find_map(|window| {
                let workspace = window.read(cx).ok().and_then(|multi_workspace| {
                    multi_workspace
                        .workspaces()
                        .find(|workspace| predicate(workspace, cx))
                        .cloned()
                })?;
                Some((window, workspace))
            })
    }

    fn find_workspace_in_current_window(
        &self,
        cx: &App,
        predicate: impl Fn(&Entity<Workspace>, &App) -> bool,
    ) -> Option<Entity<Workspace>> {
        self.multi_workspace.upgrade().and_then(|multi_workspace| {
            multi_workspace
                .read(cx)
                .workspaces()
                .find(|workspace| predicate(workspace, cx))
                .cloned()
        })
    }

    fn load_agent_thread_in_workspace(
        workspace: &Entity<Workspace>,
        metadata: &ThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let load_thread = |agent_panel: Entity<AgentPanel>,
                           metadata: &ThreadMetadata,
                           focus: bool,
                           window: &mut Window,
                           cx: &mut App| {
            agent_panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    Agent::from(metadata.agent_id.clone()),
                    metadata.thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                    focus,
                    AgentThreadSource::Sidebar,
                    window,
                    cx,
                );
            });
        };

        let mut existing_panel = None;
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                existing_panel = Some(panel);
            }
        });

        // Threads live as tabs in the agent panel's own pane, so opening one
        // also reveals (or focuses) the panel dock.
        if let Some(agent_panel) = existing_panel {
            load_thread(agent_panel, metadata, focus, window, cx);
            workspace.update(cx, |workspace, cx| {
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            });
            return;
        }

        let workspace = workspace.downgrade();
        let metadata = metadata.clone();
        let mut async_window_cx = window.to_async(cx);
        cx.spawn(async move |_cx| {
            let panel = AgentPanel::load(workspace.clone(), async_window_cx.clone()).await?;

            workspace.update_in(&mut async_window_cx, |workspace, window, cx| {
                let panel = workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
                    workspace.add_panel(panel.clone(), window, cx);
                    panel.clone()
                });
                load_thread(panel, &metadata, focus, window, cx);
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_closed_native_thread_as_markdown(
        session_id: &acp::SessionId,
        title: Option<SharedString>,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let thread_store = ThreadStore::global(cx);
        let load_task =
            thread_store.update(cx, |store, cx| store.load_thread(session_id.clone(), cx));

        let thread_title = title
            .map(|t| t.to_string())
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.to_string());

        let workspace = workspace.clone();

        window
            .spawn(cx, async move |cx| {
                let db_thread = load_task.await?;
                let Some(db_thread) = db_thread else {
                    anyhow::bail!("Thread not found in database");
                };

                let markdown = db_thread.to_markdown();

                cx.update(|window, cx| {
                    agent_ui::open_markdown_in_workspace(
                        thread_title,
                        markdown,
                        workspace,
                        window,
                        cx,
                    )
                })?
                .await
            })
            .detach_and_log_err(cx);
    }

    fn show_thread_title_toast(workspace: Entity<Workspace>, message: &'static str, cx: &mut App) {
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn show_no_thread_summary_model_toast(workspace: Entity<Workspace>, cx: &mut App) {
        Self::show_thread_title_toast(
            workspace,
            "No model is configured for summarizing titles.",
            cx,
        );
    }

    fn regenerate_thread_title(
        &mut self,
        session_id: &acp::SessionId,
        thread_id: ThreadId,
        folder_paths: PathList,
        thread_workspace: Option<Entity<Workspace>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = thread_workspace
            .as_ref()
            .and_then(|w| w.read(cx).panel::<AgentPanel>(cx))
        {
            match panel.update(cx, |panel, cx| panel.regenerate_thread_title(thread_id, cx)) {
                ThreadTitleRegenerationResult::Started
                | ThreadTitleRegenerationResult::AlreadyGenerating => return,
                ThreadTitleRegenerationResult::NoModel => {
                    if let Some(workspace) = self.active_workspace(cx) {
                        Self::show_no_thread_summary_model_toast(workspace, cx);
                    }
                    return;
                }
                ThreadTitleRegenerationResult::NotOpen => {}
            }
        }

        let Some(configured_model) =
            LanguageModelRegistry::read_global(cx).thread_summary_model(cx)
        else {
            if let Some(workspace) = self.active_workspace(cx) {
                Self::show_no_thread_summary_model_toast(workspace, cx);
            }
            return;
        };

        if !self.regenerating_titles.insert(thread_id) {
            return;
        }

        let model = configured_model.model;
        let temperature = AgentSettings::temperature_for_model(&model, cx);

        let thread_store = ThreadStore::global(cx);
        let load_task =
            thread_store.update(cx, |store, cx| store.load_thread(session_id.clone(), cx));
        let session_id = session_id.clone();

        cx.notify();

        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<SharedString> = async {
                let Some(db_thread) = load_task.await? else {
                    anyhow::bail!("Thread not found in database");
                };

                let request = agent::build_thread_title_request(&db_thread.messages, temperature);
                let title =
                    SharedString::from(agent::stream_thread_title(model, request, cx).await?);

                let Some(mut db_thread) = thread_store
                    .update(cx, |store, cx| store.load_thread(session_id.clone(), cx))
                    .await?
                else {
                    anyhow::bail!("Thread not found in database");
                };
                db_thread.title = title.clone();

                thread_store
                    .update(cx, |store, cx| {
                        store.save_thread(session_id, db_thread, folder_paths, cx)
                    })
                    .await?;

                anyhow::Ok(title)
            }
            .await;

            this.update(cx, |this, cx| {
                this.regenerating_titles.remove(&thread_id);
                match &result {
                    Ok(title) => {
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            store.set_generated_title(thread_id, title.clone(), cx);
                        });
                    }
                    Err(_) => {
                        if let Some(workspace) = this.active_workspace(cx) {
                            Self::show_thread_title_toast(
                                workspace,
                                "Failed to regenerate the title.",
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            })
            .ok();

            result.map(|_| ())
        })
        .detach_and_log_err(cx);
    }

    fn is_thread_active_in_workspace(
        &self,
        thread_id: &ThreadId,
        workspace: &Entity<Workspace>,
        cx: &App,
    ) -> bool {
        self.active_workspace(cx).as_ref() == Some(workspace)
            && self.active_entry.as_ref().is_some_and(|entry| {
                entry.is_active_thread(thread_id) && entry.workspace() == workspace
            })
    }

    /// Test-only: forces `active_entry` to point at a thread, reproducing the
    /// stale state (a restored or stuck-pending activation whose tab no longer
    /// exists) that the `activate_thread_locally` fast path must not trust.
    #[cfg(test)]
    pub(crate) fn set_stale_thread_active_entry_for_test(
        &mut self,
        thread_id: agent_ui::ThreadId,
        session_id: Option<acp::SessionId>,
        workspace: Entity<Workspace>,
    ) {
        self.active_entry = Some(ActiveEntry::Thread {
            thread_id,
            session_id,
            workspace,
        });
    }

    fn activate_thread_locally(
        &mut self,
        metadata: &ThreadMetadata,
        workspace: &Entity<Workspace>,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        if self.is_thread_active_in_workspace(&metadata.thread_id, workspace, cx) {
            // "Active" here trusts a possibly-stale active_entry: it can still
            // point at a thread whose tab was closed, or one from a restored
            // session whose view hasn't rehydrated. activate_thread_tab reports
            // whether a tab actually hosts the thread; when it doesn't, fall
            // through to the load path below so the thread reopens instead of
            // silently no-op'ing.
            let thread_id = metadata.thread_id;
            let activated = workspace.update(cx, |workspace, cx| {
                workspace.focus_panel::<AgentPanel>(window, cx);
                workspace.panel::<AgentPanel>(cx).is_some_and(|panel| {
                    panel.update(cx, |panel, cx| {
                        panel.activate_thread_tab(thread_id, true, window, cx)
                    })
                })
            });
            if activated {
                return;
            }
        }

        // Set active_entry eagerly so the sidebar highlight updates
        // immediately, rather than waiting for a deferred AgentPanel
        // event which can race with ActiveWorkspaceChanged clearing it.
        self.active_entry = Some(ActiveEntry::Thread {
            thread_id: metadata.thread_id,
            session_id: metadata.session_id.clone(),
            workspace: workspace.clone(),
        });
        self.record_thread_access(&metadata.thread_id);
        self.pending_thread_activation = Some(metadata.thread_id);

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if retain {
                multi_workspace.retain_active_workspace(cx);
            }
        });

        Self::load_agent_thread_in_workspace(workspace, metadata, true, window, cx);

        self.update_entries(cx);
    }

    fn activate_thread_in_other_window(
        &self,
        metadata: ThreadMetadata,
        workspace: Entity<Workspace>,
        target_window: WindowHandle<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let target_session_id = metadata.session_id.clone();
        let metadata_thread_id = metadata.thread_id;
        let workspace_for_entry = workspace.clone();

        let activated = target_window
            .update(cx, |multi_workspace, window, cx| {
                window.activate_window();
                multi_workspace.activate(workspace.clone(), None, window, cx);
                Self::load_agent_thread_in_workspace(&workspace, &metadata, true, window, cx);
            })
            .log_err()
            .is_some();

        if activated {
            if let Some(target_sidebar) = target_window
                .read(cx)
                .ok()
                .and_then(|multi_workspace| {
                    multi_workspace.sidebar().map(|sidebar| sidebar.to_any())
                })
                .and_then(|sidebar| sidebar.downcast::<Self>().ok())
            {
                target_sidebar.update(cx, |sidebar, cx| {
                    sidebar.pending_thread_activation = Some(metadata_thread_id);
                    sidebar.active_entry = Some(ActiveEntry::Thread {
                        thread_id: metadata_thread_id,
                        session_id: target_session_id.clone(),
                        workspace: workspace_for_entry.clone(),
                    });
                    sidebar.record_thread_access(&metadata_thread_id);
                    sidebar.update_entries(cx);
                });
            }
        }
    }

    fn activate_thread(
        &mut self,
        metadata: ThreadMetadata,
        workspace: &Entity<Workspace>,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .find_workspace_in_current_window(cx, |candidate, _| candidate == workspace)
            .is_some()
        {
            self.activate_thread_locally(&metadata, &workspace, retain, window, cx);
            return;
        }

        let Some((target_window, workspace)) =
            self.find_workspace_across_windows(cx, |candidate, _| candidate == workspace)
        else {
            return;
        };

        self.activate_thread_in_other_window(metadata, workspace, target_window, cx);
    }

    fn open_workspace_and_activate_thread(
        &mut self,
        metadata: ThreadMetadata,
        folder_paths: PathList,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let pending_thread_id = metadata.thread_id;
        // Mark the pending thread activation so rebuild_contents
        // preserves the Thread active_entry during loading and
        // reconciliation cannot synthesize an empty fallback draft.
        self.pending_thread_activation = Some(pending_thread_id);

        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            // Dismiss the modal as soon as the open attempt completes so
            // failures or cancellations do not leave a stale connection modal behind.
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);

            if result.is_err() {
                this.update(cx, |this, _cx| {
                    if this.pending_thread_activation == Some(pending_thread_id) {
                        this.pending_thread_activation = None;
                    }
                })
                .ok();
            }

            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                this.activate_thread(metadata, &workspace, false, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn find_current_workspace_for_path_list(
        &self,
        path_list: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        cx: &App,
    ) -> Option<Entity<Workspace>> {
        self.find_workspace_in_current_window(cx, |workspace, cx| {
            workspace_path_list(workspace, cx).paths() == path_list.paths()
                && same_remote_connection_identity(
                    workspace
                        .read(cx)
                        .project()
                        .read(cx)
                        .remote_connection_options(cx)
                        .as_ref(),
                    remote_connection,
                )
        })
    }

    fn find_open_workspace_for_path_list(
        &self,
        path_list: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        cx: &App,
    ) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
        self.find_workspace_across_windows(cx, |workspace, cx| {
            workspace_path_list(workspace, cx).paths() == path_list.paths()
                && same_remote_connection_identity(
                    workspace
                        .read(cx)
                        .project()
                        .read(cx)
                        .remote_connection_options(cx)
                        .as_ref(),
                    remote_connection,
                )
        })
    }

    fn open_thread_from_archive(
        &mut self,
        metadata: ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = metadata.thread_id;

        if metadata.folder_paths().paths().is_empty() {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.unarchive(thread_id, cx));

            let active_workspace = self
                .multi_workspace
                .upgrade()
                .map(|w| w.read(cx).workspace().clone());

            if let Some(workspace) = active_workspace {
                self.activate_thread_locally(&metadata, &workspace, false, window, cx);
            } else {
                let path_list = metadata.folder_paths().clone();
                if let Some((target_window, workspace)) = self.find_open_workspace_for_path_list(
                    &path_list,
                    metadata.remote_connection.as_ref(),
                    cx,
                ) {
                    self.activate_thread_in_other_window(metadata, workspace, target_window, cx);
                } else {
                    let key = ProjectGroupKey::from_worktree_paths(
                        &metadata.worktree_paths,
                        metadata.remote_connection.clone(),
                    );
                    self.open_workspace_and_activate_thread(metadata, path_list, &key, window, cx);
                }
            }
            return;
        }

        let store = ThreadMetadataStore::global(cx);
        let task = if metadata.archived {
            store
                .read(cx)
                .get_archived_worktrees_for_thread(thread_id, cx)
        } else {
            Task::ready(Ok(Vec::new()))
        };
        let path_list = metadata.folder_paths().clone();

        let restore_task = cx.spawn_in(window, async move |this, cx| {
            let result: anyhow::Result<()> = async {
                let archived_worktrees = task.await?;

                if archived_worktrees.is_empty() {
                    this.update_in(cx, |this, window, cx| {
                        this.restoring_tasks.remove(&thread_id);
                        if metadata.archived {
                            ThreadMetadataStore::global(cx)
                                .update(cx, |store, cx| store.unarchive(thread_id, cx));
                        }

                        if let Some(workspace) = this.find_current_workspace_for_path_list(
                            &path_list,
                            metadata.remote_connection.as_ref(),
                            cx,
                        ) {
                            this.activate_thread_locally(&metadata, &workspace, false, window, cx);
                        } else if let Some((target_window, workspace)) = this
                            .find_open_workspace_for_path_list(
                                &path_list,
                                metadata.remote_connection.as_ref(),
                                cx,
                            )
                        {
                            this.activate_thread_in_other_window(
                                metadata,
                                workspace,
                                target_window,
                                cx,
                            );
                        } else {
                            let key = ProjectGroupKey::from_worktree_paths(
                                &metadata.worktree_paths,
                                metadata.remote_connection.clone(),
                            );
                            this.open_workspace_and_activate_thread(
                                metadata, path_list, &key, window, cx,
                            );
                        }
                    })?;
                    return anyhow::Ok(());
                }

                let mut path_replacements: Vec<(PathBuf, PathBuf)> = Vec::new();
                for row in &archived_worktrees {
                    match thread_worktree_archive::restore_worktree_via_git(
                        row,
                        metadata.remote_connection.as_ref(),
                        &mut *cx,
                    )
                    .await
                    {
                        Ok(restored_path) => {
                            thread_worktree_archive::cleanup_archived_worktree_record(
                                row,
                                metadata.remote_connection.as_ref(),
                                &mut *cx,
                            )
                            .await;
                            path_replacements.push((row.worktree_path.clone(), restored_path));
                        }
                        Err(error) => {
                            log::error!("Failed to restore worktree: {error:#}");
                            this.update_in(cx, |this, _window, cx| {
                                this.restoring_tasks.remove(&thread_id);

                                if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                                    let workspace = multi_workspace.read(cx).workspace().clone();
                                    workspace.update(cx, |workspace, cx| {
                                        struct RestoreWorktreeErrorToast;
                                        workspace.show_toast(
                                            Toast::new(
                                                NotificationId::unique::<RestoreWorktreeErrorToast>(
                                                ),
                                                format!("Failed to restore worktree: {error:#}"),
                                            )
                                            .autohide(),
                                            cx,
                                        );
                                    });
                                }
                            })
                            .ok();
                            return anyhow::Ok(());
                        }
                    }
                }

                if !path_replacements.is_empty() {
                    cx.update(|_window, cx| {
                        store.update(cx, |store, cx| {
                            store.update_restored_worktree_paths(thread_id, &path_replacements, cx);
                        });
                    })?;

                    let updated_metadata =
                        cx.update(|_window, cx| store.read(cx).entry(thread_id).cloned())?;

                    if let Some(updated_metadata) = updated_metadata {
                        let new_paths = updated_metadata.folder_paths().clone();
                        let key = ProjectGroupKey::from_worktree_paths(
                            &updated_metadata.worktree_paths,
                            updated_metadata.remote_connection.clone(),
                        );

                        cx.update(|_window, cx| {
                            store.update(cx, |store, cx| {
                                store.unarchive(updated_metadata.thread_id, cx);
                            });
                        })?;

                        this.update_in(cx, |this, window, cx| {
                            this.restoring_tasks.remove(&thread_id);
                            this.open_workspace_and_activate_thread(
                                updated_metadata,
                                new_paths,
                                &key,
                                window,
                                cx,
                            );
                        })?;
                    }
                }

                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                log::error!("{error:#}");
            }
        });
        self.restoring_tasks.insert(thread_id, restore_task);
    }

    /// Find the neighbor thread in the sidebar (by display position).
    /// Look below first, then above, for the nearest thread that isn't
    /// the one being archived. We capture both the neighbor's metadata
    /// (for activation) and its workspace paths (for the workspace
    /// removal fallback).
    fn neighboring_activatable_entry(
        &self,
        current_position: usize,
        remote_connection: Option<&RemoteConnectionOptions>,
        exclude: Option<EntryIdentity>,
    ) -> Option<ActivatableEntry> {
        let after = self
            .contents
            .entries
            .get(current_position.checked_add(1)?..)?;
        let before = self.contents.entries.get(..current_position)?;
        after
            .iter()
            .chain(before.iter().rev())
            // Archived rows stay in the list but are not activation targets,
            // and neighbors must share the removed entry's remote identity.
            // An open thread now appears in both Active and All Threads, so
            // its other occurrence can be right next to the one being
            // removed — exclude it explicitly rather than picking the same
            // thread as its own "neighbor".
            .filter(|entry| exclude.is_none_or(|exclude| entry_identity(entry) != Some(exclude)))
            .filter(|entry| match entry {
                ListEntry::Thread(thread) => {
                    !thread.metadata.archived
                        && thread.metadata.matches_remote_connection(remote_connection)
                }
                ListEntry::Terminal(terminal) => same_remote_connection_identity(
                    terminal.metadata.remote_connection.as_ref(),
                    remote_connection,
                ),
                ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => false,
            })
            .find_map(ActivatableEntry::from_list_entry)
    }

    fn activate_entry(
        &mut self,
        entry: &ActivatableEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match entry {
            ActivatableEntry::Thread { metadata, .. } => {
                let Some(workspace) = self.multi_workspace.upgrade().and_then(|multi_workspace| {
                    multi_workspace
                        .read(cx)
                        .workspace_for_paths(metadata.folder_paths(), None, cx)
                }) else {
                    return false;
                };

                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.activate_workspace(&workspace, window, cx);
                Self::load_agent_thread_in_workspace(&workspace, metadata, true, window, cx);
                true
            }
            ActivatableEntry::Terminal {
                metadata,
                workspace,
            } => {
                self.activate_terminal_entry(
                    metadata.clone(),
                    workspace.clone(),
                    false,
                    window,
                    cx,
                );
                true
            }
        }
    }

    fn activate_terminal_entry(
        &mut self,
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match workspace {
            ThreadEntryWorkspace::Open(workspace) => {
                self.activate_terminal_in_workspace(&workspace, metadata, retain, window, cx);
            }
            ThreadEntryWorkspace::Closed {
                folder_paths,
                project_group_key,
            } => {
                self.open_workspace_and_activate_terminal(
                    metadata,
                    folder_paths,
                    &project_group_key,
                    window,
                    cx,
                );
            }
        }
    }

    fn load_agent_terminal_in_workspace(
        workspace: &Entity<Workspace>,
        metadata: &TerminalThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let restore_terminal = |agent_panel: Entity<AgentPanel>,
                                metadata: &TerminalThreadMetadata,
                                focus: bool,
                                workspace: Option<&Workspace>,
                                window: &mut Window,
                                cx: &mut App| {
            agent_panel.update(cx, |panel, cx| {
                panel.restore_terminal(
                    metadata.clone(),
                    focus,
                    AgentThreadSource::Sidebar,
                    workspace,
                    window,
                    cx,
                );
            });
        };

        let mut existing_panel = None;
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                existing_panel = Some(panel);
            }
        });

        if let Some(agent_panel) = existing_panel {
            restore_terminal(agent_panel, metadata, focus, None, window, cx);
            workspace.update(cx, |workspace, cx| {
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            });
            return;
        }

        let workspace = workspace.downgrade();
        let metadata = metadata.clone();
        let mut async_window_cx = window.to_async(cx);
        cx.spawn(async move |_cx| {
            let panel = AgentPanel::load(workspace.clone(), async_window_cx.clone()).await?;

            workspace.update_in(&mut async_window_cx, |workspace, window, cx| {
                let panel = workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
                    workspace.add_panel(panel.clone(), window, cx);
                    panel.clone()
                });
                restore_terminal(panel, &metadata, focus, Some(workspace), window, cx);
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn activate_terminal_in_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        metadata: TerminalThreadMetadata,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let terminal_id = metadata.terminal_id;
        self.record_terminal_access(terminal_id);
        self.active_entry = Some(ActiveEntry::Terminal {
            terminal_id,
            workspace: workspace.clone(),
        });

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if retain {
                multi_workspace.retain_active_workspace(cx);
            }
        });

        Self::load_agent_terminal_in_workspace(workspace, &metadata, true, window, cx);

        self.update_entries(cx);
    }

    fn open_workspace_and_activate_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        folder_paths: PathList,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                this.activate_terminal_in_workspace(&workspace, metadata, false, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn should_load_closed_workspace_for_archive(
        &self,
        folder_paths: &PathList,
        project_group_key: &ProjectGroupKey,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_thread_id: Option<ThreadId>,
        except_terminal_id: Option<TerminalId>,
        cx: &App,
    ) -> bool {
        if folder_paths.is_empty() || folder_paths == project_group_key.path_list() {
            return false;
        }

        let archive_workspaces = self.archive_workspaces(cx);

        // No workspace load is needed when a folder path is already a root
        // of an open workspace with a matching remote identity: archive root
        // planning can inspect repositories through that workspace.
        let any_path_open = folder_paths.ordered_paths().any(|path| {
            archive_workspaces.iter().any(|workspace| {
                let project = workspace.read(cx).project().read(cx);
                same_remote_connection_identity(
                    project.remote_connection_options(cx).as_ref(),
                    remote_connection,
                ) && workspace
                    .read(cx)
                    .root_paths(cx)
                    .iter()
                    .any(|root| root.as_ref() == path)
            })
        });
        if any_path_open {
            return false;
        }
        let thread_store = ThreadMetadataStore::global(cx);
        let thread_store = thread_store.read(cx);
        if folder_paths.ordered_paths().any(|path| {
            Self::path_is_referenced_by_unarchived_threads_for_archive(
                &thread_store,
                except_thread_id,
                path,
                remote_connection,
                &archive_workspaces,
                cx,
            )
        }) {
            return false;
        }

        TerminalThreadMetadataStore::try_global(cx).is_none_or(|terminal_store| {
            let terminal_store = terminal_store.read(cx);
            !folder_paths.ordered_paths().any(|path| {
                terminal_store.path_is_referenced_by_terminal(
                    except_terminal_id,
                    path,
                    remote_connection,
                )
            })
        })
    }

    fn path_is_referenced_by_unarchived_threads_for_archive(
        thread_store: &ThreadMetadataStore,
        except_thread_id: Option<ThreadId>,
        path: &Path,
        remote_connection: Option<&RemoteConnectionOptions>,
        archive_workspaces: &[Entity<Workspace>],
        cx: &App,
    ) -> bool {
        thread_store.path_is_referenced_by_unarchived_threads_matching(
            except_thread_id,
            path,
            remote_connection,
            |thread| Self::thread_blocks_worktree_archive(thread, archive_workspaces, cx),
        )
    }

    fn archive_workspaces(&self, cx: &App) -> Vec<Entity<Workspace>> {
        let multi_workspace = self.multi_workspace.upgrade();
        thread_worktree_archive::workspaces_for_archive(multi_workspace.as_ref(), cx)
    }

    fn count_threads_blocking_worktree_archive(
        &self,
        path_list: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_thread_id: Option<ThreadId>,
        cx: &App,
    ) -> usize {
        let archive_workspaces = self.archive_workspaces(cx);
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries_for_path(path_list, remote_connection)
            .filter(|thread| Some(thread.thread_id) != except_thread_id)
            .filter(|thread| Self::thread_blocks_worktree_archive(thread, &archive_workspaces, cx))
            .count()
    }

    fn roots_to_archive_for_paths(
        &self,
        folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_thread_id: Option<ThreadId>,
        except_terminal_id: Option<TerminalId>,
        cx: &App,
    ) -> Vec<thread_worktree_archive::RootPlan> {
        let workspaces = self.archive_workspaces(cx);
        folder_paths
            .ordered_paths()
            .filter_map(|path| {
                thread_worktree_archive::build_root_plan(path, remote_connection, &workspaces, cx)
            })
            .filter(|plan| {
                let store = ThreadMetadataStore::global(cx);
                let store = store.read(cx);
                !Self::path_is_referenced_by_unarchived_threads_for_archive(
                    &store,
                    except_thread_id,
                    plan.root_path.as_path(),
                    remote_connection,
                    &workspaces,
                    cx,
                )
            })
            .filter(|root| {
                TerminalThreadMetadataStore::try_global(cx).is_none_or(|terminal_store| {
                    !terminal_store.read(cx).path_is_referenced_by_terminal(
                        except_terminal_id,
                        root.root_path.as_path(),
                        remote_connection,
                    )
                })
            })
            .collect()
    }

    fn linked_worktree_workspace_to_remove(
        &self,
        folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_thread_id: Option<ThreadId>,
        except_terminal_id: Option<TerminalId>,
        roots_to_archive: &[thread_worktree_archive::RootPlan],
        cx: &App,
    ) -> Option<Entity<Workspace>> {
        if folder_paths.is_empty() {
            return None;
        }

        let remaining = self.count_threads_blocking_worktree_archive(
            folder_paths,
            remote_connection,
            except_thread_id,
            cx,
        );

        if remaining > 0 {
            return None;
        }

        let multi_workspace = self.multi_workspace.upgrade()?;
        let workspace =
            multi_workspace
                .read(cx)
                .workspace_for_paths(folder_paths, remote_connection, cx)?;

        if workspace_has_terminal_metadata_except(&workspace, except_terminal_id, cx) {
            return None;
        }

        if !roots_to_archive.is_empty() {
            let archive_paths: HashSet<&Path> = roots_to_archive
                .iter()
                .map(|root| root.root_path.as_path())
                .collect();
            let project = workspace.read(cx).project().clone();
            let visible_worktree_paths = project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path())
                .collect::<Vec<_>>();
            return (!visible_worktree_paths.is_empty()
                && visible_worktree_paths
                    .iter()
                    .all(|path| archive_paths.contains(path.as_ref())))
            .then_some(workspace);
        }

        let group_key = workspace.read(cx).project_group_key(cx);
        (group_key.path_list() != folder_paths).then_some(workspace)
    }

    fn delete_empty_drafts_for_archive_roots(
        &self,
        roots: &[thread_worktree_archive::RootPlan],
        cx: &mut Context<Self>,
    ) {
        self.delete_empty_drafts_for_archive_targets(
            roots
                .iter()
                .map(|root| (root.root_path.as_path(), root.remote_connection.as_ref())),
            cx,
        );
    }

    fn delete_empty_drafts_for_archive_paths(
        &self,
        paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        cx: &mut Context<Self>,
    ) {
        self.delete_empty_drafts_for_archive_targets(
            paths
                .ordered_paths()
                .map(|path| (path.as_path(), remote_connection)),
            cx,
        );
    }

    fn delete_empty_drafts_for_archive_targets<'a>(
        &self,
        targets: impl IntoIterator<Item = (&'a Path, Option<&'a RemoteConnectionOptions>)>,
        cx: &mut Context<Self>,
    ) {
        let targets = targets.into_iter().collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }

        let archive_workspaces = self.archive_workspaces(cx);
        let draft_thread_ids = ThreadMetadataStore::global(cx)
            .read(cx)
            .unarchived_draft_ids_matching(|thread| {
                targets.iter().any(|(path, remote_connection)| {
                    thread.matches_remote_connection(*remote_connection)
                        && thread.references_folder_path(path)
                }) && !Self::thread_blocks_worktree_archive(thread, &archive_workspaces, cx)
            });
        if draft_thread_ids.is_empty() {
            return;
        }

        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.delete_all(draft_thread_ids, cx);
        });
    }

    fn thread_blocks_worktree_archive(
        thread: &ThreadMetadata,
        archive_workspaces: &[Entity<Workspace>],
        cx: &App,
    ) -> bool {
        if !thread.is_draft() {
            return true;
        }

        agent_ui::draft_prompt_store::draft_has_user_content(
            thread.thread_id,
            archive_workspaces,
            cx,
        )
    }

    async fn wait_for_archive_workspace_metadata(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::AsyncApp,
    ) {
        let scans_complete =
            workspace.read_with(cx, |workspace, cx| workspace.worktree_scans_complete(cx));
        scans_complete.await;

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let barriers = project.update(cx, |project, cx| {
            let repositories = project
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            repositories
                .into_iter()
                .map(|repository| repository.update(cx, |repository, _| repository.barrier()))
                .collect::<Vec<_>>()
        });
        for barrier in barriers {
            let result: anyhow::Result<()> = barrier.await.map_err(|_| {
                anyhow::anyhow!("git repository barrier canceled while archiving worktree")
            });
            result.log_err();
        }
    }

    /// Closed linked-worktree entries need an open workspace so archive root
    /// planning can inspect repositories before deleting the worktree.
    fn open_workspace_for_archive(
        &mut self,
        folder_paths: PathList,
        project_group_key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Entity<Workspace>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let host = project_group_key.host();
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                Some(project_group_key),
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Add,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            Self::wait_for_archive_workspace_metadata(&workspace, cx).await;

            this.update_in(cx, |this, window, cx| then(this, workspace, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_terminal(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let ThreadEntryWorkspace::Closed {
            folder_paths,
            project_group_key,
        } = workspace
            && self.should_load_closed_workspace_for_archive(
                folder_paths,
                project_group_key,
                metadata.remote_connection.as_ref(),
                None,
                Some(metadata.terminal_id),
                cx,
            )
        {
            let metadata = metadata.clone();
            self.open_workspace_for_archive(
                folder_paths.clone(),
                project_group_key.clone(),
                window,
                cx,
                move |this, workspace, window, cx| {
                    this.close_terminal(
                        &metadata,
                        &ThreadEntryWorkspace::Open(workspace),
                        window,
                        cx,
                    );
                },
            );
            return;
        }

        let terminal_id = metadata.terminal_id;
        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| entry.is_active_terminal(terminal_id));
        let neighbor = self
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Terminal(terminal)
                        if terminal.metadata.terminal_id == terminal_id
                )
            })
            .and_then(|position| {
                self.neighboring_activatable_entry(
                    position,
                    metadata.remote_connection.as_ref(),
                    Some(EntryIdentity::Terminal(terminal_id)),
                )
            });

        let terminal_folder_paths = metadata.folder_paths().clone();
        let roots_to_archive = self.roots_to_archive_for_paths(
            metadata.folder_paths(),
            metadata.remote_connection.as_ref(),
            None,
            Some(terminal_id),
            cx,
        );

        let workspace_to_remove = self.linked_worktree_workspace_to_remove(
            &terminal_folder_paths,
            metadata.remote_connection.as_ref(),
            None,
            Some(terminal_id),
            &roots_to_archive,
            cx,
        );

        let mut workspaces_to_remove: Vec<Entity<Workspace>> =
            workspace_to_remove.into_iter().collect();
        let close_item_tasks = self.close_items_for_archived_worktrees(
            &roots_to_archive,
            &mut workspaces_to_remove,
            window,
            cx,
        );

        let terminal_workspace_removed = matches!(
            workspace,
            ThreadEntryWorkspace::Open(workspace) if workspaces_to_remove.contains(workspace)
        );
        let metadata = metadata.clone();
        let workspace = workspace.clone();

        self.remove_workspaces_then(
            workspaces_to_remove,
            close_item_tasks,
            window,
            cx,
            move |this, window, cx| {
                if terminal_workspace_removed {
                    this.delete_empty_drafts_for_archive_paths(
                        metadata.folder_paths(),
                        metadata.remote_connection.as_ref(),
                        cx,
                    );
                }
                // If the terminal's workspace has already been removed, don't
                // synthesize a fallback draft in the detached AgentPanel.
                this.close_terminal_entry(
                    &metadata,
                    &workspace,
                    is_active,
                    neighbor.as_ref(),
                    !terminal_workspace_removed,
                    roots_to_archive,
                    window,
                    cx,
                );
            },
        );
    }

    fn close_terminal_entry(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        is_active: bool,
        neighbor: Option<&ActivatableEntry>,
        activate_panel_draft: bool,
        roots_to_archive: Vec<thread_worktree_archive::RootPlan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = metadata.terminal_id;
        let defer_draft_activation = activate_panel_draft && is_active && neighbor.is_some();

        // Fallback to a neighbor entry instead of a panel draft when the
        // closed terminal was the active entry. Under the tabs model a
        // panel draft opens a workspace tab (with a visible sidebar row),
        // so it must only be created when there is nothing else to show.
        let activate_panel_draft = activate_panel_draft && !(is_active && neighbor.is_some());

        // Closing from the sidebar must not steal focus, since the row's
        // workspace may not be the active workspace.
        if let ThreadEntryWorkspace::Open(workspace) = workspace {
            workspace.update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        if defer_draft_activation || !activate_panel_draft {
                            panel.close_terminal_without_activating_draft(terminal_id, window, cx);
                        } else {
                            panel.close_terminal(terminal_id, window, cx);
                        }
                    });
                }
            });
        }
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.delete(terminal_id, cx);
            });
        }

        self.start_detached_archive_worktree_task(roots_to_archive, cx);

        if is_active {
            self.active_entry = None;
            if neighbor
                .as_ref()
                .is_some_and(|neighbor| self.activate_entry(neighbor, window, cx))
            {
                return;
            }
            if defer_draft_activation && let ThreadEntryWorkspace::Open(workspace) = workspace {
                workspace.update(cx, |workspace, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
                        });
                    }
                });
            }
            self.sync_active_entry_from_active_workspace(cx);
        }
        self.update_entries(cx);
    }

    fn remove_workspaces_then(
        &mut self,
        workspaces_to_remove: Vec<Entity<Workspace>>,
        close_item_tasks: Vec<Task<anyhow::Result<()>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        finish: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if workspaces_to_remove.is_empty() && close_item_tasks.is_empty() {
            finish(self, window, cx);
            return;
        }

        let remove_task = if workspaces_to_remove.is_empty() {
            None
        } else {
            let Some(multi_workspace) = self.multi_workspace.upgrade() else {
                return;
            };
            Some(multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.remove(workspaces_to_remove, RemovalIntent::KeepProject, window, cx)
            }))
        };

        cx.spawn_in(window, async move |this, cx| {
            if let Some(remove_task) = remove_task
                && !remove_task.await?
            {
                return anyhow::Ok(());
            }

            for task in close_item_tasks {
                let result: anyhow::Result<()> = task.await;
                result.log_err();
            }

            this.update_in(cx, |this, window, cx| finish(this, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_items_for_archived_worktrees(
        &self,
        roots_to_archive: &[thread_worktree_archive::RootPlan],
        workspaces_to_remove: &mut Vec<Entity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Task<anyhow::Result<()>>> {
        if roots_to_archive.is_empty() {
            return Vec::new();
        }

        let archive_paths: HashSet<&Path> = roots_to_archive
            .iter()
            .map(|root| root.root_path.as_path())
            .collect();

        let mut mixed_workspaces: Vec<(Entity<Workspace>, Vec<WorktreeId>)> = Vec::new();

        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let all_workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();

            for workspace in all_workspaces {
                if workspaces_to_remove.contains(&workspace) {
                    continue;
                }

                let project = workspace.read(cx).project().read(cx);
                let visible_worktrees: Vec<_> = project
                    .visible_worktrees(cx)
                    .map(|worktree| (worktree.read(cx).id(), worktree.read(cx).abs_path()))
                    .collect();

                let archived_worktree_ids: Vec<WorktreeId> = visible_worktrees
                    .iter()
                    .filter(|(_, path)| archive_paths.contains(path.as_ref()))
                    .map(|(id, _)| *id)
                    .collect();

                if archived_worktree_ids.is_empty() {
                    continue;
                }

                if visible_worktrees.len() == archived_worktree_ids.len() {
                    workspaces_to_remove.push(workspace);
                } else {
                    mixed_workspaces.push((workspace, archived_worktree_ids));
                }
            }
        }

        let mut close_item_tasks = Vec::new();
        for (workspace, archived_worktree_ids) in &mixed_workspaces {
            let panes: Vec<_> = workspace.read(cx).panes().to_vec();
            for pane in panes {
                let items_to_close: Vec<EntityId> = pane
                    .read(cx)
                    .items()
                    .filter(|item| {
                        item.project_path(cx)
                            .is_some_and(|pp| archived_worktree_ids.contains(&pp.worktree_id))
                    })
                    .map(|item| item.item_id())
                    .collect();

                if !items_to_close.is_empty() {
                    let task = pane.update(cx, |pane, cx| {
                        pane.close_items(window, cx, SaveIntent::Close, &|item_id| {
                            items_to_close.contains(&item_id)
                        })
                    });
                    close_item_tasks.push(task);
                }
            }
        }

        close_item_tasks
    }

    /// Deletes the selected thread from the history list. Only archived
    /// threads can be deleted; archive is the first step for live ones.
    fn remove_selected_thread(
        &mut self,
        _: &RemoveSelectedThread,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };
        let Some(ListEntry::Thread(thread)) = self.contents.entries.get(ix) else {
            return;
        };
        if !thread.metadata.archived {
            return;
        }
        let metadata = thread.metadata.clone();
        self.delete_thread(&metadata, cx);
    }

    /// Permanently deletes a thread: removes its metadata row, cleans up any
    /// archived worktree snapshots, and asks the owning agent to delete the
    /// underlying session.
    fn delete_thread(&mut self, metadata: &ThreadMetadata, cx: &mut Context<Self>) {
        let thread_id = metadata.thread_id;
        let session_id = metadata.session_id.clone();
        let agent = Agent::from(metadata.agent_id.clone());

        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete(thread_id, cx));

        let Some(agent_panel) = self
            .active_workspace(cx)
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
        else {
            return;
        };
        let agent_connection_store = agent_panel.read(cx).connection_store().clone();
        let fs = <dyn fs::Fs>::global(cx);

        let task = agent_connection_store.update(cx, |store, cx| {
            store
                .request_connection(agent.clone(), agent.server(fs, ThreadStore::global(cx)), cx)
                .read(cx)
                .wait_for_connection()
        });
        cx.spawn(async move |_this, cx| {
            thread_worktree_archive::cleanup_thread_archived_worktrees(thread_id, cx).await;

            let state = task.await?;
            let task = cx.update(|cx| {
                if let Some(session_id) = &session_id {
                    if let Some(list) = state
                        .connection
                        .session_list(cx)
                        .filter(|list| list.supports_delete())
                    {
                        list.delete_session(session_id, cx)
                    } else {
                        Task::ready(Ok(()))
                    }
                } else {
                    Task::ready(Ok(()))
                }
            });
            task.await
        })
        .detach_and_log_err(cx);
    }

    fn archive_thread(
        &mut self,
        session_id: &acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = ThreadMetadataStore::global(cx);
        let metadata = store.read(cx).entry_by_session(session_id).cloned();
        let metadata_thread_id = metadata.as_ref().map(|metadata| metadata.thread_id);
        let thread_entry = self.contents.entries.iter().find_map(|entry| match entry {
            ListEntry::Thread(thread) => metadata_thread_id
                .map_or_else(
                    || thread.metadata.session_id.as_ref() == Some(session_id),
                    |thread_id| thread.metadata.thread_id == thread_id,
                )
                .then(|| thread.clone()),
            _ => None,
        });
        let thread_id = metadata_thread_id.or_else(|| {
            thread_entry
                .as_ref()
                .map(|thread| thread.metadata.thread_id)
        });
        let active_workspace = thread_id.and_then(|thread_id| {
            self.active_entry.as_ref().and_then(|entry| {
                if entry.is_active_thread(&thread_id) {
                    Some(entry.workspace().clone())
                } else {
                    None
                }
            })
        });
        let thread_folder_paths = metadata
            .as_ref()
            .map(|metadata| metadata.folder_paths().clone())
            .or_else(|| {
                thread_entry
                    .as_ref()
                    .map(|thread| thread.metadata.folder_paths().clone())
            })
            .or_else(|| {
                active_workspace
                    .as_ref()
                    .map(|workspace| PathList::new(&workspace.read(cx).root_paths(cx)))
            });
        let thread_entry_workspace = thread_entry.map(|thread| thread.workspace.clone());

        if let (
            Some(metadata),
            Some(ThreadEntryWorkspace::Closed {
                folder_paths,
                project_group_key,
            }),
        ) = (metadata.as_ref(), thread_entry_workspace)
            && self.should_load_closed_workspace_for_archive(
                &folder_paths,
                &project_group_key,
                metadata.remote_connection.as_ref(),
                Some(metadata.thread_id),
                None,
                cx,
            )
        {
            let session_id = session_id.clone();
            self.open_workspace_for_archive(
                folder_paths,
                project_group_key,
                window,
                cx,
                move |this, _workspace, window, cx| {
                    this.update_entries(cx);
                    this.archive_thread(&session_id, window, cx);
                },
            );
            return;
        }

        // Compute which linked worktree roots should be archived from disk if
        // this thread is archived. This must happen before we remove any
        // workspace from the MultiWorkspace, because `build_root_plan` needs
        // the currently open workspaces in order to find the affected projects
        // and repository handles for each linked worktree.
        let roots_to_archive = metadata
            .as_ref()
            .map(|metadata| {
                self.roots_to_archive_for_paths(
                    metadata.folder_paths(),
                    metadata.remote_connection.as_ref(),
                    thread_id,
                    None,
                    cx,
                )
            })
            .unwrap_or_default();

        let current_pos = self.contents.entries.iter().position(|entry| match entry {
            ListEntry::Thread(thread) => thread_id.map_or_else(
                || thread.metadata.session_id.as_ref() == Some(session_id),
                |tid| thread.metadata.thread_id == tid,
            ),
            _ => false,
        });
        let neighbor = current_pos.and_then(|position| {
            self.neighboring_activatable_entry(
                position,
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.remote_connection.as_ref()),
                thread_id.map(EntryIdentity::Thread),
            )
        });

        // Check if archiving this thread would leave its worktree workspace
        // with no threads, requiring workspace removal.
        let workspace_to_remove = thread_folder_paths.as_ref().and_then(|folder_paths| {
            let thread_remote_connection =
                metadata.as_ref().and_then(|m| m.remote_connection.as_ref());
            self.linked_worktree_workspace_to_remove(
                folder_paths,
                thread_remote_connection,
                thread_id,
                None,
                &roots_to_archive,
                cx,
            )
        });

        // Also find workspaces for root plans that aren't covered by
        // workspace_to_remove. For workspaces that exclusively contain
        // worktrees being archived, remove the whole workspace. For
        // "mixed" workspaces (containing both archived and non-archived
        // worktrees), close only the editor items referencing the
        // archived worktrees so their Entity<Worktree> handles are
        // dropped without destroying the user's workspace layout.
        let mut workspaces_to_remove: Vec<Entity<Workspace>> =
            workspace_to_remove.into_iter().collect();
        let close_item_tasks = self.close_items_for_archived_worktrees(
            &roots_to_archive,
            &mut workspaces_to_remove,
            window,
            cx,
        );

        let removed_workspace = !workspaces_to_remove.is_empty();
        let session_id = session_id.clone();
        let thread_remote_connection = metadata
            .as_ref()
            .and_then(|metadata| metadata.remote_connection.clone());

        self.remove_workspaces_then(
            workspaces_to_remove,
            close_item_tasks,
            window,
            cx,
            move |this, window, cx| {
                if removed_workspace && let Some(thread_folder_paths) = thread_folder_paths.as_ref()
                {
                    this.delete_empty_drafts_for_archive_paths(
                        thread_folder_paths,
                        thread_remote_connection.as_ref(),
                        cx,
                    );
                }
                let in_flight = thread_id
                    .and_then(|tid| this.start_archive_worktree_task(tid, roots_to_archive, cx));
                this.archive_and_activate(
                    &session_id,
                    thread_id,
                    neighbor.as_ref(),
                    thread_folder_paths.as_ref(),
                    thread_remote_connection.as_ref(),
                    in_flight,
                    window,
                    cx,
                );
            },
        );
    }

    /// Archive a thread and activate the nearest neighbor or a draft.
    ///
    /// IMPORTANT: when activating a neighbor or creating a fallback draft,
    /// this method also activates the target workspace in the MultiWorkspace.
    /// This is critical because `rebuild_contents` derives the active
    /// workspace from `mw.workspace()`. If the linked worktree workspace is
    /// still active after archiving its last thread, `rebuild_contents` sees
    /// the threadless linked worktree as active and emits a spurious
    /// "+ New Thread" entry with the worktree chip — keeping the worktree
    /// alive and preventing disk cleanup.
    ///
    /// When `in_flight_archive` is present, it is the background task that
    /// persists the linked worktree's git state and deletes it from disk.
    /// We attach it to the metadata store at the same time we mark the thread
    /// archived so failures can automatically unarchive the thread and user-
    /// initiated unarchive can cancel the task.
    fn archive_and_activate(
        &mut self,
        _session_id: &acp::SessionId,
        thread_id: Option<agent_ui::ThreadId>,
        neighbor: Option<&ActivatableEntry>,
        thread_folder_paths: Option<&PathList>,
        thread_remote_connection: Option<&RemoteConnectionOptions>,
        in_flight_archive: Option<(Task<()>, async_channel::Sender<()>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tearing_down_worktree = in_flight_archive.is_some();
        if let Some(thread_id) = thread_id {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.archive(thread_id, in_flight_archive, cx);
            });
        }

        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| thread_id.is_some_and(|tid| entry.is_active_thread(&tid)));

        if is_active {
            self.active_entry = None;
        }

        if !is_active {
            // The user is looking at a different thread/draft. Clear the
            // archived thread from its workspace's panel so that switching
            // to that workspace later doesn't show a stale thread.
            if let Some(folder_paths) = thread_folder_paths {
                if let Some(workspace) = self.multi_workspace.upgrade().and_then(|mw| {
                    mw.read(cx)
                        .workspace_for_paths(folder_paths, thread_remote_connection, cx)
                }) {
                    if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                        let panel_shows_archived = panel
                            .read(cx)
                            .active_conversation_view()
                            .map(|cv| cv.read(cx).parent_id())
                            .is_some_and(|live_thread_id| {
                                thread_id.is_some_and(|id| id == live_thread_id)
                            });
                        if panel_shows_archived {
                            panel.update(cx, |panel, cx| {
                                if tearing_down_worktree {
                                    panel.clear_base_view_without_draft(cx);
                                } else {
                                    panel.clear_base_view(window, cx);
                                }
                            });
                        }
                    }
                }
            }
            return;
        }

        if neighbor.is_some_and(|neighbor| self.activate_entry(neighbor, window, cx)) {
            return;
        }

        // No neighbor or its workspace isn't open — just clear the
        // panel so the group is left empty.
        if let Some(folder_paths) = thread_folder_paths {
            let workspace = self.multi_workspace.upgrade().and_then(|mw| {
                mw.read(cx)
                    .workspace_for_paths(folder_paths, thread_remote_connection, cx)
            });
            if let Some(workspace) = workspace {
                if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        if tearing_down_worktree {
                            panel.clear_base_view_without_draft(cx);
                        } else {
                            panel.clear_base_view(window, cx);
                        }
                    });
                }
            }
        }
    }

    fn start_archive_worktree_task(
        &self,
        thread_id: ThreadId,
        roots: Vec<thread_worktree_archive::RootPlan>,
        cx: &mut Context<Self>,
    ) -> Option<(Task<()>, async_channel::Sender<()>)> {
        if roots.is_empty() {
            return None;
        }

        self.delete_empty_drafts_for_archive_roots(&roots, cx);

        let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
        let task = cx.spawn(async move |_this, cx| {
            match Self::archive_worktree_roots(roots, cancel_rx, cx).await {
                Ok(ArchiveWorktreeOutcome::Success) => {
                    cx.update(|cx| {
                        ThreadMetadataStore::global(cx).update(cx, |store, _cx| {
                            store.cleanup_completed_archive(thread_id);
                        });
                    });
                }
                Ok(ArchiveWorktreeOutcome::Cancelled) => {}
                Err(error) => {
                    log::error!("Failed to archive worktree: {error:#}");
                    cx.update(|cx| {
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            store.unarchive(thread_id, cx);
                        });
                    });
                }
            }
        });

        Some((task, cancel_tx))
    }

    fn start_detached_archive_worktree_task(
        &self,
        roots: Vec<thread_worktree_archive::RootPlan>,
        cx: &mut Context<Self>,
    ) {
        if roots.is_empty() {
            return;
        }

        self.delete_empty_drafts_for_archive_roots(&roots, cx);

        let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
        cx.spawn(async move |_this, cx| {
            let outcome = Self::archive_worktree_roots(roots, cancel_rx, cx).await;
            drop(cancel_tx);
            match outcome {
                Ok(ArchiveWorktreeOutcome::Success | ArchiveWorktreeOutcome::Cancelled) => {}
                Err(error) => {
                    log::error!("Failed to archive worktree after closing sidebar item: {error:#}");
                }
            }
        })
        .detach();
    }

    async fn archive_worktree_roots(
        roots: Vec<thread_worktree_archive::RootPlan>,
        cancel_rx: async_channel::Receiver<()>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<ArchiveWorktreeOutcome> {
        let mut completed_persists: Vec<(i64, thread_worktree_archive::RootPlan)> = Vec::new();

        for root in &roots {
            if cancel_rx.is_closed() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            match thread_worktree_archive::persist_worktree_state(root, cx).await {
                Ok(id) => {
                    completed_persists.push((id, root.clone()));
                }
                Err(error) => {
                    for &(id, ref completed_root) in completed_persists.iter().rev() {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                    }
                    return Err(error);
                }
            }

            if cancel_rx.is_closed() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            if let Err(error) = thread_worktree_archive::remove_root(root.clone(), cx).await {
                if let Some(&(id, ref completed_root)) = completed_persists.last() {
                    if completed_root.root_path == root.root_path {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                        completed_persists.pop();
                    }
                }
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Err(error);
            }
        }

        Ok(ArchiveWorktreeOutcome::Success)
    }

    fn activate_workspace(
        &self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |mw, cx| {
                mw.activate(workspace.clone(), None, window, cx);
            });
        }
    }

    fn archive_selected_thread(
        &mut self,
        _: &ArchiveSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else {
            return;
        };
        match self.contents.entries.get(ix) {
            Some(ListEntry::Thread(thread)) => {
                match thread.status {
                    AgentThreadStatus::Running | AgentThreadStatus::WaitingForConfirmation => {
                        return;
                    }
                    AgentThreadStatus::Completed | AgentThreadStatus::Error => {}
                }
                if thread.draft.is_some() {
                    let workspace = thread.workspace.clone();
                    let draft_id = thread.metadata.thread_id;
                    self.remove_draft(draft_id, &workspace, window, cx);
                } else if let Some(session_id) = thread.metadata.session_id.clone() {
                    self.archive_thread(&session_id, window, cx);
                }
            }
            Some(ListEntry::Terminal(terminal)) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.close_terminal(&metadata, &workspace, window, cx);
            }
            _ => {}
        }
    }

    fn rename_selected_thread(
        &mut self,
        _: &RenameSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else {
            return;
        };
        let Some(ListEntry::Thread(thread)) = self.contents.entries.get(ix) else {
            return;
        };
        let thread_id = thread.metadata.thread_id;
        let title = thread.metadata.display_title();
        self.start_renaming_thread(ix, thread_id, title, window, cx);
    }

    fn record_thread_access(&mut self, id: &ThreadId) {
        self.thread_last_accessed.insert(*id, Utc::now());
    }

    fn record_terminal_access(&mut self, id: TerminalId) {
        self.terminal_last_accessed.insert(id, Utc::now());
    }

    fn record_thread_interacted(&mut self, thread_id: &agent_ui::ThreadId, cx: &mut App) {
        let store = ThreadMetadataStore::global(cx);
        store.update(cx, |store, cx| {
            store.update_interacted_at(thread_id, Utc::now(), cx);
        })
    }

    fn thread_display_time(metadata: &ThreadMetadata) -> DateTime<Utc> {
        metadata.interacted_at.unwrap_or(metadata.updated_at)
    }

    /// The sort order used by the ctrl-tab switcher
    fn switcher_entry_cmp(
        &self,
        left: &ThreadSwitcherEntry,
        right: &ThreadSwitcherEntry,
    ) -> Ordering {
        let sort_time = |entry: &ThreadSwitcherEntry| match entry {
            ThreadSwitcherEntry::Thread(entry) => self
                .thread_last_accessed
                .get(&entry.metadata.thread_id)
                .copied()
                .or(entry.metadata.interacted_at)
                .unwrap_or(entry.metadata.updated_at),
            ThreadSwitcherEntry::Terminal(entry) => self
                .terminal_last_accessed
                .get(&entry.metadata.terminal_id)
                .copied()
                .unwrap_or(entry.metadata.created_at),
        };

        // .reverse() = most recent first
        sort_time(left).cmp(&sort_time(right)).reverse()
    }

    fn mru_entries_for_switcher(&self, cx: &App) -> Vec<ThreadSwitcherEntry> {
        fn project_name(folder_paths: &PathList) -> Option<SharedString> {
            let names = folder_paths
                .paths()
                .iter()
                .filter_map(|p| p.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .join(", ");
            (!names.is_empty()).then(|| SharedString::from(names))
        }

        // Collapsing a section hides its rows, not its threads: the switcher
        // lists every thread regardless. An open thread appears in both
        // Active and All Threads now, so dedupe by thread_id to keep the
        // switcher itself a proper set of distinct threads.
        let mut seen_thread_ids: HashSet<agent_ui::ThreadId> = HashSet::default();
        let mut entries: Vec<ThreadSwitcherEntry> = self
            .contents
            .all_entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => None,
                ListEntry::Thread(thread) => {
                    if thread.draft == Some(DraftKind::Empty) {
                        return None;
                    }
                    if !seen_thread_ids.insert(thread.metadata.thread_id) {
                        return None;
                    }
                    let workspace = match &thread.workspace {
                        ThreadEntryWorkspace::Open(workspace) => Some(workspace.clone()),
                        ThreadEntryWorkspace::Closed {
                            project_group_key, ..
                        } => self.multi_workspace.upgrade().and_then(|mw| {
                            mw.read(cx).workspace_for_paths(
                                project_group_key.path_list(),
                                project_group_key.host().as_ref(),
                                cx,
                            )
                        }),
                    }?;
                    let notified = self.contents.is_thread_notified(&thread.metadata.thread_id);
                    let timestamp: SharedString =
                        format_history_entry_timestamp(Self::thread_display_time(&thread.metadata))
                            .into();
                    Some(ThreadSwitcherEntry::Thread(ThreadSwitcherThreadEntry {
                        title: thread.metadata.display_title(),
                        icon: thread.icon,
                        icon_from_external_svg: thread.icon_from_external_svg.clone(),
                        status: thread.status,
                        project_name: project_name(thread.metadata.folder_paths()),
                        metadata: thread.metadata.clone(),
                        workspace,
                        worktrees: thread
                            .worktrees
                            .iter()
                            .cloned()
                            .map(|mut wt| {
                                wt.highlight_positions = Vec::new();
                                wt
                            })
                            .collect(),
                        diff_stats: thread.diff_stats,
                        is_draft: thread.draft.is_some(),
                        is_title_generating: thread.is_title_generating,
                        notified,
                        timestamp,
                    }))
                }
                ListEntry::Terminal(terminal) => {
                    let timestamp: SharedString =
                        format_history_entry_timestamp(terminal.metadata.created_at).into();
                    Some(ThreadSwitcherEntry::Terminal(ThreadSwitcherTerminalEntry {
                        project_name: project_name(terminal.metadata.folder_paths()),
                        metadata: terminal.metadata.clone(),
                        workspace: terminal.workspace.clone(),
                        worktrees: terminal
                            .worktrees
                            .iter()
                            .cloned()
                            .map(|mut wt| {
                                wt.highlight_positions = Vec::new();
                                wt
                            })
                            .collect(),
                        notified: self
                            .contents
                            .is_terminal_notified(terminal.metadata.terminal_id),
                        timestamp,
                    }))
                }
            })
            .collect();

        entries.sort_by(|a, b| self.switcher_entry_cmp(a, b));

        entries
    }

    fn dismiss_thread_switcher(&mut self, cx: &mut Context<Self>) {
        self.thread_switcher = None;
        self._thread_switcher_subscriptions.clear();
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(None, cx);
            });
        }
    }

    fn on_toggle_thread_switcher(
        &mut self,
        action: &ToggleThreadSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(action.select_last, window, cx);
    }

    fn preview_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Thread {
                metadata,
                workspace,
            } => {
                if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        multi_workspace.activate(workspace.clone(), None, window, cx);
                    });
                }
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.update_entries(cx);
                Self::load_agent_thread_in_workspace(workspace, metadata, false, window, cx);
            }
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                if let ThreadEntryWorkspace::Open(workspace) = workspace {
                    if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                        multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.activate(workspace.clone(), None, window, cx);
                        });
                    }
                    self.active_entry = Some(ActiveEntry::Terminal {
                        terminal_id: metadata.terminal_id,
                        workspace: workspace.clone(),
                    });
                    self.update_entries(cx);
                    Self::load_agent_terminal_in_workspace(workspace, metadata, false, window, cx);
                }
            }
        }
    }

    fn confirm_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Thread {
                metadata,
                workspace,
            } => {
                if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        multi_workspace.activate(workspace.clone(), None, window, cx);
                        multi_workspace.retain_active_workspace(cx);
                    });
                }
                self.record_thread_access(&metadata.thread_id);
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.update_entries(cx);
                self.dismiss_thread_switcher(cx);
                Self::load_agent_thread_in_workspace(workspace, metadata, true, window, cx);
            }
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                self.dismiss_thread_switcher(cx);
                self.activate_terminal_entry(metadata.clone(), workspace.clone(), true, window, cx);
            }
        }
    }

    fn toggle_thread_switcher_impl(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread_switcher) = &self.thread_switcher {
            thread_switcher.update(cx, |switcher, cx| {
                if select_last {
                    switcher.select_last(cx);
                } else {
                    switcher.cycle_selection(cx);
                }
            });
            return;
        }

        let entries = self.mru_entries_for_switcher(cx);
        if entries.len() < 2 {
            return;
        }

        let weak_multi_workspace = self.multi_workspace.clone();

        // Snapshot the active entry (thread or terminal) so dismissal can
        // restore it.
        let original_active_entry = self.active_entry.clone();
        let original_metadata = match &original_active_entry {
            Some(ActiveEntry::Thread { thread_id, .. }) => {
                entries.iter().find_map(|entry| match entry {
                    ThreadSwitcherEntry::Thread(entry)
                        if *thread_id == entry.metadata.thread_id =>
                    {
                        Some(entry.metadata.clone())
                    }
                    _ => None,
                })
            }
            _ => None,
        };
        let original_workspace = self
            .multi_workspace
            .upgrade()
            .map(|mw| mw.read(cx).workspace().clone());

        let thread_switcher = cx.new(|cx| ThreadSwitcher::new(entries, select_last, window, cx));

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(&thread_switcher, window, {
            let thread_switcher = thread_switcher.clone();
            move |this, _emitter, event: &ThreadSwitcherEvent, window, cx| match event {
                ThreadSwitcherEvent::Preview(selection) => {
                    this.preview_switcher_selection(selection, window, cx);
                    let focus = thread_switcher.focus_handle(cx);
                    window.focus(&focus, cx);
                }
                ThreadSwitcherEvent::Confirmed(selection) => {
                    this.confirm_switcher_selection(selection, window, cx);
                }
                ThreadSwitcherEvent::Dismissed => {
                    if let Some(mw) = weak_multi_workspace.upgrade() {
                        if let Some(original_ws) = &original_workspace {
                            mw.update(cx, |mw, cx| {
                                mw.activate(original_ws.clone(), None, window, cx);
                            });
                        }
                    }
                    match &original_active_entry {
                        Some(ActiveEntry::Thread { .. }) => {
                            if let (Some(metadata), Some(original_ws)) =
                                (&original_metadata, &original_workspace)
                            {
                                this.active_entry = Some(ActiveEntry::Thread {
                                    thread_id: metadata.thread_id,
                                    session_id: metadata.session_id.clone(),
                                    workspace: original_ws.clone(),
                                });
                                this.update_entries(cx);
                                Self::load_agent_thread_in_workspace(
                                    original_ws,
                                    metadata,
                                    false,
                                    window,
                                    cx,
                                );
                            }
                        }
                        Some(ActiveEntry::Terminal {
                            terminal_id,
                            workspace,
                        }) => {
                            let terminal_id = *terminal_id;
                            let workspace = workspace.clone();
                            this.active_entry = Some(ActiveEntry::Terminal {
                                terminal_id,
                                workspace: workspace.clone(),
                            });
                            this.update_entries(cx);
                            workspace.update(cx, |workspace, cx| {
                                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                    panel.update(cx, |panel, cx| {
                                        panel.activate_terminal(terminal_id, false, window, cx);
                                    });
                                }
                            });
                        }
                        None => {}
                    }
                    this.dismiss_thread_switcher(cx);
                }
            }
        }));

        subscriptions.push(cx.subscribe_in(
            &thread_switcher,
            window,
            |this, _emitter, _event: &gpui::DismissEvent, _window, cx| {
                this.dismiss_thread_switcher(cx);
            },
        ));

        let focus = thread_switcher.focus_handle(cx);
        let overlay_view = gpui::AnyView::from(thread_switcher.clone());

        // Replay the initial preview that was emitted during construction
        // before subscriptions were wired up.
        let initial_preview = thread_switcher
            .read(cx)
            .selected_entry()
            .map(ThreadSwitcherEntry::selection);

        self.thread_switcher = Some(thread_switcher);
        self._thread_switcher_subscriptions = subscriptions;
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(Some(overlay_view), cx);
            });
        }

        if let Some(selection) = initial_preview {
            self.preview_switcher_selection(&selection, window, cx);
        }

        window.focus(&focus, cx);
    }

    fn render_thread(
        &self,
        ix: usize,
        thread: &ThreadEntry,
        is_active: bool,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = self.render_thread_row(ix, thread, is_active, is_focused, cx);
        if !thread.under_worktree_header {
            return row;
        }
        // A row that belongs to a worktree stands in from the edge, so the
        // header above it reads as holding the rows under it rather than as a
        // label that happens to precede them. A worktree with one thread is not
        // a group and stays flush.
        div().pl_2().child(row).into_any_element()
    }

    fn render_thread_row(
        &self,
        ix: usize,
        thread: &ThreadEntry,
        is_active: bool,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_notification = self.contents.is_thread_notified(&thread.metadata.thread_id);

        let title: SharedString = thread.metadata.display_title();
        let metadata = thread.metadata.clone();
        let thread_workspace = thread.workspace.clone();

        let is_hovered = self.hovered_thread_index == Some(ix);
        let is_selected = is_active;
        let is_draft = thread.draft.is_some();
        // Every thread row carries its own PR chip. Hiding it on rows under a
        // header meant the header alone reported PR state for its threads,
        // which read fine when a worktree held one branch and one CI story but
        // was hostile the moment the row was where you were looking to decide
        // which thread had the failing PR. The header keeps its chip only when
        // it is collapsed and the rows are not visible.
        let row_pr_chips = Self::thread_pr_chips(thread, cx);
        let is_archived = thread.metadata.archived;
        // Only Active-section rows are open as tabs, so only they get a
        // close-the-tab affordance.
        let is_active_section = self.section_of_entry(ix) == Some(SidebarSection::OpenInZed);
        let is_restoring = self
            .restoring_tasks
            .contains_key(&thread.metadata.thread_id);
        let is_renaming = self.renaming_thread_id == Some(thread.metadata.thread_id);

        let thread_id_for_actions = thread.metadata.thread_id;
        let session_id_for_delete = thread.metadata.session_id.clone();
        let focus_handle = self.focus_handle.clone();
        let title_editor = self.thread_rename_editor.clone();

        let id = SharedString::from(format!("thread-entry-{}", ix));

        let color = cx.theme().colors();
        let sidebar_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let is_remote = thread.workspace.is_remote(cx);

        // Rows never show the workspace name; the branch chip and title
        // identify the worktree.
        let mut worktrees = thread.worktrees.clone();
        for worktree in &mut worktrees {
            worktree.worktree_name = None;
        }

        let (icon, icon_svg) = if is_draft {
            (IconName::Circle, None)
        } else {
            (thread.icon, thread.icon_from_external_svg.clone())
        };

        let title_generating = thread.is_title_generating
            || self
                .regenerating_titles
                .contains(&thread.metadata.thread_id);

        let thread_item = ThreadItem::new(id, title.clone())
            .base_bg(sidebar_bg)
            .icon(icon)
            // A subtle brand tint tells Claude and Codex threads apart. An
            // archived thread reads the same as an active one (only its small
            // archive glyph marks the state), so the tint applies regardless.
            .when_some(
                agent_ui::agent_brand_color(&thread.metadata.agent_id).filter(|_| !is_draft),
                |this, color| this.icon_color(Color::Custom(color)),
            )
            .when(is_draft, |this| {
                this.icon_color(Color::Custom(cx.theme().colors().icon_muted.opacity(0.2)))
            })
            .status(thread.status)
            .when(is_restoring, |this| this.status(AgentThreadStatus::Running))
            .archived(is_archived)
            .is_remote(is_remote)
            .when_some(icon_svg, |this, svg| {
                this.custom_icon_from_external_svg(svg)
            })
            // Every thread row reads the same, in every section: one line,
            // the age trailing the title, and no metadata line. Under a
            // workspace header that header names the worktree, branch, and PR
            // state; elsewhere the row's hover card carries them.
            .worktrees(Vec::new())
            .timestamp(format_history_entry_timestamp(Self::thread_display_time(
                &thread.metadata,
            )))
            .compact(true)
            .highlight_positions(thread.highlight_positions.to_vec())
            .title_generating(title_generating)
            .notified(has_notification)
            .when(thread.diff_stats.lines_added > 0, |this| {
                this.added(thread.diff_stats.lines_added as usize)
            })
            .when(thread.diff_stats.lines_removed > 0, |this| {
                this.removed(thread.diff_stats.lines_removed as usize)
            })
            .selected(is_selected)
            .focused(is_focused)
            .hovered(is_hovered)
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_thread_index = Some(ix);
                } else if this.hovered_thread_index == Some(ix) {
                    this.hovered_thread_index = None;
                }
                cx.notify();
            }))
            .when(is_renaming, |this| {
                this.is_truncated(false).title_slot(
                    div()
                        .h_full()
                        .min_w_0()
                        .flex_1()
                        .capture_action(cx.listener(
                            |this, _: &editor::actions::Newline, window, cx| {
                                this.finish_thread_rename(window, cx);
                            },
                        ))
                        .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                            this.finish_thread_rename(window, cx);
                        }))
                        .on_action(
                            cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                                this.cancel_thread_rename(window, cx);
                            }),
                        )
                        .child(title_editor),
                )
            })
            .when(is_hovered && !is_renaming, |this| {
                let pr_chips = row_pr_chips.clone().into_iter().enumerate().map(
                    |(chip_ix, chip)| {
                        ui::PrChip::new(
                            SharedString::from(format!("thread-row-pr-{ix}-{chip_ix}")),
                            chip,
                        )
                    },
                );
                // Renaming lives on the row's context menu, and stopping a turn
                // lives in the thread's message bar; rows keep only the action
                // that belongs to their state.
                let contextual_action: Option<AnyElement> = if is_restoring {
                    Some(
                        IconButton::new("cancel-restore", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Cancel Restore"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.restoring_tasks.remove(&thread_id_for_actions);
                                cx.notify();
                            }))
                            .into_any_element(),
                    )
                } else if is_archived {
                    Some(
                        IconButton::new("delete-thread", IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            // Deleting the record of a thread, which also cleans
                            // up any worktree archived away with it.
                            .tooltip(Tooltip::text("Delete Thread"))
                            .on_click({
                                let metadata = metadata.clone();
                                cx.listener(move |this, _, _window, cx| {
                                    this.delete_thread(&metadata, cx);
                                })
                            })
                            .into_any_element(),
                    )
                } else {
                    match thread.draft {
                        Some(DraftKind::Empty) => None,
                        Some(DraftKind::WithContent) => Some(
                            IconButton::new("discard_thread", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Discard Draft"))
                                .on_click({
                                    let thread_workspace = thread_workspace.clone();
                                    cx.listener(move |this, _, window, cx| {
                                        this.remove_draft(
                                            thread_id_for_actions,
                                            &thread_workspace,
                                            window,
                                            cx,
                                        );
                                    })
                                })
                                .into_any_element(),
                        ),
                        None => {
                            // An open (Active-section) row can close its tab; the
                            // X sits to the left of the archive button.
                            let close_tab_button = if is_active_section
                                && let ThreadEntryWorkspace::Open(workspace) = &thread_workspace
                            {
                                let workspace = workspace.clone();
                                Some(
                                    IconButton::new("close-thread-tab", IconName::Close)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text("Close Tab"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.close_thread_tab(
                                                thread_id_for_actions,
                                                &workspace,
                                                window,
                                                cx,
                                            );
                                        }))
                                        .into_any_element(),
                                )
                            } else {
                                None
                            };
                            let archive_button =
                                IconButton::new("archive-thread", IconName::Archive)
                                    .icon_size(IconSize::Small)
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        // The only thread in a linked worktree
                                        // takes the worktree with it, so the
                                        // button says what it will do.
                                        let takes_worktree = thread
                                            .solo_worktree
                                            .as_ref()
                                            .is_some_and(|solo| solo.is_linked_worktree);
                                        move |_window, cx| {
                                            Tooltip::for_action_in(
                                                if takes_worktree {
                                                    "Archive Worktree"
                                                } else {
                                                    "Archive Thread"
                                                },
                                                &ArchiveSelectedThread,
                                                &focus_handle,
                                                cx,
                                            )
                                        }
                                    })
                                    .on_click({
                                        let session_id = session_id_for_delete.clone();
                                        cx.listener(move |this, _, window, cx| {
                                            if let Some(ref session_id) = session_id {
                                                this.archive_thread(session_id, window, cx);
                                            }
                                        })
                                    });
                            // A row archives its own thread wherever it sits.
                            // The worktree's own archive lives on its header,
                            // one row up, and takes the whole group — except
                            // for a row that is its own worktree, which has no
                            // header and so carries the worktree's + itself.
                            // Its archive is already the worktree's: archiving
                            // the only thread takes the worktree with it.
                            let solo = thread.solo_worktree.clone();
                            let new_thread_button = solo
                                .as_ref()
                                .and_then(|solo| solo.workspace.clone())
                                .map(|workspace| {
                                    IconButton::new("new-thread-in-worktree", IconName::Plus)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text("New Thread in This Worktree"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.new_thread_in_worktree(&workspace, window, cx);
                                        }))
                                        .into_any_element()
                                });
                            Some(
                                h_flex()
                                    .gap_0p5()
                                    .when_some(new_thread_button, |this, button| this.child(button))
                                    .when_some(close_tab_button, |this, button| this.child(button))
                                    .child(archive_button)
                                    .into_any_element(),
                            )
                        }
                    }
                };

                // Chips ride in the action slot whether or not there is a
                // hover action beside them: a hovered row with chips but no
                // action (an empty draft) used to lose its chips to this
                // branch's `when_some`.
                let has_action = contextual_action.is_some();
                let has_chips = !row_pr_chips.is_empty();
                this.when(has_action || has_chips, |this| {
                    let slot = h_flex().gap_0p5().children(pr_chips);
                    let slot = match contextual_action {
                        Some(action) => slot.child(action),
                        None => slot,
                    };
                    this.action_slot(slot)
                })
            })
            // Every thread row carries its PR chips whether the pointer is on
            // it or not; the chips ride in the action slot because a compact
            // row draws no metadata line, which is where `ThreadItem` puts
            // them.
            .when(!row_pr_chips.is_empty() && !(is_hovered && !is_renaming), |this| {
                this.action_slot(h_flex().gap_0p5().children(row_pr_chips.clone().into_iter().enumerate().map(
                    |(chip_ix, chip)| {
                        ui::PrChip::new(
                            SharedString::from(format!("thread-row-pr-{ix}-{chip_ix}")),
                            chip,
                        )
                    },
                )))
            })
            .on_click({
                let thread_workspace = thread_workspace.clone();
                cx.listener(move |this, _, window, cx| {
                    this.selection = None;
                    if is_restoring {
                        return;
                    }
                    // Opening an archived thread unarchives it (restoring its
                    // worktrees if they were snapshotted away).
                    if is_archived {
                        this.open_thread_from_archive(metadata.clone(), window, cx);
                        return;
                    }
                    match &thread_workspace {
                        ThreadEntryWorkspace::Open(workspace) => {
                            this.activate_thread(metadata.clone(), workspace, false, window, cx);
                        }
                        ThreadEntryWorkspace::Closed {
                            folder_paths,
                            project_group_key,
                        } => {
                            this.open_workspace_and_activate_thread(
                                metadata.clone(),
                                folder_paths.clone(),
                                project_group_key,
                                window,
                                cx,
                            );
                        }
                    }
                })
            });

        if is_draft || thread.metadata.session_id.is_none() {
            return thread_item.into_any_element();
        }

        let Some(session_id) = thread.metadata.session_id.clone() else {
            return thread_item.into_any_element();
        };

        let context_menu_id = SharedString::from(format!("thread-context-menu-{}", ix));
        let sidebar = cx.weak_entity();

        let active_workspace = self.active_workspace(cx);
        let thread_workspace = match &thread_workspace {
            ThreadEntryWorkspace::Open(workspace) => Some(workspace.clone()),
            ThreadEntryWorkspace::Closed { .. } => None,
        };

        let is_zed_thread = thread.metadata.agent_id.as_ref() == ZED_AGENT_ID.as_ref();
        let can_open_as_markdown = thread.is_live || is_zed_thread;
        let folder_paths = thread.metadata.folder_paths().clone();

        // Hovering a row says where the thread lives: worktree and branch,
        // with the full path underneath.
        let hover_worktrees: Vec<(SharedString, SharedString)> = thread
            .worktrees
            .iter()
            .map(|worktree| {
                let name = worktree
                    .worktree_name
                    .clone()
                    .unwrap_or_else(|| SharedString::from("worktree"));
                let label: SharedString = match &worktree.branch_name {
                    Some(branch) => format!("{name} ({branch})").into(),
                    None => name,
                };
                (label, worktree.full_path.clone())
            })
            .collect();

        let row = right_click_menu(context_menu_id)
            .trigger(move |_, _, _| {
                div()
                    .id("thread-row-hover")
                    .when(!hover_worktrees.is_empty(), |this| {
                        let hover_worktrees = hover_worktrees.clone();
                        this.tooltip(Tooltip::element(move |_, _| {
                            v_flex()
                                .gap_1()
                                .max_w_128()
                                .children(hover_worktrees.iter().map(|(label, path)| {
                                    v_flex()
                                        .child(Label::new(label.clone()).size(LabelSize::Small))
                                        .child(
                                            Label::new(path.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                }))
                                .into_any()
                        }))
                    })
                    .child(thread_item)
            })
            .menu({
                let thread_id = thread.metadata.thread_id;
                let markdown_title = Some(thread.metadata.display_title());
                let rename_title = title;
                let menu_metadata = thread.metadata.clone();
                move |_window, cx| {
                    let session_id = session_id.clone();
                    let sidebar = sidebar.clone();
                    let active_workspace = active_workspace.clone();
                    let thread_workspace = thread_workspace.clone();
                    let markdown_title = markdown_title.clone();
                    let rename_title = rename_title.clone();
                    let folder_paths = folder_paths.clone();
                    let menu_metadata = menu_metadata.clone();
                    ContextMenu::build(_window, cx, move |mut menu, _window, _cx| {
                        menu = menu.entry("Rename Title", None, {
                            let sidebar = sidebar.clone();
                            let rename_title = rename_title.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.start_renaming_thread(
                                            ix,
                                            thread_id,
                                            rename_title.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        });

                        if is_zed_thread {
                            menu = menu.entry("Regenerate Title", None, {
                                let session_id = session_id.clone();
                                let sidebar = sidebar.clone();
                                let thread_workspace = thread_workspace.clone();
                                let folder_paths = folder_paths.clone();
                                move |_window, cx| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.regenerate_thread_title(
                                                &session_id,
                                                thread_id,
                                                folder_paths.clone(),
                                                thread_workspace.clone(),
                                                cx,
                                            );
                                        })
                                        .ok();
                                }
                            });
                        }

                        if can_open_as_markdown {
                            menu = menu.entry("Open Conversation as Markdown", None, {
                                let session_id = session_id.clone();
                                let markdown_title = markdown_title.clone();
                                let thread_workspace = thread_workspace.clone();
                                move |window, cx| {
                                    if let Some(thread_workspace) = thread_workspace.as_ref()
                                        && let Some(panel) =
                                            thread_workspace.read(cx).panel::<AgentPanel>(cx)
                                    {
                                        let opened = panel.update(cx, |panel, cx| {
                                            panel.open_thread_as_markdown(
                                                thread_id,
                                                thread_workspace.clone(),
                                                window,
                                                cx,
                                            )
                                        });
                                        if opened {
                                            return;
                                        }
                                    }

                                    if is_zed_thread
                                        && let Some(active_workspace) = &active_workspace
                                    {
                                        Self::open_closed_native_thread_as_markdown(
                                            &session_id,
                                            markdown_title.clone(),
                                            active_workspace,
                                            window,
                                            cx,
                                        );
                                    }
                                }
                            });
                        }

                        if is_archived {
                            menu.separator()
                                .entry("Restore Worktree", None, {
                                    let sidebar = sidebar.clone();
                                    let metadata = menu_metadata.clone();
                                    move |window, cx| {
                                        sidebar
                                            .update(cx, |sidebar, cx| {
                                                sidebar.open_thread_from_archive(
                                                    metadata.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                    }
                                })
                                .entry("Delete Worktree", None, {
                                    let metadata = menu_metadata.clone();
                                    move |_window, cx| {
                                        sidebar
                                            .update(cx, |sidebar, cx| {
                                                sidebar.delete_thread(&metadata, cx);
                                            })
                                            .ok();
                                    }
                                })
                        } else {
                            menu.separator().entry("Archive Worktree", None, {
                                let session_id = session_id.clone();
                                move |window, cx| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.archive_thread(&session_id, window, cx);
                                        })
                                        .ok();
                                }
                            })
                        }
                    })
                }
            })
            .into_any_element();

        // A header used to set one worktree apart from the next. A row that
        // stands in for its worktree keeps that gap itself, so a list of solo
        // worktrees does not read as one undivided run of threads.
        if thread.solo_worktree.is_some() {
            return div().pt_2().child(row).into_any_element();
        }
        row
    }

    fn render_terminal(
        &self,
        ix: usize,
        terminal: &TerminalEntry,
        is_active: bool,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::from(format!("terminal-{}", terminal.metadata.terminal_id));
        let is_hovered = self.hovered_thread_index == Some(ix);
        let color = cx.theme().colors();
        let sidebar_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));
        let metadata = terminal.metadata.clone();
        let workspace = terminal.workspace.clone();
        let focus_handle = self.focus_handle.clone();
        // Rows never show the workspace name; the branch chip and title
        // identify the worktree.
        let mut worktrees = terminal.worktrees.clone();
        for worktree in &mut worktrees {
            worktree.worktree_name = None;
        }
        let is_remote = terminal.workspace.is_remote(cx);

        let display_title = terminal.metadata.display_title();
        let (icon_char, title, highlight_positions) =
            match split_leading_icon_char(&display_title, &terminal.highlight_positions) {
                Some((icon_char, title, positions)) => (Some(icon_char), title, positions),
                None => (None, display_title, terminal.highlight_positions.clone()),
            };

        ThreadItem::new(id, title)
            .base_bg(sidebar_bg)
            .icon(IconName::Terminal)
            .when_some(icon_char, |this, icon_char| this.icon_char(icon_char))
            .is_remote(is_remote)
            .worktrees(worktrees)
            .timestamp(format_history_entry_timestamp(terminal.metadata.created_at))
            .notified(terminal.has_notification)
            .highlight_positions(highlight_positions)
            .selected(is_active)
            .focused(is_focused)
            .hovered(is_hovered)
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_thread_index = Some(ix);
                } else if this.hovered_thread_index == Some(ix) {
                    this.hovered_thread_index = None;
                }
                cx.notify();
            }))
            .when(is_hovered, |this| {
                this.action_slot(
                    IconButton::new("close-terminal", IconName::Close)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip({
                            let focus_handle = focus_handle.clone();
                            move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Close Terminal",
                                    &ArchiveSelectedThread,
                                    &focus_handle,
                                    cx,
                                )
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.close_terminal(&metadata, &workspace, window, cx);
                        })),
                )
            })
            .on_click(cx.listener({
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                move |this, _, window, cx| {
                    this.activate_terminal_entry(
                        metadata.clone(),
                        workspace.clone(),
                        false,
                        window,
                        cx,
                    );
                }
            }))
            .into_any_element()
    }

    fn render_filter_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .min_w_0()
            .flex_1()
            .capture_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.editor_confirm(window, cx);
                }),
            )
            .child(self.filter_editor.clone())
    }

    fn new_thread_in_group(
        &mut self,
        _: &NewThreadInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = None;
        if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_entry(&workspace, window, cx);
        }
    }

    fn new_terminal_thread(
        &mut self,
        _: &NewTerminalThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();

        self.selection = None;
        if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_terminal(&workspace, window, cx);
        }
    }

    fn remove_draft(
        &mut self,
        draft_id: ThreadId,
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let metadata = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(draft_id)
            .cloned();

        if let ThreadEntryWorkspace::Closed {
            folder_paths,
            project_group_key,
        } = workspace
            && self.should_load_closed_workspace_for_archive(
                folder_paths,
                project_group_key,
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.remote_connection.as_ref()),
                Some(draft_id),
                None,
                cx,
            )
        {
            self.open_workspace_for_archive(
                folder_paths.clone(),
                project_group_key.clone(),
                window,
                cx,
                move |this, workspace, window, cx| {
                    this.remove_draft(draft_id, &ThreadEntryWorkspace::Open(workspace), window, cx);
                },
            );
            return;
        }

        let draft_folder_paths = metadata
            .as_ref()
            .map(|metadata| metadata.folder_paths().clone())
            .or_else(|| match workspace {
                ThreadEntryWorkspace::Open(workspace) => {
                    Some(PathList::new(&workspace.read(cx).root_paths(cx)))
                }
                ThreadEntryWorkspace::Closed { folder_paths, .. } => Some(folder_paths.clone()),
            });
        let draft_remote_connection = metadata
            .as_ref()
            .and_then(|metadata| metadata.remote_connection.clone());
        let roots_to_archive = metadata
            .as_ref()
            .map(|metadata| {
                self.roots_to_archive_for_paths(
                    metadata.folder_paths(),
                    metadata.remote_connection.as_ref(),
                    Some(draft_id),
                    None,
                    cx,
                )
            })
            .unwrap_or_default();

        let was_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| entry.is_active_thread(&draft_id));
        let neighbor = self
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id
                )
            })
            .and_then(|position| {
                self.neighboring_activatable_entry(
                    position,
                    draft_remote_connection.as_ref(),
                    Some(EntryIdentity::Thread(draft_id)),
                )
            });

        let workspace_to_remove = draft_folder_paths.as_ref().and_then(|folder_paths| {
            self.linked_worktree_workspace_to_remove(
                folder_paths,
                draft_remote_connection.as_ref(),
                Some(draft_id),
                None,
                &roots_to_archive,
                cx,
            )
        });
        let mut workspaces_to_remove: Vec<Entity<Workspace>> =
            workspace_to_remove.into_iter().collect();
        let close_item_tasks = self.close_items_for_archived_worktrees(
            &roots_to_archive,
            &mut workspaces_to_remove,
            window,
            cx,
        );

        let draft_workspace_removed = matches!(
            workspace,
            ThreadEntryWorkspace::Open(workspace) if workspaces_to_remove.contains(workspace)
        );
        let workspace = workspace.clone();

        self.remove_workspaces_then(
            workspaces_to_remove,
            close_item_tasks,
            window,
            cx,
            move |this, window, cx| {
                if draft_workspace_removed
                    && let Some(draft_folder_paths) = draft_folder_paths.as_ref()
                {
                    this.delete_empty_drafts_for_archive_paths(
                        draft_folder_paths,
                        draft_remote_connection.as_ref(),
                        cx,
                    );
                }
                this.remove_draft_entry(
                    draft_id,
                    &workspace,
                    was_active,
                    neighbor.as_ref(),
                    !draft_workspace_removed,
                    roots_to_archive,
                    window,
                    cx,
                );
            },
        );
    }

    fn remove_draft_entry(
        &mut self,
        draft_id: ThreadId,
        workspace: &ThreadEntryWorkspace,
        was_active: bool,
        neighbor: Option<&ActivatableEntry>,
        activate_panel_draft: bool,
        roots_to_archive: Vec<thread_worktree_archive::RootPlan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Fallback to a neighbor thread when the discarded
        // draft was the active entry.
        let activate_panel_draft = activate_panel_draft && !(was_active && neighbor.is_some());

        let removed_from_panel = if let ThreadEntryWorkspace::Open(workspace) = workspace {
            workspace.update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        if activate_panel_draft {
                            panel.remove_thread(draft_id, window, cx);
                        } else {
                            panel.remove_thread_without_activating_draft(draft_id, window, cx);
                        }
                    });
                    true
                } else {
                    false
                }
            })
        } else {
            false
        };

        if !removed_from_panel {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete(draft_id, cx);
            });
        }

        self.start_detached_archive_worktree_task(roots_to_archive, cx);

        if was_active {
            self.active_entry = None;
            if !activate_panel_draft {
                if neighbor
                    .as_ref()
                    .is_some_and(|neighbor| self.activate_entry(neighbor, window, cx))
                {
                    return;
                }
                self.sync_active_entry_from_active_workspace(cx);
            }
        }

        self.update_entries(cx);
    }

    fn create_new_entry(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        self.create_new_thread(workspace, window, cx);
    }

    /// Starts a draft pinned to `workspace`'s worktree (the sidebar's
    /// per-worktree +), rather than the default new worktree.
    fn new_thread_in_worktree(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_new_thread(workspace, window, cx);
        if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| {
                panel.set_active_draft_worktree_choice(agent_ui::DraftWorktreeChoice::Current, cx);
            });
        }
    }

    fn create_new_thread(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });

        // A freshly opened workspace loads its agent panel asynchronously;
        // park the request and fulfill it from the `PanelAdded` handler.
        if workspace.read(cx).panel::<AgentPanel>(cx).is_none() {
            self.pending_new_thread_workspace = Some(workspace.downgrade());
            return;
        }

        let draft_id = workspace.update(cx, |workspace, cx| {
            let panel = workspace.panel::<AgentPanel>(cx)?;
            let draft_id = panel.update(cx, |panel, cx| {
                panel.activate_new_thread(true, AgentThreadSource::Sidebar, window, cx);
                panel.active_thread_id(cx)
            });
            workspace.focus_panel::<AgentPanel>(window, cx);
            draft_id
        });

        if let Some(draft_id) = draft_id {
            self.active_entry = Some(ActiveEntry::Thread {
                thread_id: draft_id,
                session_id: None,
                workspace: workspace.clone(),
            });
        }
    }

    fn create_new_terminal(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });

        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.new_terminal(Some(workspace), AgentThreadSource::Sidebar, window, cx);
                });
            }
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
    }

    fn active_project_group_key(&self, cx: &App) -> Option<ProjectGroupKey> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let multi_workspace = multi_workspace.read(cx);
        Some(multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx))
    }

    fn cycle_project_impl(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let keys = multi_workspace.read(cx).project_group_keys();
        if keys.is_empty() {
            return;
        }

        let current_pos = self
            .active_project_group_key(cx)
            .and_then(|active_key| keys.iter().position(|key| *key == active_key));

        let next_pos = match current_pos {
            Some(pos) => {
                let count = keys.len();
                if forward {
                    (pos + 1) % count
                } else {
                    (pos + count - 1) % count
                }
            }
            None => 0,
        };

        let key = keys[next_pos].clone();

        if let Some(workspace) = self.multi_workspace.upgrade().and_then(|mw| {
            mw.read(cx)
                .workspace_for_paths(key.path_list(), key.host().as_ref(), cx)
        }) {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace, None, window, cx);
                multi_workspace.retain_active_workspace(cx);
            });
        } else {
            self.open_workspace_for_group(&key, window, cx);
        }
    }

    fn on_next_project(&mut self, _: &NextProject, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(true, window, cx);
    }

    fn on_previous_project(
        &mut self,
        _: &PreviousProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_project_impl(false, window, cx);
    }

    fn cycle_thread_impl(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let thread_indices: Vec<usize> = self
            .contents
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| match entry {
                ListEntry::Thread(_) | ListEntry::Terminal(_) => Some(ix),
                _ => None,
            })
            .collect();

        if thread_indices.is_empty() {
            return;
        }

        let current_thread_pos = self.active_entry.as_ref().and_then(|active| {
            thread_indices
                .iter()
                .position(|&ix| active.matches_entry(&self.contents.entries[ix]))
        });

        let next_pos = match current_thread_pos {
            Some(pos) => {
                let count = thread_indices.len();
                if forward {
                    (pos + 1) % count
                } else {
                    (pos + count - 1) % count
                }
            }
            None => 0,
        };

        let entry_ix = thread_indices[next_pos];
        match &self.contents.entries[entry_ix] {
            ListEntry::Thread(thread) => {
                let metadata = thread.metadata.clone();
                match &thread.workspace {
                    ThreadEntryWorkspace::Open(workspace) => {
                        let workspace = workspace.clone();
                        self.activate_thread(metadata, &workspace, true, window, cx);
                    }
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        let folder_paths = folder_paths.clone();
                        let project_group_key = project_group_key.clone();
                        self.open_workspace_and_activate_thread(
                            metadata,
                            folder_paths,
                            &project_group_key,
                            window,
                            cx,
                        );
                    }
                }
            }
            ListEntry::Terminal(terminal) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.activate_terminal_entry(metadata, workspace, true, window, cx);
            }
            ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => {}
        }
    }

    fn on_next_thread(&mut self, _: &NextThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread_impl(true, window, cx);
    }

    fn on_previous_thread(
        &mut self,
        _: &PreviousThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_thread_impl(false, window, cx);
    }

    fn render_no_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = self.has_filter_query(cx);
        let message = if has_query {
            "No worktrees match your search."
        } else {
            "No worktrees yet"
        };

        v_flex()
            .id("sidebar-no-results")
            .p_4()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ProjectEmptyState::new(
            "Worktrees Sidebar",
            self.focus_handle(cx),
            KeyBinding::for_action(&workspace::Open::default(), cx),
        )
        .on_open_project(|_, window, cx| {
            let side = match AgentSettings::get_global(cx).sidebar_side() {
                SidebarSide::Left => "left",
                SidebarSide::Right => "right",
            };
            telemetry::event!("Sidebar Add Project Clicked", side = side);
            window.dispatch_action(
                Open {
                    create_new_window: Some(false),
                }
                .boxed_clone(),
                cx,
            );
        })
        .on_clone_repo(|_, window, cx| {
            window.dispatch_action(git::Clone.boxed_clone(), cx);
        })
    }

    fn render_sidebar_header(
        &self,
        no_open_projects: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_query = self.has_filter_query(cx);
        let sidebar_on_left = self.side(cx) == SidebarSide::Left;
        let sidebar_on_right = self.side(cx) == SidebarSide::Right;
        let not_fullscreen = !window.is_fullscreen() && !window.is_simple_fullscreen();
        let traffic_lights = cfg!(target_os = "macos") && not_fullscreen && sidebar_on_left;
        let left_window_controls = !cfg!(target_os = "macos") && not_fullscreen && sidebar_on_left;
        let right_window_controls =
            !cfg!(target_os = "macos") && not_fullscreen && sidebar_on_right;
        let header_height = platform_title_bar_height(window);

        h_flex()
            .h(header_height)
            .map(|header| match window.window_decorations() {
                Decorations::Client { .. } => header.mt(px(-1.)),
                Decorations::Server => header.mt_px().pb_px(),
            })
            .when(left_window_controls, |this| {
                this.children(Self::render_left_window_controls(window, cx))
            })
            .map(|this| {
                if traffic_lights {
                    this.pl(px(ui::utils::TRAFFIC_LIGHT_PADDING))
                } else if !left_window_controls {
                    this.pl_1p5()
                } else {
                    this
                }
            })
            .when(!right_window_controls, |this| this.pr_1p5())
            .gap_1()
            .when(!no_open_projects, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .when(traffic_lights, |this| {
                this.child(Divider::vertical().color(ui::DividerColor::Border))
            })
            // The toggle is the only way back out of the sidebar, so it stays
            // even when there is nothing to list.
            .child(
                IconButton::new(
                    "toggle-workspace-sidebar",
                    if sidebar_on_right {
                        IconName::ThreadsSidebarRightOpen
                    } else {
                        IconName::ThreadsSidebarLeftOpen
                    },
                )
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(|_, cx| {
                    Tooltip::for_action(
                        "Toggle Worktrees Sidebar",
                        &workspace::ToggleWorkspaceSidebar,
                        cx,
                    )
                })
                // Dispatched as an action: updating the MultiWorkspace
                // from inside this listener would re-enter the sidebar
                // entity and panic.
                .on_click(|_, window, cx| {
                    window.dispatch_action(workspace::ToggleWorkspaceSidebar.boxed_clone(), cx);
                }),
            )
            .when(!no_open_projects, |this| {
                this.child(
                    div().ml_1().child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(self.render_filter_input(cx))
                .child(
                    h_flex()
                        .gap_1()
                        .when(
                            self.selection.is_some()
                                && !self.filter_editor.focus_handle(cx).is_focused(window),
                            |this| this.child(KeyBinding::for_action(&FocusSidebarFilter, cx)),
                        )
                        .when(has_query, |this| {
                            this.child(
                                IconButton::new("clear_filter", IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Clear Search"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_filter_editor_text(window, cx);
                                        this.update_entries(cx);
                                    })),
                            )
                        }),
                )
            })
            .when(right_window_controls, |this| {
                this.children(Self::render_right_window_controls(window, cx))
            })
    }

    fn render_left_window_controls(window: &Window, cx: &mut App) -> Option<AnyElement> {
        platform_title_bar::render_left_window_controls(
            cx.button_layout(),
            Box::new(CloseWindow),
            window,
        )
    }

    fn render_right_window_controls(window: &Window, cx: &mut App) -> Option<AnyElement> {
        platform_title_bar::render_right_window_controls(
            cx.button_layout(),
            Box::new(CloseWindow),
            window,
        )
    }

    fn active_workspace(&self, cx: &App) -> Option<Entity<Workspace>> {
        self.multi_workspace
            .upgrade()
            .map(|w| w.read(cx).workspace().clone())
    }

    fn show_thread_import_modal(
        &mut self,
        source: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Threads Import Clicked",
            source = source,
            side = match self.side(cx) {
                SidebarSide::Left => "left",
                SidebarSide::Right => "right",
            }
        );

        let Some(active_workspace) = self.active_workspace(cx) else {
            return;
        };

        let Some(agent_registry_store) = AgentRegistryStore::try_global(cx) else {
            return;
        };

        let agent_server_store = active_workspace
            .read(cx)
            .project()
            .read(cx)
            .agent_server_store()
            .clone();

        let workspace_handle = active_workspace.downgrade();
        let multi_workspace = self.multi_workspace.clone();

        active_workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                ThreadImportModal::new(
                    agent_server_store,
                    agent_registry_store,
                    workspace_handle.clone(),
                    multi_workspace.clone(),
                    window,
                    cx,
                )
            });
        });
    }

    fn should_render_acp_import_onboarding(&self, cx: &App) -> bool {
        let has_external_agents = self
            .active_workspace(cx)
            .map(|ws| {
                ws.read(cx)
                    .project()
                    .read(cx)
                    .agent_server_store()
                    .read(cx)
                    .has_external_agents()
            })
            .unwrap_or(false);

        has_external_agents && !AcpThreadImportOnboarding::dismissed(cx)
    }

    fn render_acp_import_onboarding(
        &mut self,
        verbose_labels: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_import = cx.listener(|this, _, window, cx| {
            this.show_thread_import_modal("external_agent_onboarding", window, cx);
        });
        render_import_onboarding_banner(
            "acp",
            "Looking for conversations from external agents?",
            "Import conversations from agents like Claude Agent, Codex, and more, whether started in Zed or another client.",
            if verbose_labels {
                "Import Conversations from External Agents"
            } else {
                "Import Conversations"
            },
            |_, _window, cx| AcpThreadImportOnboarding::dismiss(cx),
            on_import,
            cx,
        )
    }

    fn should_render_cross_channel_import_onboarding(&self, cx: &App) -> bool {
        !CrossChannelImportOnboarding::dismissed(cx)
            && !self.cross_channel_import_channels.is_empty()
    }

    fn render_cross_channel_import_onboarding(
        &mut self,
        verbose_labels: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let channel_names = self
            .cross_channel_import_channels
            .iter()
            .map(SharedString::as_str)
            .join(" and ");

        let description = format!(
            "Import conversations from {} to continue where you left off.",
            channel_names
        );

        let on_import = cx.listener(|this, _, _window, cx| {
            telemetry::event!(
                "Agent Threads Import Clicked",
                source = "cross_channel_onboarding",
                side = match this.side(cx) {
                    SidebarSide::Left => "left",
                    SidebarSide::Right => "right",
                }
            );
            CrossChannelImportOnboarding::dismiss(cx);
            if let Some(workspace) = this.active_workspace(cx) {
                workspace.update(cx, |workspace, cx| {
                    import_threads_from_other_channels(workspace, cx);
                });
            }
        });
        render_import_onboarding_banner(
            "channel",
            "Conversations found from other channels",
            description,
            if verbose_labels {
                "Import Conversations from Other Channels"
            } else {
                "Import Conversations"
            },
            |_, _window, cx| CrossChannelImportOnboarding::dismiss(cx),
            on_import,
            cx,
        )
    }
}

fn render_import_onboarding_banner(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    button_label: impl Into<SharedString>,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_import: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let id: SharedString = id.into();
    let bg = cx.theme().colors().text_accent;

    v_flex()
        .min_w_0()
        .w_full()
        .p_2()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .bg(linear_gradient(
            360.,
            linear_color_stop(bg.opacity(0.06), 1.),
            linear_color_stop(bg.opacity(0.), 0.),
        ))
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .items_start()
                .justify_between()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .child(Label::new(title).size(LabelSize::Small)),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("close-{id}-onboarding")),
                        IconName::Close,
                    )
                    .icon_size(IconSize::Small)
                    .on_click(on_dismiss),
                ),
        )
        .child(
            Label::new(description)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .mb_2(),
        )
        .child(
            Button::new(SharedString::from(format!("import-{id}")), button_label)
                .full_width()
                .style(ButtonStyle::OutlinedCustom(cx.theme().colors().border))
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Download)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(on_import),
        )
}

impl WorkspaceSidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        !self.contents.notified_threads.is_empty() || !self.contents.notified_terminals.is_empty()
    }

    fn is_threads_list_view_active(&self) -> bool {
        true
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
    }

    fn prepare_for_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        cx.notify();
    }

    fn toggle_thread_switcher(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(select_last, window, cx);
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(forward, window, cx);
    }

    fn cycle_thread(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread_impl(forward, window, cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        let serialized = SerializedSidebar {
            width: Some(f32::from(self.width)),
            collapsed_sections: self.collapsed_sections.iter().copied().sorted().collect(),
        };
        serde_json::to_string(&serialized).ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(serialized) = serde_json::from_str::<SerializedSidebar>(state).log_err() {
            if let Some(width) = serialized.width {
                self.width = px(width).clamp(MIN_WIDTH, MAX_WIDTH);
            }
            self.collapsed_sections = serialized.collapsed_sections.into_iter().collect();
            // Restore runs while the MultiWorkspace is mid-update, which
            // rebuilding entries would read back into.
            self.schedule_update_entries(false, cx);
        }
        cx.notify();
    }
}

impl gpui::EventEmitter<workspace::SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _titlebar_height = ui::utils::platform_title_bar_height(window);
        let ui_font = theme_settings::setup_ui_font(window, cx);

        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let no_open_projects = !self.contents.has_open_projects;
        let no_search_results = self.contents.entries.is_empty();

        v_flex()
            .id("workspace-sidebar")
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::editor_move_down))
            .on_action(cx.listener(Self::editor_move_up))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::archive_selected_thread))
            .on_action(cx.listener(Self::remove_selected_thread))
            .on_action(cx.listener(Self::rename_selected_thread))
            .on_action(cx.listener(Self::new_thread_in_group))
            .on_action(cx.listener(Self::new_terminal_thread))
            .on_action(cx.listener(Self::focus_sidebar_filter))
            .on_action(cx.listener(Self::on_toggle_thread_switcher))
            .on_action(cx.listener(Self::on_next_project))
            .on_action(cx.listener(Self::on_previous_project))
            .on_action(cx.listener(Self::on_next_thread))
            .on_action(cx.listener(Self::on_previous_thread))
            .on_action(cx.listener(|this, _: &OpenRecent, window, cx| {
                this.recent_projects_popover_handle.toggle(window, cx);
            }))
            .font(ui_font)
            .map(|el| {
                let on_left = self.side(cx) == SidebarSide::Left;
                match window.window_decorations() {
                    Decorations::Server => el.h_full().w(self.width),
                    // With client-side decorations the sidebar owns the window
                    // corners on its side, so round them like the title bar and
                    // status bar do. The sidebar is stretched 1px outwards over
                    // the window border on untiled edges (with compensating
                    // padding) so its rounded background lines up exactly with
                    // the window shape, avoiding a transparent gap in the
                    // rounded corners.
                    Decorations::Client { tiling, .. } => el
                        .absolute()
                        .top(if tiling.top { px(0.) } else { px(-1.) })
                        .bottom(if tiling.bottom { px(0.) } else { px(-1.) })
                        .when(!tiling.top, |el| el.pt_px())
                        .when(!tiling.bottom, |el| el.pb_px())
                        .map(|el| {
                            if on_left {
                                el.right(px(0.))
                                    .left(if tiling.left { px(0.) } else { px(-1.) })
                                    .when(!tiling.left, |el| el.pl(px(1.)))
                            } else {
                                el.left(px(0.))
                                    .right(if tiling.right { px(0.) } else { px(-1.) })
                                    .when(!tiling.right, |el| el.pr(px(1.)))
                            }
                        })
                        .when(on_left && !(tiling.top || tiling.left), |el| {
                            el.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(on_left && !(tiling.bottom || tiling.left), |el| {
                            el.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.top || tiling.right), |el| {
                            el.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.bottom || tiling.right), |el| {
                            el.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                        }),
                }
            })
            .bg(bg)
            .when(self.side(cx) == SidebarSide::Left, |el| el.border_r_1())
            .when(self.side(cx) == SidebarSide::Right, |el| el.border_l_1())
            .border_color(color.border)
            .child(self.render_sidebar_header(no_open_projects, window, cx))
            .map(|this| {
                if no_open_projects {
                    this.child(self.render_empty_state(cx))
                } else {
                    this.child(
                        v_flex()
                            .relative()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                list(
                                    self.list_state.clone(),
                                    cx.processor(Self::render_list_entry),
                                )
                                .flex_1()
                                .size_full(),
                            )
                            .when(no_search_results, |this| {
                                this.child(self.render_no_results(cx))
                            })
                            .custom_scrollbars(
                                Scrollbars::new(ScrollAxes::Vertical)
                                    .tracked_scroll_handle(&self.list_state),
                                window,
                                cx,
                            ),
                    )
                }
            })
            .map(|this| {
                let show_acp = self.should_render_acp_import_onboarding(cx);
                let show_cross_channel = self.should_render_cross_channel_import_onboarding(cx);

                let verbose = *self
                    .import_banners_use_verbose_labels
                    .get_or_insert(show_acp && show_cross_channel);

                this.when(show_acp, |this| {
                    this.child(self.render_acp_import_onboarding(verbose, cx))
                })
                .when(show_cross_channel, |this| {
                    this.child(self.render_cross_channel_import_onboarding(verbose, cx))
                })
            })
    }
}

fn all_thread_infos_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> impl Iterator<Item = ActiveThreadInfo> {
    let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
        return None.into_iter().flatten();
    };
    let agent_panel = agent_panel.read(cx);
    let threads = agent_panel
        .conversation_views()
        .into_iter()
        .filter_map(|conversation_view| {
            let has_pending_tool_call = conversation_view
                .read(cx)
                .root_thread_has_pending_tool_call(cx);
            let thread_view = conversation_view.read(cx).root_thread_view()?;
            let thread_view_ref = thread_view.read(cx);
            let thread = thread_view_ref.thread.read(cx);

            let icon = thread_view_ref.agent_icon;
            let icon_from_external_svg = thread_view_ref.agent_icon_from_external_svg.clone();
            let title = thread
                .title()
                .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into());
            let is_title_generating = thread_view_ref
                .as_native_thread(cx)
                .is_some_and(|native_thread| native_thread.read(cx).is_generating_title());
            let session_id = thread.session_id().clone();

            let status = if has_pending_tool_call {
                AgentThreadStatus::WaitingForConfirmation
            } else if thread.had_error() {
                AgentThreadStatus::Error
            } else {
                match thread.status() {
                    ThreadStatus::Generating => AgentThreadStatus::Running,
                    ThreadStatus::Idle => AgentThreadStatus::Completed,
                }
            };

            let diff_stats = thread.action_log().read(cx).diff_stats(cx);

            Some(ActiveThreadInfo {
                session_id,
                title,
                status,
                icon,
                icon_from_external_svg,
                is_title_generating,
                diff_stats,
            })
        });

    Some(threads).into_iter().flatten()
}

pub fn dump_workspace_info(
    workspace: &mut Workspace,
    _: &DumpWorkspaceInfo,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    use std::fmt::Write;

    let mut output = String::new();
    let this_entity = cx.entity();

    let multi_workspace = workspace.multi_workspace().and_then(|weak| weak.upgrade());
    let workspaces: Vec<gpui::Entity<Workspace>> = match &multi_workspace {
        Some(mw) => mw.read(cx).workspaces().cloned().collect(),
        None => vec![this_entity.clone()],
    };
    let active_workspace = multi_workspace
        .as_ref()
        .map(|mw| mw.read(cx).workspace().clone());

    writeln!(output, "MultiWorkspace: {} workspace(s)", workspaces.len()).ok();

    if let Some(mw) = &multi_workspace {
        let keys: Vec<_> = mw.read(cx).project_group_keys();
        writeln!(output, "Project group keys ({}):", keys.len()).ok();
        for key in keys {
            writeln!(output, "  - {key:?}").ok();
        }
    }

    writeln!(output).ok();

    for (index, ws) in workspaces.iter().enumerate() {
        let is_active = active_workspace.as_ref() == Some(ws);
        writeln!(
            output,
            "--- Workspace {index}{} ---",
            if is_active { " (active)" } else { "" }
        )
        .ok();

        // project_group_key_for_workspace internally reads the workspace,
        // so we can only call it for workspaces other than this_entity
        // (which is already being updated).
        if let Some(mw) = &multi_workspace {
            if *ws == this_entity {
                let workspace_key = workspace.project_group_key(cx);
                writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
            } else {
                let effective_key = mw.read(cx).project_group_key_for_workspace(ws, cx);
                let workspace_key = ws.read(cx).project_group_key(cx);
                if effective_key != workspace_key {
                    writeln!(
                        output,
                        "ProjectGroupKey (multi_workspace): {effective_key:?}"
                    )
                    .ok();
                    writeln!(
                        output,
                        "ProjectGroupKey (workspace, DISAGREES): {workspace_key:?}"
                    )
                    .ok();
                } else {
                    writeln!(output, "ProjectGroupKey: {effective_key:?}").ok();
                }
            }
        } else {
            let workspace_key = workspace.project_group_key(cx);
            writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
        }

        // The action handler is already inside an update on `this_entity`,
        // so we must avoid a nested read/update on that same entity.
        if *ws == this_entity {
            dump_single_workspace(workspace, &mut output, cx);
        } else {
            ws.read_with(cx, |ws, cx| {
                dump_single_workspace(ws, &mut output, cx);
            });
        }
    }

    let project = workspace.project().clone();
    cx.spawn_in(window, async move |_this, cx| {
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, false, cx))
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(output, cx);
        });

        let buffer = cx.new(|cx| {
            editor::MultiBuffer::singleton(buffer, cx).with_title("Workspace Info".into())
        });

        _this.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(cx.new(|cx| {
                    let mut editor =
                        editor::Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                    editor.set_read_only(true);
                    editor.set_should_serialize(false, cx);
                    editor.set_breadcrumb_header("Workspace Info".into());
                    editor
                })),
                None,
                true,
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

fn dump_single_workspace(workspace: &Workspace, output: &mut String, cx: &gpui::App) {
    use std::fmt::Write;

    let workspace_db_id = workspace.database_id();
    match workspace_db_id {
        Some(id) => writeln!(output, "Workspace DB ID: {id:?}").ok(),
        None => writeln!(output, "Workspace DB ID: (none)").ok(),
    };

    let project = workspace.project().read(cx);

    let repos: Vec<_> = project
        .repositories(cx)
        .values()
        .map(|repo| repo.read(cx).snapshot())
        .collect();

    writeln!(output, "Worktrees:").ok();
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let abs_path = worktree.abs_path();
        let visible = worktree.is_visible();

        let repo_info = repos
            .iter()
            .find(|snapshot| abs_path.starts_with(&*snapshot.work_directory_abs_path));

        let is_linked = repo_info.map(|s| s.is_linked_worktree()).unwrap_or(false);
        let main_worktree_path = repo_info.and_then(|s| s.main_worktree_abs_path());
        let branch = repo_info.and_then(|s| s.branch.as_ref().map(|b| b.ref_name.clone()));

        write!(output, "  - {}", abs_path.display()).ok();
        if !visible {
            write!(output, " (hidden)").ok();
        }
        if let Some(branch) = &branch {
            write!(output, " [branch: {branch}]").ok();
        }
        if is_linked {
            if let Some(main_worktree_path) = main_worktree_path {
                write!(
                    output,
                    " [linked worktree -> {}]",
                    main_worktree_path.display()
                )
                .ok();
            } else {
                write!(output, " [linked worktree]").ok();
            }
        }
        writeln!(output).ok();
    }

    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        let panel = panel.read(cx);

        let panel_workspace_id = panel.workspace_id();
        if panel_workspace_id != workspace_db_id {
            writeln!(
                output,
                "  \u{26a0} workspace ID mismatch! panel has {panel_workspace_id:?}, workspace has {workspace_db_id:?}"
            )
            .ok();
        }

        if let Some(thread) = panel.active_agent_thread(cx) {
            let thread = thread.read(cx);
            let title = thread.title().unwrap_or_else(|| "(untitled)".into());
            let session_id = thread.session_id();
            let status = match thread.status() {
                ThreadStatus::Idle => "idle",
                ThreadStatus::Generating => "generating",
            };
            let entry_count = thread.entries().len();
            write!(output, "Active thread: {title} (session: {session_id})").ok();
            write!(output, " [{status}, {entry_count} entries").ok();
            if panel
                .active_conversation_view()
                .is_some_and(|conversation_view| {
                    conversation_view
                        .read(cx)
                        .root_thread_has_pending_tool_call(cx)
                })
            {
                write!(output, ", awaiting confirmation").ok();
            }
            writeln!(output, "]").ok();
        } else {
            writeln!(output, "Active thread: (none)").ok();
        }

        let open_tabs = panel.open_thread_tab_ids(cx);
        if !open_tabs.is_empty() {
            writeln!(output, "Open thread tabs ({}): ", open_tabs.len()).ok();
            for thread_id in open_tabs {
                let Some(conversation_view) = panel.conversation_view_for_id(&thread_id, cx) else {
                    writeln!(output, "  - (missing view) (thread: {thread_id:?})").ok();
                    continue;
                };
                if let Some(thread_view) = conversation_view.read(cx).root_thread_view() {
                    let thread = thread_view.read(cx).thread.read(cx);
                    let title = thread.title().unwrap_or_else(|| "(untitled)".into());
                    let status = match thread.status() {
                        ThreadStatus::Idle => "idle",
                        ThreadStatus::Generating => "generating",
                    };
                    let entry_count = thread.entries().len();
                    write!(output, "  - {title} (thread: {thread_id:?})").ok();
                    write!(output, " [{status}, {entry_count} entries").ok();
                    if conversation_view
                        .read(cx)
                        .root_thread_has_pending_tool_call(cx)
                    {
                        write!(output, ", awaiting confirmation").ok();
                    }
                    writeln!(output, "]").ok();
                } else {
                    writeln!(output, "  - (not connected) (thread: {thread_id:?})").ok();
                }
            }
        }
    } else {
        writeln!(output, "Agent panel: not loaded").ok();
    }

    writeln!(output).ok();
}
