//! Tracks GitHub PR and CI status for git branches by polling the `gh` CLI.

mod graphql;

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
use ui::{ChecksGlyph, Color, IconName, PrCheckCounts, PrChipDetail, ThreadItemPrChip};
use util::ResultExt as _;

use crate::graphql::{Answer, Ask, RepoId};

/// The poll loop's grain; each subject decides by its own interval whether a tick asks about it.
const POLL_INTERVAL: Duration = Duration::from_secs(20);

const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// An open PR with nothing in flight moves on a push (which refreshes it directly) or a review.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The hold after a refusal when GitHub has not said when the window resets.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(10 * 60);

const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// A cap on per-subject `gh` invocations, so the first refusal stops the ones behind it rather
/// than arriving after the whole flood is out.
const MAX_FETCHES_IN_FLIGHT: usize = 4;

/// Past this, one query asks GitHub for more nodes than it will answer for.
const MAX_SUBJECTS_PER_BATCH: usize = 50;

/// A window opening registers its watches together; waiting turns them into one query.
const BATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// A query that keeps failing would otherwise cost a wasted request on top of the per-subject
/// fallback on every poll.
const GRAPHQL_FAILURES_BEFORE_BACKOFF: usize = 3;
const GRAPHQL_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Checkouts `gh` would not name fall back to the per-subject path; a remote can be added later,
/// so this is a hold rather than a verdict.
const REPO_ID_RETRY: Duration = Duration::from_secs(30 * 60);

/// A `gh` invocation that hung used to wedge its branch's chip for the life of the process.
const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// No theme role is purple, and readers expect GitHub's merged purple (#8250df).
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
    /// Capped at [`MAX_LISTED_CHECKS`]; the rest are counted in `extra_failing_checks`.
    #[serde(default)]
    pub failing_checks: Vec<SharedString>,
    #[serde(default)]
    pub extra_failing_checks: usize,
    #[serde(default)]
    pub merge: MergeState,
    /// How the checks divide up, so a chip can say how much CI is left.
    #[serde(default)]
    pub check_counts: CheckCounts,
}

/// How many checks are in each state. GitHub counts these over the whole rollup, so a
/// truncated `contexts` page still reports the right totals; counted from the flattened
/// list when that is all a `gh pr list` answer carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckCounts {
    #[serde(default)]
    pub failed: usize,
    #[serde(default)]
    pub running: usize,
    #[serde(default)]
    pub passed: usize,
    #[serde(default)]
    pub skipped: usize,
}

impl CheckCounts {
    /// `CheckRunState` and `StatusState` values. Only an explicitly finished value is
    /// counted as finished, so a state GitHub adds later reads as still running rather
    /// than making a pull request look greener than it is.
    pub(crate) fn add(&mut self, state: &str, count: usize) {
        match state {
            "SUCCESS" | "NEUTRAL" | "STALE" => self.passed += count,
            "SKIPPED" => self.skipped += count,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED"
            | "CANCELLED" => self.failed += count,
            _ => self.running += count,
        }
    }

    pub(crate) fn from_states<'a>(states: impl IntoIterator<Item = (&'a str, usize)>) -> Self {
        let mut counts = Self::default();
        for (state, count) in states {
            counts.add(state, count);
        }
        counts
    }

    pub fn total(self) -> usize {
        self.failed + self.running + self.passed + self.skipped
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// The fallback: one pass over the rollup GitHub did flatten for us.
fn counts_from_checks(checks: &[GhCheck]) -> CheckCounts {
    let mut counts = CheckCounts::default();
    for check in checks {
        if check.conclusion.as_deref() == Some("SKIPPED") {
            counts.skipped += 1;
            continue;
        }
        match check.outcome() {
            CheckOutcome::Failing => counts.failed += 1,
            CheckOutcome::Pending => counts.running += 1,
            CheckOutcome::Passing => counts.passed += 1,
        }
    }
    counts
}

/// Whether GitHub will merge the PR, which green checks do not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeState {
    /// GitHub computes mergeability lazily and often answers `UNKNOWN` first. Never rendered, so a
    /// chip does not flicker between "can merge" and "unknown".
    #[default]
    Unknown,
    Mergeable,
    Behind,
    Conflicting,
    /// Branch protection: a required review, a required check that has not reported, etc.
    Blocked,
}

impl MergeState {
    fn blocked_reason(self) -> Option<&'static str> {
        match self {
            MergeState::Behind => Some("behind base branch"),
            MergeState::Conflicting => Some("conflicts with base branch"),
            MergeState::Blocked => Some("blocked by branch protection"),
            MergeState::Unknown | MergeState::Mergeable => None,
        }
    }

    /// Stands in for the checks glyph. Behind is a rebase away, a conflict is that gone wrong, and
    /// branch protection is someone else's call that only has to stop the chip reading as ready.
    fn blocked_glyph(self) -> Option<(IconName, Color)> {
        match self {
            MergeState::Behind => Some((IconName::ArrowDown, Color::Warning)),
            MergeState::Conflicting => Some((IconName::Warning, Color::Warning)),
            MergeState::Blocked => Some((IconName::Lock, Color::Muted)),
            MergeState::Unknown | MergeState::Mergeable => None,
        }
    }
}

