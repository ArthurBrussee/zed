//! Dev servers from a thread's bottom bar, driven by tagged Zed tasks.
//!
//! A repository opts in from its own `.zed/tasks.json` (or the user's global
//! tasks), with no schema of the fork's own: a task tagged `bar` gets a button
//! in the bottom bar of every thread whose worktree has it, resolved against
//! *that* worktree, so each worktree runs its own server.
//!
//! ```json
//! { "label": "portico", "command": "pnpm dev", "cwd": "$ZED_WORKTREE_ROOT/portico",
//!   "reveal": "never", "tags": ["bar", "icon:~/.config/zed/icons/atrium.png",
//!   "open:atrium://portico.monumental.build/?_port={port}"] }
//! ```
//!
//! A task whose `cwd` is missing from a worktree gets no button there. `icon:`
//! is a Zed icon name or an image file (an `.svg` is tinted like the other
//! glyphs). `open:` is a path appended to the detected URL or a full URL of any
//! scheme, and `{port}` in it is the port the server printed.

use std::{
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
    time::Duration,
};

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task, WeakEntity, Window};
use project::{Project, TaskSourceKind, WorktreeId};
use util::rel_path::RelPath;
use remote::RemoteClient;
use task::{TaskContext, TaskTemplate, TaskVariables, VariableName};
use terminal::Terminal;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Icon, IconName, SharedString};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// The tag that earns a task a button.
pub const BAR_TAG: &str = "bar";
const ICON_PREFIX: &str = "icon:";
const OPEN_PREFIX: &str = "open:";
/// Enough of the tail to hold the line a dev server prints its URL on.
const PORT_SCAN_LINES: usize = 50;
/// `ssh -L` has no handshake to wait on, so the tunnel gets a moment before the
/// browser is pointed at it.
const FORWARD_SETTLE: Duration = Duration::from_millis(300);

/// What a dev server's button and pill wear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevServerGlyph {
    Named(IconName),
    /// Tinted like the other glyphs.
    Svg(SharedString),
    /// Drawn as it is.
    Image(SharedString),
}

impl DevServerGlyph {
    /// An icon name, else a file: absolute, `~`-relative or under
    /// `$ZED_WORKTREE_ROOT`, read on this machine. Anything else is the
    /// default glyph.
    fn parse(tag: &str, worktree_root: &Path) -> Option<Self> {
        if let Ok(name) = IconName::from_str(tag) {
            return Some(Self::Named(name));
        }
        let path = expand_worktree_root(tag, worktree_root);
        let path = match path.strip_prefix("~/") {
            Some(rest) => util::paths::home_dir().join(rest),
            None => PathBuf::from(path),
        };
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        let is_svg = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
        let path = SharedString::from(path.to_string_lossy().into_owned());
        Some(if is_svg {
            Self::Svg(path)
        } else {
            Self::Image(path)
        })
    }

    pub fn icon(&self) -> Icon {
        match self {
            Self::Named(name) => Icon::new(*name),
            Self::Svg(path) => Icon::from_external_svg(path.clone()),
            Self::Image(path) => Icon::from_path(path.clone()),
        }
    }

    /// An image keeps its own colours.
    pub fn is_tinted(&self) -> bool {
        !matches!(self, Self::Image(_))
    }
}

/// A `bar` task, with what its other tags say about it.
#[derive(Clone, Debug)]
pub struct DevServerTask {
    pub label: SharedString,
    pub glyph: DevServerGlyph,
    /// What opening it goes to: a path appended to the detected URL, or a URL
    /// used as it stands. `{port}` is the detected port in either.
    pub open: Option<String>,
    source_kind: TaskSourceKind,
    template: TaskTemplate,
}

