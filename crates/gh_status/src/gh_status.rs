//! Tracks GitHub PR and CI status for git branches by polling the `gh` CLI.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Hsla, SharedString, Task,
};
use serde::{Deserialize, Serialize};
use ui::{ChecksGlyph, Color, IconName, PrChipDetail, ThreadItemPrChip};
use util::ResultExt as _;

/// How often the poll loop wakes. What it does on a tick is decided per
/// branch, by how long ago that branch was last asked about; this is only the
/// finest grain those intervals can have.
pub const POLL_INTERVAL: Duration = Duration::from_secs(20);

/// How often a branch with checks still running is asked about. A run in
/// progress changes within a minute or two and then stops changing for hours.
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// How often a branch whose PRs are open but have nothing in flight is asked
/// about. What moves such a PR is a push or a review: a push already refreshes
/// it directly, and a review is not something to spend a call a minute on.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long the store stops asking after GitHub says the hourly budget is
/// gone, when it cannot find out when the window resets. The window is an
/// hour long, so this retries a handful of times inside one rather than the
/// five hundred that spending the budget used to cost.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// The window GitHub's budget is counted over, and so the window this app
/// counts its own requests over.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// How many `gh` invocations may be in flight at once.
///
/// Every watched branch and PR used to be spawned the moment its interval
/// elapsed, so a session with a hundred of them opened a hundred processes and
/// a hundred requests in the same tick. GitHub answers the first few and
/// refuses the rest, and by the time the first refusal arrives the whole
/// flood is already out — which is how one exhausted budget produced 118
/// identical log lines in the same second. A cap turns the flood into a
/// trickle, so the first refusal stops the ones behind it. What does not go
/// out this tick goes out on the next; nothing is dropped.
const MAX_FETCHES_IN_FLIGHT: usize = 4;

/// How long one `gh` invocation gets before it is killed and counted as a
/// failed refresh. `gh` used to be run with no timeout and no kill, so a single
/// invocation that hung — a network that went away mid-call, an auth prompt, a
/// proxy that never answered — wedged that branch's chip for the life of the
/// process, quietly, while every other branch kept updating.
const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// GitHub renders a merged pull request purple, and readers of the chip expect
/// that. No theme status or accent role is purple in any theme, so the merged
/// state carries its own color instead of borrowing an unrelated role. Roughly
/// GitHub's merged purple (#8250df), light enough to read on dark backgrounds
/// and dark enough to read on light ones.
const MERGED_PR_COLOR: Hsla = Hsla {
    h: 261. / 360.,
    s: 0.69,
    l: 0.62,
    a: 1.0,
};

/// Install the global [`GhStatusStore`]. Idempotent.
pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalGhStatusStore>() {
        return;
    }
    let store = cx.new(GhStatusStore::new);
    cx.set_global(GlobalGhStatusStore(store));
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub number: u64,
    pub url: SharedString,
    pub title: SharedString,
    pub state: PrState,
    pub checks: ChecksState,
    pub review: ReviewState,
    /// Names of failing/errored checks, so a hover card can say which check
    /// failed rather than only that some did. Capped at [`MAX_LISTED_CHECKS`];
    /// the count of the ones over the cap is in `extra_failing_checks`. Empty
    /// when the PR has no failing checks or when the check data carries no
    /// names to report.
    #[serde(default)]
    pub failing_checks: Vec<SharedString>,
    /// How many failing checks the PR carries beyond what `failing_checks`
    /// listed. Zero when everything failing is listed. Read alongside
    /// `failing_checks` for a "and N more" line.
    #[serde(default)]
    pub extra_failing_checks: usize,
    /// Whether GitHub will actually let this merge, which green checks do not
    /// answer. `#[serde(default)]` so a snapshot persisted before this field
    /// existed still reads, as [`MergeState::Unknown`].
    #[serde(default)]
    pub merge: MergeState,
}

/// Whether GitHub will merge the pull request, and when it will not, why.
///
/// Independent of [`ChecksState`]: a PR can be entirely green and still
/// unmergeable because it is behind its base, conflicts with it, or is waiting
/// on a required review. Those look identical on a chip that only reports
/// checks, which is the whole reason this exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeState {
    /// Not yet known. GitHub computes mergeability lazily and answers
    /// `UNKNOWN` on the first ask surprisingly often, settling a second later.
    /// This is never a state to render: a surface shows exactly what it would
    /// have shown without the field and lets the next poll settle it, because
    /// a chip that flickers between "can merge" and "unknown" is worse than
    /// one that says nothing.
    #[default]
    Unknown,
    /// Nothing beyond the checks stands between the PR and its merge button.
    Mergeable,
    /// The branch is behind its base and needs updating first.
    Behind,
    /// The branch conflicts with its base.
    Conflicting,
    /// Branch protection is holding it: a required review, a required check
    /// that has not reported, an unsigned commit.
    Blocked,
}

impl MergeState {
    /// Why the PR cannot merge, phrased for a hover card. `None` when it can
    /// merge and when mergeability is not yet known — neither is a reason, and
    /// "unknown" is not a thing to tell anyone.
    pub fn blocked_reason(self) -> Option<&'static str> {
        match self {
            MergeState::Behind => Some("behind base branch"),
            MergeState::Conflicting => Some("conflicts with base branch"),
            MergeState::Blocked => Some("blocked by branch protection"),
            MergeState::Unknown | MergeState::Mergeable => None,
        }
    }

    /// The glyph that stands in for the checks glyph when the PR cannot merge,
    /// and `None` when nothing is in the way. It stays in the checks slot
    /// rather than adding one, because the chip is small and already carries a
    /// state glyph.
    ///
    /// The blockers are not the same kind of thing and do not read the same. A
    /// branch that is behind is a rebase away, so it is drawn as work to do; a
    /// conflict is the same work gone wrong, and keeps the warning it has had.
    /// Branch protection is somebody else's decision, and all it has to do is
    /// stop the chip reading as ready.
    pub fn blocked_glyph(self) -> Option<(IconName, Color)> {
        match self {
            MergeState::Behind => Some((IconName::ArrowDown, Color::Warning)),
            MergeState::Conflicting => Some((IconName::Warning, Color::Warning)),
            MergeState::Blocked => Some((IconName::Lock, Color::Muted)),
            MergeState::Unknown | MergeState::Mergeable => None,
        }
    }
}