const MAX_LISTED_CHECKS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Merged,
    Closed,
    Draft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChecksState {
    Pending,
    Passing,
    Failing,
    /// GitHub answered with an empty rollup: the PR has no CI.
    None,
    /// No rollup has been read. Kept apart from `None` so a card never claims "no checks" when
    /// nobody looked.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    None,
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Global store of GitHub PR and CI status for watched branches and PRs.
pub struct GhStatusStore {
    watched: HashMap<WatchKey, WatchedBranch>,
    /// Until this passes no fetch starts: every one would be refused.
    rate_limited_until: Option<Instant>,
    rate_limit_reset: Option<Instant>,
    /// GitHub's budget is per account, so counting this app's own requests is what says whether
    /// it is the spender or a bystander.
    requests_this_hour: VecDeque<Instant>,
    last_rate_limit: Option<graphql::RateLimit>,
    repo_ids: HashMap<PathBuf, RepoId>,
    unresolvable_repos: HashMap<PathBuf, Instant>,
    batch_task: Option<Task<()>>,
    batch_arm_task: Option<Task<()>>,
    graphql_failures: usize,
    graphql_quiet_until: Option<Instant>,
    graphql_fallback_logged: bool,
    _reset_task: Option<Task<()>>,
    _poll_task: Task<()>,
}

impl GhStatusStore {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalGhStatusStore>()
            .map(|global| global.0.clone())
    }

    /// `None` until the first successful fetch completes.
    pub fn prs_for_branch(&self, repo_path: &Path, branch: &str) -> Option<&Vec<PrStatus>> {
        self.watched
            .get(&WatchKey::branch(repo_path, branch))
            .and_then(|watched| watched.prs.as_ref())
    }

    /// Watches are refcounted; the branch is polled until a matching number of `unwatch` calls.
    pub fn watch(&mut self, repo_path: PathBuf, branch: String, cx: &mut Context<Self>) {
        self.watch_key(
            WatchKey {
                repo_path,
                subject: WatchSubject::Branch(branch),
            },
            cx,
        );
    }

    pub fn unwatch(&mut self, repo_path: &Path, branch: &str, cx: &mut Context<Self>) {
        self.unwatch_key(&WatchKey::branch(repo_path, branch), cx);
    }

    /// Watch one pull request by number, whatever branch it is on, including one in a repository
    /// nobody here has checked out.
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
        self.unwatch_key(&WatchKey::number(repo_path, number, repo), cx);
    }

    pub fn pr_by_number(
        &self,
        repo_path: &Path,
        number: u64,
        repo: Option<&str>,
    ) -> Option<&PrStatus> {
        self.watched
            .get(&WatchKey::number(repo_path, number, repo))
            .and_then(|watched| watched.prs.as_ref())
            .and_then(|prs| prs.first())
    }

    fn watch_key(&mut self, key: WatchKey, cx: &mut Context<Self>) {
        let watched = self.watched.entry(key).or_default();
        watched.watch_count += 1;
        if watched.watch_count == 1 {
            // A never-polled subject is due, so the armed batch picks it up.
            self.arm_batch(cx);
        }
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

    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        let keys = self.keys_to_ask_about(|_| true);
        self.ask_about(keys, cx);
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
            rate_limited_until: None,
            rate_limit_reset: None,
            requests_this_hour: VecDeque::new(),
            last_rate_limit: None,
            repo_ids: HashMap::default(),
            unresolvable_repos: HashMap::default(),
            batch_task: None,
            batch_arm_task: None,
            graphql_failures: 0,
            graphql_quiet_until: None,
            graphql_fallback_logged: false,
            _reset_task: None,
            _poll_task: poll_task,
        }
    }

    fn refresh_due(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let due = self.keys_to_ask_about(|watched| watched.is_due(now));
        self.ask_about(due, cx);
    }

    /// Longest-waiting first, so the cap delays a subject rather than starving one.
    fn keys_to_ask_about(&self, mut want: impl FnMut(&WatchedBranch) -> bool) -> Vec<WatchKey> {
        let mut keys = self
            .watched
            .iter()
            .filter(|(_, watched)| !watched.is_refreshing() && want(watched))
            .map(|(key, watched)| (key.clone(), watched.last_polled))
            .collect::<Vec<_>>();
        keys.sort_by_key(|(_, last_polled)| *last_polled);
        keys.truncate(MAX_SUBJECTS_PER_BATCH);
        keys.into_iter().map(|(key, _)| key).collect()
    }

    fn ask_about(&mut self, keys: Vec<WatchKey>, cx: &mut Context<Self>) {
        if self.batched_polling_is_quiet() {
            self.ask_individually(keys, cx);
        } else {
            self.start_batch(keys, cx);
        }
    }

    fn batched_polling_is_quiet(&self) -> bool {
        self.graphql_quiet_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn is_rate_limited(&self) -> bool {
        self.rate_limited_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn ask_individually(&mut self, keys: Vec<WatchKey>, cx: &mut Context<Self>) {
        for key in keys {
            if self.fetches_in_flight() >= MAX_FETCHES_IN_FLIGHT {
                break;
            }
            self.refresh_branch(key, cx);
        }
    }

    fn arm_batch(&mut self, cx: &mut Context<Self>) {
        if self.batch_arm_task.is_some() {
            return;
        }
        self.batch_arm_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BATCH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.batch_arm_task = None;
                this.refresh_due(cx);
            })
            .ok();
        }));
    }

    fn start_batch(&mut self, keys: Vec<WatchKey>, cx: &mut Context<Self>) {
        if keys.is_empty() || self.batch_task.is_some() || self.is_rate_limited() {
            return;
        }
        let now = Instant::now();
        for key in &keys {
            if let Some(watched) = self.watched.get_mut(key) {
                watched.last_polled = Some(now);
                watched.in_batch = true;
            }
        }
        self.record_request();
        let known_repos = self.repo_ids.clone();
        let unresolvable = self
            .unresolvable_repos
            .iter()
            .filter(|(_, retry_at)| now < **retry_at)
            .map(|(repo_path, _)| repo_path.clone())
            .collect();
        self.batch_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let outcome = fetch_batch(keys, known_repos, unresolvable, &executor).await;
            this.update(cx, |this, cx| this.finish_batch(outcome, cx))
                .ok();
        }));
    }

    fn finish_batch(&mut self, outcome: BatchOutcome, cx: &mut Context<Self>) {
        self.batch_task = None;
        for key in &outcome.asked_about {
            if let Some(watched) = self.watched.get_mut(key) {
                watched.in_batch = false;
            }
        }
        // Each resolution attempt was its own `gh repo view` request.
        for (repo_path, repo) in outcome.resolved_repos {
            self.record_request();
            self.unresolvable_repos.remove(&repo_path);
            self.repo_ids.insert(repo_path, repo);
        }
        let retry_at = Instant::now() + REPO_ID_RETRY;
        for repo_path in outcome.unresolvable_repos {
            self.record_request();
            self.unresolvable_repos.insert(repo_path, retry_at);
        }
        if let Some(rate_limit) = &outcome.rate_limit {
            if let Some(reset_at) = &rate_limit.reset_at
                && let Some(reset_in) = parse_reset_at(reset_at).log_err()
            {
                self.rate_limit_reset = Some(Instant::now() + reset_in);
            }
            self.last_rate_limit = Some(rate_limit.clone());
        }
        for error in &outcome.errors {
            log::warn!("gh_status: GitHub reported on the batched query: {error}");
        }

        if let Some(error) = outcome.query_error {
            self.fail_batch(error, outcome.asked_about, cx);
            return;
        }

        self.graphql_failures = 0;
        self.graphql_fallback_logged = false;
        let mut ask_again = Vec::new();
        for (key, answer) in outcome.answers {
            match answer {
                Answer::Prs(prs) => {
                    let prs = prs.into_iter().map(PrStatus::from_gh).collect();
                    self.finish_refresh(&key, Ok(prs), cx);
                }
                Answer::Missing => {
                    let subject = key.subject.describe();
                    self.finish_refresh(
                        &key,
                        Err(anyhow::anyhow!("GitHub knows no {subject}")),
                        cx,
                    );
                }
                Answer::Unanswered(why) => {
                    log::warn!(
                        "gh_status: the batched query said nothing about {}: {why}; asking on \
                         its own",
                        key.subject.describe()
                    );
                    ask_again.push(key);
                }
            }
        }
        self.ask_individually(ask_again, cx);
    }

    /// A refusal is the answer for every subject the query carried, so it is not retried per
    /// subject; any other failure falls back to the per-subject path for this poll.
    fn fail_batch(
        &mut self,
        error: anyhow::Error,
        asked_about: Vec<WatchKey>,
        cx: &mut Context<Self>,
    ) {
        let message = format!("{error:#}");
        if is_rate_limit_error(&message) {
            for key in asked_about {
                self.finish_refresh(&key, Err(anyhow::anyhow!(message.clone())), cx);
            }
            return;
        }

        self.graphql_failures += 1;
        if !self.graphql_fallback_logged {
            self.graphql_fallback_logged = true;
            log::warn!(
                "gh_status: the batched query failed ({message}); asking per branch for this \
                 poll"
            );
        }
        if self.graphql_failures >= GRAPHQL_FAILURES_BEFORE_BACKOFF {
            self.graphql_quiet_until = Some(Instant::now() + GRAPHQL_BACKOFF);
            log::warn!(
                "gh_status: the batched query has failed {} times running; leaving it alone for \
                 {} minutes",
                self.graphql_failures,
                GRAPHQL_BACKOFF.as_secs() / 60
            );
        }
        self.ask_individually(asked_about, cx);
    }

    fn fetches_in_flight(&self) -> usize {
        self.watched
            .values()
            .filter(|watched| watched.refresh_task.is_some())
            .count()
    }

    fn refresh_branch(&mut self, key: WatchKey, cx: &mut Context<Self>) {
        if self.is_rate_limited()
            || self
                .watched
                .get(&key)
                .is_none_or(|watched| watched.is_refreshing())
        {
            return;
        }
        self.record_request();
        let Some(watched) = self.watched.get_mut(&key) else {
            return;
        };
        watched.last_polled = Some(Instant::now());
        watched.refresh_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = fetch_prs(&key.repo_path, &key.subject, &executor).await;
            this.update(cx, |this, cx| this.finish_refresh(&key, result, cx))
                .ok();
        }));
    }

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

    /// GitHub's own figures once a batched answer has arrived; this app's request count before.
    fn budget_line(&self) -> String {
        match &self.last_rate_limit {
            Some(rate_limit) => format!(
                "quiet-ui perf: the last batched poll cost {} of GitHub's hourly budget, with \
                 {} left",
                rate_limit.cost, rate_limit.remaining
            ),
            None => format!(
                "quiet-ui perf: this app made {} GitHub requests in the last hour",
                self.requests_this_hour()
            ),
        }
    }

    fn requests_this_hour(&self) -> usize {
        let now = Instant::now();
        self.requests_this_hour
            .iter()
            .filter(|started| now.duration_since(**started) <= RATE_LIMIT_WINDOW)
            .count()
    }

    /// `gh api rate_limit` costs nothing against the budget, so it is safe to ask once it is spent.
    fn ask_when_the_budget_returns(&mut self, cx: &mut Context<Self>) {
        self._reset_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let Some(seconds) = fetch_rate_limit_reset_in(&executor).await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                // Releasing on the exact tick races GitHub's clock, and losing costs a window.
                let reset = Instant::now() + seconds + Duration::from_secs(1);
                this.rate_limit_reset = Some(reset);
                if this.is_rate_limited() {
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
        watched.in_batch = false;
        match result {
            Ok(prs) => {
                let changed = watched.prs.as_ref() != Some(&prs);
                watched.prs = Some(prs);
                self.rate_limited_until = None;
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if is_rate_limit_error(&message) {
                    // Every subject shares the budget, so say it once per window and hold.
                    let already_holding = self.is_rate_limited();
                    let until = self
                        .rate_limit_reset
                        .filter(|reset| Instant::now() < *reset)
                        .unwrap_or_else(|| Instant::now() + RATE_LIMIT_BACKOFF);
                    if !already_holding {
                        log::warn!(
                            "gh_status: GitHub's API budget is spent; not asking again for {} \
                             minutes. {}",
                            until.saturating_duration_since(Instant::now()).as_secs() / 60,
                            self.budget_line()
                        );
                        self.ask_when_the_budget_returns(cx);
                    }
                    self.rate_limited_until =
                        Some(until.max(self.rate_limited_until.unwrap_or_else(|| Instant::now())));
                } else {
                    log::warn!(
                        "gh_status: failed to fetch PRs for {}: {message}",
                        key.subject.describe()
                    );
                }
            }
        }
    }
}