impl DevServerTask {
    fn new(source_kind: TaskSourceKind, template: TaskTemplate, worktree_root: &Path) -> Self {
        let mut glyph = DevServerGlyph::Named(IconName::Server);
        let mut open = None;
        for tag in &template.tags {
            if let Some(icon) = tag.strip_prefix(ICON_PREFIX) {
                if let Some(parsed) = DevServerGlyph::parse(icon, worktree_root) {
                    glyph = parsed;
                }
            } else if let Some(target) = tag.strip_prefix(OPEN_PREFIX) {
                open = Some(target.to_string());
            }
        }
        Self {
            label: template.label.clone().into(),
            glyph,
            open,
            source_kind,
            template,
        }
    }
}

/// Every `bar` task a worktree offers, the repository's and the user's own,
/// less those whose `cwd` that worktree does not have: a global task for one
/// repository's folder would only fail everywhere else.
pub fn bar_tasks(project: &Entity<Project>, worktree: WorktreeId, cx: &App) -> Vec<DevServerTask> {
    let project = project.read(cx);
    let Some(inventory) = project.task_store().read(cx).task_inventory().cloned() else {
        return Vec::new();
    };
    let Some(worktree) = project.worktree_for_id(worktree, cx) else {
        return Vec::new();
    };
    let worktree = worktree.read(cx);
    let root = worktree.abs_path();
    let path_style = project.path_style(cx);
    inventory
        .read(cx)
        .templates_with_tag(BAR_TAG, worktree.id())
        .into_iter()
        .filter(|(_, template)| {
            let Some(relative) = cwd_in_worktree(template.cwd.as_deref(), &root) else {
                return true;
            };
            RelPath::new(&relative, path_style)
                .ok()
                .and_then(|relative| worktree.entry_for_path(&relative))
                .is_some_and(|entry| entry.is_dir())
        })
        .map(|(source_kind, template)| DevServerTask::new(source_kind, template, &root))
        .collect()
}

/// A task's `cwd` relative to the worktree root, when it lies inside the
/// worktree. `None` for no `cwd`, or one this worktree cannot say anything
/// about (outside it, or naming variables other than the root).
fn cwd_in_worktree(cwd: Option<&str>, worktree_root: &Path) -> Option<PathBuf> {
    let cwd = expand_worktree_root(cwd?, worktree_root);
    if cwd.contains('$') {
        return None;
    }
    let cwd = Path::new(&cwd);
    if cwd.is_relative() {
        return Some(cwd.to_path_buf());
    }
    cwd.strip_prefix(worktree_root).ok().map(Path::to_path_buf)
}

fn expand_worktree_root(text: &str, worktree_root: &Path) -> String {
    let root = worktree_root.to_string_lossy();
    text.replace("${ZED_WORKTREE_ROOT}", &root)
        .replace("$ZED_WORKTREE_ROOT", &root)
}

/// A dev server is one task running in one worktree, so two worktrees of the
/// same repository each get their own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DevServerKey {
    worktree_root: Arc<Path>,
    label: SharedString,
}

