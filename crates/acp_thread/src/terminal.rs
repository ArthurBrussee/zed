use agent_client_protocol::schema::{MaybeUndefined, v1 as acp_v1, v2 as acp_v2};
use anyhow::{Result, bail};
use collections::HashMap;
use futures::{FutureExt as _, StreamExt as _, future::Shared};
use git::{
    repository::RepoPath,
    status::{DiffStat, FileStatus},
};
use gpui::{App, AppContext, AsyncApp, Context, Entity, Subscription, Task};
use http_proxy::Allowlist;
use language::LanguageRegistry;
use markdown::Markdown;
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap as StdHashMap,
    path::PathBuf,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use task::Shell;
use util::get_default_system_shell_preferring_bash;

/// Request to run a terminal command inside an OS-level sandbox.
///
/// Passed to [`super::AcpThread::create_terminal`]. The actual sandboxing
/// mechanism is platform-specific (macOS Seatbelt; Linux Bubblewrap; Windows
/// via Bubblewrap inside WSL), so callers describe the *intent* with plain data
/// here rather than constructing platform-specific types directly.
///
/// Default is the fully-sandboxed run (no network, project-only writes).
/// Setting `network` / `allow_fs_write` requests a relaxation; the caller is
/// responsible for having obtained user approval before reaching this point.
#[derive(Clone, Debug, Default)]
pub struct SandboxWrap {
    /// Directory subtrees the sandbox should allow writes to. Pass the
    /// project's worktree paths (and any per-command scratch directory)
    /// here — *not* the command's working directory, which is model-
    /// controlled and would let the model widen its own writable scope.
    pub writable_paths: Vec<PathBuf>,
    /// Additional write subtrees the user explicitly approved for this
    /// command (per-path write grants). Kept separate from `writable_paths`
    /// to make the trust boundary explicit: these originate from
    /// model-requested paths that passed a user-approval prompt. They are
    /// merged with `writable_paths` when generating the sandbox policy.
    ///
    /// Each grant carries the canonical target it resolved to at approval
    /// time; enforcement rebuilds the location via a verifying reopen (see
    /// [`granted_write_path_to_location`]) rather than re-resolving the bare
    /// requested path, which closes a symlink TOCTOU.
    pub extra_write_paths: Vec<settings::GrantedWritePath>,
    /// Outbound network access explicitly approved for this command.
    pub network: SandboxNetworkAccess,
    /// Additional paths that should remain readable but not writable, even when
    /// they fall under writable paths.
    pub protected_paths: Vec<PathBuf>,
    /// Allow unrestricted filesystem writes except for protected paths (ignores
    /// ordinary writable paths).
    pub allow_fs_write: bool,
    /// Whether the project (and therefore this terminal) is local. The
    /// enforcing proxy binds a loopback port on this host, so it can only
    /// confine local commands; a remote terminal can't reach it.
    pub is_local: bool,
    /// Windows/WSL only: `(release channel, version)` of the Linux `zed` to
    /// provision inside WSL as the sandbox helper (version `latest` for dev
    /// builds). Resolved by the agent (which can read the running app's release
    /// info) and forwarded to the sandbox. `None` on other platforms, or when
    /// the release can't be determined, in which case the WSL backend falls back
    /// to running bwrap without in-sandbox bind validation.
    pub wsl_zed_release: Option<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub enum SandboxNetworkAccess {
    /// Block all outbound network access.
    #[default]
    None,
    /// Allow only hosts in this allowlist, enforced by routing HTTP/HTTPS
    /// through an in-process proxy and confining the command to the proxy's
    /// loopback port.
    Restricted(Allowlist),
    /// Allow unrestricted outbound network access.
    All,
}

/// A structured, serializable reason the OS sandbox could not be created for a
/// command. Mirrors the Linux/WSL Bubblewrap failure modes; surfaced to the user
/// (and persisted in tool-call metadata) so the UI can
/// explain what went wrong.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinuxWslSandboxError {
    /// No usable `bwrap` binary was found on `PATH`.
    BwrapNotFound,
    /// The only `bwrap` found is setuid-root, which Zed refuses to run.
    SetuidRejected,
    /// `bwrap` is present but couldn't set up the sandbox (typically because
    /// unprivileged user namespaces are disabled).
    SandboxProbeFailed,
    /// Any other failure, with a human-readable description.
    Other(String),
}

impl From<sandbox::SandboxError> for LinuxWslSandboxError {
    fn from(error: sandbox::SandboxError) -> Self {
        match error {
            sandbox::SandboxError::BwrapNotFound => Self::BwrapNotFound,
            sandbox::SandboxError::BwrapSetuidRejected => Self::SetuidRejected,
            sandbox::SandboxError::SandboxProbeFailed => Self::SandboxProbeFailed,
            error => Self::Other(error.to_string()),
        }
    }
}

impl LinuxWslSandboxError {
    /// A short, user-facing explanation of why the sandbox couldn't be created,
    /// suitable for display in the agent panel.
    pub fn user_facing_message(&self) -> String {
        match self {
            LinuxWslSandboxError::BwrapNotFound => {
                "No usable `bwrap` binary was found on your PATH. Install Bubblewrap to let \
                 the agent sandbox terminal commands."
                    .to_string()
            }
            LinuxWslSandboxError::SetuidRejected => {
                "The only `bwrap` available is setuid-root, which Zed refuses to run. Install \
                 a non-setuid Bubblewrap to let the agent sandbox terminal commands."
                    .to_string()
            }
            LinuxWslSandboxError::SandboxProbeFailed => {
                "`bwrap` is installed but couldn't create a sandbox, likely because \
                 unprivileged user namespaces are disabled on this system."
                    .to_string()
            }
            LinuxWslSandboxError::Other(message) => message.clone(),
        }
    }

    /// The slug of the sandboxing docs section that best explains how to resolve
    /// this failure, for deep-linking from the UI. Pair with
    /// `client::zed_urls::sandboxing_docs`.
    pub fn docs_section(&self) -> &'static str {
        match self {
            // Both "no bwrap" and "only a setuid-root bwrap" are resolved by
            // installing a non-setuid Bubblewrap.
            LinuxWslSandboxError::BwrapNotFound | LinuxWslSandboxError::SetuidRejected => {
                "installing-bubblewrap"
            }
            // A failed probe on Linux is almost always disabled unprivileged
            // user namespaces, which the Ubuntu-specific section covers.
            LinuxWslSandboxError::SandboxProbeFailed => "installing-bubblewrap-ubuntu",
            // Catch-all (includes WSL/Windows messages): point at the platform
            // overview for the current OS.
            LinuxWslSandboxError::Other(_) => {
                if cfg!(target_os = "windows") {
                    "windows"
                } else {
                    "linux"
                }
            }
        }
    }
}

