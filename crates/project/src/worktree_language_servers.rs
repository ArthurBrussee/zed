//! Which worktrees run language servers.
//!
//! Off is the default, everywhere. Starting a server in a worktree costs a
//! full index of a copy of the repository into its own target directory, and
//! a machine that restores fourteen worktree windows pays that fourteen times
//! over at once: one launch measured a load average of 188 on a 15-core
//! machine, with eighteen compiler processes, and took ten minutes to become
//! usable. Almost none of those worktrees wanted a language server — an agent
//! does not use one — so the worktree that does asks for it, through the
//! status-bar switch, and is remembered.
//!
//! This is a switch rather than a setting: nothing is written to the worktree,
//! so the checkout stays clean. What is remembered across restarts is
//! remembered by whoever installed the switch, which hands the paths back
//! through [`WorktreeLanguageServers::restore`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use collections::HashSet;
use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::worktree_store::WorktreeStore;

/// The worktrees whose language servers are switched on, by absolute path.
/// A worktree that is not named here runs none, whoever created it and
/// whatever its settings ask for.
#[derive(Default)]
pub struct WorktreeLanguageServers {
    enabled: HashSet<Arc<Path>>,
}

struct GlobalWorktreeLanguageServers(Entity<WorktreeLanguageServers>);

impl Global for GlobalWorktreeLanguageServers {}

/// Install the global store. Idempotent.
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

    /// Switches one worktree's language servers on or off. Returns whether
    /// this changed anything, so a caller can skip the work of starting or
    /// stopping servers that are already in the state asked for.
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

    /// The worktrees switched on, for whoever is persisting them.
    pub fn enabled_paths(&self) -> Vec<PathBuf> {
        self.enabled
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect()
    }

    /// Puts back what was remembered from a previous run. Anything switched
    /// on in this one stays on: a worktree that asked before the restore
    /// landed has already made its choice.
    pub fn restore(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        let before = self.enabled.len();
        self.enabled
            .extend(paths.into_iter().map(|path| Arc::from(path.as_path())));
        if self.enabled.len() != before {
            cx.notify();
        }
    }
}

/// Whether the worktree with this id runs language servers. False for every
/// worktree nobody switched on.
///
/// The two escapes are deliberate. Without the global — a test, a headless
/// run, anything that did not install the switch — the answer is yes, because
/// a switch that was never installed must not be what stops a server. And a
/// worktree whose path cannot be resolved is a worktree the switch has no way
/// to name, so it cannot be the one that was turned on; it reads as off along
/// with everything else.
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

    /// Off is the default, and the switch is what makes an exception of one
    /// worktree. The old store worked the other way round — every worktree on
    /// unless it was named — which is what had fourteen restored windows
    /// starting eighteen compiler processes between them.
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

    /// What the last run remembered is what is switched on again, and a
    /// worktree switched on before the restore lands keeps its answer.
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
