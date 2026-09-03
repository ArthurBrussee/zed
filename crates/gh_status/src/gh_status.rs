//! Tracks GitHub PR and CI status for git branches by polling the `gh` CLI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Hsla, SharedString, Task,
};
use serde::{Deserialize, Serialize};
use ui::{Color, IconName, PrChipDetail, ThreadItemPrChip};

/// How often every watched branch is looked at.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often the branches with checks still running are looked at. A run in
/// progress changes within a minute or two and then stops changing for hours,
/// so it is worth asking about more often than a settled PR is; everything
/// else keeps [`POLL_INTERVAL`].
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Ticks of [`PENDING_POLL_INTERVAL`] between full refreshes, so the poll loop
/// stays one timer and one counter rather than a per-branch state machine.
const PENDING_POLLS_PER_FULL_POLL: u32 = 3;

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
    Pending,
    Passing,
    Failing,
    /// The PR has no status checks.
    None,
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
            .iter()
            .find(|(key, _)| key.repo_path == repo_path && key.branch == branch)
            .and_then(|(_, watched)| watched.prs.as_ref())
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
        let key = WatchKey { repo_path, branch };
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
        let Some((key, watched)) = self
            .watched
            .iter_mut()
            .find(|(key, _)| key.repo_path == repo_path && key.branch == branch)
        else {
            return;
        };
        watched.watch_count = watched.watch_count.saturating_sub(1);
        if watched.watch_count == 0 {
            let key = key.clone();
            self.watched.remove(&key);
            cx.notify();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_prs_for_test(&mut self, repo_path: PathBuf, branch: String, prs: Vec<PrStatus>) {
        let key = WatchKey { repo_path, branch };
        let watched = self.watched.entry(key).or_default();
        watched.prs = Some(prs);
    }

    /// Refresh all watched branches now instead of waiting for the next poll.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        let keys = self.watched.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.refresh_branch(key, cx);
        }
    }

    /// Refresh only the branches whose checks are still running, which is what
    /// makes polling them harder than a settled PR cheap.
    fn refresh_pending(&mut self, cx: &mut Context<Self>) {
        let keys = self
            .watched
            .iter()
            .filter(|(_, watched)| watched.has_checks_in_flight())
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.refresh_branch(key, cx);
        }
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            let mut ticks: u32 = 0;
            loop {
                cx.background_executor().timer(PENDING_POLL_INTERVAL).await;
                ticks = ticks.wrapping_add(1);
                let full_poll = ticks.is_multiple_of(PENDING_POLLS_PER_FULL_POLL);
                let refreshed = this.update(cx, |this, cx| {
                    if full_poll {
                        this.refresh_now(cx)
                    } else {
                        this.refresh_pending(cx)
                    }
                });
                if refreshed.is_err() {
                    break;
                }
            }
        });
        Self {
            watched: HashMap::default(),
            last_error: None,
            _poll_task: poll_task,
        }
    }

    fn refresh_branch(&mut self, key: WatchKey, cx: &mut Context<Self>) {
        let Some(watched) = self.watched.get_mut(&key) else {
            return;
        };
        if watched.refresh_task.is_some() {
            return;
        }
        watched.refresh_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = fetch_prs(&key.repo_path, &key.branch, &executor).await;
            this.update(cx, |this, cx| this.finish_refresh(&key, result, cx))
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
                self.last_error = None;
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                log::warn!(
                    "gh_status: failed to fetch PRs for branch {}: {error:#}",
                    key.branch
                );
                self.last_error = Some(SharedString::from(format!("{error:#}")));
                cx.notify();
            }
        }
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
    let mut prs: Vec<&PrStatus> = Vec::new();
    let mut has_branch = false;

    for (repo_path, branch) in branches {
        has_branch = true;
        let Some(branch_prs) = store.and_then(|store| store.prs_for_branch(repo_path, branch))
        else {
            continue;
        };
        prs.extend(branch_prs);
    }

    let mut chips = pr_chips_for_prs(prs);
    if chips.is_empty() && has_branch {
        chips.push(no_pr_chip());
    }
    chips
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
    store: Option<&GhStatusStore>,
    snapshot: impl FnOnce() -> Option<Vec<PrStatus>>,
) -> Vec<ThreadItemPrChip> {
    let mut chips = pr_chips_for_branches(branches, store);

    if chips.iter().all(|chip| chip.url.is_none())
        && let Some(prs) = snapshot().filter(|prs| !prs.is_empty())
    {
        return pr_chips_for_prs(prs.iter());
    }

    if chips.is_empty() {
        chips.push(no_pr_chip());
    }
    chips
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
        ChecksState::Passing => (Some((IconName::Check, Color::Success)), "checks passing"),
        ChecksState::Failing => (Some((IconName::XCircle, Color::Error)), "checks failing"),
        ChecksState::Pending => (
            Some((IconName::ArrowCircle, Color::Warning)),
            "checks pending",
        ),
        ChecksState::None => (None, "no checks"),
    };
    let review_label = match pr.review {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes requested",
        ReviewState::ReviewRequired => "review required",
        ReviewState::None => "no review",
    };
    // A merged or closed PR is not waiting to merge, so mergeability has
    // nothing to say about it.
    let blocked_reason = match pr.state {
        PrState::Open | PrState::Draft => pr.merge.blocked_reason(),
        PrState::Merged | PrState::Closed => None,
    };
    // Mergeability does not get a glyph of its own — the chip is small and
    // already carries a state colour and a checks glyph. It changes what the
    // checks glyph says instead: passing checks on a PR that cannot merge are
    // not green, because reading them as ready is the exact mistake this is
    // here to stop. The reason goes in the hover card, where there is room for
    // a sentence.
    let checks = match blocked_reason {
        Some(_) if pr.checks == ChecksState::Passing => {
            Some((IconName::Warning, Color::Warning))
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
    repo_path: PathBuf,
    branch: String,
}

#[derive(Default)]
struct WatchedBranch {
    watch_count: usize,
    prs: Option<Vec<PrStatus>>,
    refresh_task: Option<Task<()>>,
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
}

const GH_JSON_FIELDS: &str =
    "number,url,title,state,isDraft,reviewDecision,statusCheckRollup,mergeable,mergeStateStatus";

// The gh CLI selects `statusCheckRollup` as a whole subtree, so requesting
// `--json statusCheckRollup` already returns each check's `name`,
// `workflowName`, `context`, `state`, `status`, and `conclusion`; the additional
// fields the fork needs for the hover card are parsed out in `GhCheck` without
// widening `GH_JSON_FIELDS`.

async fn fetch_prs(
    repo_path: &Path,
    branch: &str,
    executor: &BackgroundExecutor,
) -> Result<Vec<PrStatus>> {
    use util::command::Stdio;

    let child = util::command::new_command("gh")
        .args(["pr", "list", "--head", branch, "--state", "all", "--json"])
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
    parse_pr_list(&String::from_utf8_lossy(&output.stdout))
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
/// report `status` while running and `conclusion` once complete. A check run
/// carries a `name` (its job name) and a `workflowName` (its workflow's own
/// name); a commit status carries a `context` in place of both.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCheck {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workflow_name: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

impl GhCheck {
    fn outcome(&self) -> &str {
        self.state
            .as_deref()
            .or_else(|| self.conclusion.as_deref().filter(|c| !c.is_empty()))
            .or(self.status.as_deref())
            .unwrap_or("")
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
        let checks = pr.status_check_rollup.as_deref().unwrap_or(&[]);
        let (failing_checks, extra_failing_checks) = failing_check_names(checks);
        Self {
            number: pr.number,
            url: pr.url.into(),
            title: pr.title.into(),
            state: pr_state(&pr.state, pr.is_draft),
            checks: checks_state(checks),
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
        if !matches!(check.outcome(), "FAILURE" | "ERROR") {
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

fn checks_state(checks: &[GhCheck]) -> ChecksState {
    if checks.is_empty() {
        return ChecksState::None;
    }
    let outcomes = checks.iter().map(GhCheck::outcome).collect::<Vec<_>>();
    if outcomes
        .iter()
        .any(|outcome| matches!(*outcome, "FAILURE" | "ERROR"))
    {
        ChecksState::Failing
    } else if outcomes
        .iter()
        .any(|outcome| matches!(*outcome, "PENDING" | "QUEUED" | "IN_PROGRESS"))
    {
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
        let chips = thread_pr_chips([(branch, "feature")], None, || Some(vec![merged.clone()]));
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].url.as_deref(), Some(merged.url.as_ref()));

        // Nothing anywhere still says so: the badge is the same shape before
        // and after a PR arrives.
        let chips = thread_pr_chips([(branch, "feature")], None, || None);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].url.is_none());

        // Not even a branch, which is the case the branch-only helper leaves
        // empty and every caller had to patch up itself.
        let chips = thread_pr_chips([], None, || None);
        assert_eq!(chips.len(), 1);
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
        assert_eq!(unknown_chip.checks, Some((IconName::Check, Color::Success)));
        assert_eq!(unknown_chip.checks, mergeable_chip.checks);
        assert_eq!(unknown_chip.tooltip, mergeable_chip.tooltip);
        assert!(
            unknown_chip
                .detail
                .is_some_and(|detail| detail.merge_blocker.is_none())
        );
    }

    /// The complaint itself: green checks and an unmergeable PR looked
    /// identical, and the only way to find out was to open it.
    #[test]
    fn passing_checks_on_a_pr_that_cannot_merge_are_not_green() {
        for (state, reason) in [
            (MergeState::Behind, "behind base branch"),
            (MergeState::Conflicting, "conflicts with base branch"),
            (MergeState::Blocked, "blocked by branch protection"),
        ] {
            let mut pr = green_pr();
            pr.merge = state;
            let chip = pr_chip(&pr);
            assert_eq!(
                chip.checks,
                Some((IconName::Warning, Color::Warning)),
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
            branch: "main".into(),
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

    /// Which branches the faster poll picks up. Checks in progress change
    /// within a minute or two and then stop changing for hours, so they are the
    /// only ones worth asking about between full polls.
    #[test]
    fn only_branches_with_checks_running_are_polled_harder() {
        let branch = |prs: Option<Vec<PrStatus>>| WatchedBranch {
            watch_count: 1,
            prs,
            refresh_task: None,
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

    #[test]
    fn empty_null_or_missing_rollup_is_no_checks() {
        let json = r#"[
            {"number": 4, "url": "u", "title": "t", "state": "OPEN", "statusCheckRollup": []},
            {"number": 5, "url": "u", "title": "t", "state": "OPEN", "statusCheckRollup": null},
            {"number": 6, "url": "u", "title": "t", "state": "OPEN"}
        ]"#;
        let prs = parse_pr_list(json).unwrap();
        assert!(prs.iter().all(|pr| pr.checks == ChecksState::None));
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
        assert_eq!(chip.checks, Some((IconName::Check, Color::Success)));
    }

    #[test]
    fn empty_pr_list_parses() {
        assert_eq!(parse_pr_list("[]").unwrap(), vec![]);
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_pr_list("not json").is_err());
    }
}