/// Rebuild a user-approved write grant into an enforceable
/// [`sandbox::HostFilesystemLocation`].
///
/// When the grant carries a resolved canonical (the normal case, established at
/// approval time), the location is rebuilt via a verifying
/// [`sandbox::HostFilesystemLocation::reopen`] — the load-bearing step of the
/// TOCTOU fix. A legacy bare-string grant (no resolved canonical) falls back to
/// a fresh [`sandbox::HostFilesystemLocation::capture`].
pub fn granted_write_path_to_location(
    granted: &settings::GrantedWritePath,
) -> std::io::Result<sandbox::HostFilesystemLocation> {
    match &granted.resolved {
        Some(resolved) => sandbox::HostFilesystemLocation::reopen(&granted.requested, resolved),
        None => sandbox::HostFilesystemLocation::capture(&granted.requested),
    }
}

/// Rebuild a grant for enforcement, or log and drop it (fail-closed) if it
/// can't be verified.
///
/// A failure here is frequently the symlink-TOCTOU defense firing: the grant's
/// canonical was redirected or replaced by a symlink since approval, so
/// [`sandbox::HostFilesystemLocation::reopen`] refuses it. That is a
/// security-relevant event, so it must be logged rather than silently
/// swallowed. The grant is dropped (the command runs without it) rather than
/// bound unverified.
///
/// Only for **display** policies (the sandbox-status UI), where a stale grant
/// should simply not be shown. Enforcement must not drop grants silently — a
/// command would run with less access than the user approved with no signal —
/// so [`SandboxWrap::to_policy`] uses the erroring
/// [`granted_write_path_to_location`] instead.
pub fn granted_write_path_to_location_or_log(
    granted: &settings::GrantedWritePath,
) -> Option<sandbox::HostFilesystemLocation> {
    granted_write_path_to_location(granted)
        .inspect_err(|error| {
            log::warn!(
                "dropping sandbox write grant {}: {error}",
                granted.requested.display()
            );
        })
        .ok()
}

impl SandboxWrap {
    /// Whether the OS sandbox for this request can actually be created right now,
    /// returning a structured [`LinuxWslSandboxError`] when it can't.
    ///
    /// The sandbox implementation never runs a command unsandboxed on its own —
    /// it aborts if it can't create the sandbox. This lets a caller decide, up
    /// front, whether to run sandboxed, fall back to an unsandboxed run
    /// (fail-open), or refuse (fail-closed). It runs a brief probe subprocess on
    /// Linux, so call it off the main thread. On platforms whose sandbox can't
    /// fail to set up this way it always returns `Ok`.
    pub fn can_create_sandbox(&self) -> Result<(), LinuxWslSandboxError> {
        let policy = self
            .to_policy()
            .map_err(|error| LinuxWslSandboxError::Other(format!("{error:#}")))?;
        sandbox::Sandbox::can_create(&policy).map_err(LinuxWslSandboxError::from)
    }

    /// Translate this request into the cross-platform [`sandbox::SandboxPolicy`].
    ///
    /// This is the enforcement-policy construction point, so it **captures** each
    /// grant as a [`sandbox::HostFilesystemLocation`] (pinning the inode / canonical
    /// path) rather than passing a re-resolvable path.
    ///
    /// This function has **no filesystem side effects**: it never creates paths,
    /// and it **fails** (rather than silently narrowing the policy) when a
    /// writable path or approved grant can't be captured — running anyway would
    /// give the command silently less access than the model and user were told
    /// it has. On Linux a writable grant that doesn't exist can't be captured
    /// (bwrap can't bind a missing path); the sanctioned way to get a grant to a
    /// new directory is the `create_directory` tool, which creates it (pinning
    /// the inode) before the grant is recorded. On macOS a missing leaf still
    /// canonicalizes, so such grants are captured directly.
    ///
    /// It is used both by the side-effect-free [`Self::can_create_sandbox`] probe
    /// and by real sandbox construction, and must behave identically.
    ///
    /// A grant failure here can also be the symlink-TOCTOU defense firing: the
    /// grant's canonical was redirected or replaced by a symlink since approval,
    /// so the verifying reopen refuses it. Failing the command surfaces that
    /// security-relevant event instead of running with the grant quietly
    /// missing.
    ///
    /// Protected paths, by contrast, are **best-effort**: we protect only the
    /// ones that exist at creation time (`capture` succeeding *is* the existence
    /// check), and silently drop the rest. Unlike a writable grant, a protection
    /// can't be materialized — you can't pin the inode of a path that isn't
    /// there — and there is an inherent, *accepted* loophole regardless: a
    /// command in a non-git directory can `git init` and write hooks into a
    /// `.git` that didn't exist when the sandbox was built. Since the protection
    /// is defeatable that way no matter what, failing sandbox creation over a
    /// currently-absent (or otherwise uncapturable) `.git` would only break
    /// legitimate cases — non-git projects, single-file worktrees whose
    /// synthesized `settings.json/.git` routes through a file — without closing
    /// the hole. So we drop and move on.
    fn to_policy(&self) -> Result<sandbox::SandboxPolicy> {
        let protected_paths = self
            .protected_paths
            .iter()
            .filter_map(|path| sandbox::HostFilesystemLocation::capture(path).ok())
            .collect::<Vec<_>>();
        let fs = if self.allow_fs_write {
            sandbox::SandboxFsPolicy::Unrestricted { protected_paths }
        } else {
            // Project worktree paths are captured fresh; user-approved grants are
            // rebuilt via the verifying reopen (or captured when legacy bare
            // strings) through `granted_write_path_to_location`. A path that
            // can't be captured fails the whole construction (never created).
            let mut locations = Vec::new();
            for path in &self.writable_paths {
                let location = sandbox::HostFilesystemLocation::capture(path).map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "cannot capture writable sandbox path `{}`",
                        path.display()
                    ))
                })?;
                locations.push(location);
            }
            for granted in &self.extra_write_paths {
                let location = granted_write_path_to_location(granted).map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "cannot re-verify approved sandbox write grant `{}` (if the \
                         directory was removed, remove the grant or recreate the \
                         directory)",
                        granted.requested.display()
                    ))
                })?;
                locations.push(location);
            }
            // Dedupe to a minimal cover on the captured canonical paths, so a
            // grant nested under a worktree root (or another grant) is dropped
            // rather than bound redundantly.
            let writable_paths =
                sandbox::normalize_host_filesystem_locations(locations.into_iter());
            sandbox::SandboxFsPolicy::Restricted {
                writable_paths,
                protected_paths,
            }
        };
        let network = match &self.network {
            SandboxNetworkAccess::None => sandbox::SandboxNetPolicy::Blocked,
            SandboxNetworkAccess::All => sandbox::SandboxNetPolicy::Unrestricted,
            SandboxNetworkAccess::Restricted(allowlist) => sandbox::SandboxNetPolicy::Restricted {
                allowed_domains: allowlist
                    .patterns()
                    .iter()
                    .map(|pattern| pattern.to_string())
                    .collect(),
            },
        };
        Ok(sandbox::SandboxPolicy { fs, network })
    }
}

