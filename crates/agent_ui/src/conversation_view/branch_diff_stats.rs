use std::{collections::HashSet, path::PathBuf, time::Duration};

use anyhow::Result;
use git::{
    repository::DiffType,
    status::{DiffStat, FileStatus},
};
use gpui::{App, AsyncApp, Context, Entity, SharedString, Subscription, Task, WeakEntity};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use util::ResultExt as _;

/// Collapses a turn's worth of file writes (one git status event each) into a
/// single recomputation.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// What the working tree is compared against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffStatsBase {
    /// The merge base with the repository's default branch: the base the
    /// unified diff view uses, so the readout and the view it opens agree.
    DefaultBranch(SharedString),
    /// No default branch resolved (detached head, no main/master, no remote),
    /// so the readout falls back to the nearest sensible base: the last commit.
    Head,
    /// The thread's worktree is not in a git repository.
    #[default]
    NoRepository,
}

/// The diff of a thread's worktree against the start of its branch: the merge
/// base with the default branch, which is what the branch diff view shows. The
/// numbers come from git, so they count the user's own edits, earlier sessions,
/// and committed work, and stay correct across commits. They are not a sum of
/// the agent's edit tool calls.
pub struct BranchDiffStats {
    project: WeakEntity<Project>,
    /// The thread's own work dirs. Empty means the agent has not reported any,
    /// and the thread's project stands in, as it does for its branch chips.
    work_dirs: Vec<PathBuf>,
    default_branch: Option<SharedString>,
    base: DiffStatsBase,
    stats: DiffStat,
    _git_subscription: Option<Subscription>,
    _refresh: Task<()>,
}

impl BranchDiffStats {
    pub fn new(project: WeakEntity<Project>, cx: &mut Context<Self>) -> Self {
        let git_subscription = project.upgrade().map(|project| {
            let git_store = project.read(cx).git_store().clone();
            cx.subscribe(&git_store, |this, _git_store, event, cx| {
                let head_changed = matches!(
                    event,
                    GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _)
                );
                let affects_diff = head_changed
                    || matches!(
                        event,
                        GitStoreEvent::RepositoryUpdated(
                            _,
                            RepositoryEvent::StatusesChanged
                                | RepositoryEvent::GitWorktreeListChanged,
                            _,
                        ) | GitStoreEvent::RepositoryAdded
                            | GitStoreEvent::RepositoryRemoved(_)
                    );
                if !affects_diff {
                    return;
                }
                if head_changed {
                    // A commit or a checkout can move the default branch out
                    // from under the cached value.
                    this.default_branch = None;
                }
                this.schedule_refresh(cx);
            })
        });

