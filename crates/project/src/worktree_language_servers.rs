//! Which worktrees run language servers. Off by default: restoring many worktree windows otherwise
//! starts a server indexing a copy of the same repository in each.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use collections::HashSet;
use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::worktree_store::WorktreeStore;

#[derive(Default)]
pub struct WorktreeLanguageServers {
    enabled: HashSet<Arc<Path>>,
}

struct GlobalWorktreeLanguageServers(Entity<WorktreeLanguageServers>);

impl Global for GlobalWorktreeLanguageServers {}

pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalWorktreeLanguageServers>() {
        return;
    }
    let store = cx.new(|_| WorktreeLanguageServers::default());
    cx.set_global(GlobalWorktreeLanguageServers(store));
}

impl WorktreeLanguageServers {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalWorktreeLanguageServers>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalWorktreeLanguageServers>()
            .map(|global| global.0.clone())
    }

    pub fn is_enabled(&self, abs_path: &Path) -> bool {
        self.enabled.contains(abs_path)
    }

    /// Returns whether anything changed.
    pub fn set_enabled(&mut self, abs_path: &Path, enabled: bool, cx: &mut Context<Self>) -> bool {
        let changed = if enabled {
            self.enabled.insert(abs_path.into())
        } else {
            self.enabled.remove(abs_path)
        };
        if changed {
            cx.notify();
        }
        changed
    }

    pub fn enabled_paths(&self) -> Vec<PathBuf> {
        self.enabled
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect()
    }

    pub fn restore(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        let before = self.enabled.len();
        self.enabled
            .extend(paths.into_iter().map(|path| Arc::from(path.as_path())));
        if self.enabled.len() != before {
            cx.notify();
        }
    }
}

/// True when the switch isn't installed (tests, headless), so only an installed switch stops servers.
pub fn worktree_runs_language_servers(
    worktree_store: &Entity<WorktreeStore>,
    worktree_id: worktree::WorktreeId,
    cx: &App,
) -> bool {
    let Some(store) = WorktreeLanguageServers::try_global(cx) else {
        return true;
    };
    if store.read(cx).enabled.is_empty() {
        return false;
    }
    let Some(worktree) = worktree_store.read(cx).worktree_for_id(worktree_id, cx) else {
        return false;
    };
    let abs_path = worktree.read(cx).abs_path();
    store.read(cx).is_enabled(&abs_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_a_worktree_runs_no_language_servers_until_it_is_switched_on(cx: &mut TestAppContext) {
        let store = cx.new(|_| WorktreeLanguageServers::default());
        let wanted = Path::new("/repo/wants-servers");
        let other = Path::new("/repo/does-not");

        store.update(cx, |store, cx| {
            assert!(!store.is_enabled(wanted), "off before anyone asks");
            assert!(!store.is_enabled(other));

            assert!(store.set_enabled(wanted, true, cx), "switching on changes it");
            assert!(!store.set_enabled(wanted, true, cx), "and again does not");
            assert!(store.is_enabled(wanted));
            assert!(
                !store.is_enabled(other),
                "one worktree asking must not answer for the rest"
            );

            assert_eq!(store.enabled_paths(), vec![wanted.to_path_buf()]);

            assert!(store.set_enabled(wanted, false, cx));
            assert!(!store.is_enabled(wanted));
            assert!(store.enabled_paths().is_empty());
        });
    }

    #[gpui::test]
    fn test_restore_puts_back_the_worktrees_that_asked(cx: &mut TestAppContext) {
        let store = cx.new(|_| WorktreeLanguageServers::default());
        let early = Path::new("/repo/switched-on-early");
        let remembered = Path::new("/repo/remembered");

        store.update(cx, |store, cx| {
            store.set_enabled(early, true, cx);
            store.restore([remembered.to_path_buf()], cx);
            assert!(store.is_enabled(remembered));
            assert!(store.is_enabled(early));
        });
    }
}
