//! The switch that says whether a worktree runs language servers, and the
//! memory of which worktrees were switched on.
//!
//! The switch itself is [`project::worktree_language_servers`], which is
//! where the answer is read when servers are resolved. This module is
//! everything around it: remembering the choice across restarts, and the
//! status bar control that flips it.

use std::path::{Path, PathBuf};

use db::kvp::KeyValueStore;
use gpui::{App, Context, Empty, Entity, Task, Window};
use project::{Project, worktree_language_servers::WorktreeLanguageServers};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

const NAMESPACE: &str = "worktree_language_servers";
/// The worktrees switched on. A different key from the `disabled` list the
/// switch kept while off was the exception rather than the rule: that list
/// says which worktrees were the unusual ones under the old default, which
/// is not an answer to the question this one asks. It is left where it is
/// and not read.
const ENABLED_KEY: &str = "enabled";

/// Installs the switch and puts back what the last run remembered.
pub fn init(cx: &mut App) {
    project::worktree_language_servers::init(cx);
    let remembered = read_enabled(cx);
    if remembered.is_empty() {
        return;
    }
    WorktreeLanguageServers::global(cx).update(cx, |store, cx| store.restore(remembered, cx));
}

fn read_enabled(cx: &App) -> Vec<PathBuf> {
    let Some(Some(value)) = KeyValueStore::global(cx)
        .scoped(NAMESPACE)
        .read(ENABLED_KEY)
        .log_err()
    else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<PathBuf>>(&value)
        .log_err()
        .unwrap_or_default()
}

fn remember(cx: &App) -> Task<()> {
    let enabled = WorktreeLanguageServers::global(cx).read(cx).enabled_paths();
    let store = KeyValueStore::global(cx);
    let value = serde_json::to_string(&enabled);
    cx.background_spawn(async move {
        if let Some(value) = value.log_err() {
            store.scoped(NAMESPACE).write(ENABLED_KEY.to_string(), value).await.log_err();
        }
    })
}

/// Flips one worktree's switch, and moves the servers to match: the buffers
/// open in that worktree lose theirs, or get them.
pub fn set_enabled(project: &Entity<Project>, worktree_path: &Path, enabled: bool, cx: &mut App) {
    let Some(store) = WorktreeLanguageServers::try_global(cx) else {
        return;
    };
    if !store.update(cx, |store, cx| {
        store.set_enabled(worktree_path, enabled, cx)
    }) {
        return;
    }
    remember(cx).detach();

    let buffers = buffers_in_worktree(project, worktree_path, cx);
    if buffers.is_empty() {
        return;
    }
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            if enabled {
                lsp_store.restart_language_servers_for_buffers(buffers, Default::default(), true, cx);
            } else {
                lsp_store
                    .stop_language_servers_for_buffers(buffers, Default::default(), cx)
                    .detach_and_log_err(cx);
            }
        })
    });
}

fn buffers_in_worktree(
    project: &Entity<Project>,
    worktree_path: &Path,
    cx: &App,
) -> Vec<Entity<language::Buffer>> {
    project
        .read(cx)
        .buffer_store()
        .read(cx)
        .buffers()
        .filter(|buffer| {
            buffer
                .read(cx)
                .file()
                .and_then(|file| file.as_local())
                .is_some_and(|file| file.abs_path(cx).starts_with(worktree_path))
        })
        .collect()
}

/// The status bar's switch: whether the worktree the active file lives in
/// runs language servers. Absent when the window has no worktree to speak
/// for, which is every window showing nothing but threads.
pub struct WorktreeLanguageServerSwitch {
    project: Entity<Project>,
    worktree: Option<(PathBuf, SharedString)>,
}

impl WorktreeLanguageServerSwitch {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        if let Some(store) = WorktreeLanguageServers::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            project: workspace.project().clone(),
            worktree: None,
        }
    }

    fn update_worktree(&mut self, item: Option<&dyn ItemHandle>, cx: &mut Context<Self>) {
        // The status bar calls `set_active_pane_item` from inside the
        // workspace's own update, so reading the workspace here is a double
        // lease and gpui panics. The project is its own entity and the active
        // item carries its path, which is all this needs.
        let worktree = {
            let project = self.project.read(cx);
            item.and_then(|item| item.project_path(cx))
                .and_then(|path| project.worktree_for_id(path.worktree_id, cx))
                .or_else(|| project.visible_worktrees(cx).next())
                .map(|worktree| {
                    let worktree = worktree.read(cx);
                    (
                        worktree.abs_path().as_ref().to_path_buf(),
                        SharedString::from(worktree.root_name_str().to_string()),
                    )
                })
        };
        if worktree != self.worktree {
            self.worktree = worktree;
            cx.notify();
        }
    }
}

impl Render for WorktreeLanguageServerSwitch {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((path, name)) = self.worktree.clone() else {
            return Empty.into_any_element();
        };
        let Some(store) = WorktreeLanguageServers::try_global(cx) else {
            return Empty.into_any_element();
        };
        let enabled = store.read(cx).is_enabled(&path);
        let project = self.project.clone();

        IconButton::new(
            "worktree-language-servers",
            if enabled {
                IconName::BoltFilled
            } else {
                IconName::BoltOutlined
            },
        )
        .icon_size(IconSize::Small)
        .icon_color(if enabled { Color::Default } else { Color::Muted })
        .tooltip(move |_window, cx| {
            Tooltip::with_meta(
                if enabled {
                    "Language Servers On"
                } else {
                    "Language Servers Off"
                },
                None,
                format!(
                    "In {name}. Click to turn them {}.",
                    if enabled { "off" } else { "on" }
                ),
                cx,
            )
        })
        .on_click(move |_, _window, cx| {
            set_enabled(&project, &path, !enabled, cx);
        })
        .into_any_element()
    }
}

impl StatusItemView for WorktreeLanguageServerSwitch {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_worktree(active_pane_item, cx);
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