/// A hover card cannot grow without bound; a workflow with forty checks would
/// eat the surface it sits on. The card lists this many and counts the rest.
pub const MAX_LISTED_CHECKS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Merged,
    Closed,
    Draft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChecksState {
    /// Something is still running, queued, waiting, or expected.
    Pending,
    Passing,
    Failing,
    /// The PR has no status checks. GitHub answered with a rollup and the
    /// rollup was empty.
    None,
    /// Nothing has answered for this PR's checks yet: no rollup has been read
    /// for it. Distinct from [`ChecksState::None`], which is an answer — a PR
    /// that genuinely has no CI — because the two want opposite things said
    /// about them. "No checks" is a fact worth putting on a hover card; not
    /// knowing is not, and a surface that conflates them tells the reader a
    /// PR has no CI when all that happened is that nobody looked.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    None,
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Global store of GitHub PR and CI status for watched branches.
///
/// Observe the entity returned by [`GhStatusStore::global`] to re-render on
/// changes; the store calls `cx.notify` whenever a watched branch's PR list
/// or the last error changes.
pub struct GhStatusStore {
    watched: HashMap<WatchKey, WatchedBranch>,
    last_error: Option<SharedString>,
    /// Set when GitHub answered that the hourly budget is gone. Until it
    /// passes, no fetch is started at all: the calls would fail anyway, and
    /// five hundred failures an hour is what spending the budget looked like.
    rate_limited_until: Option<Instant>,
    /// When GitHub says the hourly window turns over, once it has been asked.
    /// Unset until the first refusal, and again once the window has passed.
    rate_limit_reset: Option<Instant>,
    /// When each `gh` invocation of the last hour was started.
    ///
    /// GitHub's budget is per account, not per app, so an exhausted one says
    /// nothing about who spent it. Zed's own requests are the one side of that
    /// it can count, and one invocation is one request: a number here against
    /// an hourly limit of five thousand settles whether this app is the
    /// spender or a bystander, which the log could not answer at all.
    requests_this_hour: VecDeque<Instant>,
    _reset_task: Option<Task<()>>,
    _poll_task: Task<()>,
}

impl GhStatusStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalGhStatusStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalGhStatusStore>()
            .map(|global| global.0.clone())
    }

    /// Cached PR statuses for a watched branch. `None` until the first
    /// successful fetch completes.
    pub fn prs_for_branch(&self, repo_path: &Path, branch: &str) -> Option<&Vec<PrStatus>> {
        self.watched
            .get(&WatchKey {
                repo_path: repo_path.to_path_buf(),
                subject: WatchSubject::Branch(branch.to_string()),
            })
            .and_then(|watched| watched.prs.as_ref())
    }

    /// Error from the most recent failed `gh` invocation, e.g. when the CLI
    /// is not installed. Cleared by the next successful fetch.
    pub fn last_error(&self) -> Option<SharedString> {
        self.last_error.clone()
    }

    /// Register interest in a branch. Watches are refcounted; the branch is
    /// polled until a matching number of `unwatch` calls. Triggers an
    /// immediate fetch for newly watched branches.
    pub fn watch(&mut self, repo_path: PathBuf, branch: String, cx: &mut Context<Self>) {
        self.watch_key(
            WatchKey {
                repo_path,
                subject: WatchSubject::Branch(branch),
            },
            cx,
        );
    }

    /// Watch one pull request by number. The branch watch exists to discover
    /// numbers; this is for a PR a thread cares about whatever branch it is
    /// on, including one in a repository nobody here has checked out.
    pub fn watch_pr(
        &mut self,
        repo_path: PathBuf,
        number: u64,
        repo: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.watch_key(
            WatchKey {
                repo_path,
                subject: WatchSubject::Number { number, repo },
            },
            cx,
        );
    }

    pub fn unwatch_pr(
        &mut self,
        repo_path: &Path,
        number: u64,
        repo: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let key = WatchKey {
            repo_path: repo_path.to_path_buf(),
            subject: WatchSubject::Number {
                number,
                repo: repo.map(str::to_string),
            },
        };
        self.unwatch_key(&key, cx);
    }

    /// The PR a number watch last brought back, if it has answered.
    pub fn pr_by_number(&self, repo_path: &Path, number: u64, repo: Option<&str>) -> Option<&PrStatus> {
        let key = WatchKey {
            repo_path: repo_path.to_path_buf(),
            subject: WatchSubject::Number {
                number,
                repo: repo.map(str::to_string),
            },
        };
        self.watched
            .get(&key)
            .and_then(|watched| watched.prs.as_ref())
            .and_then(|prs| prs.first())
    }

    fn watch_key(&mut self, key: WatchKey, cx: &mut Context<Self>) {
        let watched = self
            .watched
            .entry(key.clone())
            .or_insert_with(WatchedBranch::default);
        watched.watch_count += 1;
        if watched.watch_count == 1 {
            self.refresh_branch(key, cx);
        }
    }

    pub fn unwatch(&mut self, repo_path: &Path, branch: &str, cx: &mut Context<Self>) {
        let key = WatchKey {
            repo_path: repo_path.to_path_buf(),
            subject: WatchSubject::Branch(branch.to_string()),
        };
        self.unwatch_key(&key, cx);
    }

    fn unwatch_key(&mut self, key: &WatchKey, cx: &mut Context<Self>) {
        let Some(watched) = self.watched.get_mut(key) else {
            return;
        };
        watched.watch_count = watched.watch_count.saturating_sub(1);
        if watched.watch_count == 0 {
            self.watched.remove(key);
            cx.notify();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_prs_for_test(&mut self, repo_path: PathBuf, branch: String, prs: Vec<PrStatus>) {
        let key = WatchKey {
            repo_path,
            subject: WatchSubject::Branch(branch),
        };
        let watched = self.watched.entry(key).or_default();
        watched.prs = Some(prs);
    }

    /// Refresh all watched branches now instead of waiting for the next poll.
    ///
    /// "Now" is still capped: a window coming into focus with a hundred
    /// branches behind it is the same flood as a poll tick with a hundred due,
    /// and the next tick picks up what this one did not start.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        let mut keys = self
            .watched
            .iter()
            .map(|(key, watched)| (key.clone(), watched.last_polled))
            .collect::<Vec<_>>();
        keys.sort_by_key(|(_, last_polled)| *last_polled);
        for (key, _) in keys {
            if self.fetches_in_flight() >= MAX_FETCHES_IN_FLIGHT {
                break;
            }
            self.refresh_branch(key, cx);
        }
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this.update(cx, |this, cx| this.refresh_due(cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            watched: HashMap::default(),
            last_error: None,
            rate_limited_until: None,
            rate_limit_reset: None,
            requests_this_hour: VecDeque::new(),
            _reset_task: None,
            _poll_task: poll_task,
        }
    }

    /// Asks about the branches whose own interval has elapsed. This is the
    /// only thing on a timer: everything else refreshes at a moment worth
    /// spending a call on, which is a push or the window coming into focus.
    fn refresh_due(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut due = self
            .watched
            .iter()
            .filter(|(_, watched)| watched.is_due(now))
            .map(|(key, watched)| (key.clone(), watched.last_polled))
            .collect::<Vec<_>>();
        // Longest-waiting first, so the cap below delays a branch rather than
        // starving one. A branch never polled sorts ahead of every other.
        due.sort_by_key(|(_, last_polled)| *last_polled);
        for (key, _) in due {
            if self.fetches_in_flight() >= MAX_FETCHES_IN_FLIGHT {
                break;
            }
            self.refresh_branch(key, cx);
        }
    }

    fn fetches_in_flight(&self) -> usize {
        self.watched
            .values()
            .filter(|watched| watched.refresh_task.is_some())
            .count()
    }

    fn refresh_branch(&mut self, key: WatchKey, cx: &mut Context<Self>) {
        // GitHub has said there is no budget left. Every fetch started before
        // the window resets fails, and failing is not free: it is what turned
        // an exhausted budget into an hour of five hundred errors.
        if self
            .rate_limited_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let Some(watched) = self.watched.get_mut(&key) else {
            return;
        };
        if watched.refresh_task.is_some() {
            return;
        }
        watched.last_polled = Some(Instant::now());
        self.record_request();
        let Some(watched) = self.watched.get_mut(&key) else {
            return;
        };
        watched.refresh_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = fetch_prs(&key.repo_path, &key.subject, &executor).await;
            this.update(cx, |this, cx| this.finish_refresh(&key, result, cx))
                .ok();
        }));
    }

    /// Notes that a request is going out, and forgets the ones over an hour
    /// old. The window is GitHub's: an hourly budget wants an hourly count.
    fn record_request(&mut self) {
        let now = Instant::now();
        while self
            .requests_this_hour
            .front()
            .is_some_and(|started| now.duration_since(*started) > RATE_LIMIT_WINDOW)
        {
            self.requests_this_hour.pop_front();
        }
        self.requests_this_hour.push_back(now);
    }

    /// How many requests this app has made in the last hour.
    pub fn requests_this_hour(&self) -> usize {
        let now = Instant::now();
        self.requests_this_hour
            .iter()
            .filter(|started| now.duration_since(**started) <= RATE_LIMIT_WINDOW)
            .count()
    }

    /// Asks GitHub when the hourly window turns over, and holds until then.
    ///
    /// `gh api rate_limit` is the one endpoint that costs nothing against the
    /// budget, so this is safe to run at the moment the budget is gone. Until
    /// it answers, the fixed backoff stands; if it never answers, the fixed
    /// backoff is all there is, which is where this started.
    fn ask_when_the_budget_returns(&mut self, cx: &mut Context<Self>) {
        self._reset_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let Some(seconds) = fetch_rate_limit_reset_in(&executor).await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                // A second of slack: releasing on the tick GitHub named races
                // its own clock, and losing costs another whole window.
                let reset = Instant::now() + seconds + Duration::from_secs(1);
                this.rate_limit_reset = Some(reset);
                if this
                    .rate_limited_until
                    .is_some_and(|until| Instant::now() < until)
                {
                    log::warn!(
                        "gh_status: GitHub's budget returns in {} minutes; holding until then",
                        seconds.as_secs() / 60
                    );
                    this.rate_limited_until = Some(reset);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn finish_refresh(
        &mut self,
        key: &WatchKey,
        result: Result<Vec<PrStatus>>,
        cx: &mut Context<Self>,
    ) {
        let Some(watched) = self.watched.get_mut(key) else {
            return;
        };
        watched.refresh_task = None;
        match result {
            Ok(prs) => {
                let changed = watched.prs.as_ref() != Some(&prs) || self.last_error.is_some();
                watched.prs = Some(prs);
                watched.last_fetched = Some(Instant::now());
                self.last_error = None;
                self.rate_limited_until = None;
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if is_rate_limited(&message) {
                    // Say it once and stop asking. Every branch shares the
                    // budget, so one refusal is the whole store's answer.
                    //
                    // "Once" means once per window: the guard used to compare
                    // the stored deadline against `now + backoff`, which is
                    // later than it by construction, so every refusal in the
                    // flood logged. Whether the store was already holding is
                    // the question, and that is what it asks now.
                    let already_holding = self
                        .rate_limited_until
                        .is_some_and(|until| Instant::now() < until);
                    // GitHub says when the window resets; asking it is free
                    // (`rate_limit` costs nothing against the budget) and it
                    // beats guessing. A fixed ten minutes releases everything
                    // together, which is the next burst, and releases it
                    // before the window has turned over anyway.
                    let until = self
                        .rate_limit_reset
                        .filter(|reset| Instant::now() < *reset)
                        .unwrap_or_else(|| Instant::now() + RATE_LIMIT_BACKOFF);
                    if !already_holding {
                        log::warn!(
                            "gh_status: GitHub's API budget is spent; not asking again for {} \
                             minutes. quiet-ui perf: this app made {} GitHub requests in the \
                             last hour",
                            until.saturating_duration_since(Instant::now()).as_secs() / 60,
                            self.requests_this_hour()
                        );
                        self.ask_when_the_budget_returns(cx);
                    }
                    self.rate_limited_until = Some(until.max(
                        self.rate_limited_until
                            .unwrap_or_else(|| Instant::now()),
                    ));
                } else {
                    log::warn!(
                        "gh_status: failed to fetch PRs for {}: {message}",
                        key.subject.describe()
                    );
                }
                self.last_error = Some(SharedString::from(message));
                cx.notify();
            }
        }
    }

    /// How long ago the chips for this branch were last true, or `None` when
    /// nothing has come back for it yet. A surface that wants to say a chip is
    /// stale asks this rather than guessing from the last error, which belongs
    /// to whichever branch failed most recently.
    pub fn fetched_ago(&self, repo_path: &Path, branch: &str) -> Option<Duration> {
        self.watched
            .get(&WatchKey {
                repo_path: repo_path.to_path_buf(),
                subject: WatchSubject::Branch(branch.to_string()),
            })
            .and_then(|watched| watched.last_fetched)
            .map(|last| Instant::now().duration_since(last))
    }

    /// Whether the store has stopped asking because GitHub's budget is spent.
    /// What the chips show is from before that, and does not move until this
    /// clears.
    pub fn is_rate_limited(&self) -> bool {
        self.rate_limited_until
            .is_some_and(|until| Instant::now() < until)
    }
}

/// PR badges for one thread's branches: one badge per PR across those
/// branches, deduplicated by URL, plus a muted inert "no PR" pill when the
/// branches have no PR at all. The caller passes only the branches of the
/// worktrees that thread owns; the store holds PRs for every branch watched in
/// the window, so passing more than the thread's own branches is exactly the
/// bug this signature exists to prevent.
pub fn pr_chips_for_branches<'a>(
    branches: impl IntoIterator<Item = (&'a Path, &'a str)>,
    store: Option<&GhStatusStore>,
) -> Vec<ThreadItemPrChip> {
    let mut has_branch = false;
    let branches = branches.into_iter().inspect(|_| has_branch = true);
    let prs = prs_for_branches(branches, store);
    let mut chips = pr_chips_for_prs(prs.iter());
    // No branch at all is not "a branch with no PR": the caller decides what
    // a thread with nowhere to look should say.
    if chips.is_empty() && has_branch {
        chips.push(no_pr_chip());
    }
    chips
}