impl DevServerKey {
    pub fn new(worktree_root: &Path, label: &SharedString) -> Self {
        Self {
            worktree_root: worktree_root.into(),
            label: label.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevServerState {
    /// Spawning, or running without having printed a URL yet.
    Starting,
    Running {
        port: Option<u16>,
    },
    Exited {
        code: Option<i32>,
    },
}

#[derive(Default)]
struct DevServer {
    state: Option<DevServerState>,
    /// The task's, for the sidebar's pill.
    glyph: Option<DevServerGlyph>,
    terminal: Option<WeakEntity<Terminal>>,
    /// Which machine it runs on; a box's port needs forwarding before it opens.
    remote: Option<Entity<RemoteClient>>,
    /// The `ssh -N -L` child that makes a box's port reachable here. Held for
    /// as long as this server runs, and killed when it is dropped.
    forward: Option<(u16, util::command::Child)>,
    _output: Option<Subscription>,
    _exit: Option<Task<()>>,
}

#[derive(Default)]
pub struct DevServerStore {
    servers: HashMap<DevServerKey, DevServer>,
}

struct GlobalDevServerStore(Entity<DevServerStore>);

impl Global for GlobalDevServerStore {}

impl DevServerStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalDevServerStore>() {
            let store = cx.new(|_| DevServerStore::default());
            cx.set_global(GlobalDevServerStore(store));
        }
        cx.global::<GlobalDevServerStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDevServerStore>()
            .map(|global| global.0.clone())
    }

    pub fn state(&self, key: &DevServerKey) -> Option<&DevServerState> {
        self.servers.get(key)?.state.as_ref()
    }

    /// What the sidebar row shows, read-only: a pill per server running for
    /// these worktrees, the task's glyph and its port once it is known, else
    /// the task's label. Interaction stays in the bottom bar.
    pub fn row_pills(&self, worktree_roots: &[PathBuf]) -> Vec<(DevServerGlyph, SharedString)> {
        let mut pills: Vec<(DevServerGlyph, SharedString)> = self
            .servers
            .iter()
            .filter(|(key, _)| {
                worktree_roots
                    .iter()
                    .any(|root| root.as_path() == key.worktree_root.as_ref())
            })
            .filter_map(|(key, server)| {
                let label = match server.state {
                    Some(DevServerState::Running { port: Some(port) }) => {
                        SharedString::from(port.to_string())
                    }
                    Some(DevServerState::Running { port: None } | DevServerState::Starting) => {
                        key.label.clone()
                    }
                    _ => return None,
                };
                let glyph = server
                    .glyph
                    .clone()
                    .unwrap_or(DevServerGlyph::Named(IconName::Server));
                Some((glyph, label))
            })
            .collect();
        pills.sort_by(|(_, a), (_, b)| a.cmp(b));
        pills
    }

    pub fn start(
        &mut self,
        task: &DevServerTask,
        worktree_root: &Path,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = DevServerKey::new(worktree_root, &task.label);
        if matches!(
            self.state(&key),
            Some(DevServerState::Starting | DevServerState::Running { .. })
        ) {
            return;
        }
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            log::warn!(
                "quiet-ui dev server: no terminal panel to run {} in",
                task.label
            );
            return;
        };
        let Some(resolved) = task.template.resolve_task(
            &task.source_kind.to_id_base(),
            &worktree_task_context(worktree_root),
        ) else {
            log::warn!("quiet-ui dev server: {} did not resolve", task.label);
            return;
        };
        let remote = workspace.read(cx).project().read(cx).remote_client();

        let server = self.servers.entry(key.clone()).or_default();
        server.state = Some(DevServerState::Starting);
        server.glyph = Some(task.glyph.clone());
        server.remote = remote;
        cx.notify();

        let spawn = terminal_panel.update(cx, |panel, cx| {
            panel.spawn_task(&resolved.resolved, window, cx)
        });
        let watcher = cx.spawn_in(window, {
            let key = key.clone();
            async move |this, cx| {
                let terminal = spawn.await;
                this.update(cx, |this, cx| match terminal {
                    Ok(terminal) => match terminal.upgrade() {
                        Some(terminal) => this.attach(key, terminal, cx),
                        None => this.note_exit(&key, None, cx),
                    },
                    Err(error) => {
                        log::error!("quiet-ui dev server: spawn failed: {error:#}");
                        this.note_exit(&key, None, cx);
                    }
                })
                .ok();
            }
        });
        if let Some(server) = self.servers.get_mut(&key) {
            server._exit = Some(watcher);
        }
    }

    /// Stops the task. The server's own terminal stays where it is, so its
    /// output is still there to read.
    pub fn stop(&mut self, key: &DevServerKey, cx: &mut Context<Self>) {
        let Some(server) = self.servers.get_mut(key) else {
            return;
        };
        if let Some(terminal) = server.terminal.as_ref().and_then(|t| t.upgrade()) {
            terminal.update(cx, |terminal, _cx| terminal.kill_active_task());
        } else {
            server.state = Some(DevServerState::Exited { code: None });
        }
        server.forward = None;
        cx.notify();
    }