/// Every PR chip a thread shows, on its sidebar row and in the thread: live data for its branches
/// and its watched PRs, else the snapshot persisted while it was live (an archived thread's
/// worktree is gone), else an inert "no PR" chip so the surface keeps its shape.
pub fn thread_pr_chips<'a>(
    branches: impl IntoIterator<Item = (&'a Path, &'a str)>,
    watched: &[PrStatus],
    dismissed: &[(Option<String>, u64)],
    store: Option<&GhStatusStore>,
    snapshot: impl FnOnce() -> Option<Vec<PrStatus>>,
) -> Vec<ThreadItemPrChip> {
    let live = branches
        .into_iter()
        .filter_map(|(repo_path, branch)| store?.prs_for_branch(repo_path, branch))
        .flatten()
        .chain(watched)
        .filter(|pr| !is_dismissed(pr, dismissed))
        .cloned()
        .collect::<Vec<_>>();
    let prs = if live.is_empty() {
        snapshot()
            .unwrap_or_default()
            .into_iter()
            .filter(|pr| !is_dismissed(pr, dismissed))
            .collect()
    } else {
        live
    };
    if prs.is_empty() {
        return vec![no_pr_chip()];
    }
    let mut seen_urls: HashSet<SharedString> = HashSet::default();
    prs.iter()
        .filter(|pr| seen_urls.insert(pr.url.clone()))
        .map(pr_chip)
        .collect()
}

/// A dismissal mined from a branch names no repository and matches on the number alone.
fn is_dismissed(pr: &PrStatus, dismissed: &[(Option<String>, u64)]) -> bool {
    dismissed.iter().any(|(repo, number)| {
        *number == pr.number
            && repo
                .as_ref()
                .is_none_or(|repo| pr.url.contains(&format!("/{repo}/")))
    })
}

/// `None` when no branch has been fetched yet: only a fetched branch with no PR should overwrite a
/// persisted snapshot.
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

fn no_pr_chip() -> ThreadItemPrChip {
    ThreadItemPrChip {
        label: "no PR".into(),
        state_icon: IconName::PullRequest,
        state_color: Color::Muted,
        checks: None,
        check_counts: None,
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
        ChecksState::Pending => (
            Some(ChecksGlyph::running(IconName::LoadCircle, Color::Warning)),
            "checks running",
        ),
        ChecksState::None => (None, "no checks"),
        ChecksState::Unknown => (None, ""),
    };
    let review_label = match pr.review {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes requested",
        ReviewState::ReviewRequired => "review required",
        ReviewState::None => "no review",
    };
    let merge = match (pr.state, pr.merge) {
        (PrState::Merged | PrState::Closed, _) => MergeState::Unknown,
        // GitHub reports `BLOCKED` for any draft under branch protection, which restates the draft
        // state the chip already shows. A conflict or a stale base still counts.
        (PrState::Draft, MergeState::Blocked) => MergeState::Unknown,
        (_, merge) => merge,
    };
    let blocked_reason = merge.blocked_reason();
    // Passing checks on a PR that cannot merge must not read as green.
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
    let failing = pr.checks == ChecksState::Failing;
    // A merged pull request's CI is history, and an unknown rollup has nothing to count.
    let check_counts = (pr.state != PrState::Merged
        && pr.checks != ChecksState::Unknown
        && pr.check_counts.total() > 0)
        .then(|| PrCheckCounts {
            failed: pr.check_counts.failed,
            running: pr.check_counts.running,
            passed: pr.check_counts.passed,
            skipped: pr.check_counts.skipped,
        });
    ThreadItemPrChip {
        label: SharedString::from(format!("#{}", pr.number)),
        state_icon: IconName::PullRequest,
        state_color,
        checks,
        check_counts,
        url: Some(pr.url.clone()),
        tooltip: SharedString::from(format!("{} ({summary})", pr.title)),
        detail: Some(PrChipDetail {
            title: pr.title.clone(),
            number: pr.number,
            state: state_label.into(),
            state_color,
            checks: checks_label.into(),
            checks_icon: checks,
            check_counts,
            review: review_label.into(),
            failing_checks: if failing {
                pr.failing_checks.clone()
            } else {
                Vec::new()
            },
            extra_failing_checks: if failing { pr.extra_failing_checks } else { 0 },
            merge_blocker: blocked_reason.map(SharedString::from),
        }),
    }
}

struct GlobalGhStatusStore(Entity<GhStatusStore>);