/// The PRs the store currently holds for these branches, deduplicated.
fn prs_for_branches<'a>(
    branches: impl IntoIterator<Item = (&'a Path, &'a str)>,
    store: Option<&GhStatusStore>,
) -> Vec<PrStatus> {
    let mut prs: Vec<PrStatus> = Vec::new();
    for (repo_path, branch) in branches {
        let Some(branch_prs) = store.and_then(|store| store.prs_for_branch(repo_path, branch))
        else {
            continue;
        };
        for pr in branch_prs {
            if !prs.iter().any(|seen| seen.url == pr.url) {
                prs.push(pr.clone());
            }
        }
    }
    prs
}

/// Every PR badge a thread should show, wherever it is shown. This is the one
/// answer to "does this thread have a PR": live gh data for its branches, the
/// state persisted while it was live when gh has none (an archived thread's
/// worktree is gone, so its branch cannot be resolved, and a live thread's
/// first fetch may not have landed), and an inert "no PR" pill when there is
/// nothing at all, so a surface does not change shape when the first branch or
/// PR arrives. The snapshot is read lazily because most rows never need it.
///
/// Callers still choose the branches, which is a real difference: the sidebar
/// knows a thread's recorded worktrees and the thread view knows the workspace
/// it is open in.
pub fn thread_pr_chips<'a>(
    branches: impl IntoIterator<Item = (&'a Path, &'a str)>,
    watched: &[PrStatus],
    dismissed: &[(Option<String>, u64)],
    store: Option<&GhStatusStore>,
    snapshot: impl FnOnce() -> Option<Vec<PrStatus>>,
) -> Vec<ThreadItemPrChip> {
    let mut prs = prs_for_branches(branches, store);
    // A PR watched by number sits beside the branch's own, and is the only
    // source for one on a branch nobody here has checked out.
    for pr in watched {
        if !prs.iter().any(|seen| seen.url == pr.url) {
            prs.push(pr.clone());
        }
    }
    prs.retain(|pr| !is_dismissed(pr, dismissed));

    if prs.is_empty()
        && let Some(prs) = snapshot().filter(|prs| !prs.is_empty())
    {
        let mut prs = prs;
        prs.retain(|pr| !is_dismissed(pr, dismissed));
        if !prs.is_empty() {
            return pr_chips_for_prs(prs.iter());
        }
    }

    if prs.is_empty() {
        return vec![no_pr_chip()];
    }
    pr_chips_for_prs(prs.iter())
}

/// Whether a PR is one the thread was told to stop watching. A dismissal that
/// named a repository only matches that repository's PR; one mined from a
/// branch named none, and matches on the number alone.
fn is_dismissed(pr: &PrStatus, dismissed: &[(Option<String>, u64)]) -> bool {
    dismissed.iter().any(|(repo, number)| {
        *number == pr.number
            && repo
                .as_ref()
                .is_none_or(|repo| pr.url.contains(&format!("/{repo}/")))
    })
}

/// One badge per PR, deduplicated by URL. Used for both live gh data and the
/// PR snapshot persisted on a thread, whose worktree may be gone.
pub fn pr_chips_for_prs<'a>(prs: impl IntoIterator<Item = &'a PrStatus>) -> Vec<ThreadItemPrChip> {
    let mut seen_urls: HashSet<SharedString> = HashSet::default();
    prs.into_iter()
        .filter(|pr| seen_urls.insert(pr.url.clone()))
        .map(pr_chip)
        .collect()
}

/// The PRs the store has fetched for these branches, or `None` when no branch
/// has been fetched yet (an unfetched branch and a branch with no PR are
/// different: only the latter should overwrite a persisted snapshot).
pub fn fetched_prs_for_branches<'a>(
    branches: impl IntoIterator<Item = (&'a Path, &'a str)>,
    store: &GhStatusStore,
) -> Option<Vec<PrStatus>> {
    let mut fetched = false;
    let mut prs = Vec::new();
    for (repo_path, branch) in branches {
        let Some(branch_prs) = store.prs_for_branch(repo_path, branch) else {
            continue;
        };
        fetched = true;
        prs.extend(branch_prs.iter().cloned());
    }
    fetched.then_some(prs)
}

/// The muted, inert badge that stands in for a pull request that does not
/// exist, so a row's PR state is visible even when there is nothing to show.
pub fn no_pr_chip() -> ThreadItemPrChip {
    ThreadItemPrChip {
        label: "no PR".into(),
        state_icon: IconName::PullRequest,
        state_color: Color::Muted,
        checks: None,
        url: None,
        tooltip: "No pull request for this branch".into(),
        detail: None,
    }
}