/// Why the OS sandbox was *not* applied to a terminal command, even though
/// sandboxing is active for the thread. Persisted in tool-call metadata so the
/// UI can explain the situation after the fact.
///
/// This is deliberately platform-agnostic — every variant exists on every
/// platform — so the serialized form stored in the thread database never
/// depends on which OS wrote it. Today only Linux/WSL can fail to create a
/// sandbox (`ErrorLinuxWsl`), but the variant is named so macOS/Windows can
/// grow their own failure cases later without a migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxNotAppliedReason {
    /// The user disabled the sandbox for the rest of this thread, so the command
    /// ran without one. This happens either when the user approved a
    /// model-requested `unsandboxed: true` escape "for this thread", or when
    /// they chose to run unsandboxed for the thread after a sandbox-creation
    /// failure (in which case a preceding tool call's reason is
    /// [`SandboxNotAppliedReason::ErrorLinuxWsl`]).
    DisabledForThisThread,
    /// The Linux/WSL (Bubblewrap) sandbox could not be created for this command.
    ErrorLinuxWsl(LinuxWslSandboxError),
}

/// The live sandbox kept alive for its per-command resources (the network proxy
/// and, on macOS, the Seatbelt policy file) until the terminal exits.
type SandboxConfigHandle = sandbox::Sandbox;

/// Upper bound on preparing a WSL-sandboxed command. Deliberately generous:
/// the first invocation after the WSL utility VM has shut down (or after boot)
/// has to start the VM and the distro, which routinely takes 10-30 seconds on
/// slow disks or under antivirus scanning.
#[cfg(target_os = "windows")]
pub(crate) const WSL_SANDBOX_WRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Wrap `(program, args)` for sandboxed execution, returning the wrapped
/// invocation (program, argv, env) plus the live [`sandbox::Sandbox`] that must
/// be kept alive for the command's duration. When `sandbox_wrap` is `None` the
/// command is returned unchanged.
///
/// The sandbox owns the network proxy (for restricted-network policies) and any
/// per-command policy file; the env it returns already routes through that
/// proxy when applicable.
pub(crate) async fn prepare_sandbox_wrap(
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    sandbox_wrap: Option<SandboxWrap>,
    env: HashMap<String, String>,
) -> anyhow::Result<(
    String,
    Vec<String>,
    HashMap<String, String>,
    Option<SandboxConfigHandle>,
)> {
    let Some(sandbox_wrap) = sandbox_wrap else {
        return Ok((program, args, env, None));
    };

    let mut sandbox =
        sandbox::Sandbox::new(sandbox_wrap.to_policy()?).map_err(anyhow::Error::new)?;
    // Windows/WSL only: tell the sandbox which Linux `zed` to provision inside
    // WSL as its `--wsl-sandbox-helper`. A no-op (and a no-op setter) elsewhere.
    #[cfg(target_os = "windows")]
    if let Some((channel, version)) = sandbox_wrap.wsl_zed_release.clone() {
        sandbox.set_wsl_zed_release(channel, version);
    }
    let command = sandbox::CommandAndArgs {
        program,
        args,
        env: env.into_iter().collect::<StdHashMap<_, _>>(),
        cwd,
    };
    let wrapped = sandbox.wrap(&command).await.map_err(anyhow::Error::new)?;
    Ok((
        wrapped.program,
        wrapped.args,
        wrapped.env.into_iter().collect(),
        Some(sandbox),
    ))
}

/// How long to wait after a command exits for the repository's first status
/// event: statuses arrive on debounced filesystem events.
const FIRST_CHANGE_TIMEOUT: Duration = Duration::from_secs(6);
/// How long the repository must stay quiet, once something was reported.
const QUIET_AFTER_CHANGE: Duration = Duration::from_millis(750);
const SETTLE_ROUNDS: usize = 30;
/// Slack around the command's run for file timestamps.
const WRITE_WINDOW_GRACE: Duration = Duration::from_secs(2);
/// Caps on the before-text kept for already-dirty files, so a worktree full of
/// large dirty files does not make every command cost megabytes to start.
const MAX_PRE_COMMAND_FILES: usize = 16;
const MAX_PRE_COMMAND_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// A file a command changed, as the repository saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: ProjectPath,
    pub added: u32,
    pub deleted: u32,
    /// Whether the file already had uncommitted changes when the command
    /// started, so its diff against HEAD is not all the command's.
    pub pre_command_dirty: bool,
    /// What an already-dirty file held when the command started, unless it
    /// was too large or unreadable to keep.
    pub pre_command_text: Option<Arc<str>>,
}

/// The diff stat is included because an already-modified file keeps its
/// status when changed again.
type StatusSnapshot = HashMap<RepoPath, (FileStatus, Option<DiffStat>)>;

fn status_snapshot(repository: &Entity<Repository>, cx: &App) -> StatusSnapshot {
    repository
        .read(cx)
        .cached_status()
        .map(|entry| (entry.repo_path, (entry.status, entry.unstaged_diff_stat)))
        .collect()
}

struct RepositoryWatch {
    repository: Entity<Repository>,
    project: Entity<Project>,
    fs: Arc<dyn project::Fs>,
    baseline: StatusSnapshot,
    pre_command_text: Shared<Task<Arc<HashMap<RepoPath, Arc<str>>>>>,
    started_at: SystemTime,
    candidates: Vec<(RepoPath, DiffStat)>,
    reported: Vec<ChangedFile>,
}

/// Reads the already-dirty files before the command overwrites them; for a
/// clean file HEAD is the before-text.
fn capture_pre_command_text(
    baseline: &StatusSnapshot,
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    fs: Arc<dyn project::Fs>,
    cx: &App,
) -> Task<Arc<HashMap<RepoPath, Arc<str>>>> {
    let mut paths: Vec<(RepoPath, PathBuf)> = {
        let repo = repository.read(cx);
        baseline
            .keys()
            .filter_map(|repo_path| {
                let path = repo.repo_path_to_project_path(repo_path, cx)?;
                let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
                Some((repo_path.clone(), worktree.read(cx).absolutize(&path.path)))
            })
            .collect()
    };
    // Sorted so the cap keeps a deterministic set.
    paths.sort_by(|(left, _), (right, _)| left.cmp(right));
    paths.truncate(MAX_PRE_COMMAND_FILES);

    cx.background_spawn(async move {
        let mut captured: HashMap<RepoPath, Arc<str>> = HashMap::default();
        for (repo_path, abs_path) in paths {
            if let Ok(text) = fs.load(&abs_path).await
                && text.len() <= MAX_PRE_COMMAND_BYTES
            {
                captured.insert(repo_path, text.as_str().into());
            }
        }
        Arc::new(captured)
    })
}

