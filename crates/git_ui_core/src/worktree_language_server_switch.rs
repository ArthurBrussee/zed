//! Persistence and the status bar control for [`project::worktree_language_servers`].

use std::path::{Path, PathBuf};

use db::kvp::KeyValueStore;
use gpui::{App, Context, Empty, Entity, Task, Window};
use project::{
    Project,
    worktree_language_servers::{self, WorktreeLanguageServers},
};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

const NAMESPACE: &str = "worktree_language_servers";
/// Not the old `disabled` key, which recorded exceptions under the opposite default.
const ENABLED_KEY: &str = "enabled";

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

pub fn set_enabled(project: &Entity<Project>, worktree_path: &Path, enabled: bool, cx: &mut App) {
    let remote = project.read(cx).remote_client();
    let lsp_store = remote.is_none().then(|| project.read(cx).lsp_store());
    if !worktree_language_servers::switch(lsp_store.as_ref(), worktree_path, enabled, cx) {
        return;
    }
    remember(cx).detach();

    if let Some(remote) = remote {
        let (enabled_paths, disabled_paths) = if enabled {
            (vec![worktree_path.to_path_buf()], Vec::new())
        } else {
            (Vec::new(), vec![worktree_path.to_path_buf()])
        };
        let send = worktree_language_servers::send_to_remote(
            &remote.read(cx).proto_client(),
            enabled_paths,
            disabled_paths,
        );
        cx.background_spawn(send).detach();
    }
}

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
        // Called from inside the workspace's update, so reading the workspace here would panic.
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