fn pr_chip(pr: &PrStatus) -> ThreadItemPrChip {
    let (state_color, state_label) = match pr.state {
        PrState::Open => (Color::Success, "open"),
        PrState::Draft => (Color::Muted, "draft"),
        PrState::Merged => (Color::Custom(MERGED_PR_COLOR), "merged"),
        PrState::Closed => (Color::Error, "closed"),
    };
    // A merged PR passed by definition, so it carries no checks glyph and no
    // checks line in its hover card.
    let (checks, checks_label) = match pr.checks {
        _ if pr.state == PrState::Merged => (None, ""),
        ChecksState::Passing => (
            Some(ChecksGlyph::settled(IconName::Check, Color::Success)),
            "checks passing",
        ),
        ChecksState::Failing => (
            Some(ChecksGlyph::settled(IconName::XCircle, Color::Error)),
            "checks failing",
        ),
        // The one glyph in the set that is about something happening rather
        // than something concluded, so it is the one that moves.
        ChecksState::Pending => (
            Some(ChecksGlyph::running(IconName::LoadCircle, Color::Warning)),
            "checks running",
        ),
        ChecksState::None => (None, "no checks"),
        // Nothing to draw and nothing to say: the next poll answers, and
        // until it does the chip claims neither CI nor the absence of it.
        ChecksState::Unknown => (None, ""),
    };
    let review_label = match pr.review {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes requested",
        ReviewState::ReviewRequired => "review required",
        ReviewState::None => "no review",
    };
    let merge = match (pr.state, pr.merge) {
        // A merged or closed PR is not waiting to merge, so mergeability has
        // nothing to say about it.
        (PrState::Merged | PrState::Closed, _) => MergeState::Unknown,
        // GitHub answers `BLOCKED` for a draft in a repository with branch
        // protection, so on a draft that blocker is the draft state restated —
        // and the chip already says "draft", muted, without warning about it.
        // A conflict or a stale base is a real thing a draft can be, and
        // `mergeable` reports a conflict whatever the draft state, so those
        // still count.
        (PrState::Draft, MergeState::Blocked) => MergeState::Unknown,
        (_, merge) => merge,
    };
    let blocked_reason = merge.blocked_reason();
    // Mergeability does not get a glyph slot of its own. It changes what the
    // checks glyph says instead: passing checks on a PR that cannot merge are
    // not green, because reading them as ready is the exact mistake this is
    // here to stop. The reason goes in the hover card, where there is room for
    // a sentence.
    let checks = match merge.blocked_glyph() {
        Some((icon, color)) if pr.checks == ChecksState::Passing => {
            Some(ChecksGlyph::settled(icon, color))
        }
        _ => checks,
    };
    let summary = [
        Some(state_label),
        (!checks_label.is_empty()).then_some(checks_label),
        blocked_reason,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    ThreadItemPrChip {
        label: SharedString::from(format!("#{}", pr.number)),
        state_icon: IconName::PullRequest,
        state_color,
        checks,
        url: Some(pr.url.clone()),
        tooltip: SharedString::from(format!("{} ({summary})", pr.title)),
        detail: Some(PrChipDetail {
            title: pr.title.clone(),
            number: pr.number,
            state: state_label.into(),
            state_color,
            checks: checks_label.into(),
            checks_icon: checks,
            review: review_label.into(),
            // A hover card that says "checks failing" and stops there is a
            // hover card that sends the reader to a browser to find out which.
            // Names are only listed when a failure is what needs pointing at;
            // a passing PR has nothing to name.
            failing_checks: match pr.checks {
                ChecksState::Failing => pr.failing_checks.clone(),
                _ => Vec::new(),
            },
            extra_failing_checks: match pr.checks {
                ChecksState::Failing => pr.extra_failing_checks,
                _ => 0,
            },
            merge_blocker: blocked_reason.map(SharedString::from),
        }),
    }
}

struct GlobalGhStatusStore(Entity<GhStatusStore>);

impl Global for GlobalGhStatusStore {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WatchKey {
    /// The directory `gh` runs in. For a PR watched by number in another
    /// repository this is only a working directory; `subject` carries the
    /// repository the question is actually about.
    repo_path: PathBuf,
    subject: WatchSubject,
}

/// What one watch asks about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WatchSubject {
    /// Every pull request open on a branch. This is how a number is
    /// discovered in the first place.
    Branch(String),
    /// One pull request, by number, whatever branch it is on and whatever
    /// repository it belongs to.
    Number {
        number: u64,
        /// `owner/name`, when the number came from somewhere that said so (a
        /// URL in the thread, or a PR added by hand). Without it the number
        /// is read against whatever repository `repo_path` is.
        repo: Option<String>,
    },
}

impl WatchSubject {
    fn describe(&self) -> String {
        match self {
            WatchSubject::Branch(branch) => format!("branch {branch}"),
            WatchSubject::Number { number, repo: None } => format!("#{number}"),
            WatchSubject::Number {
                number,
                repo: Some(repo),
            } => format!("{repo}#{number}"),
        }
    }
}

#[derive(Default)]
struct WatchedBranch {
    watch_count: usize,
    prs: Option<Vec<PrStatus>>,
    refresh_task: Option<Task<()>>,
    /// When this branch was last asked about, successfully or not, so the
    /// poll loop can ask about a branch with a run in flight often and one
    /// with nothing in flight rarely.
    last_polled: Option<Instant>,
    /// When this branch's PRs last came back from `gh`. What the chips show
    /// is from this moment, which is what makes them stale rather than wrong
    /// while the store is backing off.
    last_fetched: Option<Instant>,
}

impl WatchedBranch {
    /// Whether any of this branch's pull requests still has checks running.
    /// A branch that has never been fetched counts as settled: there is no
    /// reason to think it is interesting until the first answer arrives.
    fn has_checks_in_flight(&self) -> bool {
        self.prs.as_ref().is_some_and(|prs| {
            prs.iter().any(|pr| {
                matches!(pr.state, PrState::Open | PrState::Draft)
                    && pr.checks == ChecksState::Pending
            })
        })
    }

    /// Whether this branch has nothing left that could change. A merged pull
    /// request never moves again, and a branch whose every PR is merged or
    /// closed is a row the store can keep serving from what it has.
    fn is_settled(&self) -> bool {
        self.prs.as_ref().is_some_and(|prs| {
            !prs.is_empty()
                && prs
                    .iter()
                    .all(|pr| matches!(pr.state, PrState::Merged | PrState::Closed))
        })
    }

    /// How long to leave this branch alone before asking again, or `None` for
    /// a branch that will never change.
    fn poll_interval(&self) -> Option<Duration> {
        if self.is_settled() {
            None
        } else if self.prs.is_none() || self.has_checks_in_flight() {
            Some(PENDING_POLL_INTERVAL)
        } else {
            Some(IDLE_POLL_INTERVAL)
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        let Some(interval) = self.poll_interval() else {
            return false;
        };
        self.last_polled
            .is_none_or(|last| now.duration_since(last) >= interval)
    }
}

/// What every fetch asks for, the check rollup included.
///
/// The rollup was briefly left out of polls whose checks looked settled, to
/// spend less of GitHub's budget per call. What that costs is everything:
/// nothing else in the answer reports CI, so a poll without the rollup keeps
/// the state it already had, and `needs_rollup` only came back true while the
/// checks were already pending. A PR that had ever reached green or red was
/// therefore never asked about again — through a push, a re-run, a broken
/// build — for the life of the process. A settled PR is exactly the one whose
/// next push moves it, so every poll asks. The saving was never worth much
/// either: a branch whose checks have settled polls once every five minutes,
/// and a branch whose every PR is merged or closed is not polled at all.
const GH_JSON_FIELDS: &str = "number,url,title,state,isDraft,reviewDecision,\
    mergeable,mergeStateStatus,statusCheckRollup";

// The gh CLI selects `statusCheckRollup` as a whole subtree, so asking for it
// already returns each check's `name`, `workflowName`, `context`, `state`,
// `status`, and `conclusion`; the additional fields the fork needs for the
// hover card are parsed out in `GhCheck` without naming them here.

/// Whether a failed `gh` invocation failed because GitHub's hourly budget is
/// gone. Both spellings GitHub uses land here: the first refusal says the
/// limit was exceeded, and every one after it says it was already exceeded.
fn is_rate_limited(error: &str) -> bool {
    error.contains("API rate limit")
}

/// How long until GitHub's hourly budget refills, straight from GitHub.
///
/// The `rate_limit` endpoint is documented as not counting against the limit,
/// which is what makes it askable at the one moment the answer matters.
async fn fetch_rate_limit_reset_in(executor: &BackgroundExecutor) -> Result<Duration> {
    use util::command::Stdio;

    let child = util::command::new_command("gh")
        .args(["api", "rate_limit", "--jq", ".rate.reset - now"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run gh; is the GitHub CLI installed?")?;

    let output = futures::select_biased! {
        output = child.output().fuse() => {
            output.context("failed to run gh api rate_limit")?
        }
        _ = executor.timer(GH_TIMEOUT).fuse() => {
            bail!("gh api rate_limit timed out after {} seconds", GH_TIMEOUT.as_secs());
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api rate_limit exited with {}: {}", output.status, stderr.trim());
    }
    parse_rate_limit_reset_in(&String::from_utf8_lossy(&output.stdout))
}

/// Seconds until the reset, as `--jq '.rate.reset - now'` prints them.
///
/// A window that has already turned over reads as zero rather than as a
/// negative wait, and anything longer than an hour is not a window GitHub
/// hands out — it is a clock disagreeing, and a guess beats holding for a day.
fn parse_rate_limit_reset_in(stdout: &str) -> Result<Duration> {
    let seconds: f64 = stdout
        .trim()
        .parse()
        .with_context(|| format!("unreadable rate limit reset: {:?}", stdout.trim()))?;
    if !seconds.is_finite() || seconds > 3600.0 {
        bail!("implausible rate limit reset: {seconds} seconds away");
    }
    Ok(Duration::from_secs_f64(seconds.max(0.0)))
}

async fn fetch_prs(
    repo_path: &Path,
    subject: &WatchSubject,
    executor: &BackgroundExecutor,
) -> Result<Vec<PrStatus>> {
    use util::command::Stdio;

    let mut command = util::command::new_command("gh");
    match subject {
        WatchSubject::Branch(branch) => {
            command.args(["pr", "list", "--head", branch, "--state", "all", "--json"]);
        }
        WatchSubject::Number { number, repo } => {
            command.args(["pr", "view", &number.to_string()]);
            if let Some(repo) = repo {
                command.args(["--repo", repo]);
            }
            command.arg("--json");
        }
    }
    let child = command
        .arg(GH_JSON_FIELDS)
        .current_dir(repo_path)
        // `gh` must never be able to sit waiting to be typed at: a credential
        // prompt with nowhere to read from is a hang, and a hang is what used
        // to wedge a branch permanently.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Losing the race below drops this child, and dropping it kills the
        // process rather than leaving one behind for every poll.
        .kill_on_drop(true)
        .spawn()
        .context("failed to run gh; is the GitHub CLI installed?")?;

    let output = futures::select_biased! {
        output = child.output().fuse() => {
            output.context("failed to run gh; is the GitHub CLI installed?")?
        }
        _ = executor.timer(GH_TIMEOUT).fuse() => {
            // A timeout is an ordinary failed refresh: the task finishes, the
            // branch stops being "already refreshing", and the next poll tries
            // again.
            bail!("gh timed out after {} seconds", GH_TIMEOUT.as_secs());
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh exited with {}: {}", output.status, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match subject {
        WatchSubject::Branch(_) => parse_pr_list(&stdout),
        // `gh pr view` answers with the one pull request rather than a list.
        WatchSubject::Number { .. } => parse_pr_view(&stdout).map(|pr| vec![pr]),
    }
}

fn parse_pr_view(json: &str) -> Result<PrStatus> {
    let pr: GhPr = serde_json::from_str(json).context("failed to parse gh pr view output")?;
    Ok(PrStatus::from_gh(pr))
}

fn parse_pr_list(json: &str) -> Result<Vec<PrStatus>> {
    let prs: Vec<GhPr> = serde_json::from_str(json).context("failed to parse gh pr list output")?;
    Ok(prs.into_iter().map(PrStatus::from_gh).collect())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    url: String,
    title: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Vec<GhCheck>>,
    /// The conflict question: `MERGEABLE`, `CONFLICTING`, or `UNKNOWN`.
    #[serde(default)]
    mergeable: Option<String>,
    /// The richer "why not": `BEHIND`, `DIRTY`, `BLOCKED`, `UNSTABLE`,
    /// `CLEAN`, `DRAFT`, `HAS_HOOKS`, `UNKNOWN`.
    #[serde(default)]
    merge_state_status: Option<String>,
}

/// One statusCheckRollup entry. Commit statuses report `state`; check runs
/// report `conclusion` once complete and nothing but a running `status` until
/// then, which is why the status itself is not read: its absence from the
/// conclusion says the same thing, and says it for every value GitHub has.
/// A check run carries a `name` (its job name) and a `workflowName` (its
/// workflow's own name); a commit status carries a `context` in place of both.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCheck {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workflow_name: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

/// What one check in the rollup amounts to. Three answers, because a chip has
/// three glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckOutcome {
    /// Nothing has concluded here yet, whatever the reason.
    Pending,
    Passing,
    Failing,
}

impl GhCheck {
    /// Which of the three this check is.
    ///
    /// Only an explicitly good terminal value passes and only an explicitly
    /// bad one fails; everything else — a status GitHub has added since, a
    /// check run with no conclusion, an empty string — is pending. That
    /// direction matters: the old reading recognised three running statuses
    /// and let the rest fall through to passing, so a check that was merely
    /// `WAITING` made the whole PR read green, and a cancelled or timed-out
    /// one did too.
    fn outcome(&self) -> CheckOutcome {
        // A commit status reports `state` and nothing else.
        if let Some(state) = self.state.as_deref().filter(|s| !s.is_empty()) {
            return match state {
                "SUCCESS" => CheckOutcome::Passing,
                "FAILURE" | "ERROR" => CheckOutcome::Failing,
                // `PENDING`, `EXPECTED`, and whatever GitHub adds next.
                _ => CheckOutcome::Pending,
            };
        }
        // A check run reports `conclusion` once it is over and `status` until
        // then, so the conclusion is the only thing that can settle it.
        match self.conclusion.as_deref().filter(|c| !c.is_empty()) {
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED" | "STALE") => CheckOutcome::Passing,
            // A run that was cancelled, ran out of time, failed to start, or
            // is waiting on someone to approve it did not pass. Reading those
            // as green is how a PR nobody had run showed a tick.
            Some(
                "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                | "STARTUP_FAILURE",
            ) => CheckOutcome::Failing,
            // No conclusion, whatever the `status` says — `QUEUED`,
            // `IN_PROGRESS`, `WAITING`, `REQUESTED`, `PENDING` — or a
            // conclusion this does not know.
            _ => CheckOutcome::Pending,
        }
    }

    /// A one-line name for a hover card. A check run says `workflow / job` when
    /// both are known so `test / clippy` and `build / clippy` don't collapse
    /// into two identical `clippy` lines; a commit status carries its own
    /// context. Empty when nothing named the check.
    fn label(&self) -> Option<String> {
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            return Some(match self.workflow_name.as_deref().filter(|w| !w.is_empty()) {
                Some(workflow) if workflow != name => format!("{workflow} / {name}"),
                _ => name.to_string(),
            });
        }
        self.context
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(str::to_string)
    }
}

impl PrStatus {
    fn from_gh(pr: GhPr) -> Self {
        // A rollup that is absent or null is not an empty rollup. GitHub
        // answers `null` for a PR whose checks it would not report, and a
        // `gh` too old for the field answers nothing at all; neither is a PR
        // without CI.
        let rollup = pr.status_check_rollup.as_deref();
        let checks = rollup.unwrap_or(&[]);
        let (failing_checks, extra_failing_checks) = failing_check_names(checks);
        Self {
            number: pr.number,
            url: pr.url.into(),
            title: pr.title.into(),
            state: pr_state(&pr.state, pr.is_draft),
            checks: match rollup {
                Some(checks) => checks_state(checks),
                None => ChecksState::Unknown,
            },
            review: review_state(pr.review_decision.as_deref()),
            failing_checks,
            extra_failing_checks,
            merge: merge_state(
                pr.mergeable.as_deref(),
                pr.merge_state_status.as_deref(),
            ),
        }
    }
}

/// What the two mergeability fields say together. Both are computed lazily by
/// GitHub and both answer `UNKNOWN` until the background job that computes
/// them has run, which is why every unrecognised combination lands on
/// [`MergeState::Unknown`] rather than on a guess.
fn merge_state(mergeable: Option<&str>, status: Option<&str>) -> MergeState {
    match (mergeable, status) {
        (Some("CONFLICTING"), _) | (_, Some("DIRTY")) => MergeState::Conflicting,
        (_, Some("BEHIND")) => MergeState::Behind,
        (_, Some("BLOCKED")) => MergeState::Blocked,
        // `UNSTABLE` is a failing or pending check and `DRAFT` is the pull
        // request's own state; the chip already carries both, so neither adds
        // a reason here.
        (Some("MERGEABLE"), _) => MergeState::Mergeable,
        _ => MergeState::Unknown,
    }
}

/// The names of the checks that failed, capped at [`MAX_LISTED_CHECKS`], plus
/// how many the cap left out. A failing check with no name at all is counted
/// silently; there is no line to draw for it. The order is the rollup's own
/// order, so a check the user recognises stays where they last saw it.
fn failing_check_names(checks: &[GhCheck]) -> (Vec<SharedString>, usize) {
    let mut named: Vec<SharedString> = Vec::new();
    let mut extra = 0;
    for check in checks {
        if check.outcome() != CheckOutcome::Failing {
            continue;
        }
        let Some(label) = check.label() else { continue };
        if named.len() < MAX_LISTED_CHECKS {
            named.push(label.into());
        } else {
            extra += 1;
        }
    }
    (named, extra)
}

fn pr_state(state: &str, is_draft: bool) -> PrState {
    match state {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if is_draft => PrState::Draft,
        _ => PrState::Open,
    }
}

fn review_state(decision: Option<&str>) -> ReviewState {
    match decision {
        Some("APPROVED") => ReviewState::Approved,
        Some("CHANGES_REQUESTED") => ReviewState::ChangesRequested,
        Some("REVIEW_REQUIRED") => ReviewState::ReviewRequired,
        _ => ReviewState::None,
    }
}

/// What a whole rollup amounts to: a failure outranks a run still going, and
/// a run still going outranks the rest passing. An empty rollup is a PR with
/// no CI; a rollup that was never fetched does not reach here.
fn checks_state(checks: &[GhCheck]) -> ChecksState {
    if checks.is_empty() {
        return ChecksState::None;
    }
    let mut pending = false;
    for check in checks {
        match check.outcome() {
            CheckOutcome::Failing => return ChecksState::Failing,
            CheckOutcome::Pending => pending = true,
            CheckOutcome::Passing => {}
        }
    }
    if pending {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_gets_the_same_answer_wherever_it_is_asked() {
        let merged = PrStatus {
            number: 10461,
            url: "https://github.com/org/repo/pull/10461".into(),
            title: "Fix things".into(),
            state: PrState::Merged,
            checks: ChecksState::None,
            review: ReviewState::Approved,
            failing_checks: Vec::new(),
            extra_failing_checks: 0,
            merge: MergeState::Unknown,
        };
        let branch = Path::new("/repo");

        // No live data — an archived thread's worktree is gone, or the first
        // fetch has not landed — so the state persisted while it was live
        // stands in, rather than the row saying there is no PR.
        let chips = thread_pr_chips([(branch, "feature")], &[], &[], None, || {
            Some(vec![merged.clone()])
        });
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].url.as_deref(), Some(merged.url.as_ref()));

        // Nothing anywhere still says so: the badge is the same shape before
        // and after a PR arrives.
        let chips = thread_pr_chips([(branch, "feature")], &[], &[], None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none());

        // Not even a branch, which is the case the branch-only helper leaves
        // empty and every caller had to patch up itself.
        let chips = thread_pr_chips([], &[], &[], None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none());
    }

    /// A branch is one way a PR gets into a thread, not the definition: a PR
    /// watched by number shows on a thread whose branch has none at all, and
    /// one taken out by hand stops showing however it got in.
    #[test]
    fn a_watched_pr_shows_without_a_branch_and_a_dismissed_one_does_not() {
        let watched = PrStatus {
            number: 412,
            url: "https://github.com/owner/name/pull/412".into(),
            title: "Reviewed, never checked out".into(),
            state: PrState::Open,
            checks: ChecksState::Passing,
            review: ReviewState::None,
            failing_checks: Vec::new(),
            extra_failing_checks: 0,
            merge: MergeState::Unknown,
        };

        let chips = thread_pr_chips([], std::slice::from_ref(&watched), &[], None, || None);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].url.as_deref(), Some(watched.url.as_ref()));

        // Named repository and number: dismissed.
        let dismissed = [(Some("owner/name".to_string()), 412)];
        let chips = thread_pr_chips([], std::slice::from_ref(&watched), &dismissed, None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none(), "a dismissed PR leaves no badge");

        // A dismissal naming another repository's #412 is not this one.
        let elsewhere = [(Some("other/repo".to_string()), 412)];
        let chips = thread_pr_chips([], std::slice::from_ref(&watched), &elsewhere, None, || None);
        assert_eq!(chips[0].url.as_deref(), Some(watched.url.as_ref()));

        // A dismissal from a branch names no repository, so the number alone
        // has to be enough.
        let by_number = [(None, 412)];
        let chips = thread_pr_chips([], std::slice::from_ref(&watched), &by_number, None, || None);
        assert!(chips[0].url.is_none());
    }

    /// The `mergeable` field is computed lazily and comes back `UNKNOWN`
    /// surprisingly often on the first ask. It must read exactly like the
    /// field having never been asked for: same glyph, same card, nothing that
    /// can flicker back a second later.
    #[test]
    fn mergeability_not_yet_known_shows_what_it_would_have_shown_without_it() {
        let unknown = merge_state(Some("UNKNOWN"), Some("UNKNOWN"));
        assert_eq!(unknown, MergeState::Unknown);
        assert_eq!(unknown.blocked_reason(), None);

        let mut pr = green_pr();
        pr.merge = MergeState::Unknown;
        let unknown_chip = pr_chip(&pr);
        pr.merge = MergeState::Mergeable;
        let mergeable_chip = pr_chip(&pr);

        // A green PR reads green either way; only a known blocker changes it.
        assert_eq!(unknown_chip.checks, Some(passing_glyph()));
        assert_eq!(unknown_chip.checks, mergeable_chip.checks);
        assert_eq!(unknown_chip.tooltip, mergeable_chip.tooltip);
        assert!(
            unknown_chip
                .detail
                .is_some_and(|detail| detail.merge_blocker.is_none())
        );
    }

    /// The complaint itself: green checks and an unmergeable PR looked
    /// identical, and the only way to find out was to open it. Each blocker
    /// draws its own glyph, so a branch that is a rebase away is not read as
    /// one waiting on somebody else's approval.
    #[test]
    fn passing_checks_on_a_pr_that_cannot_merge_are_not_green() {
        for (state, glyph, reason) in [
            (
                MergeState::Behind,
                (IconName::ArrowDown, Color::Warning),
                "behind base branch",
            ),
            (
                MergeState::Conflicting,
                (IconName::Warning, Color::Warning),
                "conflicts with base branch",
            ),
            (
                MergeState::Blocked,
                (IconName::Lock, Color::Muted),
                "blocked by branch protection",
            ),
        ] {
            let mut pr = green_pr();
            pr.merge = state;
            let chip = pr_chip(&pr);
            let (icon, color) = glyph;
            assert_eq!(
                chip.checks,
                Some(ChecksGlyph::settled(icon, color)),
                "{state:?} did not draw its own glyph"
            );
            assert_ne!(
                chip.checks,
                Some(passing_glyph()),
                "{state:?} still rendered as green"
            );
            assert!(chip.tooltip.contains(reason), "{state:?}: {}", chip.tooltip);
            assert_eq!(
                chip.detail.unwrap().merge_blocker.as_deref(),
                Some(reason),
                "{state:?} did not carry its reason into the hover card"
            );
        }
    }

    /// A draft in a repository with branch protection reports `BLOCKED`, which
    /// said "blocked by branch protection" and replaced the checks glyph with
    /// a warning — so an unfinished PR looked exactly like one that cannot
    /// merge. Being a draft is not a problem to warn about; the chip already
    /// says so, muted.
    #[test]
    fn a_draft_is_not_warned_about_for_being_a_draft() {
        let json = r#"[{
            "number": 1,
            "url": "https://example.com/1",
            "title": "Half written",
            "state": "OPEN",
            "isDraft": true,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED",
            "statusCheckRollup": [{"conclusion": "SUCCESS"}]
        }]"#;
        let pr = parse_pr_list(json).unwrap().remove(0);
        assert_eq!(pr.state, PrState::Draft);
        assert_eq!(pr.merge, MergeState::Blocked);

        let chip = pr_chip(&pr);
        assert_eq!(chip.checks, Some(passing_glyph()));
        assert!(!chip.tooltip.contains("blocked"), "{}", chip.tooltip);
        assert_eq!(chip.detail.unwrap().merge_blocker, None);

        // What a draft can still be told: a conflict is reported by
        // `mergeable`, which does not answer for draftness.
        let mut conflicting = pr;
        conflicting.merge = MergeState::Conflicting;
        let chip = pr_chip(&conflicting);
        assert_eq!(
            chip.checks,
            Some(ChecksGlyph::settled(IconName::Warning, Color::Warning))
        );
        assert_eq!(
            chip.detail.unwrap().merge_blocker.as_deref(),
            Some("conflicts with base branch")
        );
    }

    /// A merged PR merged, and a closed one is not waiting to; neither has a
    /// mergeability story to tell.
    #[test]
    fn a_settled_pr_says_nothing_about_merging() {
        for state in [PrState::Merged, PrState::Closed] {
            let mut pr = green_pr();
            pr.state = state;
            pr.merge = MergeState::Behind;
            let chip = pr_chip(&pr);
            assert_eq!(chip.detail.unwrap().merge_blocker, None, "{state:?}");
            assert!(!chip.tooltip.contains("behind"), "{state:?}");
        }
    }

    /// Snapshots are persisted per thread and outlive the field being added,
    /// exactly as the check-name fields were.
    #[test]
    fn a_snapshot_written_before_mergeability_existed_still_reads() {
        let json = r#"{
            "number": 1,
            "url": "https://example.com/1",
            "title": "Old",
            "state": "Open",
            "checks": "Passing",
            "review": "None"
        }"#;
        let pr: PrStatus = serde_json::from_str(json).unwrap();
        assert_eq!(pr.merge, MergeState::Unknown);
        assert_eq!(pr.failing_checks, Vec::<SharedString>::new());
    }

    #[test]
    fn reads_both_mergeability_fields() {
        let json = r#"[{
            "number": 1,
            "url": "https://example.com/1",
            "title": "Behind",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "BEHIND",
            "statusCheckRollup": []
        }]"#;
        assert_eq!(parse_pr_list(json).unwrap()[0].merge, MergeState::Behind);

        // A conflict is reported by either field, and `DIRTY` is the reason
        // `mergeable` gives when it says `CONFLICTING`.
        assert_eq!(
            merge_state(Some("CONFLICTING"), Some("UNKNOWN")),
            MergeState::Conflicting
        );
        assert_eq!(merge_state(None, Some("DIRTY")), MergeState::Conflicting);
        assert_eq!(merge_state(Some("MERGEABLE"), Some("BLOCKED")), MergeState::Blocked);
        // Failing checks and draft state are already on the chip, so neither
        // becomes a merge reason of its own.
        assert_eq!(
            merge_state(Some("MERGEABLE"), Some("UNSTABLE")),
            MergeState::Mergeable
        );
        assert_eq!(merge_state(Some("MERGEABLE"), Some("CLEAN")), MergeState::Mergeable);
        // A `gh` too old to know the fields at all.
        assert_eq!(merge_state(None, None), MergeState::Unknown);
    }

    /// The wedge the `gh` timeout exists to prevent. A hung invocation killed
    /// by the timeout is an ordinary failed refresh, and a failed refresh must
    /// leave the branch refreshable: `refresh_branch` returns early while
    /// `refresh_task` is set, so a refresh that never finishes stops that
    /// branch's chip updating for the life of the process, quietly, while
    /// every other branch keeps going.
    #[gpui::test]
    fn a_failed_refresh_leaves_the_branch_refreshable(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        let key = WatchKey {
            repo_path: PathBuf::from("/repo"),
            subject: WatchSubject::Branch("main".into()),
        };
        store.update(cx, |store, cx| {
            let known = vec![pr(1, "https://example.com/1")];
            let watched = store.watched.entry(key.clone()).or_default();
            watched.watch_count = 1;
            watched.prs = Some(known.clone());
            // Stand in for the invocation that is in flight.
            watched.refresh_task = Some(cx.spawn(async move |_, _| {}));

            store.finish_refresh(
                &key,
                Err(anyhow::anyhow!("gh timed out after 20 seconds")),
                cx,
            );

            assert!(
                store.watched[&key].refresh_task.is_none(),
                "a failed refresh left the branch marked as still refreshing, \
                 so the next poll would skip it forever"
            );
            // What was already known stays on the chip; the failure is
            // reported rather than blanking the row.
            assert_eq!(store.watched[&key].prs.as_ref(), Some(&known));
            assert!(store.last_error.is_some());
        });
    }

    /// The flood. Every due branch used to be spawned in the same tick, so a
    /// session with a hundred of them made a hundred requests before GitHub
    /// had answered one — and the first refusal arrived after the whole flood
    /// was already out. The cap is what turns it into a trickle a refusal can
    /// stop.
    #[gpui::test]
    fn a_poll_tick_does_not_ask_about_everything_at_once(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            for n in 0..20u64 {
                let key = WatchKey {
                    repo_path: PathBuf::from("/repo"),
                    subject: WatchSubject::Number {
                        number: n,
                        repo: None,
                    },
                };
                let watched = store.watched.entry(key).or_default();
                watched.watch_count = 1;
            }

            store.refresh_due(cx);
            assert_eq!(
                store.fetches_in_flight(),
                MAX_FETCHES_IN_FLIGHT,
                "a tick starts the cap and no more"
            );
            let started: Vec<WatchKey> = store
                .watched
                .iter()
                .filter(|(_, watched)| watched.refresh_task.is_some())
                .map(|(key, _)| key.clone())
                .collect();

            // The tick after it starts nothing new while they are in flight,
            // rather than piling a second round on top.
            store.refresh_due(cx);
            assert_eq!(store.fetches_in_flight(), MAX_FETCHES_IN_FLIGHT);

            // What did not go out is not dropped: it goes out once there is
            // room, and the ones that have waited longest go first.
            for key in &started {
                store.watched.get_mut(key).unwrap().refresh_task = None;
            }
            store.refresh_due(cx);
            let second_round: Vec<WatchKey> = store
                .watched
                .iter()
                .filter(|(_, watched)| watched.refresh_task.is_some())
                .map(|(key, _)| key.clone())
                .collect();
            assert_eq!(second_round.len(), MAX_FETCHES_IN_FLIGHT);
            assert!(
                second_round.iter().all(|key| !started.contains(key)),
                "the branches already asked about do not go again before the rest go once"
            );
        });
    }

    /// "Say it once" said it for every request in the flood: the guard
    /// compared the stored deadline against `now + backoff`, which is later
    /// than it by construction. 118 identical lines in the same second is
    /// what that looked like.
    #[gpui::test]
    fn a_spent_budget_is_reported_once_per_window(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let refuse = |store: &mut GhStatusStore, n: u64, cx: &mut Context<GhStatusStore>| {
                let key = WatchKey {
                    repo_path: PathBuf::from("/repo"),
                    subject: WatchSubject::Number {
                        number: n,
                        repo: None,
                    },
                };
                let watched = store.watched.entry(key.clone()).or_default();
                watched.watch_count = 1;
                watched.refresh_task = Some(cx.spawn(async move |_, _| {}));
                store.finish_refresh(
                    &key,
                    Err(anyhow::anyhow!("API rate limit exceeded for user ID 1")),
                    cx,
                );
            };

            refuse(store, 1, cx);
            let first = store
                .rate_limited_until
                .expect("the first refusal holds every fetch");
            assert!(store.is_rate_limited());

            // The rest of the flood lands on a store already holding, which
            // is the state the guard has to recognise.
            for n in 2..10 {
                refuse(store, n, cx);
            }
            assert!(
                store.rate_limited_until.is_some_and(|until| until >= first),
                "a later refusal never shortens the hold"
            );
        });
    }

    /// What the app spent is the one side of the budget it can count, and
    /// one `gh` invocation is one request.
    #[gpui::test]
    fn the_requests_this_app_made_are_counted(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            assert_eq!(store.requests_this_hour(), 0);
            for n in 0..3u64 {
                let key = WatchKey {
                    repo_path: PathBuf::from("/repo"),
                    subject: WatchSubject::Number {
                        number: n,
                        repo: None,
                    },
                };
                store.watched.entry(key.clone()).or_default().watch_count = 1;
                store.refresh_branch(key, cx);
            }
            assert_eq!(
                store.requests_this_hour(),
                3,
                "three branches asked about is three requests"
            );
        });
    }

    /// GitHub says when the window turns over; the fixed backoff is only for
    /// when it has not been asked or would not say.
    #[test]
    fn the_reset_github_gives_is_read_and_sanity_checked() {
        assert_eq!(
            parse_rate_limit_reset_in("1793\n").unwrap(),
            Duration::from_secs(1793)
        );
        // A window already turned over is no wait at all, not a negative one.
        assert_eq!(
            parse_rate_limit_reset_in("-12").unwrap(),
            Duration::from_secs(0)
        );
        // Longer than the window itself is a clock disagreeing, and holding
        // for a day on it would be worse than guessing.
        assert!(parse_rate_limit_reset_in("90000").is_err());
        assert!(parse_rate_limit_reset_in("").is_err());
        assert!(parse_rate_limit_reset_in("soon").is_err());
    }

    /// Which branches the faster poll picks up. Checks in progress change
    /// within a minute or two and then stop changing for hours, so they are the
    /// only ones worth asking about between full polls.
    #[test]
    fn only_branches_with_checks_running_are_polled_harder() {
        let branch = |prs: Option<Vec<PrStatus>>| WatchedBranch {
            watch_count: 1,
            prs,
            refresh_task: None,
            last_polled: None,
            last_fetched: None,
        };
        let with = |state: PrState, checks: ChecksState| {
            let mut pr = green_pr();
            pr.state = state;
            pr.checks = checks;
            vec![pr]
        };

        // Never fetched, and fetched with no PR: nothing says either is
        // interesting yet.
        assert!(!branch(None).has_checks_in_flight());
        assert!(!branch(Some(Vec::new())).has_checks_in_flight());

        assert!(branch(Some(with(PrState::Open, ChecksState::Pending))).has_checks_in_flight());
        assert!(branch(Some(with(PrState::Draft, ChecksState::Pending))).has_checks_in_flight());

        // Settled: a green PR, and a merged one whose checks never move again.
        assert!(!branch(Some(with(PrState::Open, ChecksState::Passing))).has_checks_in_flight());
        assert!(!branch(Some(with(PrState::Open, ChecksState::Failing))).has_checks_in_flight());
        assert!(!branch(Some(with(PrState::Merged, ChecksState::Pending))).has_checks_in_flight());
    }

    /// How often each kind of branch is asked about. The nineteen branches
    /// this fork watches were every one of them polled every minute, rollup
    /// and all, which spent GitHub's hourly budget in twenty minutes.
    #[test]
    fn a_branch_is_polled_as_often_as_it_could_change() {
        let branch = |prs: Option<Vec<PrStatus>>| WatchedBranch {
            watch_count: 1,
            prs,
            refresh_task: None,
            last_polled: None,
            last_fetched: None,
        };
        let one = |state: PrState, checks: ChecksState| {
            let mut pr = green_pr();
            pr.state = state;
            pr.checks = checks;
            vec![pr]
        };

        // Never fetched: ask now, since nothing is known about what the
        // checks are doing.
        assert_eq!(branch(None).poll_interval(), Some(PENDING_POLL_INTERVAL));

        // A run in flight is the one thing worth a call every twenty seconds.
        let running = branch(Some(one(PrState::Open, ChecksState::Pending)));
        assert_eq!(running.poll_interval(), Some(PENDING_POLL_INTERVAL));

        // Green and open: what moves it is a push or a review, and neither is
        // worth twenty seconds. Five minutes still asks what the checks are
        // doing, which is what makes a push visible at all.
        let green = branch(Some(one(PrState::Open, ChecksState::Passing)));
        assert_eq!(green.poll_interval(), Some(IDLE_POLL_INTERVAL));

        // Merged and closed never move again, so they are never asked about.
        assert_eq!(branch(Some(one(PrState::Merged, ChecksState::Passing))).poll_interval(), None);
        assert_eq!(branch(Some(one(PrState::Closed, ChecksState::Failing))).poll_interval(), None);

        // A branch with no PR at all is not settled — one can still be opened
        // on it — but nothing about it moves on its own, and a PR opened from
        // here arrives with the push that refreshes it anyway.
        assert_eq!(
            branch(Some(Vec::new())).poll_interval(),
            Some(IDLE_POLL_INTERVAL)
        );
    }

    /// The error GitHub answers with once the budget is gone, in both the
    /// spellings it uses, against the errors that mean something else.
    #[test]
    fn a_spent_budget_is_told_apart_from_an_ordinary_failure() {
        assert!(is_rate_limited(
            "gh exited with exit status: 1: GraphQL: API rate limit exceeded for user ID 5551212."
        ));
        assert!(is_rate_limited(
            "gh exited with exit status: 1: GraphQL: API rate limit already exceeded (rateLimit)"
        ));
        assert!(!is_rate_limited("gh timed out after 20 seconds"));
        assert!(!is_rate_limited(
            "failed to run gh; is the GitHub CLI installed?"
        ));
    }

    /// The rollup is the only thing in the answer that reports CI, so a poll
    /// that leaves it out cannot notice anything changing. Leaving it out of
    /// the polls whose checks looked settled is what froze a green chip across
    /// a push that broke the build.
    #[test]
    fn every_poll_asks_what_the_checks_are_doing() {
        assert!(
            GH_JSON_FIELDS.contains("statusCheckRollup"),
            "a poll without the rollup carries yesterday's CI state forward"
        );
    }

    /// The last case the complaint listed: a PR whose rollup fetch failed
    /// after an earlier pending answer. A failure is not an answer, so the
    /// chip keeps saying what it last knew rather than losing its glyph.
    #[gpui::test]
    fn a_failed_poll_keeps_the_pending_glyph_it_already_had(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        let key = WatchKey {
            repo_path: PathBuf::from("/repo"),
            subject: WatchSubject::Branch("main".into()),
        };
        store.update(cx, |store, cx| {
            let mut pending = pr(1, "https://example.com/1");
            pending.checks = ChecksState::Pending;
            let watched = store.watched.entry(key.clone()).or_default();
            watched.watch_count = 1;
            watched.prs = Some(vec![pending]);
            watched.refresh_task = Some(cx.spawn(async move |_, _| {}));

            store.finish_refresh(
                &key,
                Err(anyhow::anyhow!(
                    "gh exited with exit status: 1: GraphQL: API rate limit exceeded"
                )),
                cx,
            );

            let prs = store.watched[&key].prs.as_deref().expect("kept");
            assert_eq!(prs[0].checks, ChecksState::Pending);
            let glyph = pr_chip(&prs[0]).checks.expect("still a running run");
            assert!(glyph.spinning);
        });
    }

    /// An open PR whose checks all pass: the case that used to read as ready
    /// whatever its mergeability said.
    fn green_pr() -> PrStatus {
        PrStatus {
            number: 10461,
            url: "https://github.com/org/repo/pull/10461".into(),
            title: "Fix things".into(),
            state: PrState::Open,
            checks: ChecksState::Passing,
            review: ReviewState::Approved,
            failing_checks: Vec::new(),
            extra_failing_checks: 0,
            merge: MergeState::Unknown,
        }
    }

    #[test]
    fn parses_open_pr_with_passing_checks_and_approval() {
        let json = r#"[{
            "number": 10461,
            "url": "https://github.com/org/repo/pull/10461",
            "title": "Fix things",
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "state": "SUCCESS"}
            ]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(
            prs,
            vec![PrStatus {
                number: 10461,
                url: "https://github.com/org/repo/pull/10461".into(),
                title: "Fix things".into(),
                state: PrState::Open,
                checks: ChecksState::Passing,
                review: ReviewState::Approved,
                failing_checks: Vec::new(),
                extra_failing_checks: 0,
                merge: MergeState::Unknown,
            }]
        );
    }

    #[test]
    fn failing_check_outranks_pending() {
        let json = r#"[{
            "number": 1,
            "url": "https://example.com/1",
            "title": "t",
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""},
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"}
            ]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::Failing);
        assert_eq!(prs[0].review, ReviewState::None);
    }

    #[test]
    fn in_progress_check_run_is_pending() {
        let json = r#"[{
            "number": 2,
            "url": "https://example.com/2",
            "title": "t",
            "state": "OPEN",
            "isDraft": false,
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "status": "QUEUED", "conclusion": ""},
                {"__typename": "StatusContext", "state": "PENDING"}
            ]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::Pending);
    }

    #[test]
    fn failing_check_names_are_listed_with_their_workflow() {
        let json = r#"[{
            "number": 100,
            "url": "https://example.com/100",
            "title": "t",
            "state": "OPEN",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "clippy", "workflowName": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"__typename": "CheckRun", "name": "clippy", "workflowName": "release", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"__typename": "CheckRun", "name": "unit", "workflowName": "unit", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"__typename": "CheckRun", "name": "build", "workflowName": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "context": "dco", "state": "ERROR"}
            ]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::Failing);
        assert_eq!(
            prs[0].failing_checks,
            vec![
                SharedString::from("ci / clippy"),
                SharedString::from("release / clippy"),
                SharedString::from("unit"),
                SharedString::from("dco"),
            ],
            "workflow name disambiguates two checks named the same; a status \
             context stands in for its context; a job whose name equals its \
             workflow name reads once"
        );
        assert_eq!(prs[0].extra_failing_checks, 0);
    }

    #[test]
    fn failing_check_names_beyond_the_cap_are_counted() {
        // MAX_LISTED_CHECKS is 6; 8 failures leave 2 unreported.
        let checks = (0..8)
            .map(|ix| {
                format!(
                    r#"{{"__typename": "CheckRun", "name": "job-{ix}", "status": "COMPLETED", "conclusion": "FAILURE"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"[{{
                "number": 200,
                "url": "https://example.com/200",
                "title": "t",
                "state": "OPEN",
                "statusCheckRollup": [{checks}]
            }}]"#
        );
        let prs = parse_pr_list(&json).unwrap();
        assert_eq!(prs[0].failing_checks.len(), MAX_LISTED_CHECKS);
        assert_eq!(prs[0].extra_failing_checks, 8 - MAX_LISTED_CHECKS);
    }

    #[test]
    fn a_failing_check_with_no_name_still_counts_but_lists_nothing() {
        let json = r#"[{
            "number": 300,
            "url": "https://example.com/300",
            "title": "t",
            "state": "OPEN",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"}
            ]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::Failing);
        assert!(prs[0].failing_checks.is_empty());
        assert_eq!(prs[0].extra_failing_checks, 0);
    }

    #[test]
    fn error_status_context_is_failing() {
        let json = r#"[{
            "number": 3,
            "url": "https://example.com/3",
            "title": "t",
            "state": "OPEN",
            "statusCheckRollup": [{"__typename": "StatusContext", "state": "ERROR"}]
        }]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::Failing);
    }

    /// An empty rollup is an answer — this PR has no CI — and a missing one is
    /// not. They used to be the same value, which is how a hover card came to
    /// say "no checks" about a PR whose rollup nobody had read.
    #[test]
    fn an_empty_rollup_is_not_a_missing_one() {
        let json = r#"[
            {"number": 4, "url": "u", "title": "t", "state": "OPEN", "statusCheckRollup": []},
            {"number": 5, "url": "u", "title": "t", "state": "OPEN", "statusCheckRollup": null},
            {"number": 6, "url": "u", "title": "t", "state": "OPEN"}
        ]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].checks, ChecksState::None);
        assert_eq!(prs[1].checks, ChecksState::Unknown);
        assert_eq!(prs[2].checks, ChecksState::Unknown);

        // Neither draws a glyph. What differs is what the card says: "no
        // checks" is only claimed about the PR that answered.
        assert!(pr_chip(&prs[0]).checks.is_none());
        assert!(pr_chip(&prs[1]).checks.is_none());
        assert_eq!(
            pr_chip(&prs[0]).detail.unwrap().checks,
            SharedString::from("no checks")
        );
        assert!(pr_chip(&prs[1]).detail.unwrap().checks.is_empty());
    }

    #[test]
    fn draft_only_applies_to_open_prs() {
        let json = r#"[
            {"number": 7, "url": "u", "title": "t", "state": "OPEN", "isDraft": true},
            {"number": 8, "url": "u", "title": "t", "state": "MERGED", "isDraft": true},
            {"number": 9, "url": "u", "title": "t", "state": "CLOSED", "isDraft": false}
        ]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].state, PrState::Draft);
        assert_eq!(prs[1].state, PrState::Merged);
        assert_eq!(prs[2].state, PrState::Closed);
    }

    #[test]
    fn review_decision_variants() {
        let json = r#"[
            {"number": 10, "url": "u", "title": "t", "state": "OPEN", "reviewDecision": "CHANGES_REQUESTED"},
            {"number": 11, "url": "u", "title": "t", "state": "OPEN", "reviewDecision": "REVIEW_REQUIRED"},
            {"number": 12, "url": "u", "title": "t", "state": "OPEN", "reviewDecision": null},
            {"number": 13, "url": "u", "title": "t", "state": "OPEN", "reviewDecision": ""}
        ]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs[0].review, ReviewState::ChangesRequested);
        assert_eq!(prs[1].review, ReviewState::ReviewRequired);
        assert_eq!(prs[2].review, ReviewState::None);
        assert_eq!(prs[3].review, ReviewState::None);
    }

    fn pr(number: u64, url: &str) -> PrStatus {
        PrStatus {
            number,
            url: url.into(),
            title: format!("PR {number}").into(),
            state: PrState::Open,
            checks: ChecksState::Passing,
            review: ReviewState::None,
            failing_checks: Vec::new(),
            extra_failing_checks: 0,
            merge: MergeState::Unknown,
        }
    }

    fn store_with_prs(cx: &mut gpui::TestAppContext) -> Entity<GhStatusStore> {
        cx.new(|cx| {
            let mut store = GhStatusStore::new(cx);
            store.set_prs_for_test(
                PathBuf::from("/worktrees/mine"),
                "mine".into(),
                vec![
                    pr(1, "https://example.com/1"),
                    pr(2, "https://example.com/2"),
                ],
            );
            store.set_prs_for_test(
                PathBuf::from("/repo"),
                "main".into(),
                vec![pr(3, "https://example.com/3")],
            );
            store.set_prs_for_test(
                PathBuf::from("/worktrees/other"),
                "other".into(),
                vec![pr(4, "https://example.com/4")],
            );
            store
        })
    }

    #[gpui::test]
    fn chips_only_cover_the_given_branches(cx: &mut gpui::TestAppContext) {
        let store = store_with_prs(cx);
        store.read_with(cx, |store, _cx| {
            let chips =
                pr_chips_for_branches([(Path::new("/worktrees/mine"), "mine")], Some(store));
            let labels: Vec<_> = chips.iter().map(|chip| chip.label.to_string()).collect();
            assert_eq!(
                labels,
                vec!["#1", "#2"],
                "only the given branch's PRs should show, including several PRs for one branch"
            );
        });
    }

    #[gpui::test]
    fn a_branch_without_prs_gets_an_inert_no_pr_pill(cx: &mut gpui::TestAppContext) {
        let store = store_with_prs(cx);
        store.read_with(cx, |store, _cx| {
            let chips =
                pr_chips_for_branches([(Path::new("/worktrees/fresh"), "fresh")], Some(store));
            assert_eq!(chips.len(), 1);
            assert_eq!(chips[0].label, SharedString::from("no PR"));
            assert!(chips[0].url.is_none());
        });
    }

    #[gpui::test]
    fn no_branches_means_no_chips(cx: &mut gpui::TestAppContext) {
        let store = store_with_prs(cx);
        store.read_with(cx, |store, _cx| {
            assert!(pr_chips_for_branches([], Some(store)).is_empty());
        });
    }

    #[gpui::test]
    fn the_same_pr_across_two_branches_shows_once(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|cx| {
            let mut store = GhStatusStore::new(cx);
            store.set_prs_for_test(
                PathBuf::from("/a"),
                "shared".into(),
                vec![pr(7, "https://example.com/7")],
            );
            store.set_prs_for_test(
                PathBuf::from("/b"),
                "shared".into(),
                vec![pr(7, "https://example.com/7")],
            );
            store
        });
        store.read_with(cx, |store, _cx| {
            let chips = pr_chips_for_branches(
                [(Path::new("/a"), "shared"), (Path::new("/b"), "shared")],
                Some(store),
            );
            assert_eq!(chips.len(), 1);
        });
    }

    #[test]
    fn a_merged_pr_is_purple_and_shows_no_checks() {
        let mut merged = pr(5, "https://example.com/5");
        merged.state = PrState::Merged;
        merged.checks = ChecksState::Passing;

        let chip = pr_chip(&merged);
        assert_eq!(chip.state_color, Color::Custom(MERGED_PR_COLOR));
        assert!(chip.checks.is_none(), "a merged PR passed by definition");
        let detail = chip.detail.expect("a real PR carries a hover card");
        assert!(detail.checks.is_empty());
        assert!(detail.checks_icon.is_none());
        assert_eq!(chip.tooltip, SharedString::from("PR 5 (merged)"));
    }

    #[test]
    fn a_merged_pr_hides_even_failing_checks() {
        let mut merged = pr(6, "https://example.com/6");
        merged.state = PrState::Merged;
        merged.checks = ChecksState::Failing;
        assert!(pr_chip(&merged).checks.is_none());
    }

    #[test]
    fn an_open_pr_keeps_its_checks_glyph() {
        let chip = pr_chip(&pr(8, "https://example.com/8"));
        assert_eq!(chip.state_color, Color::Success);
        assert_eq!(chip.checks, Some(passing_glyph()));
    }

    /// The glyph a PR whose checks all passed draws, which half of these tests
    /// compare against one way or the other.
    fn passing_glyph() -> ChecksGlyph {
        ChecksGlyph::settled(IconName::Check, Color::Success)
    }

    /// The complaint, in one test: every shape of "CI is running" GitHub
    /// reports must reach the chip as a running glyph. A check run in
    /// progress, one queued, one merely waiting, a commit status pending and
    /// one only expected — the last three used to read as passing, because
    /// anything the reading did not recognise fell through to green.
    #[test]
    fn every_shape_of_running_ci_gets_the_running_glyph() {
        let running = [
            r#"{"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""}"#,
            r#"{"__typename": "CheckRun", "status": "QUEUED", "conclusion": ""}"#,
            r#"{"__typename": "CheckRun", "status": "WAITING", "conclusion": ""}"#,
            r#"{"__typename": "CheckRun", "status": "REQUESTED", "conclusion": ""}"#,
            r#"{"__typename": "CheckRun", "status": "PENDING", "conclusion": ""}"#,
            r#"{"__typename": "StatusContext", "state": "PENDING"}"#,
            r#"{"__typename": "StatusContext", "state": "EXPECTED"}"#,
            // A check run with nothing said about it at all.
            r#"{"__typename": "CheckRun"}"#,
        ];
        for check in running {
            let json = format!(
                r#"[{{
                    "number": 1,
                    "url": "https://example.com/1",
                    "title": "t",
                    "state": "OPEN",
                    "statusCheckRollup": [
                        {{"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"}},
                        {check}
                    ]
                }}]"#
            );
            let prs = parse_pr_list(&json).unwrap();
            assert_eq!(prs[0].checks, ChecksState::Pending, "{check}");
            let glyph = pr_chip(&prs[0])
                .checks
                .unwrap_or_else(|| panic!("no glyph at all for {check}"));
            assert!(glyph.spinning, "a run in progress has to look like one: {check}");
            assert_eq!(glyph.color, Color::Warning, "{check}");
        }
    }

    /// A run that was cancelled, timed out, failed to start or is waiting on
    /// somebody to approve it did not pass, and each of these used to draw a
    /// green tick. The names go on the hover card like any other failure.
    #[test]
    fn a_run_that_did_not_finish_is_not_a_run_that_passed() {
        for conclusion in ["CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE"] {
            let json = format!(
                r#"[{{
                    "number": 1,
                    "url": "https://example.com/1",
                    "title": "t",
                    "state": "OPEN",
                    "statusCheckRollup": [
                        {{"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "{conclusion}"}}
                    ]
                }}]"#
            );
            let prs = parse_pr_list(&json).unwrap();
            assert_eq!(prs[0].checks, ChecksState::Failing, "{conclusion}");
            assert_eq!(
                prs[0].failing_checks,
                vec![SharedString::from("build")],
                "{conclusion}"
            );
        }
    }

    /// The conclusions that are not failures, so a skipped job does not turn a
    /// PR red.
    #[test]
    fn a_skipped_or_neutral_check_still_passes() {
        for conclusion in ["SUCCESS", "NEUTRAL", "SKIPPED", "STALE"] {
            let json = format!(
                r#"[{{
                    "number": 1,
                    "url": "https://example.com/1",
                    "title": "t",
                    "state": "OPEN",
                    "statusCheckRollup": [
                        {{"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "{conclusion}"}}
                    ]
                }}]"#
            );
            let prs = parse_pr_list(&json).unwrap();
            assert_eq!(prs[0].checks, ChecksState::Passing, "{conclusion}");
        }
    }

    /// The chip both surfaces draw comes from one function, so this is the
    /// assertion for "the sidebar row and the thread's bottom bar both show a
    /// run in progress": `thread_pr_chips` is what each of them calls.
    #[test]
    fn a_running_run_reaches_the_chip_both_surfaces_draw() {
        let mut running = pr(9, "https://example.com/9");
        running.checks = ChecksState::Pending;

        let chips =
            thread_pr_chips([(Path::new("/repo"), "feature")], &[], &[], None, || {
                Some(vec![running])
            });
        assert_eq!(chips.len(), 1);
        let glyph = chips[0].checks.expect("a run in progress has a glyph");
        assert!(glyph.spinning);
        assert_eq!(glyph.color, Color::Warning);
        let detail = chips[0].detail.as_ref().expect("a real PR has a card");
        assert_eq!(detail.checks, SharedString::from("checks running"));
        assert_eq!(detail.checks_icon, Some(glyph));
    }

    #[test]
    fn empty_pr_list_parses() {
        assert_eq!(parse_pr_list("[]").unwrap(), vec![]);
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_pr_list("not json").is_err());
    }

    /// `gh pr view` answers with the pull request itself rather than a list
    /// of one, so it needs its own read of the same fields.
    #[test]
    fn a_pr_viewed_by_number_parses() {
        let json = r#"{
            "number": 64463,
            "url": "https://github.com/zed-industries/zed/pull/64463",
            "title": "Remove Baseten provider",
            "state": "MERGED",
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [{"conclusion": "SUCCESS"}]
        }"#;
        let pr = parse_pr_view(json).unwrap();
        assert_eq!(pr.number, 64463);
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.checks, ChecksState::Passing);
        assert_eq!(pr.review, ReviewState::Approved);
    }

    #[test]
    fn a_pr_list_payload_is_not_a_pr_view_payload() {
        assert!(parse_pr_view("[]").is_err());
    }
}