impl RepositoryWatch {
    /// `ended_at` is `None` while the command is still running.
    async fn refresh(
        &mut self,
        ended_at: Option<SystemTime>,
        terminal: &gpui::WeakEntity<Terminal>,
        cx: &mut AsyncApp,
    ) {
        let repository = self.repository.clone();
        let baseline = &self.baseline;
        let mut changed: Vec<(RepoPath, DiffStat)> = cx.update(|cx| {
            status_snapshot(&repository, cx)
                .iter()
                .filter(|(path, state)| baseline.get(*path) != Some(state))
                .map(|(path, (_, stat))| {
                    let before = baseline.get(path).and_then(|(_, stat)| *stat);
                    (path.clone(), stat_delta(before, *stat))
                })
                .collect()
        });
        changed.sort_by(|(left, _), (right, _)| left.cmp(right));
        if changed == self.candidates {
            return;
        }
        self.candidates = changed;

        let pre_command_text = self.pre_command_text.clone().await;

        let project = self.project.clone();
        let candidates: Vec<(ChangedFile, PathBuf)> = cx.update(|cx| {
            let repository = repository.read(cx);
            self.candidates
                .iter()
                .filter_map(|(repo_path, delta)| {
                    let path = repository.repo_path_to_project_path(repo_path, cx)?;
                    let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
                    let abs_path = worktree.read(cx).absolutize(&path.path);
                    let pre_command_dirty = baseline
                        .get(repo_path)
                        .and_then(|(_, stat)| *stat)
                        .is_some_and(|stat| stat.added + stat.deleted > 0);
                    Some((
                        ChangedFile {
                            path,
                            added: delta.added,
                            deleted: delta.deleted,
                            pre_command_dirty,
                            pre_command_text: pre_command_dirty
                                .then(|| pre_command_text.get(repo_path).cloned())
                                .flatten(),
                        },
                        abs_path,
                    ))
                })
                .collect()
        });

        // Status events lag the filesystem, so edits from just before or after
        // the command still arrive during the watch. A file's mtime tells them
        // apart.
        let ended_at = ended_at.unwrap_or_else(SystemTime::now);
        let window = self
            .started_at
            .checked_sub(WRITE_WINDOW_GRACE)
            .unwrap_or(self.started_at)
            ..=ended_at.checked_add(WRITE_WINDOW_GRACE).unwrap_or(ended_at);
        let fs = self.fs.clone();
        let confirmed = cx
            .background_spawn(async move {
                let mut confirmed = Vec::new();
                for (file, abs_path) in candidates {
                    // No metadata: deleted, which is worth reporting.
                    let within = match fs.metadata(&abs_path).await {
                        Ok(Some(metadata)) => window.contains(&metadata.mtime.timestamp_for_user()),
                        _ => true,
                    };
                    if within {
                        confirmed.push(file);
                    }
                }
                confirmed
            })
            .await;
        if confirmed == self.reported {
            return;
        }
        self.reported = confirmed.clone();

        terminal
            .update(cx, |terminal, cx| {
                terminal.changed_files = confirmed;
                cx.notify();
            })
            .ok();
    }
}

/// The growth of a file's unstaged diff stat while the command ran.
fn stat_delta(before: Option<DiffStat>, after: Option<DiffStat>) -> DiffStat {
    let before = before.unwrap_or_default();
    let after = after.unwrap_or_default();
    DiffStat {
        added: after.added.saturating_sub(before.added),
        deleted: after.deleted.saturating_sub(before.deleted),
    }
}

/// Tagged `bash` so every command gets shell syntax highlighting.
fn command_markdown(command: &str) -> String {
    format!("```bash\n{command}\n```")
}

/// Whether it is worth watching the repository around this command. A line
/// that moves the branch or discards the worktree wholesale is not: its
/// changes cannot be told apart from the rest of the line's.
fn command_may_write(command: &str) -> bool {
    use crate::command_parse::{DestructiveOperation, GitOperation, SegmentKind};

    let mut may_write = false;
    for segment in &crate::command_parse::parse_command(command).segments {
        match &segment.kind {
            SegmentKind::Git {
                operation: GitOperation::Modify,
                ..
            } => return false,
            SegmentKind::Destructive {
                operation: DestructiveOperation::DiscardChanges,
                paths,
            } if paths.is_empty() => return false,

            SegmentKind::Noop
            | SegmentKind::Read { .. }
            | SegmentKind::Search { .. }
            | SegmentKind::ListDirectory { .. }
            | SegmentKind::Lookup { .. }
            | SegmentKind::CountLines { .. }
            | SegmentKind::Wait { .. }
            | SegmentKind::Git { .. } => {}

            SegmentKind::WriteFile { .. }
            | SegmentKind::EditInPlace { .. }
            | SegmentKind::Destructive { .. }
            | SegmentKind::InlineScript { .. }
            | SegmentKind::GitHub { .. }
            | SegmentKind::Run { .. } => may_write = true,
        }
    }
    may_write
}

pub struct Terminal {
    id: acp_v1::TerminalId,
    command: Entity<Markdown>,
    working_dir: Option<PathBuf>,
    terminal: Entity<terminal::Terminal>,
    started_at: Instant,
    output: Option<TerminalOutput>,
    output_byte_limit: Option<usize>,
    execution: TerminalExecution,
    /// Flag indicating whether this terminal was stopped by explicit user action
    /// (e.g., clicking the Stop button). This is set before kill() is called
    /// so that code awaiting wait_for_exit() can check it deterministically.
    user_stopped: Arc<AtomicBool>,
    /// The live sandbox (Seatbelt policy file and/or network proxy) kept alive
    /// until the sandboxed command exits. `None` when the command isn't
    /// sandboxed or after it finishes. Dropping it tears down the proxy on a
    /// background thread (see `sandbox::Sandbox`'s `Drop`).
    _sandbox: Option<SandboxConfigHandle>,
    may_write: bool,
    _repository_events: Option<Subscription>,
    changed_files: Vec<ChangedFile>,
    _changed_paths_task: Option<Task<()>>,
    output_images: Vec<PathBuf>,
    _output_images_task: Option<Task<()>>,
    /// Comparable with file mtimes, unlike `started_at`.
    started_at_wall: SystemTime,
}