    /// Opens what this server serves. On a dev box the port is the box's, so it
    /// is forwarded through the connection the project already has first, and
    /// the forwarded port is the one `{port}` stands for.
    pub fn open(
        &mut self,
        key: &DevServerKey,
        task: &DevServerTask,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let target = task.open.clone();
        // A full URL that does not need the port opens as it stands.
        if let Some(url) = target
            .as_deref()
            .filter(|target| is_full_url(target) && !target.contains(PORT_PLACEHOLDER))
        {
            cx.open_url(url);
            return;
        }
        let Some(DevServerState::Running { port: Some(port) }) = self.state(key).cloned() else {
            toast(
                workspace,
                format!("{} has not said which port it is on yet", task.label),
                cx,
            );
            return;
        };
        let Some(remote) = self
            .servers
            .get(key)
            .and_then(|server| server.remote.clone())
        else {
            cx.open_url(&open_url(target.as_deref(), port));
            return;
        };

        // A forward already standing is reused, so opening twice does not pile
        // up tunnels.
        if let Some((local_port, _)) = self.servers.get(key).and_then(|s| s.forward.as_ref()) {
            cx.open_url(&open_url(target.as_deref(), *local_port));
            return;
        }

        let forward = match spawn_port_forward(&remote, port, cx) {
            Ok(forward) => forward,
            Err(error) => {
                log::error!("quiet-ui dev server: forwarding {port} failed: {error:#}");
                toast(
                    workspace,
                    format!(
                        "Could not forward port {port} from the dev box, so {} is not reachable \
                         from here",
                        task.label
                    ),
                    cx,
                );
                return;
            }
        };
        let local_port = forward.0;
        if let Some(server) = self.servers.get_mut(key) {
            server.forward = Some(forward);
        }
        cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(FORWARD_SETTLE).await;
            cx.update(|cx| cx.open_url(&open_url(target.as_deref(), local_port)));
        })
        .detach();
    }

    fn attach(&mut self, key: DevServerKey, terminal: Entity<Terminal>, cx: &mut Context<Self>) {
        let output = cx.subscribe(&terminal, {
            let key = key.clone();
            move |this, terminal, event: &terminal::Event, cx| {
                if matches!(
                    event,
                    terminal::Event::Wakeup | terminal::Event::OutputReplaced
                ) {
                    let text = tail(&terminal, cx);
                    this.note_output(&key, &text, cx);
                }
            }
        });
        let completed = terminal.read(cx).wait_for_completed_task(cx);
        let exit = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let status = completed.await;
                this.update(cx, |this, cx| {
                    this.note_exit(&key, status.and_then(|status| status.code()), cx)
                })
                .ok();
            }
        });

        let server = self.servers.entry(key.clone()).or_default();
        server.state = Some(DevServerState::Starting);
        server.terminal = Some(terminal.downgrade());
        server._output = Some(output);
        server._exit = Some(exit);

        // Output can arrive before the subscription is in place.
        let text = tail(&terminal, cx);
        self.note_output(&key, &text, cx);
        cx.notify();
    }

    /// Reads the port out of what the server printed. Public because that is
    /// the whole of what a fake terminal has to do to be one.
    pub fn note_output(&mut self, key: &DevServerKey, output: &str, cx: &mut Context<Self>) {
        let Some(server) = self.servers.get_mut(key) else {
            return;
        };
        if !matches!(
            server.state,
            Some(DevServerState::Starting | DevServerState::Running { port: None })
        ) {
            return;
        }
        match detect_port(output) {
            Some(port) => server.state = Some(DevServerState::Running { port: Some(port) }),
            None => server.state = Some(DevServerState::Running { port: None }),
        }
        cx.notify();
    }

    /// Marks a server as starting before its terminal exists, which is what
    /// [`Self::start`] does before it spawns one.
    #[cfg(test)]
    fn mark_starting(&mut self, key: &DevServerKey, cx: &mut Context<Self>) {
        self.servers.entry(key.clone()).or_default().state = Some(DevServerState::Starting);
        cx.notify();
    }

    fn note_exit(&mut self, key: &DevServerKey, code: Option<i32>, cx: &mut Context<Self>) {
        let Some(server) = self.servers.get_mut(key) else {
            return;
        };
        server.state = Some(DevServerState::Exited { code });
        server.forward = None;
        cx.notify();
    }
}