        let mut this = Self {
            project,
            work_dirs: Vec::new(),
            default_branch: None,
            base: DiffStatsBase::default(),
            stats: DiffStat::default(),
            _git_subscription: git_subscription,
            _refresh: Task::ready(()),
        };
        this.schedule_refresh(cx);
        this
    }

    pub fn stats(&self) -> DiffStat {
        self.stats
    }

    pub fn base(&self) -> &DiffStatsBase {
        &self.base
    }

    pub fn set_work_dirs(&mut self, work_dirs: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.work_dirs == work_dirs {
            return;
        }
        self.work_dirs = work_dirs;
        self.default_branch = None;
        self.schedule_refresh(cx);
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self._refresh = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let Some((base, stats)) = Self::compute(this.clone(), cx).await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.base != base || this.stats != stats {
                    this.base = base;
                    this.stats = stats;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    async fn compute(
        this: WeakEntity<Self>,
        cx: &mut AsyncApp,
    ) -> Result<(DiffStatsBase, DiffStat)> {
        let repository = this.read_with(cx, |this, cx| {
            let project = this.project.upgrade()?;
            this.resolve_repository(&project, cx)
        })?;
        let Some(repository) = repository else {
            return Ok((DiffStatsBase::NoRepository, DiffStat::default()));
        };

        // The default branch changes far less often than the stats do, so it is
        // resolved per repository and head rather than per refresh.
        let mut default_branch = this.read_with(cx, |this, _| this.default_branch.clone())?;
        if default_branch.is_none() {
            default_branch = repository
                .update(cx, |repository, _| repository.default_branch(true))
                .await
                .log_err()
                .and_then(|branch| branch.log_err())
                .flatten();
            this.update(cx, |this, _| this.default_branch = default_branch.clone())?;
        }

        let (base, diff_type) = match default_branch {
            Some(base_ref) => (
                DiffStatsBase::DefaultBranch(base_ref.clone()),
                DiffType::MergeBase { base_ref },
            ),
            None => (DiffStatsBase::Head, DiffType::HeadToWorktree),
        };

        let (patch, untracked_paths, fs) = this.update(cx, |this, cx| {
            let patch = repository.update(cx, |repository, cx| repository.diff(diff_type, cx));
            // Untracked files are read off disk, which only makes sense for a
            // local project; a remote one counts what git reports and no more.
            let fs = this
                .project
                .upgrade()
                .filter(|project| project.read(cx).is_local())
                .map(|project| project.read(cx).fs().clone());
            let untracked_paths = match fs {
                Some(_) => untracked_abs_paths(&repository, cx),
                None => Vec::new(),
            };
            (patch, untracked_paths, fs)
        })?;

        let stats = cx
            .background_executor()
            .spawn(async move {
                let mut stats = DiffStat::default();
                let mut counted = HashSet::default();
                if let Some(patch) = patch.await.log_err().and_then(|patch| patch.log_err()) {
                    stats = patch_diff_stat(&patch);
                    counted = patch_paths(&patch);
                }
                // A file the agent just created is untracked, so git's own diff
                // says nothing about it while the branch diff view shows it in
                // full. Skip the ones the patch already counted: a file stays
                // in the status snapshot as untracked for a moment after it is
                // committed, and it must not be counted twice.
                if let Some(fs) = fs {
                    for path in untracked_paths {
                        let already_counted = counted
                            .iter()
                            .any(|counted_path| path.ends_with(std::path::Path::new(counted_path)));
                        if already_counted {
                            continue;
                        }
                        if let Ok(text) = fs.load(&path).await {
                            stats.added = stats.added.saturating_add(line_count(&text));
                        }
                    }
                }
                stats
            })
            .await;

        Ok((base, stats))
    }

    /// The repository the thread works in. Scoped like the thread's branch
    /// chips: the most specific repository containing one of the thread's work
    /// dirs, so a thread never reports another worktree's diff.
    fn resolve_repository(
        &self,
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<Entity<Repository>> {
        let project = project.read(cx);
        let work_dirs: Vec<PathBuf> = if self.work_dirs.is_empty() {
            project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        } else {
            self.work_dirs.clone()
        };

        let repositories = project.repositories(cx);
        let containing = repositories
            .values()
            .filter(|repository| {
                let repo_path = repository.read(cx).snapshot().work_directory_abs_path;
                work_dirs.iter().any(|dir| dir.starts_with(&repo_path))
            })
            .max_by_key(|repository| {
                repository
                    .read(cx)
                    .snapshot()
                    .work_directory_abs_path
                    .components()
                    .count()
            });
        if let Some(repository) = containing {
            return Some(repository.clone());
        }

        // The work dir can sit above the repository (a project root holding one
        // checkout). With several repositories under it there is no single
        // answer, so the readout stays empty rather than picking one.
        let mut nested = repositories.values().filter(|repository| {
            let repo_path = repository.read(cx).snapshot().work_directory_abs_path;
            work_dirs.iter().any(|dir| repo_path.starts_with(dir))
        });
        let single = nested.next()?;
        nested.next().is_none().then(|| single.clone())
    }
}

fn untracked_abs_paths(repository: &Entity<Repository>, cx: &App) -> Vec<PathBuf> {
    let repository = repository.read(cx);
    let work_directory = repository.snapshot().work_directory_abs_path;
    repository
        .cached_status()
        .filter(|entry| matches!(entry.status, FileStatus::Untracked))
        .map(|entry| work_directory.join(entry.repo_path.as_std_path()))
        .collect()
}

fn line_count(text: &str) -> u32 {
    text.lines().count() as u32
}

/// The added and removed line counts of a unified diff, counting what
/// `git diff --numstat` counts. Hunk bodies only, so file headers (`+++`,
/// `---`) and mode lines never register, and a body line that itself starts
/// with `+` does.
/// The repo-relative paths a patch already accounts for, read off its
/// `+++ b/<path>` headers. A file the patch covers must not also be counted as
/// untracked: the status snapshot can still call a file untracked just after it
/// is committed, and counting it twice inflates the readout.
fn patch_paths(patch: &str) -> HashSet<String> {
    patch
        .lines()
        .filter_map(|line| {
            let path = line.strip_prefix("+++ ")?.trim();
            if path == "/dev/null" {
                return None;
            }
            Some(path.strip_prefix("b/").unwrap_or(path).to_string())
        })
        .collect()
}

fn patch_diff_stat(patch: &str) -> DiffStat {
    let mut stats = DiffStat::default();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => stats.added = stats.added.saturating_add(1),
                Some(b'-') => stats.deleted = stats.deleted.saturating_add(1),
                _ => {}
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, RealFs};
    use gpui::{AppContext as _, TestAppContext};
    use settings::SettingsStore;
    use std::{path::Path, process::Command, sync::Arc};

    #[gpui::test]
    async fn stats_come_from_git_not_from_the_agent(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        let repository = tempfile::tempdir().expect("temp dir");
        let repository = repository.path();
        write(repository, "committed.txt", "one\ntwo\nthree\n");
        write(repository, "edited.txt", "keep\n");
        git(repository, &["init", "-b", "main"]);
        git(repository, &["add", "."]);
        git(repository, &["commit", "-m", "initial"]);

        // The branch: one commit, plus a user edit and an agent-created file
        // that are still uncommitted.
        git(repository, &["checkout", "-b", "feature"]);
        write(repository, "committed.txt", "one\ntwo\nthree\nfour\nfive\n");
        git(repository, &["commit", "-am", "committed on the branch"]);
        write(repository, "edited.txt", "changed\n");
        write(repository, "created.txt", "new one\nnew two\n");

        let project =
            Project::test(Arc::new(RealFs::new(None, cx.executor())), [repository], cx).await;
        let stats = cx.new(|cx| BranchDiffStats::new(project.downgrade(), cx));
        settle(
            cx,
            &stats,
            &DiffStatsBase::DefaultBranch("main".into()),
            DiffStat {
                added: 5,
                deleted: 1,
            },
        );

        // +2 committed on the branch, +1 -1 edited by the user, +2 created and
        // never staged: everything the branch diff view shows against main.
        stats.read_with(cx, |stats, _| {
            assert_eq!(
                stats.base(),
                &DiffStatsBase::DefaultBranch("main".into()),
                "the base is the merge base with the default branch"
            );
            assert_eq!(
                stats.stats(),
                DiffStat {
                    added: 5,
                    deleted: 1
                }
            );
        });

        // Committing changes nothing: the base is the branch point, not HEAD.
        git(repository, &["add", "."]);
        git(repository, &["commit", "-m", "the rest"]);
        settle(
            cx,
            &stats,
            &DiffStatsBase::DefaultBranch("main".into()),
            DiffStat {
                added: 5,
                deleted: 1,
            },
        );
        stats.read_with(cx, |stats, _| {
            assert_eq!(
                stats.stats(),
                DiffStat {
                    added: 5,
                    deleted: 1
                }
            );
        });

        // A change on top of that commit accumulates rather than replacing it.
        write(repository, "edited.txt", "changed\nagain\n");
        settle(
            cx,
            &stats,
            &DiffStatsBase::DefaultBranch("main".into()),
            DiffStat {
                added: 6,
                deleted: 1,
            },
        );
        stats.read_with(cx, |stats, _| {
            assert_eq!(
                stats.stats(),
                DiffStat {
                    added: 6,
                    deleted: 1
                }
            );
        });
    }

    #[gpui::test]
    async fn without_a_default_branch_the_base_is_the_last_commit(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        // No main, no master, no remote: nothing to merge-base against.
        let repository = tempfile::tempdir().expect("temp dir");
        let repository = repository.path();
        write(repository, "file.txt", "one\n");
        git(repository, &["init", "-b", "work"]);
        git(repository, &["add", "."]);
        git(repository, &["commit", "-m", "initial"]);
        write(repository, "file.txt", "one\ntwo\n");

        let project =
            Project::test(Arc::new(RealFs::new(None, cx.executor())), [repository], cx).await;
        let stats = cx.new(|cx| BranchDiffStats::new(project.downgrade(), cx));
        settle(
            cx,
            &stats,
            &DiffStatsBase::Head,
            DiffStat {
                added: 1,
                deleted: 0,
            },
        );

        stats.read_with(cx, |stats, _| {
            assert_eq!(stats.base(), &DiffStatsBase::Head);
            assert_eq!(
                stats.stats(),
                DiffStat {
                    added: 1,
                    deleted: 0
                }
            );
        });
    }

    #[gpui::test]
    async fn stats_are_zero_without_a_repository(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            serde_json::json!({ "foo.rs": "fn main() {}\n" }),
        )
        .await;
        let project = Project::test(fs, [util::path!("/project").as_ref()], cx).await;

        let stats = cx.new(|cx| BranchDiffStats::new(project.downgrade(), cx));
        cx.executor().advance_clock(REFRESH_DEBOUNCE * 4);
        cx.run_until_parked();

        stats.read_with(cx, |stats, _| {
            assert_eq!(stats.base(), &DiffStatsBase::NoRepository);
            assert_eq!(stats.stats(), DiffStat::default());
        });
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
    }

    /// Lets the git status refresh land, the debounce elapse, and the git
    /// commands the refresh runs finish.
    ///
    /// Repository discovery and the refresh both shell out to git on a real
    /// filesystem, so this waits on real time as well as on the simulated
    /// clock, and waits for the expected value rather than for a fixed
    /// duration: a fixed wait passes alone and races under a loaded suite.
    #[track_caller]
    fn settle(
        cx: &mut TestAppContext,
        stats: &Entity<BranchDiffStats>,
        expected_base: &DiffStatsBase,
        expected_stats: DiffStat,
    ) {
        let pump = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        };

        // Always let a pending refresh land before accepting a value, so a
        // value that is correct only until the refresh completes still fails.
        for _ in 0..20 {
            pump(cx);
        }

        for _ in 0..250 {
            let settled = stats.read_with(cx, |stats, _| {
                stats.base() == expected_base && stats.stats() == expected_stats
            });
            if settled {
                return;
            }
            pump(cx);
        }

        let (base, diff_stats) =
            stats.read_with(cx, |stats, _| (stats.base().clone(), stats.stats()));
        panic!(
            "git refresh never settled: base {base:?} (expected {expected_base:?}), \
             stats {diff_stats:?} (expected {expected_stats:?})"
        );
    }

    #[track_caller]
    fn write(repository: &Path, path: &str, contents: &str) {
        std::fs::write(repository.join(path), contents).expect("write file");
    }

    #[track_caller]
    #[allow(clippy::disallowed_methods)]
    fn git(repository: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_patched_file_is_not_also_counted_as_untracked() {
        // Committing a file the agent created leaves it in the patch while the
        // status snapshot can still call it untracked for a moment. Counting
        // both inflated the readout (a real double count, seen as a flake).
        let patch = concat!(
            "diff --git a/created.txt b/created.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/created.txt\n",
            "@@ -0,0 +1,2 @@\n",
            "+new one\n",
            "+new two\n",
        );
        let paths = patch_paths(patch);
        assert!(
            paths.contains("created.txt"),
            "the patch's own paths are what untracked files are checked against"
        );
        assert!(
            !paths.contains("/dev/null"),
            "the null side of a new file is not a path"
        );
        assert!(
            std::path::Path::new("/repo/created.txt")
                .ends_with(std::path::Path::new("created.txt")),
            "an absolute untracked path is matched against the patch's repo-relative one"
        );
    }

    #[test]
    fn patch_stats_count_hunk_bodies_only() {
        let patch = concat!(
            "diff --git a/foo.rs b/foo.rs\n",
            "index 1234567..89abcde 100644\n",
            "--- a/foo.rs\n",
            "+++ b/foo.rs\n",
            "@@ -1,3 +1,4 @@\n",
            " unchanged\n",
            "-removed\n",
            "+added one\n",
            "++ still an added line\n",
            "\\ No newline at end of file\n",
            "diff --git a/bar.rs b/bar.rs\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/bar.rs\n",
            "@@ -0,0 +1,1 @@\n",
            "+only line\n",
        );
        assert_eq!(
            patch_diff_stat(patch),
            DiffStat {
                added: 3,
                deleted: 1
            }
        );
    }

    #[test]
    fn patch_stats_of_an_empty_diff_are_zero() {
        assert_eq!(patch_diff_stat(""), DiffStat::default());
    }
}