enum TerminalExecution {
    Process(Shared<Task<acp_v1::TerminalExitStatus>>),
    Display(DisplayTerminalState),
}

#[derive(Debug, Default)]
pub struct DisplayTerminalState {
    command: DisplayCommand,
    pub meta: Option<acp_v2::Meta>,
    pub output_meta: Option<acp_v2::Meta>,
    legacy_created: bool,
    legacy_exited: bool,
}

#[derive(Debug, Default)]
enum DisplayCommand {
    #[default]
    Unknown,
    LegacyCaption(String),
    Reported(Option<String>),
}

impl DisplayTerminalState {
    pub fn command(&self) -> Option<&str> {
        match &self.command {
            DisplayCommand::LegacyCaption(command) | DisplayCommand::Reported(Some(command)) => {
                Some(command)
            }
            DisplayCommand::Unknown | DisplayCommand::Reported(None) => None,
        }
    }
}

/// Terminal patches use decoded bytes; decoding and validating wire data belongs to the adapter.
#[derive(Debug, Default)]
pub struct DisplayTerminalPatch {
    pub command: MaybeUndefined<String>,
    pub cwd: MaybeUndefined<acp_v2::AbsolutePath>,
    pub output: MaybeUndefined<DisplayTerminalOutput>,
    pub exit_status: MaybeUndefined<acp_v2::TerminalExitStatus>,
    pub meta: MaybeUndefined<acp_v2::Meta>,
}

#[derive(Debug)]
pub struct DisplayTerminalOutput {
    pub data: Vec<u8>,
    pub meta: Option<acp_v2::Meta>,
}

pub struct TerminalOutput {
    pub ended_at: Instant,
    pub exit_status: acp_v2::TerminalExitStatus,
    pub content: String,
    pub original_content_len: usize,
    pub content_line_count: usize,
}

impl TerminalOutput {
    /// A status with neither a code nor a signal is not a failure.
    pub fn failed(&self) -> bool {
        self.exit_status.exit_code.is_some_and(|code| code != 0)
            || self.exit_status.signal.is_some()
    }
}

impl Terminal {
    pub fn new(
        id: acp_v1::TerminalId,
        command_label: &str,
        working_dir: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        terminal: Entity<terminal::Terminal>,
        language_registry: Arc<LanguageRegistry>,
        sandbox: Option<SandboxConfigHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let command_task = terminal.read(cx).wait_for_completed_task(cx);
        // Tear the sandbox down on a GPUI background thread when this entity is
        // released, rather than relying on `Sandbox`'s `Drop` (which would spawn
        // a throwaway thread) on whatever thread releases us. `on_release` hands
        // us an `App`, so we can drive the teardown through the background
        // executor with `drop_on_current_thread`.
        cx.on_release(|this, cx| {
            if let Some(sandbox) = this._sandbox.take() {
                cx.background_executor()
                    .spawn(async move { sandbox.drop_on_current_thread() })
                    .detach();
            }
        })
        .detach();
        Self {
            id,
            _sandbox: sandbox,
            command: cx.new(|cx| {
                Markdown::new(
                    command_markdown(command_label).into(),
                    Some(language_registry.clone()),
                    None,
                    cx,
                )
            }),
            working_dir,
            terminal,
            started_at: Instant::now(),
            output: None,
            output_byte_limit,
            user_stopped: Arc::new(AtomicBool::new(false)),
            may_write: command_may_write(command_label),
            _repository_events: None,
            changed_files: Vec::new(),
            _changed_paths_task: None,
            output_images: Vec::new(),
            _output_images_task: None,
            started_at_wall: SystemTime::now(),
            execution: TerminalExecution::Process(
                cx.spawn(async move |this, cx| {
                    let exit_status = command_task.await.map(Self::transform_exit_status);
                    let exit_status = acp_v1::TerminalExitStatus::new()
                        .exit_code(exit_status.as_ref().map(|status| status.0))
                        .signal(exit_status.and_then(|status| status.1));

                    this.update(cx, |this, cx| {
                        this.cache_output(
                            Self::exit_status_from_v1(exit_status.clone()),
                            Instant::now(),
                            cx,
                        );
                        this.terminal.update(cx, |terminal, _cx| {
                            terminal.release_pty_resources();
                            // Otherwise each finished command keeps its whole
                            // scrollback reservation while the thread is open.
                            terminal.shrink_to_used();
                        });
                        // Free the sandbox (and its network proxy) as soon as
                        // the command finishes, rather than holding it until
                        // this entity is released. The proxy's teardown joins a
                        // listener thread, so run it on the background executor
                        // to keep it off the foreground thread.
                        if let Some(sandbox) = this._sandbox.take() {
                            cx.background_executor()
                                .spawn(async move { sandbox.drop_on_current_thread() })
                                .detach();
                        }
                    })
                    .ok();

                    exit_status
                })
                .shared(),
            ),
        }
    }