impl Global for GlobalGhStatusStore {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WatchKey {
    /// Where `gh` runs. For a PR in another repository, `subject` names the repository.
    repo_path: PathBuf,
    subject: WatchSubject,
}

impl WatchKey {
    fn branch(repo_path: &Path, branch: &str) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
            subject: WatchSubject::Branch(branch.to_string()),
        }
    }

    fn number(repo_path: &Path, number: u64, repo: Option<&str>) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
            subject: WatchSubject::Number {
                number,
                repo: repo.map(str::to_string),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WatchSubject {
    Branch(String),
    Number {
        number: u64,
        /// `owner/name`; without it the number is read against `repo_path`'s repository.
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
    /// The batch holds one task for all its subjects, so this marks the ones it speaks for.
    in_batch: bool,
    last_polled: Option<Instant>,
}

impl WatchedBranch {
    fn is_refreshing(&self) -> bool {
        self.refresh_task.is_some() || self.in_batch
    }

    fn has_checks_in_flight(&self) -> bool {
        self.prs.as_ref().is_some_and(|prs| {
            prs.iter().any(|pr| {
                matches!(pr.state, PrState::Open | PrState::Draft)
                    && pr.checks == ChecksState::Pending
            })
        })
    }

    /// Every PR merged or closed: nothing left that could change.
    fn is_settled(&self) -> bool {
        self.prs.as_ref().is_some_and(|prs| {
            !prs.is_empty()
                && prs
                    .iter()
                    .all(|pr| matches!(pr.state, PrState::Merged | PrState::Closed))
        })
    }

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

/// Every poll asks for the rollup: it is the only CI signal, and leaving it out of polls that
/// looked settled froze a green chip across a push that broke the build.
const GH_JSON_FIELDS: &str = "number,url,title,state,isDraft,reviewDecision,\
    mergeable,mergeStateStatus,statusCheckRollup";

/// GitHub says "exceeded" on the first refusal and "already exceeded" after; both match.
fn is_rate_limit_error(error: &str) -> bool {
    error.contains("API rate limit")
}

async fn fetch_rate_limit_reset_in(executor: &BackgroundExecutor) -> Result<Duration> {
    let mut command = util::command::new_command("gh");
    command.args(["api", "rate_limit", "--jq", ".rate.reset"]);
    let stdout = run_gh(command, "gh api rate_limit", executor).await?;
    let reset = stdout
        .trim()
        .parse::<i64>()
        .with_context(|| format!("unreadable rate limit reset: {:?}", stdout.trim()))?;
    reset_in(reset)
}

/// Stdin is closed so a credential prompt cannot hang, and the child is killed on drop or
/// timeout, after which the subject is refreshable again.
async fn run_gh(
    mut command: util::command::Command,
    what: &str,
    executor: &BackgroundExecutor,
) -> Result<String> {
    use util::command::Stdio;

    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run gh; is the GitHub CLI installed?")?;

    let output = futures::select_biased! {
        output = child.output().fuse() => {
            output.with_context(|| format!("failed to run {what}"))?
        }
        _ = executor.timer(GH_TIMEOUT).fuse() => {
            bail!("{what} timed out after {} seconds", GH_TIMEOUT.as_secs());
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{what} exited with {}: {}", output.status, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_reset_at(reset_at: &str) -> Result<Duration> {
    let reset = chrono::DateTime::parse_from_rfc3339(reset_at.trim())
        .with_context(|| format!("failed to read GitHub's resetAt {reset_at:?}"))?;
    reset_in(reset.timestamp())
}

/// A window already turned over is no wait; one longer than the window itself is a clock
/// disagreeing, where guessing beats holding for a day.
fn reset_in(reset_timestamp: i64) -> Result<Duration> {
    let seconds = (reset_timestamp - chrono::Utc::now().timestamp()).max(0);
    let reset_in = Duration::from_secs(seconds as u64);
    if reset_in > RATE_LIMIT_WINDOW {
        bail!("GitHub's budget resets in {reset_in:?}, which is longer than its own window");
    }
    Ok(reset_in)
}

struct BatchOutcome {
    asked_about: Vec<WatchKey>,
    answers: Vec<(WatchKey, Answer)>,
    resolved_repos: Vec<(PathBuf, RepoId)>,
    unresolvable_repos: Vec<PathBuf>,
    rate_limit: Option<graphql::RateLimit>,
    errors: Vec<String>,
    /// The query itself did not come back, so nothing was answered.
    query_error: Option<anyhow::Error>,
}

/// A subject whose repository cannot be named comes back unanswered rather than holding up the
/// rest of the batch.
async fn fetch_batch(
    asked_about: Vec<WatchKey>,
    known_repos: HashMap<PathBuf, RepoId>,
    unresolvable: HashSet<PathBuf>,
    executor: &BackgroundExecutor,
) -> BatchOutcome {
    let mut repos = known_repos;
    let mut resolved_repos = Vec::new();
    // So subjects sharing an unnameable checkout do not each spend a request rediscovering it.
    let mut unresolvable_repos: Vec<PathBuf> = Vec::new();
    let mut answers = Vec::new();
    let mut asks: Vec<(RepoId, String, Ask)> = Vec::new();
    let mut alias_keys: Vec<(String, WatchKey)> = Vec::new();

    for (index, key) in asked_about.iter().enumerate() {
        let repo = match &key.subject {
            WatchSubject::Number {
                repo: Some(repo), ..
            } => match RepoId::parse(repo) {
                Ok(repo) => repo,
                Err(error) => {
                    answers.push((key.clone(), Answer::Unanswered(format!("{error:#}"))));
                    continue;
                }
            },
            _ => match repos.get(&key.repo_path) {
                Some(repo) => repo.clone(),
                None if unresolvable.contains(&key.repo_path)
                    || unresolvable_repos.contains(&key.repo_path) =>
                {
                    answers.push((
                        key.clone(),
                        Answer::Unanswered(format!(
                            "{} has no repository gh will name",
                            key.repo_path.display()
                        )),
                    ));
                    continue;
                }
                None => match fetch_repo_id(&key.repo_path, executor).await {
                    Ok(repo) => {
                        repos.insert(key.repo_path.clone(), repo.clone());
                        resolved_repos.push((key.repo_path.clone(), repo.clone()));
                        repo
                    }
                    Err(error) => {
                        unresolvable_repos.push(key.repo_path.clone());
                        answers.push((key.clone(), Answer::Unanswered(format!("{error:#}"))));
                        continue;
                    }
                },
            },
        };
        let ask = match &key.subject {
            WatchSubject::Branch(branch) => Ask::Branch(branch.clone()),
            WatchSubject::Number { number, .. } => Ask::Number(*number),
        };
        let alias = ask.alias(index);
        alias_keys.push((alias.clone(), key.clone()));
        asks.push((repo, alias, ask));
    }

    let outcome = |answers, rate_limit, errors, query_error| BatchOutcome {
        asked_about: asked_about.clone(),
        answers,
        resolved_repos: resolved_repos.clone(),
        unresolvable_repos: unresolvable_repos.clone(),
        rate_limit,
        errors,
        query_error,
    };
    if asks.is_empty() {
        return outcome(answers, None, Vec::new(), None);
    }

    let query = graphql::build_query(&asks);
    let mut command = util::command::new_command("gh");
    command
        .args(["api", "graphql", "-f"])
        .arg(format!("query={query}"));
    // Still reads this checkout's own `gh` configuration, as every other invocation does.
    if let Some(key) = asked_about.first() {
        command.current_dir(&key.repo_path);
    }
    let stdout = match run_gh(command, "gh api graphql", executor).await {
        Ok(stdout) => stdout,
        Err(error) => return outcome(answers, None, Vec::new(), Some(error)),
    };
    let mut batch = match graphql::parse_batch(&stdout, &asks) {
        Ok(batch) => batch,
        Err(error) => return outcome(answers, None, Vec::new(), Some(error)),
    };
    for (alias, key) in alias_keys {
        let answer = batch
            .answers
            .remove(&alias)
            .unwrap_or_else(|| Answer::Unanswered(format!("alias {alias} was not in the answer")));
        answers.push((key, answer));
    }
    outcome(answers, batch.rate_limit, batch.errors, None)
}

/// Asks `gh` rather than parsing remote URLs, so this agrees with every other `gh` call here.
async fn fetch_repo_id(repo_path: &Path, executor: &BackgroundExecutor) -> Result<RepoId> {
    let mut command = util::command::new_command("gh");
    command
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .current_dir(repo_path);
    let stdout = run_gh(command, "gh repo view", executor).await?;
    RepoId::parse(&stdout)
}

async fn fetch_prs(
    repo_path: &Path,
    subject: &WatchSubject,
    executor: &BackgroundExecutor,
) -> Result<Vec<PrStatus>> {
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
    command.arg(GH_JSON_FIELDS).current_dir(repo_path);
    let stdout = run_gh(command, "gh", executor).await?;
    match subject {
        WatchSubject::Branch(_) => parse_pr_list(&stdout),
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

/// One PR as `gh pr list --json` reports it, and what the GraphQL answer is lowered to.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhPr {
    pub(crate) number: u64,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) title: String,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) is_draft: bool,
    #[serde(default)]
    pub(crate) review_decision: Option<String>,
    #[serde(default)]
    pub(crate) status_check_rollup: Option<Vec<GhCheck>>,
    #[serde(default)]
    pub(crate) mergeable: Option<String>,
    #[serde(default)]
    pub(crate) merge_state_status: Option<String>,
    /// Only the batched GraphQL query asks for these; `gh pr list` has no such field.
    #[serde(default, skip)]
    pub(crate) check_counts: Option<CheckCounts>,
}

/// A commit status reports `state`; a check run reports `conclusion` once complete.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhCheck {
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) conclusion: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) workflow_name: Option<String>,
    #[serde(default)]
    pub(crate) context: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckOutcome {
    Pending,
    Passing,
    Failing,
}

impl GhCheck {
    /// Only an explicitly good value passes and only an explicitly bad one fails; anything else,
    /// including values GitHub adds later, is pending rather than falling through to green.
    fn outcome(&self) -> CheckOutcome {
        if let Some(state) = self.state.as_deref().filter(|s| !s.is_empty()) {
            return match state {
                "SUCCESS" => CheckOutcome::Passing,
                "FAILURE" | "ERROR" => CheckOutcome::Failing,
                _ => CheckOutcome::Pending,
            };
        }
        match self.conclusion.as_deref().filter(|c| !c.is_empty()) {
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED" | "STALE") => CheckOutcome::Passing,
            Some(
                "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                | "STARTUP_FAILURE",
            ) => CheckOutcome::Failing,
            _ => CheckOutcome::Pending,
        }
    }

    /// `workflow / job`, so `test / clippy` and `build / clippy` stay apart.
    fn label(&self) -> Option<String> {
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            return Some(
                match self.workflow_name.as_deref().filter(|w| !w.is_empty()) {
                    Some(workflow) if workflow != name => format!("{workflow} / {name}"),
                    _ => name.to_string(),
                },
            );
        }
        self.context
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(str::to_string)
    }
}

impl PrStatus {
    fn from_gh(pr: GhPr) -> Self {
        // A null or absent rollup (GitHub withholding it, or an old `gh`) is not a PR without CI.
        let rollup = pr.status_check_rollup.as_deref();
        let (failing_checks, extra_failing_checks) = failing_check_names(rollup.unwrap_or(&[]));
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
            merge: merge_state(pr.mergeable.as_deref(), pr.merge_state_status.as_deref()),
            check_counts: pr
                .check_counts
                .filter(|counts| !counts.is_empty())
                .unwrap_or_else(|| counts_from_checks(rollup.unwrap_or(&[]))),
        }
    }
}

