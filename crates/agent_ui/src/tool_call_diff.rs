//! One tool call's edits, as a diff multibuffer.
//!
//! A file chip opens this: the real project buffer, shown against the text as
//! it stood before the call, with every hunk expanded. It is the same shape as
//! the branch diff (a [`SplittableEditor`] over a multibuffer, so side by side
//! works) but scoped to what one call did, since that is what the chip names.
//!
//! Decorating an already-open singleton editor was the alternative, and it is
//! why this exists: that editor belongs to whoever opened it, its diff can be
//! replaced or missing, and a failure to attach one looks identical to a file
//! with no changes.

use std::any::Any;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    Editor, EditorEvent, EditorSettings, SplittableEditor, multibuffer_context_lines,
    scroll::Autoscroll,
};
use gpui::{
    AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity,
};
use language::{Buffer, Capability, OffsetRangeExt as _, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::{Icon, IconName, Label, LabelCommon as _, prelude::*};
use util::ResultExt as _;
use workspace::item::HighlightedText;
use workspace::{
    ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{Item, ItemEvent, TabContentParams},
    searchable::SearchableItemHandle,
};

/// Which call's edits to which file. Reopening the same chip activates the
/// existing tab; a different call editing the same file gets its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallDiffKey {
    pub tool_call_id: acp::ToolCallId,
    pub path: PathBuf,
}

pub struct ToolCallDiff {
    key: ToolCallDiffKey,
    title: SharedString,
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<SplittableEditor>,
    focus_handle: FocusHandle,
}

impl ToolCallDiff {
    /// Opens the diff for one edited file, reusing the tab if this call's diff
    /// for it is already open. The buffer is opened from the project, so the
    /// view is the real file with real syntax, not a copy of it.
    pub fn deploy(
        key: ToolCallDiffKey,
        base_text: Arc<str>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let existing = workspace
            .update(cx, |workspace, cx| {
                workspace
                    .items_of_type::<ToolCallDiff>(cx)
                    .find(|item| item.read(cx).key == key)
            })
            .ok()
            .flatten();
        if let Some(existing) = existing {
            let activated = workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, true, window, cx);
            });
            return Task::ready(activated);
        }

        let Some(project_path) = project
            .read(cx)
            .find_project_path(&key.path, cx)
            .or_else(|| Self::project_path_by_name(&key.path, &project, cx))
        else {
            // Nothing silent: a file we cannot place is worth saying out loud,
            // since the click looks like it did nothing.
            return Task::ready(Err(anyhow::anyhow!(
                "no file in this project matches {}",
                key.path.display()
            )));
        };

        window.spawn(cx, async move |cx| {
            let open_buffer =
                project.update(cx, |project, cx| project.open_buffer(project_path, cx));
            let buffer = open_buffer.await?;

            let (diff, set_base_text) = cx.update(|_window, cx| {
                let snapshot = buffer.read(cx).text_snapshot();
                let language = buffer.read(cx).language().cloned();
                let language_registry = buffer.read(cx).language_registry();
                // The base is the text the call found, not anything git knows.
                let diff = cx.new(|cx| {
                    BufferDiff::new(
                        &snapshot,
                        language,
                        language_registry,
                        buffer_diff::DiffBaseKind::Custom,
                        cx,
                    )
                });
                let task = diff.update(cx, |diff, cx| {
                    diff.set_base_text(Some(base_text.clone()), snapshot.clone(), cx)
                });
                (diff, task)
            })?;
            set_base_text.await;

            workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    Self::new(
                        key,
                        buffer,
                        diff,
                        project.clone(),
                        workspace.weak_handle(),
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_center(Box::new(view), window, cx);
            })?;
            Ok(())
        })
    }

    /// A path an agent reported that the project cannot resolve directly
    /// (agents report absolute paths, and remote projects report their own).
    /// Matching on the file name keeps the chip working rather than silently
    /// opening nothing.
    fn project_path_by_name(
        path: &std::path::Path,
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<ProjectPath> {
        let name = path.file_name()?;
        let project = project.read(cx);
        project.worktrees(cx).find_map(|worktree| {
            let worktree = worktree.read(cx);
            let entry = worktree
                .entries(false, 0)
                .find(|entry| entry.path.file_name().is_some_and(|entry| entry == name))?;
            Some(ProjectPath {
                worktree_id: worktree.id(),
                path: entry.path.clone(),
            })
        })
    }

    fn new(
        key: ToolCallDiffKey,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title: SharedString = key
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.path.to_string_lossy().into_owned())
            .into();
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let workspace_entity = workspace.upgrade();
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project,
                workspace_entity.expect("a diff is only opened from a workspace"),
                window,
                cx,
            );
            editor.update_editors(cx, |editor, cx| {
                editor.set_expand_all_diff_hunks(cx);
            });
            editor
        });

        let hunk_ranges: Vec<Range<Point>> = {
            let snapshot = buffer.read(cx).snapshot();
            diff.read(cx)
                .snapshot(cx)
                .hunks_intersecting_range(
                    language::Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                    &snapshot,
                )
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect()
        };

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                PathKey::for_buffer(&buffer, cx),
                buffer.clone(),
                hunk_ranges,
                multibuffer_context_lines(cx),
                diff,
                cx,
            );
        });

        Self {
            key,
            title,
            multibuffer,
            editor,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The excerpts being shown: what the call actually changed.
    pub fn multibuffer(&self) -> &Entity<MultiBuffer> {
        &self.multibuffer
    }

    fn rhs_editor(&self, cx: &App) -> Entity<Editor> {
        self.editor.read(cx).rhs_editor().clone()
    }
}