    pub fn new_display(
        id: acp_v1::TerminalId,
        command_label: Option<&str>,
        working_dir: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        terminal: Entity<terminal::Terminal>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id,
            command: cx.new(|cx| {
                Markdown::new(
                    command_markdown(command_label.unwrap_or("Terminal")).into(),
                    Some(language_registry),
                    None,
                    cx,
                )
            }),
            working_dir,
            terminal,
            started_at: Instant::now(),
            output: None,
            output_byte_limit,
            execution: TerminalExecution::Display(DisplayTerminalState {
                command: command_label
                    .map(|command| DisplayCommand::LegacyCaption(command.to_owned()))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            user_stopped: Arc::new(AtomicBool::new(false)),
            _sandbox: None,
            // An unlabelled command might write; the label arrives too late.
            may_write: command_label.is_none_or(command_may_write),
            _repository_events: None,
            changed_files: Vec::new(),
            _changed_paths_task: None,
            output_images: Vec::new(),
            _output_images_task: None,
            started_at_wall: SystemTime::now(),
        }
    }

    pub fn id(&self) -> &acp_v1::TerminalId {
        &self.id
    }

    /// Ownership is independent of the renderer's PTY: headless tasks still own a process.
    pub fn is_process_backed(&self) -> bool {
        matches!(self.execution, TerminalExecution::Process(_))
    }

    pub fn wait_for_exit(&self) -> Result<Shared<Task<acp_v1::TerminalExitStatus>>> {
        match &self.execution {
            TerminalExecution::Process(task) => Ok(task.clone()),
            TerminalExecution::Display(_) => {
                bail!("Agent-provided terminals have no client-owned process to wait for")
            }
        }
    }

    /// Files this command changed, as the repository saw them.
    pub fn changed_files(&self) -> &[ChangedFile] {
        &self.changed_files
    }

    pub fn output_images(&self) -> &[PathBuf] {
        &self.output_images
    }

    /// Pictures the command's output named that were written while it ran.
    fn watch_output_images(&mut self, fs: Arc<dyn project::Fs>, cx: &mut Context<Self>) {
        let Ok(exited) = self.wait_for_exit() else {
            return;
        };
        let working_dir = self.working_dir.clone();
        let started_at = self.started_at_wall;
        self._output_images_task = Some(cx.spawn(async move |this, cx| {
            exited.await;
            let named = this
                .read_with(cx, |this, _cx| {
                    this.output
                        .as_ref()
                        .map(|output| crate::command_output::image_paths_in_output(&output.content))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            if named.is_empty() {
                return;
            }

            let window = started_at
                .checked_sub(WRITE_WINDOW_GRACE)
                .unwrap_or(started_at)
                ..=SystemTime::now()
                    .checked_add(WRITE_WINDOW_GRACE)
                    .unwrap_or_else(SystemTime::now);
            let found = cx
                .background_spawn(async move {
                    let mut found = Vec::new();
                    for name in named {
                        let path = PathBuf::from(&name);
                        let path = if path.is_absolute() {
                            path
                        } else if let Some(working_dir) = working_dir.as_ref() {
                            working_dir.join(path)
                        } else {
                            continue;
                        };
                        let Ok(Some(metadata)) = fs.metadata(&path).await else {
                            continue;
                        };
                        if metadata.is_dir
                            || metadata.is_fifo
                            || metadata.len > MAX_OUTPUT_IMAGE_BYTES
                            || !window.contains(&metadata.mtime.timestamp_for_user())
                        {
                            continue;
                        }
                        found.push(path);
                    }
                    found
                })
                .await;
            if found.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                this.output_images = found;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Records what the command changed, as whatever the repository status
    /// says changed while it ran, rather than by reading the command. Cannot
    /// see ignored files, files outside the repository, or another process
    /// writing to the same worktree at the same time.
    pub fn watch_repository(&mut self, project: Entity<Project>, cx: &mut Context<Self>) {
        if !self.may_write {
            return;
        }

        self.watch_output_images(project.read(cx).fs().clone(), cx);

        // The innermost repository containing the working directory, so a
        // command in a submodule is watched by the one it changes.
        let git_store = project.read(cx).git_store().clone();
        let containing = self.working_dir.as_ref().and_then(|working_dir| {
            git_store
                .read(cx)
                .repositories()
                .values()
                .filter(|repository| {
                    repository
                        .read(cx)
                        .abs_path_to_repo_path(working_dir)
                        .is_some()
                })
                .max_by_key(|repository| repository.read(cx).work_directory_abs_path.clone())
                .cloned()
        });
        let Some(repository) = containing.or_else(|| project.read(cx).active_repository(cx)) else {
            return;
        };
        let Ok(exited) = self.wait_for_exit() else {
            return;
        };
        let fs = project.read(cx).fs().clone();
        let baseline = status_snapshot(&repository, cx);
        let pre_command_text =
            capture_pre_command_text(&baseline, &repository, &project, fs.clone(), cx).shared();
        let mut watch = RepositoryWatch {
            baseline,
            pre_command_text,
            fs,
            repository: repository.clone(),
            project,
            started_at: SystemTime::now(),
            candidates: Vec::new(),
            reported: Vec::new(),
        };

        let (mut moved_tx, mut moved_rx) = futures::channel::mpsc::channel(1);
        self._repository_events = Some(cx.subscribe(&repository, move |_, _, event, _| {
            if matches!(event, RepositoryEvent::StatusesChanged) {
                // A full channel already says the repository moved.
                moved_tx.try_send(()).ok();
            }
        }));

        self._changed_paths_task = Some(cx.spawn(async move |this, cx| {
            let mut exited = exited.fuse();
            let mut ended_at = None;

            loop {
                let listening = futures::select_biased! {
                    _ = exited => {
                        ended_at = Some(SystemTime::now());
                        true
                    }
                    moved = moved_rx.next() => moved.is_some(),
                };
                watch.refresh(ended_at, &this, cx).await;
                if ended_at.is_some() || !listening {
                    break;
                }
            }
            if ended_at.is_none() {
                return;
            }

            // The last writes' status events land after the command ends.
            for _ in 0..SETTLE_ROUNDS {
                let quiet = if watch.reported.is_empty() {
                    FIRST_CHANGE_TIMEOUT
                } else {
                    QUIET_AFTER_CHANGE
                };
                let moved = futures::select_biased! {
                    moved = moved_rx.next() => moved.is_some(),
                    _ = cx.background_executor().timer(quiet).fuse() => false,
                };
                if !moved {
                    break;
                }
                watch.refresh(ended_at, &this, cx).await;
            }
        }));
    }

    pub fn kill(&mut self, cx: &mut App) {
        if !self.is_process_backed() {
            return;
        }
        self.terminal.update(cx, |terminal, _cx| {
            terminal.kill_active_task();
        });
    }

    /// Marks this terminal as stopped by user action and then kills it.
    /// This should be called when the user explicitly clicks a Stop button.
    pub fn stop_by_user(&mut self, cx: &mut App) {
        if !self.is_process_backed() {
            return;
        }
        self.user_stopped.store(true, Ordering::SeqCst);
        self.kill(cx);
    }

    /// Returns whether this terminal was stopped by explicit user action.
    pub fn was_stopped_by_user(&self) -> bool {
        self.user_stopped.load(Ordering::SeqCst)
    }

    pub fn current_output(&self, cx: &App) -> acp_v1::TerminalOutputResponse {
        if let Some(output) = self.output.as_ref() {
            acp_v1::TerminalOutputResponse::new(
                output.content.clone(),
                output.original_content_len > output.content.len(),
            )
            .exit_status(
                acp_v1::TerminalExitStatus::new()
                    .exit_code(output.exit_status.exit_code)
                    .signal(output.exit_status.signal.clone())
                    .meta(output.exit_status.meta.clone()),
            )
        } else {
            let (current_content, original_len) = self.truncated_output(cx);
            let truncated = current_content.len() < original_len;
            acp_v1::TerminalOutputResponse::new(current_content, truncated)
        }
    }

    pub(crate) fn write_display_output(&mut self, data: &[u8], cx: &mut Context<Self>) {
        if self.is_process_backed() {
            return;
        }
        self.terminal
            .update(cx, |terminal, cx| terminal.write_output(data, cx));
        self.refresh_output(cx);
    }

    fn refresh_output(&mut self, cx: &mut Context<Self>) {
        if let Some(output) = &self.output {
            // A process may exit before its final output arrives. Refresh the
            // cached content without changing its completion time or status.
            self.cache_output(output.exit_status.clone(), output.ended_at, cx);
        }
    }

    pub(crate) fn finish_display(
        &mut self,
        exit_status: acp_v1::TerminalExitStatus,
        cx: &mut Context<Self>,
    ) {
        let TerminalExecution::Display(state) = &mut self.execution else {
            return;
        };
        if std::mem::replace(&mut state.legacy_exited, true) || self.output.is_some() {
            return;
        }
        self.terminal
            .update(cx, |terminal, _| terminal.shrink_to_used());
        self.cache_output(Self::exit_status_from_v1(exit_status), Instant::now(), cx);
    }

    fn exit_status_from_v1(status: acp_v1::TerminalExitStatus) -> acp_v2::TerminalExitStatus {
        acp_v2::TerminalExitStatus::new()
            .exit_code(status.exit_code)
            .signal(status.signal)
            .meta(status.meta)
    }

    pub fn display_state(&self) -> Option<&DisplayTerminalState> {
        match &self.execution {
            TerminalExecution::Display(state) => Some(state),
            TerminalExecution::Process(_) => None,
        }
    }

    pub(crate) fn initialize_legacy_display(
        &mut self,
        command: &str,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let TerminalExecution::Display(state) = &mut self.execution else {
            return;
        };
        if std::mem::replace(&mut state.legacy_created, true) {
            return;
        }
        self.update_command_label(command, cx);
        self.working_dir = cwd;
        if self.output_byte_limit != output_byte_limit {
            self.output_byte_limit = output_byte_limit;
            self.refresh_output(cx);
        }
        cx.notify();
    }

    pub(crate) fn append_display_bytes(
        &mut self,
        data: &[u8],
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.is_process_backed(),
            "Cannot patch a client-owned terminal"
        );
        self.terminal
            .update(cx, |terminal, cx| terminal.write_raw_output(data, cx));
        self.refresh_output(cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn apply_display_patch(
        &mut self,
        patch: DisplayTerminalPatch,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.is_process_backed(),
            "Cannot patch a client-owned terminal"
        );
        let DisplayTerminalPatch {
            command,
            cwd,
            output,
            exit_status,
            meta,
        } = patch;
        let output_changed = !output.is_undefined();
        let output_meta = match output {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => {
                self.terminal
                    .update(cx, |terminal, cx| terminal.replace_display_output(&[], cx))?;
                Some(None)
            }
            MaybeUndefined::Value(output) => {
                self.terminal.update(cx, |terminal, cx| {
                    terminal.replace_display_output(&output.data, cx)
                })?;
                Some(output.meta)
            }
        };

        if let TerminalExecution::Display(state) = &mut self.execution {
            if !command.is_undefined() {
                state.command = DisplayCommand::Reported(command.take());
                self.command.update(cx, |markdown, cx| {
                    markdown.replace(command_markdown(state.command().unwrap_or("Terminal")), cx);
                });
            }
            if !meta.is_undefined() {
                state.meta = meta.take();
            }
            if let Some(meta) = output_meta {
                state.output_meta = meta;
            }
        }
        if !cwd.is_undefined() {
            self.working_dir = cwd.take().map(|path| path.0);
        }
        match exit_status {
            MaybeUndefined::Undefined => {
                if output_changed {
                    self.refresh_output(cx);
                }
            }
            MaybeUndefined::Null => self.output = None,
            MaybeUndefined::Value(status) => {
                let ended_at = self
                    .output
                    .as_ref()
                    .map_or_else(Instant::now, |output| output.ended_at);
                self.terminal
                    .update(cx, |terminal, _| terminal.shrink_to_used());
                self.cache_output(status, ended_at, cx);
            }
        }
        cx.notify();
        Ok(())
    }

    fn cache_output(
        &mut self,
        exit_status: acp_v2::TerminalExitStatus,
        ended_at: Instant,
        cx: &mut Context<Self>,
    ) {
        let (content, original_content_len) = self.truncated_output(cx);
        let content_line_count = self.terminal.read(cx).total_lines();
        self.output = Some(TerminalOutput {
            ended_at,
            exit_status,
            content,
            original_content_len,
            content_line_count,
        });
        cx.notify();
    }

    fn truncated_output(&self, cx: &App) -> (String, usize) {
        let terminal = self.terminal.read(cx);
        let mut content = terminal.get_content();

        let original_content_len = content.len();

        if let Some(limit) = self.output_byte_limit
            && content.len() > limit
        {
            let mut end_ix = limit.min(content.len());
            while !content.is_char_boundary(end_ix) {
                end_ix -= 1;
            }
            // Don't truncate mid-line, clear the remainder of the last line
            end_ix = content[..end_ix].rfind('\n').unwrap_or(end_ix);
            content.truncate(end_ix);
        }

        (content, original_content_len)
    }

    pub fn command(&self) -> &Entity<Markdown> {
        &self.command
    }

    pub fn update_command_label(&mut self, label: &str, cx: &mut App) {
        if let TerminalExecution::Display(state) = &mut self.execution {
            if matches!(state.command, DisplayCommand::Reported(_))
                || state.command() == Some(label)
            {
                return;
            }
            state.command = DisplayCommand::LegacyCaption(label.to_owned());
        }
        self.command.update(cx, |command, cx| {
            command.replace(command_markdown(label), cx);
        });
    }

    pub fn working_dir(&self) -> &Option<PathBuf> {
        &self.working_dir
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn output(&self) -> Option<&TerminalOutput> {
        self.output.as_ref()
    }

    pub fn inner(&self) -> &Entity<terminal::Terminal> {
        &self.terminal
    }

    pub fn to_markdown(&self, cx: &App) -> String {
        format!(
            "Terminal:\n```\n{}\n```\n",
            self.terminal.read(cx).get_content()
        )
    }

    // This method is adapted from `portable_pty::ExitStatus::from`, version 0.9.0
    fn transform_exit_status(exit_status: ExitStatus) -> (u32, Option<String>) {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = exit_status.signal() {
                let signame = unsafe { libc::strsignal(signal) };
                let signal = if signame.is_null() {
                    format!("Signal {}", signal)
                } else {
                    let signame = unsafe { std::ffi::CStr::from_ptr(signame) };
                    signame.to_string_lossy().to_string()
                };

                return (
                    exit_status.code().map(|c| c as u32).unwrap_or(1),
                    Some(signal),
                );
            }
        }

        let code = exit_status
            .code()
            .map(|c| c as u32)
            .unwrap_or_else(|| if exit_status.success() { 0 } else { 1 });

        (code, None)
    }
}

pub async fn create_terminal_entity(
    command: String,
    args: &[String],
    env_vars: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Entity<terminal::Terminal>> {
    let mut env = if let Some(dir) = &cwd {
        project
            .update(cx, |project, cx| {
                project.environment().update(cx, |env, cx| {
                    env.directory_environment(dir.clone().into(), cx)
                })
            })
            .await
            .unwrap_or_default()
    } else {
        Default::default()
    };

    disable_pagers_through_env(&mut env);
    env.extend(env_vars);

    // Use remote shell or default system shell, as appropriate
    let shell = project
        .update(cx, |project, cx| {
            project
                .remote_client()
                .and_then(|r| r.read(cx).default_system_shell())
                .map(Shell::Program)
        })
        .unwrap_or_else(|| Shell::Program(get_default_system_shell_preferring_bash()));
    let is_windows = project.read_with(cx, |project, cx| project.path_style(cx).is_windows());
    let (task_command, task_args) = task::ShellBuilder::new(&shell, is_windows)
        .redirect_stdin_to_dev_null()
        .build(Some(command.clone()), &args);

    project
        .update(cx, |project, cx| {
            project.create_terminal_task(
                task::SpawnInTerminal {
                    command: Some(task_command),
                    args: task_args,
                    cwd,
                    env,
                    ..Default::default()
                },
                cx,
            )
        })
        .await
}

// Disable pagers so agent/terminal commands don't hang behind interactive UIs
pub(crate) fn disable_pagers_through_env(env: &mut collections::HashMap<String, String>) {
    env.insert("PAGER".into(), "".into());
    // Override user core.pager (e.g. delta) which Git prefers over PAGER
    env.insert("GIT_PAGER".into(), "cat".into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    #[gpui::test]
    async fn a_dirty_file_keeps_what_the_command_found(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "dirty.rs": "what the command found\n",
                "clean.rs": "committed\n",
            }),
        )
        .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("dirty.rs", git::status::StatusCode::Modified.worktree())],
        );
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("the project has a repository");
        let baseline = cx.update(|cx| status_snapshot(&repository, cx));
        let captured = cx
            .update(|cx| capture_pre_command_text(&baseline, &repository, &project, fs.clone(), cx))
            .await;

        let dirty = RepoPath::new("dirty.rs").unwrap();
        assert_eq!(
            captured.get(&dirty).map(|text| text.to_string()),
            Some("what the command found\n".to_string())
        );
        assert_eq!(captured.len(), 1, "a clean file needs nothing kept");
    }

    #[test]
    fn a_line_that_moves_the_branch_is_not_watched() {
        assert!(!command_may_write(
            "git rebase --onto upstream/main HEAD~3 && cargo check -p acp_thread"
        ));
        assert!(!command_may_write("git stash && cargo test"));
        assert!(!command_may_write(
            "git reset --hard origin/main && cargo build"
        ));
        assert!(!command_may_write("git clean -fd"));
        assert!(!command_may_write("git status --short"));

        assert!(command_may_write("git checkout -- src/main.rs"));
        assert!(command_may_write("cargo fmt --all"));
        assert!(command_may_write("sed -i '' 's/a/b/' src/main.rs"));
    }

    #[test]
    fn a_change_is_measured_from_where_the_file_already_was() {
        let stat = |added, deleted| Some(DiffStat { added, deleted });

        assert_eq!(
            stat_delta(None, stat(7, 2)),
            DiffStat {
                added: 7,
                deleted: 2
            }
        );

        assert_eq!(
            stat_delta(stat(10, 4), stat(13, 4)),
            DiffStat {
                added: 3,
                deleted: 0
            }
        );

        assert_eq!(stat_delta(stat(10, 4), stat(6, 4)), DiffStat::default());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    /// Regression test for the bug where enforcement-policy construction
    /// *created* missing write grants — famously turning a granted
    /// `~/.config/zed/AGENTS.md` file path into a directory. A grant whose
    /// target no longer exists must fail policy construction with an error
    /// naming it, and nothing may be created — a required safety grant that
    /// can't be honored must stop the command, not silently shrink its access.
    #[test]
    fn to_policy_fails_on_missing_grant_and_never_creates_it() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("AGENTS.md");

        let wrap = SandboxWrap {
            extra_write_paths: vec![settings::GrantedWritePath::resolved(
                missing.clone(),
                missing.clone(),
            )],
            ..Default::default()
        };

        let error = wrap
            .to_policy()
            .expect_err("a grant to a missing path must fail policy construction");
        assert!(
            format!("{error:#}").contains("AGENTS.md"),
            "error should name the failing grant: {error:#}"
        );
        assert!(
            !missing.exists(),
            "policy construction must never create the granted path"
        );
    }

    /// A baseline writable path (worktree root / scratch dir) that doesn't
    /// exist must also fail: silently narrowing the sandbox would hand the
    /// command less access than the model was told it has.
    #[test]
    fn to_policy_fails_on_missing_writable_path() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("gone");

        let wrap = SandboxWrap {
            writable_paths: vec![missing.clone()],
            ..Default::default()
        };

        wrap.to_policy()
            .expect_err("a missing writable path must fail policy construction");
        assert!(
            !missing.exists(),
            "policy construction must never create a writable path"
        );
    }

    /// Protected paths are best-effort: an uncapturable one is dropped, never
    /// fatal. That covers a missing path (`NotFound`) and one routed through a
    /// regular file (`NotADirectory`) — the latter is the synthesized `.git` of
    /// a single-file worktree (e.g. `settings.json/.git`). Unlike a writable
    /// grant, a protection can't be materialized, and `.git` protection has an
    /// inherent accepted loophole (`git init`), so failing here would only break
    /// legitimate cases. Unit-level companion to the `settings.json/.git` NixOS
    /// check.
    #[test]
    fn to_policy_skips_uncapturable_protected_paths() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let writable = temp_dir.path().join("writable");
        std::fs::create_dir(&writable).expect("create writable dir");
        let single_file_root = temp_dir.path().join("settings.json");
        std::fs::write(&single_file_root, b"{}").expect("create single-file worktree root");

        let wrap = SandboxWrap {
            writable_paths: vec![writable],
            protected_paths: vec![
                temp_dir.path().join("no-such-.git"),
                single_file_root.join(".git"),
            ],
            ..Default::default()
        };

        wrap.to_policy()
            .expect("uncapturable protected paths must be dropped, not fail the policy");
    }
}