fn merge_state(mergeable: Option<&str>, status: Option<&str>) -> MergeState {
    match (mergeable, status) {
        (Some("CONFLICTING"), _) | (_, Some("DIRTY")) => MergeState::Conflicting,
        (_, Some("BEHIND")) => MergeState::Behind,
        (_, Some("BLOCKED")) => MergeState::Blocked,
        // `UNSTABLE` and `DRAFT` restate the checks and the PR state, which the chip already shows.
        (Some("MERGEABLE"), _) => MergeState::Mergeable,
        _ => MergeState::Unknown,
    }
}

/// A failing check with no name is counted in neither: there is no line to draw for it.
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

/// A failure outranks a run still going, which outranks the rest passing.
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
            check_counts: CheckCounts::default(),
        }
    }

    #[test]
    fn a_state_is_only_finished_when_github_says_so() {
        let counts = CheckCounts::from_states([
            ("SUCCESS", 3),
            ("NEUTRAL", 1),
            ("STALE", 1),
            ("SKIPPED", 4),
            ("FAILURE", 1),
            ("ERROR", 1),
            ("TIMED_OUT", 1),
            ("STARTUP_FAILURE", 1),
            ("ACTION_REQUIRED", 1),
            ("CANCELLED", 1),
            ("QUEUED", 2),
            ("IN_PROGRESS", 1),
            ("PENDING", 1),
            ("WAITING", 1),
            ("REQUESTED", 1),
            ("EXPECTED", 1),
            // A state GitHub adds later must not read as green.
            ("SOMETHING_NEW", 1),
        ]);
        assert_eq!(
            counts,
            CheckCounts {
                failed: 6,
                running: 8,
                passed: 5,
                skipped: 4,
            }
        );
        assert_eq!(counts.total(), 23);
    }

    #[test]
    fn the_hover_cards_breakdown_names_only_what_there_is() {
        assert_eq!(
            PrCheckCounts {
                failed: 1,
                running: 10,
                passed: 23,
                skipped: 25,
            }
            .breakdown(),
            "1 failed, 10 running, 23 passed, 25 skipped"
        );
        assert_eq!(
            PrCheckCounts {
                passed: 2,
                ..Default::default()
            }
            .breakdown(),
            "2 passed"
        );
        assert_eq!(PrCheckCounts::default().breakdown(), "");
    }

    /// A chip says how much is left only while something is left to say.
    #[test]
    fn a_finished_green_pull_request_keeps_its_single_glyph() {
        let mut green = pr(1, "https://github.com/org/repo/pull/1");
        green.check_counts = CheckCounts {
            passed: 7,
            ..Default::default()
        };
        let chip = pr_chip(&green);
        assert_eq!(chip.checks, Some(passing_glyph()));
        assert_eq!(
            chip.check_counts.map(|counts| (counts.failed, counts.running)),
            Some((0, 0)),
            "the counts ride along for the hover card but draw no glyph of their own"
        );

        let mut busy = pr(2, "https://github.com/org/repo/pull/2");
        busy.checks = ChecksState::Failing;
        busy.check_counts = CheckCounts {
            failed: 1,
            running: 10,
            passed: 3,
            skipped: 0,
        };
        let chip = pr_chip(&busy);
        assert_eq!(
            chip.check_counts.map(|counts| (counts.failed, counts.running)),
            Some((1, 10))
        );

        let mut merged = pr(3, "https://github.com/org/repo/pull/3");
        merged.state = PrState::Merged;
        merged.check_counts = CheckCounts {
            passed: 7,
            ..Default::default()
        };
        assert_eq!(
            pr_chip(&merged).check_counts,
            None,
            "a merged pull request's CI is history"
        );
    }

    fn passing_glyph() -> ChecksGlyph {
        ChecksGlyph::settled(IconName::Check, Color::Success)
    }

    fn parse_one(rollup: &str) -> PrStatus {
        let json = format!(
            r#"[{{"number": 1, "url": "u", "title": "t", "state": "OPEN", "statusCheckRollup": [{rollup}]}}]"#
        );
        parse_pr_list(&json).unwrap().remove(0)
    }

    #[test]
    fn a_thread_gets_the_same_answer_wherever_it_is_asked() {
        let mut merged = pr(10461, "https://github.com/org/repo/pull/10461");
        merged.state = PrState::Merged;
        let branch = Path::new("/repo");

        // No live data, so the persisted snapshot stands in.
        let chips = thread_pr_chips([(branch, "feature")], &[], &[], None, || {
            Some(vec![merged.clone()])
        });
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].url.as_deref(), Some(merged.url.as_ref()));

        let chips = thread_pr_chips([(branch, "feature")], &[], &[], None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none());

        let chips = thread_pr_chips([], &[], &[], None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none());
    }

    #[gpui::test]
    fn chips_cover_only_the_given_branches_once_each(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|cx| {
            let mut store = GhStatusStore::new(cx);
            for (path, branch, prs) in [
                ("/a", "mine", vec![pr(1, "u1"), pr(2, "u2")]),
                ("/b", "mine", vec![pr(1, "u1")]),
                ("/c", "other", vec![pr(3, "u3")]),
            ] {
                store
                    .watched
                    .entry(WatchKey::branch(Path::new(path), branch))
                    .or_default()
                    .prs = Some(prs);
            }
            store
        });
        store.read_with(cx, |store, _| {
            let chips = thread_pr_chips(
                [(Path::new("/a"), "mine"), (Path::new("/b"), "mine")],
                &[],
                &[],
                Some(store),
                || None,
            );
            let labels = chips
                .iter()
                .map(|chip| chip.label.to_string())
                .collect::<Vec<_>>();
            assert_eq!(labels, vec!["#1", "#2"]);

            let chips =
                thread_pr_chips([(Path::new("/d"), "fresh")], &[], &[], Some(store), || None);
            assert_eq!(chips.len(), 1);
            assert_eq!(chips[0].label, SharedString::from("no PR"));
        });
    }

    #[test]
    fn a_watched_pr_shows_without_a_branch_and_a_dismissed_one_does_not() {
        let watched = pr(412, "https://github.com/owner/name/pull/412");
        let chips_with = |dismissed: &[(Option<String>, u64)]| {
            thread_pr_chips([], std::slice::from_ref(&watched), dismissed, None, || None)
        };

        let chips = chips_with(&[]);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].url.as_deref(), Some(watched.url.as_ref()));

        let chips = chips_with(&[(Some("owner/name".to_string()), 412)]);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none(), "a dismissed PR leaves no badge");

        let chips = chips_with(&[(Some("other/repo".to_string()), 412)]);
        assert_eq!(chips[0].url.as_deref(), Some(watched.url.as_ref()));

        // A dismissal from a branch names no repository.
        assert!(chips_with(&[(None, 412)])[0].url.is_none());
    }

    #[test]
    fn mergeability_not_yet_known_shows_what_it_would_have_shown_without_it() {
        let mut pr = pr(1, "u");
        pr.merge = merge_state(Some("UNKNOWN"), Some("UNKNOWN"));
        assert_eq!(pr.merge, MergeState::Unknown);
        let unknown_chip = pr_chip(&pr);
        pr.merge = MergeState::Mergeable;
        let mergeable_chip = pr_chip(&pr);

        assert_eq!(unknown_chip.checks, Some(passing_glyph()));
        assert_eq!(unknown_chip.checks, mergeable_chip.checks);
        assert_eq!(unknown_chip.tooltip, mergeable_chip.tooltip);
        assert!(
            unknown_chip
                .detail
                .is_some_and(|detail| detail.merge_blocker.is_none())
        );
    }

    #[test]
    fn passing_checks_on_a_pr_that_cannot_merge_are_not_green() {
        for (state, (icon, color), reason) in [
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
            let mut pr = pr(1, "u");
            pr.merge = state;
            let chip = pr_chip(&pr);
            assert_eq!(
                chip.checks,
                Some(ChecksGlyph::settled(icon, color)),
                "{state:?}"
            );
            assert!(chip.tooltip.contains(reason), "{state:?}: {}", chip.tooltip);
            assert_eq!(
                chip.detail.unwrap().merge_blocker.as_deref(),
                Some(reason),
                "{state:?}"
            );
        }
    }

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

        // `mergeable` reports a conflict whatever the draft state.
        let mut conflicting = pr;
        conflicting.merge = MergeState::Conflicting;
        assert_eq!(
            pr_chip(&conflicting)
                .detail
                .unwrap()
                .merge_blocker
                .as_deref(),
            Some("conflicts with base branch")
        );
    }

    #[test]
    fn a_settled_pr_says_nothing_about_merging() {
        for state in [PrState::Merged, PrState::Closed] {
            let mut pr = pr(1, "u");
            pr.state = state;
            pr.merge = MergeState::Behind;
            let chip = pr_chip(&pr);
            assert_eq!(chip.detail.unwrap().merge_blocker, None, "{state:?}");
            assert!(!chip.tooltip.contains("behind"), "{state:?}");
        }
    }

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
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "BEHIND",
            "statusCheckRollup": []
        }]"#;
        assert_eq!(parse_pr_list(json).unwrap()[0].merge, MergeState::Behind);

        assert_eq!(
            merge_state(Some("CONFLICTING"), Some("UNKNOWN")),
            MergeState::Conflicting
        );
        assert_eq!(merge_state(None, Some("DIRTY")), MergeState::Conflicting);
        assert_eq!(
            merge_state(Some("MERGEABLE"), Some("BLOCKED")),
            MergeState::Blocked
        );
        assert_eq!(
            merge_state(Some("MERGEABLE"), Some("UNSTABLE")),
            MergeState::Mergeable
        );
        assert_eq!(
            merge_state(Some("MERGEABLE"), Some("CLEAN")),
            MergeState::Mergeable
        );
        assert_eq!(merge_state(None, None), MergeState::Unknown);
    }

    /// A refresh killed by the timeout must leave the branch refreshable, and a failure is not an
    /// answer, so the chip keeps what it last knew.
    #[gpui::test]
    fn a_failed_refresh_leaves_the_branch_refreshable(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        let key = WatchKey::branch(Path::new("/repo"), "main");
        store.update(cx, |store, cx| {
            let mut pending = pr(1, "u");
            pending.checks = ChecksState::Pending;
            let watched = store.watched.entry(key.clone()).or_default();
            watched.watch_count = 1;
            watched.prs = Some(vec![pending.clone()]);
            watched.refresh_task = Some(cx.spawn(async move |_, _| {}));

            store.finish_refresh(
                &key,
                Err(anyhow::anyhow!("gh timed out after 20 seconds")),
                cx,
            );

            assert!(!store.watched[&key].is_refreshing());
            assert_eq!(store.watched[&key].prs.as_deref(), Some(&[pending][..]));
        });
    }

    #[gpui::test]
    fn a_poll_tick_asks_one_question_about_everything_due(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let over_the_cap = 10;
            watch_numbers(store, (MAX_SUBJECTS_PER_BATCH + over_the_cap) as u64);

            store.refresh_due(cx);
            assert_eq!(store.requests_this_hour(), 1);
            assert!(store.batch_task.is_some());
            assert_eq!(store.fetches_in_flight(), 0);
            let first = in_batch(store);
            assert_eq!(first.len(), MAX_SUBJECTS_PER_BATCH);

            // Nothing piles on while that query is in flight.
            store.refresh_due(cx);
            assert_eq!(store.requests_this_hour(), 1);

            store.batch_task = None;
            for watched in store.watched.values_mut() {
                watched.in_batch = false;
            }
            store.refresh_due(cx);
            assert_eq!(store.requests_this_hour(), 2);
            let second = in_batch(store);
            assert_eq!(second.len(), over_the_cap);
            assert!(
                second.iter().all(|key| !first.contains(key)),
                "the subjects already asked about do not go again before the rest go once"
            );
        });
    }

    #[gpui::test]
    fn a_window_opening_arms_one_question_rather_than_one_per_thread(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            for n in 0..20u64 {
                store.watch_pr(PathBuf::from("/repo"), n, None, cx);
            }
            assert_eq!(store.requests_this_hour(), 0);
            assert!(store.batch_arm_task.is_some());

            store.batch_arm_task = None;
            store.refresh_due(cx);
            assert_eq!(store.requests_this_hour(), 1);
            assert_eq!(in_batch(store).len(), 20);
        });
    }

    #[gpui::test]
    fn a_batched_query_that_fails_falls_back_to_asking_one_at_a_time(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let keys = watch_numbers(store, 6);
            store.refresh_due(cx);
            assert_eq!(in_batch(store).len(), 6);

            store.finish_batch(
                failed_batch(&keys, "gh api graphql exited with 1: nope"),
                cx,
            );
            assert!(in_batch(store).is_empty());
            assert_eq!(store.fetches_in_flight(), MAX_FETCHES_IN_FLIGHT);
            assert_eq!(store.requests_this_hour(), 1 + MAX_FETCHES_IN_FLIGHT);
            assert!(!store.batched_polling_is_quiet());

            for _ in 1..GRAPHQL_FAILURES_BEFORE_BACKOFF {
                store.finish_batch(
                    failed_batch(&keys, "gh api graphql exited with 1: nope"),
                    cx,
                );
            }
            assert!(store.batched_polling_is_quiet());

            for watched in store.watched.values_mut() {
                watched.refresh_task = None;
                watched.last_polled = None;
            }
            store.refresh_due(cx);
            assert!(store.batch_task.is_none());
            assert_eq!(store.fetches_in_flight(), MAX_FETCHES_IN_FLIGHT);
        });
    }

    #[gpui::test]
    fn a_refused_batch_does_not_become_a_refusal_each(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let keys = watch_numbers(store, 6);
            store.refresh_due(cx);

            store.finish_batch(
                failed_batch(&keys, "API rate limit exceeded for user ID 1"),
                cx,
            );
            assert!(store.is_rate_limited());
            assert_eq!(store.fetches_in_flight(), 0);
            assert_eq!(store.graphql_failures, 0);
        });
    }

    #[gpui::test]
    fn only_the_subjects_the_answer_missed_are_asked_again(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let keys = watch_numbers(store, 3);
            store.refresh_due(cx);

            store.finish_batch(
                BatchOutcome {
                    asked_about: keys.clone(),
                    answers: vec![
                        (keys[0].clone(), Answer::Prs(Vec::new())),
                        (keys[1].clone(), Answer::Missing),
                        (
                            keys[2].clone(),
                            Answer::Unanswered("alias p2 was not in the answer".into()),
                        ),
                    ],
                    resolved_repos: Vec::new(),
                    unresolvable_repos: Vec::new(),
                    rate_limit: Some(graphql::RateLimit {
                        cost: 1,
                        remaining: 4074,
                        reset_at: None,
                    }),
                    errors: Vec::new(),
                    query_error: None,
                },
                cx,
            );

            assert_eq!(store.watched[&keys[0]].prs.as_deref(), Some(&[][..]));
            assert!(store.watched[&keys[1]].prs.is_none());
            assert_eq!(store.fetches_in_flight(), 1);
            assert_eq!(
                store.last_rate_limit.as_ref().map(|limit| limit.cost),
                Some(1)
            );
            assert!(in_batch(store).is_empty());
        });
    }

    #[gpui::test]
    fn a_spent_budget_reports_what_the_last_poll_cost(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, _| {
            assert!(store.budget_line().contains("made 0 GitHub requests"));
            store.last_rate_limit = Some(graphql::RateLimit {
                cost: 3,
                remaining: 4074,
                reset_at: None,
            });
            let line = store.budget_line();
            assert!(line.contains("cost 3"), "{line}");
            assert!(line.contains("4074 left"), "{line}");
        });
    }

    #[test]
    fn the_reset_github_gives_is_read_and_sanity_checked() {
        assert_eq!(
            parse_reset_at("2020-01-01T00:00:00Z").unwrap(),
            Duration::from_secs(0)
        );
        let soon = chrono::Utc::now() + chrono::Duration::minutes(30);
        let read = parse_reset_at(&soon.to_rfc3339()).unwrap();
        assert!(
            read > Duration::from_secs(29 * 60) && read <= Duration::from_secs(30 * 60),
            "{read:?}"
        );
        let far = chrono::Utc::now() + chrono::Duration::days(2);
        assert!(parse_reset_at(&far.to_rfc3339()).is_err());
        assert!(parse_reset_at("soon").is_err());
        assert!(reset_in(far.timestamp()).is_err());
    }

    fn watch_numbers(store: &mut GhStatusStore, count: u64) -> Vec<WatchKey> {
        (0..count)
            .map(|n| {
                let key = WatchKey::number(Path::new("/repo"), n, None);
                store.watched.entry(key.clone()).or_default().watch_count = 1;
                key
            })
            .collect()
    }

    fn in_batch(store: &GhStatusStore) -> Vec<WatchKey> {
        store
            .watched
            .iter()
            .filter(|(_, watched)| watched.in_batch)
            .map(|(key, _)| key.clone())
            .collect()
    }

    fn failed_batch(asked_about: &[WatchKey], error: &str) -> BatchOutcome {
        BatchOutcome {
            asked_about: asked_about.to_vec(),
            answers: Vec::new(),
            resolved_repos: Vec::new(),
            unresolvable_repos: Vec::new(),
            rate_limit: None,
            errors: Vec::new(),
            query_error: Some(anyhow::anyhow!(error.to_string())),
        }
    }

    /// The guard once compared the deadline against `now + backoff`, so every refusal in a flood
    /// logged.
    #[gpui::test]
    fn a_spent_budget_is_reported_once_per_window(cx: &mut gpui::TestAppContext) {
        let store = cx.new(GhStatusStore::new);
        store.update(cx, |store, cx| {
            let keys = watch_numbers(store, 10);
            let refuse =
                |store: &mut GhStatusStore, key: &WatchKey, cx: &mut Context<GhStatusStore>| {
                    store.finish_refresh(
                        key,
                        Err(anyhow::anyhow!("API rate limit exceeded for user ID 1")),
                        cx,
                    );
                };

            refuse(store, &keys[0], cx);
            let first = store.rate_limited_until.expect("the first refusal holds");
            assert!(store._reset_task.is_some());
            store._reset_task = None;

            for key in &keys[1..] {
                refuse(store, key, cx);
            }
            assert!(
                store._reset_task.is_none(),
                "a refusal while holding says nothing"
            );
            assert!(store.rate_limited_until.is_some_and(|until| until >= first));
        });
    }

    #[test]
    fn a_branch_is_polled_as_often_as_it_could_change() {
        let branch = |prs: Option<Vec<PrStatus>>| WatchedBranch {
            watch_count: 1,
            prs,
            ..WatchedBranch::default()
        };
        let one = |state: PrState, checks: ChecksState| {
            let mut pr = pr(1, "u");
            pr.state = state;
            pr.checks = checks;
            branch(Some(vec![pr]))
        };

        assert_eq!(branch(None).poll_interval(), Some(PENDING_POLL_INTERVAL));
        assert_eq!(
            branch(Some(Vec::new())).poll_interval(),
            Some(IDLE_POLL_INTERVAL)
        );
        for (state, checks, interval) in [
            (
                PrState::Open,
                ChecksState::Pending,
                Some(PENDING_POLL_INTERVAL),
            ),
            (
                PrState::Draft,
                ChecksState::Pending,
                Some(PENDING_POLL_INTERVAL),
            ),
            (
                PrState::Open,
                ChecksState::Passing,
                Some(IDLE_POLL_INTERVAL),
            ),
            (
                PrState::Open,
                ChecksState::Failing,
                Some(IDLE_POLL_INTERVAL),
            ),
            (PrState::Merged, ChecksState::Pending, None),
            (PrState::Closed, ChecksState::Failing, None),
        ] {
            assert_eq!(
                one(state, checks).poll_interval(),
                interval,
                "{state:?} {checks:?}"
            );
        }
    }

    #[test]
    fn a_spent_budget_is_told_apart_from_an_ordinary_failure() {
        assert!(is_rate_limit_error(
            "gh exited with exit status: 1: GraphQL: API rate limit exceeded for user ID 5551212."
        ));
        assert!(is_rate_limit_error(
            "gh exited with exit status: 1: GraphQL: API rate limit already exceeded (rateLimit)"
        ));
        assert!(!is_rate_limit_error("gh timed out after 20 seconds"));
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
        let mut expected = pr(10461, "https://github.com/org/repo/pull/10461");
        expected.title = "Fix things".into();
        expected.review = ReviewState::Approved;
        // `gh pr list` carries no tallies, so the two are counted from the list itself.
        expected.check_counts = CheckCounts {
            passed: 2,
            ..Default::default()
        };
        assert_eq!(parse_pr_list(json).unwrap(), vec![expected]);
    }

    #[test]
    fn failing_check_outranks_pending() {
        let pr = parse_one(
            r#"{"status": "IN_PROGRESS", "conclusion": ""},
               {"status": "COMPLETED", "conclusion": "FAILURE"}"#,
        );
        assert_eq!(pr.checks, ChecksState::Failing);
    }

    #[test]
    fn failing_check_names_are_listed_with_their_workflow() {
        let pr = parse_one(
            r#"{"name": "clippy", "workflowName": "ci", "conclusion": "FAILURE"},
               {"name": "clippy", "workflowName": "release", "conclusion": "FAILURE"},
               {"name": "unit", "workflowName": "unit", "conclusion": "FAILURE"},
               {"name": "build", "workflowName": "ci", "conclusion": "SUCCESS"},
               {"context": "dco", "state": "ERROR"}"#,
        );
        assert_eq!(pr.checks, ChecksState::Failing);
        assert_eq!(
            pr.failing_checks,
            vec![
                SharedString::from("ci / clippy"),
                SharedString::from("release / clippy"),
                SharedString::from("unit"),
                SharedString::from("dco"),
            ]
        );
        assert_eq!(pr.extra_failing_checks, 0);
    }

    #[test]
    fn failing_check_names_beyond_the_cap_are_counted() {
        let checks = (0..8)
            .map(|ix| format!(r#"{{"name": "job-{ix}", "conclusion": "FAILURE"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let pr = parse_one(&checks);
        assert_eq!(pr.failing_checks.len(), MAX_LISTED_CHECKS);
        assert_eq!(pr.extra_failing_checks, 8 - MAX_LISTED_CHECKS);
    }

    #[test]
    fn a_failing_check_with_no_name_still_counts_but_lists_nothing() {
        let pr = parse_one(r#"{"conclusion": "FAILURE"}"#);
        assert_eq!(pr.checks, ChecksState::Failing);
        assert!(pr.failing_checks.is_empty());
        assert_eq!(pr.extra_failing_checks, 0);
    }

    /// An empty rollup is a PR without CI; a missing one is nobody having looked.
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

    #[test]
    fn a_merged_pr_is_purple_and_shows_no_checks() {
        let mut merged = pr(5, "https://example.com/5");
        merged.state = PrState::Merged;
        merged.checks = ChecksState::Failing;

        let chip = pr_chip(&merged);
        assert_eq!(chip.state_color, Color::Custom(MERGED_PR_COLOR));
        assert!(chip.checks.is_none());
        let detail = chip.detail.expect("a real PR carries a hover card");
        assert!(detail.checks.is_empty());
        assert!(detail.checks_icon.is_none());
        assert_eq!(chip.tooltip, SharedString::from("PR 5 (merged)"));
    }

    /// Every shape of "CI is running" GitHub reports; the last few used to fall through to green.
    #[test]
    fn every_shape_of_running_ci_gets_the_running_glyph() {
        for check in [
            r#"{"status": "IN_PROGRESS", "conclusion": ""}"#,
            r#"{"status": "QUEUED", "conclusion": ""}"#,
            r#"{"status": "WAITING", "conclusion": ""}"#,
            r#"{"status": "REQUESTED", "conclusion": ""}"#,
            r#"{"status": "PENDING", "conclusion": ""}"#,
            r#"{"state": "PENDING"}"#,
            r#"{"state": "EXPECTED"}"#,
            r#"{}"#,
        ] {
            let pr = parse_one(&format!(r#"{{"conclusion": "SUCCESS"}}, {check}"#));
            assert_eq!(pr.checks, ChecksState::Pending, "{check}");
            let glyph = pr_chip(&pr)
                .checks
                .unwrap_or_else(|| panic!("no glyph at all for {check}"));
            assert!(glyph.spinning, "{check}");
            assert_eq!(glyph.color, Color::Warning, "{check}");
        }
    }

    #[test]
    fn a_run_that_did_not_finish_is_not_a_run_that_passed() {
        for conclusion in [
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            let pr = parse_one(&format!(
                r#"{{"name": "build", "conclusion": "{conclusion}"}}"#
            ));
            assert_eq!(pr.checks, ChecksState::Failing, "{conclusion}");
            assert_eq!(
                pr.failing_checks,
                vec![SharedString::from("build")],
                "{conclusion}"
            );
        }
    }

    #[test]
    fn a_skipped_or_neutral_check_still_passes() {
        for conclusion in ["SUCCESS", "NEUTRAL", "SKIPPED", "STALE"] {
            let pr = parse_one(&format!(r#"{{"conclusion": "{conclusion}"}}"#));
            assert_eq!(pr.checks, ChecksState::Passing, "{conclusion}");
        }
    }

    #[test]
    fn a_pr_viewed_by_number_parses() {
        let json = r#"{
            "number": 64463,
            "url": "https://github.com/zed-industries/zed/pull/64463",
            "title": "Remove Baseten provider",
            "state": "MERGED",
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

        assert!(parse_pr_view("[]").is_err());
        assert!(parse_pr_list("not json").is_err());
        assert_eq!(parse_pr_list("[]").unwrap(), vec![]);
    }
}