/// Whether this call's diff for this file is open somewhere in the workspace,
/// so the chip that opens it can read as selected.
pub fn is_tool_call_diff_open(
    key: &ToolCallDiffKey,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> bool {
    workspace.upgrade().is_some_and(|workspace| {
        workspace
            .read(cx)
            .items_of_type::<ToolCallDiff>(cx)
            .any(|item| &item.read(cx).key == key)
    })
}

impl EventEmitter<EditorEvent> for ToolCallDiff {}

impl Focusable for ToolCallDiff {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ToolCallDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.multibuffer().read(cx).is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new("This edit left the file unchanged.")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element();
        }
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.editor.clone())
            .into_any_element()
    }
}

impl Item for ToolCallDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.title.clone())
            .when(!params.selected, |this| this.color(Color::Muted))
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} (agent edit)", self.key.path.display()).into())
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rhs_editor(cx)
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.rhs_editor(cx)
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rhs_editor(cx)
            .update(cx, |editor, _cx| editor.set_nav_history(Some(history)));
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.rhs_editor(cx)))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.rhs_editor(cx).read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn breadcrumb_location(&self, _cx: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        self.rhs_editor(cx).read(cx).breadcrumbs(cx)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Tool Call Diff Opened")
    }
}

/// Puts the cursor on the first hunk, so the view opens on the change rather
/// than at the top of the file.
pub fn reveal_first_hunk(view: &Entity<ToolCallDiff>, window: &mut Window, cx: &mut App) {
    let editor = view.read(cx).rhs_editor(cx);
    let focus_handle = view.read(cx).focus_handle.clone();
    editor.update(cx, |editor, cx| {
        let first_hunk_row = editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .diff_hunks()
            .next()
            .map(|hunk| hunk.row_range.start.0);
        if let Some(row) = first_hunk_row {
            editor.change_selections(
                editor::SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| {
                    selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
                },
            );
        }
    });
    focus_handle.focus(window, cx);
}

/// Opens the diff and reveals its first change.
pub fn open_tool_call_diff(
    key: ToolCallDiffKey,
    base_text: Arc<str>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let deploy = ToolCallDiff::deploy(
        key.clone(),
        base_text,
        project,
        workspace.clone(),
        window,
        cx,
    );
    window.spawn(cx, async move |cx| {
        if deploy.await.log_err().is_none() {
            return;
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                let Some(view) = workspace
                    .items_of_type::<ToolCallDiff>(cx)
                    .find(|item| item.read(cx).key == key)
                else {
                    return;
                };
                reveal_first_hunk(&view, window, cx);
            })
            .log_err();
    })
}