fn tail(terminal: &Entity<Terminal>, cx: &App) -> String {
    terminal
        .read(cx)
        .last_n_non_empty_lines(PORT_SCAN_LINES)
        .join("\n")
}

const PORT_PLACEHOLDER: &str = "{port}";

/// Where opening a server goes: `open:` as a full URL of any scheme, or as a
/// path on the detected URL, with `{port}` the port in either.
fn open_url(target: Option<&str>, port: u16) -> String {
    let target = target
        .unwrap_or_default()
        .replace(PORT_PLACEHOLDER, &port.to_string());
    if is_full_url(&target) {
        target
    } else {
        format!("http://localhost:{port}{target}")
    }
}

/// `scheme://…`, the scheme being RFC 3986's letters, digits, `+`, `-` and `.`.
fn is_full_url(target: &str) -> bool {
    target.split_once("://").is_some_and(|(scheme, _)| {
        scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

fn toast(workspace: &Entity<Workspace>, message: String, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<DevServerStore>(), message),
            cx,
        );
    });
}

/// `$ZED_WORKTREE_ROOT` is what makes one task serve every worktree.
fn worktree_task_context(worktree_root: &Path) -> TaskContext {
    let mut task_variables = TaskVariables::default();
    task_variables.insert(
        VariableName::WorktreeRoot,
        worktree_root.to_string_lossy().into_owned(),
    );
    TaskContext {
        cwd: Some(worktree_root.to_path_buf()),
        task_variables,
        project_env: Default::default(),
    }
}

fn spawn_port_forward(
    remote: &Entity<RemoteClient>,
    remote_port: u16,
    cx: &App,
) -> anyhow::Result<(u16, util::command::Child)> {
    // Let the OS pick, so two worktrees' servers never want the same local port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let local_port = listener.local_addr()?.port();
    drop(listener);

    let template = remote.read(cx).build_forward_ports_command(vec![(
        local_port,
        "127.0.0.1".to_string(),
        remote_port,
    )])?;
    let mut command = util::command::new_command(&template.program);
    command
        .args(&template.args)
        .envs(&template.env)
        .kill_on_drop(true);
    let child = command.spawn()?;
    Ok((local_port, child))
}

/// The first port a dev server prints its URL on. Vite moves to 5174 when 5173
/// is taken by another worktree, so the port is read rather than assumed.
pub fn detect_port(output: &str) -> Option<u16> {
    const HOSTS: [&str; 3] = ["localhost:", "127.0.0.1:", "0.0.0.0:"];
    for (index, _) in output.char_indices() {
        let rest = &output[index..];
        for host in HOSTS {
            let Some(after) = rest.strip_prefix(host) else {
                continue;
            };
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(port) = digits.parse::<u16>()
                && port != 0
            {
                return Some(port);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_port_comes_from_what_the_server_printed() {
        assert_eq!(
            detect_port("  ➜  Local:   http://localhost:5174/\n"),
            Some(5174),
            "vite's own line, after it moved off a taken port"
        );
        assert_eq!(
            detect_port("Listening on http://127.0.0.1:3000"),
            Some(3000)
        );
        assert_eq!(detect_port("server bound to 0.0.0.0:8080"), Some(8080));
        assert_eq!(
            detect_port("Local: http://localhost:5173/\nNetwork: http://localhost:5174/"),
            Some(5173),
            "the first one it printed, not the last"
        );
        assert_eq!(detect_port("$ pnpm dev\nstarting..."), None);
        assert_eq!(
            detect_port("bound to localhost:0"),
            None,
            "port 0 is what a listener says before the OS picks one"
        );
        assert_eq!(
            detect_port("localhost:99999"),
            None,
            "not a port at all, rather than a wrapped one"
        );
    }

    fn labels(pills: Vec<(DevServerGlyph, SharedString)>) -> Vec<String> {
        pills.into_iter().map(|(_, label)| label.to_string()).collect()
    }

    #[gpui::test]
    fn test_each_worktree_runs_its_own_server(cx: &mut gpui::App) {
        let store = cx.new(|_| DevServerStore::default());
        let label = SharedString::from("portico");
        let here = DevServerKey::new(Path::new("/worktrees/a"), &label);
        let there = DevServerKey::new(Path::new("/worktrees/b"), &label);
        let a = [PathBuf::from("/worktrees/a")];
        let b = [PathBuf::from("/worktrees/b")];

        store.update(cx, |store, cx| {
            store.mark_starting(&here, cx);
            store.mark_starting(&there, cx);
        });
        assert_eq!(store.read(cx).state(&here), Some(&DevServerState::Starting));
        assert_eq!(
            labels(store.read(cx).row_pills(&a)),
            vec![label.to_string()],
            "before a port is known the row says which task it is"
        );

        // Vite moves off 5173 because the other worktree's server has it.
        store.update(cx, |store, cx| {
            store.note_output(&here, "  Local:   http://localhost:5173/\n", cx);
            store.note_output(&there, "  Local:   http://localhost:5174/\n", cx);
        });
        assert_eq!(
            store.read(cx).state(&here),
            Some(&DevServerState::Running { port: Some(5173) })
        );
        assert_eq!(labels(store.read(cx).row_pills(&a)), vec!["5173"]);
        assert_eq!(labels(store.read(cx).row_pills(&b)), vec!["5174"]);

        store.update(cx, |store, cx| {
            store.note_output(&there, "proxying to http://localhost:9999/api", cx);
        });
        assert_eq!(
            store.read(cx).state(&there),
            Some(&DevServerState::Running { port: Some(5174) }),
            "the port it started on, not every URL it goes on to print"
        );

        // Stopping one leaves the other running.
        store.update(cx, |store, cx| store.stop(&here, cx));
        assert_eq!(
            store.read(cx).state(&here),
            Some(&DevServerState::Exited { code: None })
        );
        assert_eq!(
            labels(store.read(cx).row_pills(&a)),
            Vec::<String>::new(),
            "a stopped server says nothing on the row"
        );
        assert_eq!(labels(store.read(cx).row_pills(&b)), vec!["5174"]);
    }

    #[gpui::test]
    fn test_an_exited_server_says_how_it_went(cx: &mut gpui::App) {
        let store = cx.new(|_| DevServerStore::default());
        let key = DevServerKey::new(Path::new("/worktrees/a"), &SharedString::from("portico"));
        store.update(cx, |store, cx| {
            store.mark_starting(&key, cx);
            store.note_output(&key, "Local: http://localhost:5173/", cx);
            store.note_exit(&key, Some(1), cx);
        });
        assert_eq!(
            store.read(cx).state(&key),
            Some(&DevServerState::Exited { code: Some(1) })
        );
        store.update(cx, |store, cx| {
            store.note_output(&key, "Local: http://localhost:5173/", cx);
        });
        assert_eq!(
            store.read(cx).state(&key),
            Some(&DevServerState::Exited { code: Some(1) }),
            "output from an exited terminal does not revive it"
        );
    }

    #[test]
    fn test_only_a_bar_task_gets_a_button() {
        let bar = TaskTemplate {
            label: "portico".into(),
            command: "pnpm dev".into(),
            tags: vec!["bar".into(), "icon:server".into(), "open:/".into()],
            ..Default::default()
        };
        let root = Path::new("/worktrees/a");
        let task = DevServerTask::new(TaskSourceKind::UserInput, bar.clone(), root);
        assert_eq!(task.label.as_ref(), "portico");
        assert_eq!(task.glyph, DevServerGlyph::Named(IconName::Server));
        assert_eq!(task.open.as_deref(), Some("/"));

        let unknown_icon = TaskTemplate {
            tags: vec!["bar".into(), "icon:not-an-icon".into()],
            ..bar.clone()
        };
        assert_eq!(
            DevServerTask::new(TaskSourceKind::UserInput, unknown_icon, root).glyph,
            DevServerGlyph::Named(IconName::Server),
            "a glyph nobody has falls back rather than failing the button"
        );

        let bare = TaskTemplate {
            tags: vec!["bar".into()],
            ..bar
        };
        let task = DevServerTask::new(TaskSourceKind::UserInput, bare, root);
        assert_eq!(task.glyph, DevServerGlyph::Named(IconName::Server));
        assert_eq!(
            task.open, None,
            "without `open:`, Open goes to the detected URL itself"
        );
    }

    /// Arthur's log: `http://localhost:3000atrium://portico.monumental.build/?_port={port}`.
    #[test]
    fn test_open_takes_a_url_of_any_scheme_and_the_port() {
        assert_eq!(
            open_url(
                Some("atrium://portico.monumental.build/?_port={port}"),
                3000
            ),
            "atrium://portico.monumental.build/?_port=3000"
        );
        assert_eq!(open_url(Some("/docs"), 5173), "http://localhost:5173/docs");
        assert_eq!(open_url(None, 5173), "http://localhost:5173");
        assert_eq!(
            open_url(Some("/?port={port}"), 41234),
            "http://localhost:41234/?port=41234",
            "a forwarded port stands for {{port}} in a path too"
        );
        assert_eq!(
            open_url(Some("https://example.com/app"), 1),
            "https://example.com/app"
        );
        assert!(!is_full_url("/a://b"), "a path is not a scheme");
        assert!(!is_full_url("1x://b"));
    }

    #[test]
    fn test_a_task_shows_only_where_its_folder_is() {
        let root = Path::new("/worktrees/a");
        assert_eq!(
            cwd_in_worktree(Some("$ZED_WORKTREE_ROOT/portico"), root),
            Some(PathBuf::from("portico"))
        );
        assert_eq!(
            cwd_in_worktree(Some("${ZED_WORKTREE_ROOT}/web/app"), root),
            Some(PathBuf::from("web/app"))
        );
        assert_eq!(cwd_in_worktree(Some("portico"), root), Some(PathBuf::from("portico")));
        assert_eq!(cwd_in_worktree(None, root), None, "no cwd always shows");
        assert_eq!(
            cwd_in_worktree(Some("/elsewhere/portico"), root),
            None,
            "outside the worktree is not this worktree's to judge"
        );
        assert_eq!(cwd_in_worktree(Some("$HOME/portico"), root), None);
    }

    #[gpui::test]
    async fn test_a_global_bar_task_needs_its_folder(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use project::Project;
        use serde_json::json;
        use settings::SettingsStore;

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({ "with": { "portico": { "package.json": "{}" } }, "without": { "src": {} } }),
        )
        .await;
        let project = Project::test(
            fs,
            [Path::new("/root/with"), Path::new("/root/without")],
            cx,
        )
        .await;
        cx.run_until_parked();
        let inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .unwrap()
        });
        inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    project::task_store::TaskSettingsLocation::Global(Path::new("/config/tasks.json")),
                    Some(
                        &json!([{
                            "label": "portico",
                            "command": "pnpm dev",
                            "cwd": "$ZED_WORKTREE_ROOT/portico",
                            "tags": ["bar", "open:atrium://portico.monumental.build/?_port={port}"],
                        }, {
                            "label": "anywhere",
                            "command": "make serve",
                            "tags": ["bar"],
                        }])
                        .to_string(),
                    ),
                )
                .unwrap();
        });
        let buttons = |root: &str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let worktree = project
                    .read(cx)
                    .worktrees(cx)
                    .find(|worktree| worktree.read(cx).abs_path().as_ref() == Path::new(root))
                    .unwrap()
                    .read(cx)
                    .id();
                let mut labels: Vec<String> = bar_tasks(&project, worktree, cx)
                    .into_iter()
                    .map(|task| task.label.to_string())
                    .collect();
                labels.sort();
                labels
            })
        };
        assert_eq!(buttons("/root/with", cx), vec!["anywhere", "portico"]);
        assert_eq!(
            buttons("/root/without", cx),
            vec!["anywhere"],
            "no portico folder, no portico button; a task with no cwd still shows"
        );
    }
}
