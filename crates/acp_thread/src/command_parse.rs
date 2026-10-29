//! A shallow shell-command reader: separators, quoting, heredocs, redirects,
//! ssh and devshell wrappers. Everything else stays opaque argument text; it
//! never executes anything or resolves paths.

use std::ops::Range;

/// One command line, split into the pipelines that actually run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommand {
    /// The machine this runs on, when every working segment ran there.
    pub host: Option<String>,
    /// The devshell the line's work ran in, when the segments that used one
    /// agree on which.
    pub environment: Option<String>,
    pub segments: Vec<CommandSegment>,
}

/// One pipeline (`a | b | c`) of a command line, with its source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSegment {
    /// Verbatim, so the UI shows a real substring of what ran.
    pub text: String,
    pub kind: SegmentKind,
    pub host: Option<String>,
    /// The Nix devshell this segment ran in (`nix develop .#name --command …`).
    pub environment: Option<String>,
}

/// What a pipeline is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// Changing directory, setting variables: real syntax, no work.
    Noop,
    Read {
        paths: Vec<String>,
        lines: Option<Range<u32>>,
        /// For `git show rev:path`; the working copy is `None`.
        revision: Option<String>,
    },
    Search {
        query: Option<String>,
    },
    ListDirectory {
        path: Option<String>,
    },
    /// `command -v`, `which`, `type`.
    Lookup {
        program: Option<String>,
    },
    CountLines {
        paths: Vec<String>,
    },
    Git {
        operation: GitOperation,
        target: Option<String>,
    },
    /// `contents` is present for heredocs.
    WriteFile {
        path: String,
        contents: Option<String>,
    },
    /// `sed -i`, `perl -pi -e`.
    EditInPlace {
        paths: Vec<String>,
    },
    Destructive {
        operation: DestructiveOperation,
        paths: Vec<String>,
    },
    /// `python -c`, `node -e`, `bash -c`.
    InlineScript {
        interpreter: String,
        code: String,
    },
    Wait {
        seconds: Option<u32>,
    },
    GitHub {
        operation: String,
        target: Option<String>,
    },
    Run {
        program: String,
        argument: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestructiveOperation {
    Delete,
    Move,
    ChangePermissions,
    /// Throwing away work in git (`reset --hard`, `clean -fd`, `checkout --`).
    DiscardChanges,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitOperation {
    /// `diff`, `show`, `log -p`.
    ReadChanges,
    /// `status`, `log`, `branch`, `blame`.
    Inspect,
    Modify,
}

impl SegmentKind {
    pub fn is_noop(&self) -> bool {
        matches!(self, SegmentKind::Noop)
    }

    /// Waiting is kept in the classification but is never one of the acts a
    /// chip names.
    pub fn is_worth_naming(&self) -> bool {
        !self.is_noop() && !matches!(self, SegmentKind::Wait { .. })
    }

    /// What a captured `$( … )` keeps: `f=$(grep -rln sym src)` searched the
    /// tree, while `t=$(date +%s)` touched nothing.
    fn touched_the_project(&self) -> bool {
        match self {
            SegmentKind::Read { .. }
            | SegmentKind::Search { .. }
            | SegmentKind::ListDirectory { .. }
            | SegmentKind::CountLines { .. }
            | SegmentKind::Git { .. }
            | SegmentKind::GitHub { .. }
            | SegmentKind::WriteFile { .. }
            | SegmentKind::EditInPlace { .. }
            | SegmentKind::Destructive { .. } => true,
            SegmentKind::Noop
            | SegmentKind::Lookup { .. }
            | SegmentKind::Wait { .. }
            | SegmentKind::InlineScript { .. }
            | SegmentKind::Run { .. } => false,
        }
    }

    /// A search downstream of one of these is the point of the line: `ps -ax |
    /// rg cargo` is a search, `cargo check | grep '^error'` stays a build.
    fn is_just_looking(&self) -> bool {
        match self {
            SegmentKind::Noop
            | SegmentKind::Read { .. }
            | SegmentKind::ListDirectory { .. }
            | SegmentKind::Lookup { .. }
            | SegmentKind::CountLines { .. } => true,
            SegmentKind::Run { program, .. } => INSPECTION_PROGRAMS.contains(&program.as_str()),
            _ => false,
        }
    }
}

/// Programs that only report state and cannot write (so `jq` but not `yq`,
/// which has `-i`).
const INSPECTION_PROGRAMS: &[&str] = &[
    "date", "df", "env", "free", "hostname", "id", "printenv", "ps", "top", "uname", "uptime",
    "who", "whoami", "basename", "dirname", "jq", "realpath",
];

/// Anything unlisted reads as work: an unrecognised look-around costs an extra
/// chip, an unrecognised action would hide one. `api` is absent because it can
/// POST.
const GITHUB_READ_ONLY_ACTIONS: &[&str] = &["view", "list", "status", "checks", "diff", "watch"];

fn github_is_read_only(operation: &str) -> bool {
    operation
        .split_whitespace()
        .nth(1)
        .is_some_and(|action| GITHUB_READ_ONLY_ACTIONS.contains(&action))
}

/// Programs whose first argument is a subcommand, so both words name what ran.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "apt",
    "apt-get",
    "aws",
    "brew",
    "bun",
    "bundle",
    "cargo",
    "conda",
    "deno",
    "docker",
    "dotnet",
    "flutter",
    "gcloud",
    "go",
    "gradle",
    "helm",
    "jest",
    "just",
    "kubectl",
    "make",
    "mix",
    "mvn",
    "nix",
    "npm",
    "pip",
    "pip3",
    "playwright",
    "pnpm",
    "poetry",
    "rake",
    "ruff",
    "rustup",
    "systemctl",
    "task",
    "terraform",
    "ty",
    "uv",
    "vite",
    "vitest",
    "yarn",
];

/// The language whose toolchain a program belongs to, for its logo.
pub fn program_language(program: &str) -> Option<&'static str> {
    let program = base_name(program);
    Some(match program {
        "cargo" | "rustc" | "rustup" | "rustfmt" | "cross" | "clippy-driver" => "rust",
        "python" | "python3" | "pytest" | "pip" | "pip3" | "uv" | "uvx" | "poetry" | "ruff"
        | "ty" | "mypy" | "black" | "conda" | "tox" | "flake8" => "python",
        "node" | "npm" | "npx" | "pnpm" | "yarn" | "bun" | "bunx" => "javascript",
        "tsc" | "ts-node" | "tsx" | "deno" | "vite" | "vitest" | "jest" | "eslint" | "prettier" => {
            "typescript"
        }
        "go" | "gofmt" | "golangci-lint" => "go",
        "ruby" | "bundle" | "bundler" | "gem" | "rake" | "rails" | "rspec" => "ruby",
        "java" | "javac" | "gradle" | "gradlew" | "mvn" | "maven" => "java",
        "kotlin" | "kotlinc" => "kotlin",
        "swift" | "swiftc" | "xcodebuild" => "swift",
        "php" | "composer" => "php",
        "elixir" | "mix" | "iex" => "elixir",
        "dart" | "flutter" => "dart",
        "lua" | "luajit" => "lua",
        "zig" => "zig",
        "docker" | "docker-compose" | "podman" => "docker",
        "terraform" | "tofu" => "terraform",
        _ => return None,
    })
}

impl CommandSegment {
    /// The segment without its ssh and devshell wrappers, which are reported
    /// on their own.
    pub fn work_text(&self) -> &str {
        let local = unwrap_ssh(&self.text).command;
        match nix_devshell_command(local) {
            Some((_, inner)) => inner,
            None => local,
        }
    }

    /// A compact name for what this segment did, for when several segments
    /// share one chip.
    pub fn short_label(&self) -> String {
        match &self.kind {
            SegmentKind::Read { paths, .. } => match paths.first() {
                Some(path) => base_name(path).to_string(),
                None => "read".to_string(),
            },
            SegmentKind::Search { query } => match query {
                Some(query) => query.clone(),
                None => "search".to_string(),
            },
            SegmentKind::ListDirectory { path } => match path {
                Some(path) => base_name(path).to_string(),
                None => "ls".to_string(),
            },
            SegmentKind::Lookup { program } => match program {
                Some(program) => format!("which {program}"),
                None => "which".to_string(),
            },
            SegmentKind::CountLines { paths } => match paths.first() {
                Some(path) => format!("wc {}", base_name(path)),
                None => "wc".to_string(),
            },
            SegmentKind::Git { target, .. } => match target {
                Some(target) => format!("git {target}"),
                None => "git".to_string(),
            },
            SegmentKind::GitHub { operation, .. } => format!("gh {operation}"),
            SegmentKind::WriteFile { path, .. } => base_name(path).to_string(),
            SegmentKind::EditInPlace { paths } => match paths.first() {
                Some(path) => base_name(path).to_string(),
                None => "edit".to_string(),
            },
            SegmentKind::InlineScript { interpreter, .. } => format!("{interpreter} script"),
            SegmentKind::Wait { .. } => "wait".to_string(),
            // Which files were about to go is the point of noticing one.
            SegmentKind::Destructive { .. } => first_words(self.work_text(), 4),
            SegmentKind::Run { program, argument } => {
                let name = base_name(program);
                match argument.as_deref() {
                    Some(argument) if SUBCOMMAND_PROGRAMS.contains(&name) => {
                        match package_arguments(self.work_text()).as_slice() {
                            [] => format!("{name} {argument}"),
                            [only] => format!("{name} {argument} {only}"),
                            [first, rest @ ..] => {
                                format!("{name} {argument} {first} +{}", rest.len())
                            }
                        }
                    }
                    // An argument with whitespace is a program (a jq filter),
                    // which names nothing, so the line's path is used instead.
                    Some(argument) => match url_host(argument) {
                        Some(host) => format!("{name} {host}"),
                        None if argument.contains(char::is_whitespace) => {
                            match path_argument(self.work_text()) {
                                Some(path) => format!("{name} {}", base_name(&path)),
                                None => name.to_string(),
                            }
                        }
                        None if argument.contains(['/', '\\']) => {
                            format!("{name} {}", base_name(argument))
                        }
                        None => format!("{name} {argument}"),
                    },
                    None => name.to_string(),
                }
            }
            SegmentKind::Noop => first_words(self.work_text(), 3),
        }
    }

    pub fn language(&self) -> Option<&'static str> {
        match &self.kind {
            SegmentKind::Run { program, .. } => program_language(program),
            SegmentKind::InlineScript { interpreter, .. } => program_language(interpreter),
            _ => None,
        }
    }
}

/// How a whole command reads, for the chip-collapsing rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    Search,
    Read,
    ReadDiff,
    GitInfo,
    GitHub,
    /// `ps`, `df`, `jq .version package.json`.
    Inspect,
    Other,
}

pub fn parse_command(command: &str) -> ParsedCommand {
    let segments: Vec<CommandSegment> = split_segments(command)
        .into_iter()
        .flat_map(|text| {
            // `f=$(grep -rln sym src | head -1)` searched the tree: the
            // captured command stands in for the assignment.
            if let Some(captured) = captured_command(&text) {
                let captured: Vec<CommandSegment> = parse_command(captured)
                    .segments
                    .into_iter()
                    .filter(|segment| segment.kind.touched_the_project())
                    .collect();
                if !captured.is_empty() {
                    return captured;
                }
            }

            let unwrapped = unwrap_ssh(&text);
            let host = unwrapped.host.clone();
            let environment = nix_devshell(unwrapped.command);

            let local = match nix_devshell_command(unwrapped.command) {
                Some((_, inner)) => inner,
                None => unwrapped.command,
            };

            // `bash -c "cd x && cargo check"` ran several commands.
            let inner = shell_payload(local)
                .map(|payload| split_segments(&payload))
                .filter(|segments| segments.len() > 1);
            if let Some(inner) = inner {
                return inner
                    .into_iter()
                    .map(|text| CommandSegment {
                        kind: classify_segment(&text),
                        text,
                        host: host.clone(),
                        environment: environment.clone(),
                    })
                    .collect::<Vec<_>>();
            }

            vec![CommandSegment {
                kind: classify_segment(unwrapped.command),
                text,
                host,
                environment,
            }]
        })
        .collect();

    let host = {
        let mut hosts = segments
            .iter()
            .filter(|segment| !segment.kind.is_noop())
            .map(|segment| segment.host.clone());
        match hosts.next() {
            Some(Some(first)) if hosts.all(|other| other.as_deref() == Some(first.as_str())) => {
                Some(first)
            }
            _ => None,
        }
    };

    // Unlike the host, a devshell is named even when only part of the line
    // used it: that is what explains why that part had its toolchain.
    let environment = shared_environment(&segments);

    ParsedCommand {
        host,
        environment,
        segments,
    }
}

fn shared_environment(segments: &[CommandSegment]) -> Option<String> {
    let mut shared: Option<&str> = None;
    for environment in segments
        .iter()
        .filter(|segment| segment.kind.is_worth_naming())
        .filter_map(|segment| segment.environment.as_deref())
    {
        match shared {
            Some(seen) if seen != environment => return None,
            _ => shared = Some(environment),
        }
    }
    shared.map(str::to_owned)
}

impl ParsedCommand {
    /// Whether only part of the line ran in [`ParsedCommand::environment`].
    pub fn environment_is_partial(&self) -> bool {
        self.environment.is_some()
            && self
                .segments
                .iter()
                .any(|segment| segment.kind.is_worth_naming() && segment.environment.is_none())
    }
}

fn nix_devshell(command: &str) -> Option<String> {
    nix_devshell_command(command).map(|(shell, _)| shell)
}

/// `nix develop .#name --command cmd` → `("name", "cmd")`. Without
/// `--command` it opens a shell and runs nothing.
fn nix_devshell_command(command: &str) -> Option<(String, &str)> {
    let tokens = split_tokens(command);
    let word = |index: usize| tokens.get(index).map(|token| &command[token.clone()]);
    if word(0)? != "nix" {
        return None;
    }
    if !matches!(word(1)?, "develop" | "shell") {
        return None;
    }
    let mut reference = None;
    for (index, token) in tokens.iter().enumerate().skip(2) {
        let text = &command[token.clone()];
        if matches!(text, "--command" | "-c") {
            let inner = tokens.get(index + 1)?.start;
            let inner = &command[inner..];
            let inner = if tokens.len() == index + 2 {
                strip_matching_quotes(inner)
            } else {
                inner
            };
            let shell = match reference {
                Some(reference) => match strip_matching_quotes(reference).rsplit_once('#') {
                    Some((_, name)) if !name.is_empty() => name.to_string(),
                    _ => "default".to_string(),
                },
                None => "default".to_string(),
            };
            return Some((shell, inner));
        }
        if !text.starts_with('-') && reference.is_none() {
            reference = Some(text);
        }
    }
    None
}

/// A single real command anywhere makes the line `Other`.
pub fn classify_command(command: &str) -> CommandClass {
    let parsed = parse_command(command);
    let mut any_search = false;
    let mut any_read = false;
    let mut any_diff = false;
    let mut any_git_info = false;
    let mut any_github = false;
    let mut any_inspect = false;
    for segment in &parsed.segments {
        match &segment.kind {
            SegmentKind::Noop => {}
            SegmentKind::Search { .. } => any_search = true,
            SegmentKind::Read { .. }
            | SegmentKind::ListDirectory { .. }
            | SegmentKind::CountLines { .. }
            | SegmentKind::Lookup { .. } => any_read = true,
            SegmentKind::Git { operation, .. } => match operation {
                GitOperation::ReadChanges => any_diff = true,
                GitOperation::Inspect => any_git_info = true,
                GitOperation::Modify => return CommandClass::Other,
            },
            SegmentKind::GitHub { operation, .. } => {
                if github_is_read_only(operation) {
                    any_github = true;
                } else {
                    return CommandClass::Other;
                }
            }
            SegmentKind::Run { .. } => {
                if segment.kind.is_just_looking() {
                    any_inspect = true;
                } else {
                    return CommandClass::Other;
                }
            }
            SegmentKind::Wait { .. } => {}
            SegmentKind::WriteFile { .. }
            | SegmentKind::EditInPlace { .. }
            | SegmentKind::Destructive { .. }
            | SegmentKind::InlineScript { .. } => {
                return CommandClass::Other;
            }
        }
    }
    if any_search {
        CommandClass::Search
    } else if any_diff {
        CommandClass::ReadDiff
    } else if any_read {
        CommandClass::Read
    } else if any_git_info {
        CommandClass::GitInfo
    } else if any_github {
        CommandClass::GitHub
    } else if any_inspect {
        CommandClass::Inspect
    } else {
        CommandClass::Other
    }
}

/// A heredoc body a command wrote, or code it handed an interpreter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandScript {
    /// The file it was written to, or the interpreter it was fed to.
    pub label: String,
    /// A markdown language tag.
    pub language: Option<String>,
    pub code: String,
}

pub fn command_scripts(parsed: &ParsedCommand) -> Vec<CommandScript> {
    let mut scripts = Vec::new();
    for segment in &parsed.segments {
        match &segment.kind {
            SegmentKind::WriteFile {
                path,
                contents: Some(contents),
            } => scripts.push(CommandScript {
                label: path.clone(),
                language: language_for_path(path),
                code: contents.clone(),
            }),
            SegmentKind::InlineScript { interpreter, code } => scripts.push(CommandScript {
                label: interpreter.clone(),
                language: language_for_interpreter(interpreter),
                code: code.clone(),
            }),
            _ => {}
        }
    }
    scripts
}

fn language_for_path(path: &str) -> Option<String> {
    let extension = path.rsplit_once('.')?.1;
    let language = match extension {
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "rs" => "rust",
        "sh" | "bash" | "zsh" => "bash",
        "rb" => "ruby",
        "pl" => "perl",
        "json" => "json",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "sql" => "sql",
        "go" => "go",
        _ => return None,
    };
    Some(language.to_string())
}

fn language_for_interpreter(interpreter: &str) -> Option<String> {
    let language = match interpreter {
        "python" | "python3" => "python",
        "node" => "javascript",
        "vite-node" | "tsx" | "ts-node" | "deno" | "bun" => "typescript",
        "mysql" | "psql" | "sqlite3" | "duckdb" => "sql",
        "bash" | "sh" | "zsh" => "bash",
        "ruby" => "ruby",
        "perl" => "perl",
        _ => return None,
    };
    Some(language.to_string())
}

/// A short description of what a command line did, for a chip label. `None`
/// when the line is better shown act by act.
pub fn summarize_command(parsed: &ParsedCommand) -> Option<String> {
    let mut searches: Vec<&str> = Vec::new();
    let mut read_paths: Vec<&str> = Vec::new();
    let mut revisions: Vec<Option<&str>> = Vec::new();
    let mut listed = 0usize;
    let mut lookups: Vec<&str> = Vec::new();
    let mut diffs = 0usize;
    let mut git_checks = 0usize;
    let mut edited: Vec<&str> = Vec::new();
    let mut writes: Vec<&str> = Vec::new();
    let mut runs: Vec<&CommandSegment> = Vec::new();

    for segment in &parsed.segments {
        match &segment.kind {
            SegmentKind::Noop => {}
            SegmentKind::Search { query } => {
                if let Some(query) = query {
                    searches.push(query);
                } else {
                    searches.push("");
                }
            }
            SegmentKind::Read {
                paths, revision, ..
            } => {
                read_paths.extend(paths.iter().map(String::as_str));
                revisions.push(revision.as_deref());
            }
            SegmentKind::CountLines { paths } => {
                read_paths.extend(paths.iter().map(String::as_str));
                revisions.push(None);
            }
            SegmentKind::ListDirectory { .. } => listed += 1,
            SegmentKind::Lookup { program } => {
                if let Some(program) = program {
                    lookups.push(program);
                }
            }
            SegmentKind::Git { operation, .. } => match operation {
                GitOperation::ReadChanges => diffs += 1,
                GitOperation::Inspect => git_checks += 1,
                GitOperation::Modify => runs.push(segment),
            },
            SegmentKind::EditInPlace { paths } => edited.extend(paths.iter().map(String::as_str)),
            SegmentKind::Destructive { .. }
            | SegmentKind::InlineScript { .. }
            | SegmentKind::GitHub { .. } => runs.push(segment),
            SegmentKind::Wait { .. } => {}
            SegmentKind::WriteFile { path, .. } => writes.push(path),
            SegmentKind::Run { .. } => runs.push(segment),
        }
    }

    // One real command with some scaffolding around it reads as that command:
    // `pnpm lint > /tmp/out; tail /tmp/out` is a lint run.
    if runs.len() == 1
        && searches.is_empty()
        && read_paths.is_empty()
        && edited.is_empty()
        && writes.is_empty()
    {
        return Some(match &runs[0].kind {
            SegmentKind::InlineScript { interpreter, .. } => format!("{interpreter} script"),
            _ => first_words(runs[0].work_text(), 6),
        });
    }

    if !runs.is_empty() {
        return None;
    }

    // Several known queries and nothing else read better one chip per query.
    if searches.len() > 1
        && searches.iter().all(|query| !query.is_empty())
        && read_paths.is_empty()
        && edited.is_empty()
        && writes.is_empty()
        && lookups.is_empty()
        && listed == 0
        && diffs == 0
        && git_checks == 0
    {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    match searches.len() {
        0 => {}
        1 if !searches[0].is_empty() => parts.push(format!("Searched {:?}", searches[0])),
        n => parts.push(format!("Searched {n} places")),
    }
    if !edited.is_empty() {
        parts.push(format!("Edited {}", count_of_files(unique(&edited).len())));
    }
    if !writes.is_empty() {
        parts.push(format!("Wrote {}", unique(&writes).join(", ")));
    }
    if diffs > 0 {
        parts.push(format!(
            "read {diffs} diff{}",
            if diffs == 1 { "" } else { "s" }
        ));
    }
    if git_checks > 0 {
        parts.push("checked git".to_string());
    }
    let files = unique(&read_paths);
    let revision = match revisions.first() {
        Some(Some(first)) if revisions.iter().all(|revision| revision == &Some(*first)) => {
            format!(" at {first}")
        }
        _ => String::new(),
    };
    match files.len() {
        0 => {}
        1 => parts.push(format!("read {}{revision}", base_name(files[0]))),
        n => parts.push(format!("read {}{revision}", count_of_files(n))),
    }
    if listed > 0 {
        parts.push(format!(
            "listed {listed} director{}",
            if listed == 1 { "y" } else { "ies" }
        ));
    }
    let looked_for = unique(&lookups);
    match looked_for.len() {
        0 => {}
        1..=3 => parts.push(format!("checked for {}", looked_for.join(", "))),
        n => parts.push(format!("checked for {n} programs")),
    }
    if parts.is_empty() {
        return None;
    }

    let mut label = parts.join(", ");
    let mut chars = label.chars();
    if let Some(first) = chars.next() {
        label = first.to_uppercase().collect::<String>() + chars.as_str();
    }
    Some(label)
}

/// Whether a word is a path rather than, say, a jq program full of slashes and
/// dots (`.inner.impl.path // "inherent"`).
fn looks_like_path(word: &str) -> bool {
    if word.is_empty() || word.contains(char::is_whitespace) {
        return false;
    }
    if word.contains('/') || word.contains('\\') {
        return !word.contains(['|', '[', ']', '(', ')', '"']);
    }
    word.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && (1..=6).contains(&extension.len())
            && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
    })
}

fn path_argument(text: &str) -> Option<String> {
    split_tokens(text)
        .into_iter()
        .skip(1)
        .map(|range| strip_matching_quotes(&text[range]))
        .find(|word| !word.starts_with('-') && looks_like_path(word))
        .map(str::to_string)
}

/// The packages a cargo-style command names with `-p`/`--package`.
fn package_arguments(text: &str) -> Vec<String> {
    let tokens = split_tokens(text);
    let words: Vec<&str> = tokens
        .iter()
        .map(|range| strip_matching_quotes(&text[range.clone()]))
        .collect();
    let mut packages = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if let Some(value) = word.strip_prefix("--package=") {
            packages.push(value.to_string());
        } else if matches!(word, "-p" | "--package")
            && let Some(value) = words.get(index + 1).filter(|value| !value.starts_with('-'))
        {
            packages.push((*value).to_string());
            index += 1;
        }
        index += 1;
    }
    packages
}

fn url_host(argument: &str) -> Option<&str> {
    let rest = argument.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    (!host.is_empty()).then_some(host)
}

fn unique<'a>(values: &[&'a str]) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for value in values {
        if !seen.contains(value) {
            seen.push(value);
        }
    }
    seen
}

fn count_of_files(count: usize) -> String {
    format!("{count} file{}", if count == 1 { "" } else { "s" })
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn first_words(text: &str, max: usize) -> String {
    let mut words: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        if word.starts_with('>') || word.starts_with("2>") {
            break;
        }
        words.push(word);
        if words.len() == max {
            break;
        }
    }
    words.join(" ")
}

struct Unwrapped<'a> {
    host: Option<String>,
    command: &'a str,
}

impl<'a> Unwrapped<'a> {
    fn local(command: &'a str) -> Self {
        Self {
            host: None,
            command,
        }
    }
}

/// `ssh host 'cmd'` → the host and `cmd`.
fn unwrap_ssh(command: &str) -> Unwrapped<'_> {
    const VALUE_FLAGS: &[&str] = &[
        "-p", "-i", "-l", "-o", "-F", "-c", "-J", "-b", "-D", "-L", "-R", "-W", "-Q", "-S", "-w",
        "-e", "-m", "-O",
    ];
    let trimmed = command.trim();
    let Some(rest) = trimmed.strip_prefix("ssh ") else {
        return Unwrapped::local(command);
    };

    let tokens = split_tokens(rest);
    let mut index = 0;
    let mut host = None;
    while index < tokens.len() {
        let token = &tokens[index];
        let text = &rest[token.clone()];
        if VALUE_FLAGS.contains(&text) {
            index += 2;
            continue;
        }
        if text.starts_with('-') {
            index += 1;
            continue;
        }
        host = Some(text.to_string());
        index += 1;
        break;
    }

    let remote = tokens
        .get(index)
        .map(|token| {
            let start = token.start;
            let end = tokens.last().map_or(rest.len(), |last| last.end);
            &rest[start..end]
        })
        .unwrap_or("");
    let remote = if tokens.len() == index + 1 {
        strip_matching_quotes(remote)
    } else {
        remote
    };

    Unwrapped {
        host,
        command: if remote.is_empty() { command } else { remote },
    }
}

fn strip_matching_quotes(text: &str) -> &str {
    let trimmed = text.trim();
    for quote in ['\'', '"'] {
        if trimmed.len() >= 2 && trimmed.starts_with(quote) && trimmed.ends_with(quote) {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

/// Syntax left between separators: a comment, a `case` pattern like
/// `failure|cancelled)`, a lone brace.
fn is_shell_punctuation(segment: &str) -> bool {
    let text = segment.trim();
    if text.is_empty() || text.starts_with('#') {
        return true;
    }
    if text.chars().all(|ch| "{}()[];&|<>".contains(ch)) {
        return true;
    }
    !text.contains(char::is_whitespace) && text.ends_with(')') && !text.contains('(')
}

/// Whether the text ends in `name()`, making a following `{` a definition.
fn ends_with_function_header(text: &str) -> bool {
    let Some(head) = text.trim_end().strip_suffix(')') else {
        return false;
    };
    let Some(name) = head.trim_end().strip_suffix('(') else {
        return false;
    };
    let name = name.trim();
    let name = name.strip_prefix("function").map_or(name, str::trim);
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
}

/// Splits a command line on `&&`, `||`, `;` and newlines, respecting quotes,
/// heredoc bodies, `$( … )` and continuations. Pipelines stay whole.
fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.char_indices().peekable();
    let mut quote: Option<char> = None;
    // Heredoc bodies start after the line that opened them ends.
    let mut pending_heredocs: Vec<String> = Vec::new();
    let mut substitution_depth: usize = 0;

    let push = |segments: &mut Vec<String>, current: &mut String| {
        let text = current.trim().to_string();
        if !text.is_empty() {
            segments.push(text);
        }
        current.clear();
    };

    while let Some((index, ch)) = chars.next() {
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            }
            continue;
        }

        if ch == '$' && matches!(chars.peek(), Some((_, '('))) {
            current.push(ch);
            chars.next();
            current.push('(');
            substitution_depth += 1;
            continue;
        }
        if substitution_depth > 0 {
            current.push(ch);
            match ch {
                '\'' | '"' => quote = Some(ch),
                '(' => substitution_depth += 1,
                ')' => substitution_depth -= 1,
                _ => {}
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '\\' if matches!(chars.peek(), Some((_, '\n'))) => {
                chars.next();
                while current.ends_with(' ') {
                    current.pop();
                }
                current.push(' ');
                while matches!(chars.peek(), Some((_, ' ' | '\t'))) {
                    chars.next();
                }
            }
            '\\' => {
                current.push(ch);
                if let Some((_, escaped)) = chars.next() {
                    current.push(escaped);
                }
            }
            '<' if command[index..].starts_with("<<") => {
                current.push(ch);
                chars.next();
                current.push('<');
                let mut delimiter = String::new();
                let mut saw_dash = false;
                while let Some((_, next)) = chars.peek().copied() {
                    if next == '-' && delimiter.is_empty() && !saw_dash {
                        saw_dash = true;
                        current.push(next);
                        chars.next();
                        continue;
                    }
                    if next.is_whitespace() && delimiter.is_empty() {
                        current.push(next);
                        chars.next();
                        continue;
                    }
                    if next == '\n' || (next.is_whitespace() && !delimiter.is_empty()) {
                        break;
                    }
                    delimiter.push(next);
                    current.push(next);
                    chars.next();
                }
                let delimiter = strip_matching_quotes(&delimiter).to_string();
                if !delimiter.is_empty() {
                    pending_heredocs.push(delimiter);
                }
            }
            '\n' => {
                if pending_heredocs.is_empty() {
                    push(&mut segments, &mut current);
                    continue;
                }
                current.push('\n');
                for delimiter in std::mem::take(&mut pending_heredocs) {
                    let mut line = String::new();
                    loop {
                        match chars.next() {
                            None => break,
                            Some((_, '\n')) => {
                                current.push_str(&line);
                                current.push('\n');
                                if line.trim() == delimiter {
                                    break;
                                }
                                line.clear();
                            }
                            Some((_, body_char)) => line.push(body_char),
                        }
                    }
                    if !line.is_empty() {
                        current.push_str(&line);
                    }
                }
                push(&mut segments, &mut current);
            }
            // Nothing in a function body runs here, so it is not split.
            '{' if ends_with_function_header(&current) => {
                current.push(ch);
                let mut depth = 1;
                let mut body_quote: Option<char> = None;
                for (_, next) in chars.by_ref() {
                    current.push(next);
                    if let Some(open) = body_quote {
                        if next == open {
                            body_quote = None;
                        }
                        continue;
                    }
                    match next {
                        '\'' | '"' => body_quote = Some(next),
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                push(&mut segments, &mut current);
            }
            ';' => push(&mut segments, &mut current),
            '&' if matches!(chars.peek(), Some((_, '&'))) => {
                chars.next();
                push(&mut segments, &mut current);
            }
            '|' if matches!(chars.peek(), Some((_, '|'))) => {
                chars.next();
                push(&mut segments, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push(&mut segments, &mut current);
    segments
}

/// Quote-aware token ranges; quotes stay inside the range.
fn split_tokens(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    for (index, ch) in text.char_indices() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                start.get_or_insert(index);
                quote = Some(ch);
            }
            ch if ch.is_whitespace() => {
                if let Some(begin) = start.take() {
                    tokens.push(begin..index);
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
    }
    if let Some(begin) = start {
        tokens.push(begin..text.len());
    }
    tokens
}

fn classify_segment(segment: &str) -> SegmentKind {
    if let Some((header, _)) = segment.split_once('{')
        && ends_with_function_header(header)
    {
        return SegmentKind::Noop;
    }

    if is_shell_punctuation(segment) {
        return SegmentKind::Noop;
    }

    if let Some(write) = write_target(segment) {
        return write;
    }

    // A heredoc with nowhere to be written feeds stdin: `python3 - <<'PY'`.
    if let Some(heredoc_at) = unquoted_find(segment, "<<") {
        let prefix = &segment[..heredoc_at];
        if let Some(interpreter) = stdin_interpreter(prefix)
            && let Some(code) = heredoc_body(&segment[heredoc_at..])
        {
            return SegmentKind::InlineScript { interpreter, code };
        }
        return classify_segment(prefix);
    }

    // A pipeline's meaning comes from where its data starts; a later search
    // only wins over a source that was itself just looking.
    let mut result = SegmentKind::Noop;
    for (index, stage) in split_stages(segment).into_iter().enumerate() {
        let kind = classify_stage(&stage);
        if matches!(kind, SegmentKind::Search { .. }) && result.is_just_looking() {
            return kind;
        }
        if index == 0 || result.is_noop() {
            if !kind.is_noop() {
                result = kind;
            }
            continue;
        }
        // `git show rev:file | sed -n '1,240p'` wanted those lines.
        if let (
            SegmentKind::Read {
                lines: lines @ None,
                ..
            },
            SegmentKind::Read {
                lines: Some(range), ..
            },
        ) = (&mut result, &kind)
        {
            *lines = Some(range.clone());
        }
    }
    result
}

fn split_stages(segment: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in segment.chars() {
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '|' => {
                if !current.trim().is_empty() {
                    stages.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        stages.push(current.trim().to_string());
    }
    stages
}

/// The file a segment writes by redirect or heredoc. `2>/dev/null` and `2>&1`
/// are not writing.
fn write_target(segment: &str) -> Option<SegmentKind> {
    let heredoc_at = unquoted_find(segment, "<<");
    if let Some(heredoc_at) = heredoc_at
        && let Some(path) = redirect_path(&segment[..heredoc_at])
    {
        let contents = heredoc_body(&segment[heredoc_at..]);
        return Some(SegmentKind::WriteFile { path, contents });
    }

    // A heredoc body is data: the `>=` in a SQL body is not a redirect.
    let segment = heredoc_at.map_or(segment, |at| &segment[..at]);

    let mut quote: Option<char> = None;
    let bytes: Vec<char> = segment.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '>' => {
                let tail: String = bytes[index + 1..].iter().collect();
                let tail = tail.trim_start_matches('>').trim();
                if !tail.is_empty() && !tail.starts_with('&') && !tail.starts_with("/dev/null") {
                    let path = tail.split_whitespace().next()?.to_string();
                    let path = strip_matching_quotes(&path).to_string();
                    // `pnpm lint > /tmp/out.txt` captured output, it did not
                    // author a file.
                    if is_scratch_path(&path) {
                        return None;
                    }
                    return Some(SegmentKind::WriteFile {
                        path,
                        contents: None,
                    });
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn unquoted_find(text: &str, needle: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, ch) in text.char_indices() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            _ if text[index..].starts_with(needle) => return Some(index),
            _ => {}
        }
    }
    None
}

/// The files a `sed` or `perl` invocation edits in place. The script is the
/// first operand unless `-e`/`-f` supplied it.
fn in_place_edit_paths(program: &str, rest: &[&str]) -> Vec<String> {
    let mut script_supplied = false;
    let mut operands: Vec<&str> = Vec::new();
    let mut words = rest.iter().copied().peekable();
    while let Some(word) = words.next() {
        if word == "--" {
            operands.extend(words.by_ref());
            break;
        }
        // `-` on its own is stdin, which is not a flag and not a file.
        if !word.starts_with('-') || word == "-" {
            operands.push(word);
            continue;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            if matches!(name, "expression" | "file") {
                script_supplied = true;
                if attached.is_none() {
                    words.next();
                }
            }
            continue;
        }
        let mut flags = word[1..].chars();
        while let Some(flag) = flags.next() {
            match flag {
                'e' | 'f' => {
                    script_supplied = true;
                    if flags.as_str().is_empty() {
                        words.next();
                    }
                    break;
                }
                'i' => {
                    // BSD sed takes the backup suffix as the next word
                    // (`sed -i ''`); GNU never does, so only an empty or
                    // dot-led word is taken as one.
                    if flags.as_str().is_empty() && program == "sed" {
                        let suffix = words.peek().map(|next| strip_matching_quotes(next));
                        if suffix.is_some_and(|s| s.is_empty() || s.starts_with('.')) {
                            words.next();
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    }

    let mut operands = operands.into_iter();
    if !script_supplied {
        operands.next();
    }
    operands
        .map(strip_matching_quotes)
        .filter(|word| !word.is_empty())
        .filter(|word| !is_unexpanded_variable(word))
        // Guards against an unknown flag's value being named as the file.
        .filter(|word| looks_like_path(word) || plausible_bare_filename(word))
        .map(str::to_string)
        .collect()
}

/// `Makefile` and `f` are filenames; `d;}` is not.
fn plausible_bare_filename(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

/// Whether the flags ask for editing in place. `-i` can sit anywhere in a
/// cluster (`-Ei`), but not after `-e`/`-f`, whose value follows them.
fn is_in_place(rest: &[&str]) -> bool {
    rest.iter().any(|word| {
        if let Some(long) = word.strip_prefix("--") {
            return long == "in-place" || long.starts_with("in-place=");
        }
        let Some(flags) = word.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            return false;
        };
        for flag in flags.chars() {
            match flag {
                'i' => return true,
                'e' | 'f' => return false,
                _ => {}
            }
        }
        false
    })
}

/// The interpreter a heredoc is fed to, given everything before the `<<`.
fn stdin_interpreter(prefix: &str) -> Option<String> {
    let program = prefix.split_whitespace().next()?;
    matches!(
        program,
        "python"
            | "python3"
            | "node"
            | "bash"
            | "sh"
            | "zsh"
            | "ruby"
            | "perl"
            | "psql"
            | "mysql"
            | "sqlite3"
            | "duckdb"
    )
    .then(|| program.to_string())
}

/// `-e` means "env var" to some tools and "evaluate" to others.
fn looks_like_code(payload: &str) -> bool {
    payload.contains('(') || payload.contains(';') || payload.contains('{')
}

fn inline_code(rest: &[&str]) -> Option<String> {
    let index = rest
        .iter()
        .position(|word| matches!(*word, "-c" | "-e" | "--eval" | "--command"))?;
    let code = rest.get(index + 1)?;
    Some(strip_matching_quotes(code).to_string())
}

/// The command line handed to `bash -c '…'`. A multi-line payload is a script
/// and keeps its own chip.
fn shell_payload(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let program = base_name(words.next()?);
    if !matches!(program, "sh" | "bash" | "zsh" | "dash" | "ksh") {
        return None;
    }
    let tokens = split_tokens(command);
    let rest: Vec<&str> = tokens
        .iter()
        .map(|range| &command[range.clone()])
        .skip(1)
        .collect();
    inline_code(&rest).filter(|code| !code.contains('\n'))
}

/// `origin/master:src/lib.rs` → the revision and the path.
fn revision_and_path(target: &str) -> Option<(String, String)> {
    let (revision, path) = target.split_once(':')?;
    if revision.is_empty() || path.is_empty() || path.starts_with('-') {
        return None;
    }
    Some((revision.to_string(), path.to_string()))
}

/// The range a simple `sed -n 'A,Bp'` asks for.
fn sed_line_range(script: &str) -> Option<Range<u32>> {
    let script = strip_matching_quotes(script).trim_end_matches('p');
    let (start, end) = script.split_once(',')?;
    let start = start.trim().parse::<u32>().ok()?;
    let end = end.trim().parse::<u32>().ok()?;
    (start <= end).then_some(start..end)
}

fn is_scratch_path(path: &str) -> bool {
    path.starts_with("/tmp/")
        || path.starts_with("/var/tmp/")
        || path.starts_with("/var/folders/")
        || path.starts_with("/dev/")
        || path.starts_with("$TMPDIR")
}

fn redirect_path(prefix: &str) -> Option<String> {
    let mut parts = prefix.rsplit('>');
    let target = parts.next()?.trim();
    if parts.next().is_none() {
        return None;
    }
    let target = target.split_whitespace().next()?;
    Some(strip_matching_quotes(target).to_string())
}

fn heredoc_body(text: &str) -> Option<String> {
    let after = text.strip_prefix("<<")?;
    let after = after.strip_prefix('-').unwrap_or(after);
    let after = after.trim_start();
    let mut delimiter = String::new();
    let mut rest = after;
    for (index, ch) in after.char_indices() {
        if ch == '\n' {
            rest = &after[index + 1..];
            break;
        }
        delimiter.push(ch);
    }
    let delimiter = strip_matching_quotes(delimiter.trim()).to_string();
    if delimiter.is_empty() {
        return None;
    }
    let mut body = String::new();
    for line in rest.lines() {
        if line.trim() == delimiter {
            return Some(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    (!body.is_empty()).then_some(body)
}

/// Strips subshell parentheses, group braces and a trailing `&`.
fn trim_brackets(text: &str) -> &str {
    let mut trimmed = text.trim();
    loop {
        let shorter = trimmed
            .trim_start_matches(['(', '{'])
            .trim_end_matches([')', '}'])
            .trim_end_matches('&')
            .trim();
        if shorter == trimmed {
            break trimmed;
        }
        trimmed = shorter;
    }
}

/// `retries` or `retries[$id]`, the part of an assignment before `=`.
fn is_assignment_target(name: &str) -> bool {
    let ident = match name.split_once('[') {
        Some((ident, rest)) if rest.ends_with(']') => ident,
        Some(_) => return false,
        None => name,
    };
    !ident.is_empty()
        && ident
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The command in `f=$(grep -rln sym src)`, when that is the whole segment.
fn captured_command(segment: &str) -> Option<&str> {
    let (name, value) = segment.split_once('=')?;
    if !is_assignment_target(name) {
        return None;
    }
    command_substitution(value)
}

/// The command inside a word that is nothing but one `$( … )`.
fn command_substitution(word: &str) -> Option<&str> {
    let word = strip_matching_quotes(word.trim());
    let inner = word.strip_prefix("$(")?;
    // `$((` is arithmetic.
    if inner.starts_with('(') {
        return None;
    }
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    for (index, ch) in inner.char_indices() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (index + 1 == inner.len()).then(|| &inner[..index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `$f` names no file: what it stood for is not in the text.
fn is_unexpanded_variable(word: &str) -> bool {
    word.starts_with('$')
}

fn classify_stage(stage: &str) -> SegmentKind {
    let stage = trim_brackets(stage);

    let tokens = split_tokens(stage);
    let mut words: Vec<&str> = tokens.iter().map(|range| &stage[range.clone()]).collect();

    // Strip assignments and wrappers in front of the real program.
    while let Some(word) = words.first() {
        // On every pass: `do (grpcurl …` shows its bracket only once `do` is
        // peeled off.
        let debracketed = trim_brackets(word);
        if debracketed != *word {
            if debracketed.is_empty() {
                words.remove(0);
            } else {
                words[0] = debracketed;
            }
            continue;
        }
        let assigned = word
            .split_once('=')
            .filter(|(name, _)| is_assignment_target(name));
        // Captures that touched the project were already lifted out in
        // `parse_command`.
        if assigned.is_some_and(|(_, value)| value.starts_with("$(")) {
            return SegmentKind::Noop;
        }
        let is_assignment = assigned.is_some();
        // `command -v foo` is a lookup, not a wrapper.
        if *word == "command" && words.get(1).is_some_and(|word| word.starts_with('-')) {
            break;
        }
        if is_assignment
            || matches!(
                *word,
                "command" | "env" | "do" | "then" | "else" | "time" | "nohup"
            )
        {
            words.remove(0);
            continue;
        }
        // Runners that set up an environment and hand over.
        let skip = match *word {
            "npx" | "pnpx" | "bunx" => 1,
            "direnv" if words.get(1) == Some(&"exec") => 3,
            "poetry" | "uv" | "pipenv" | "rye" | "hatch" if words.get(1) == Some(&"run") => 2,
            "python" | "python3" if words.get(1) == Some(&"-m") => 2,
            "bundle" | "dotenv" if words.get(1) == Some(&"exec") => 2,
            "nix" if matches!(words.get(1).copied(), Some("develop" | "shell")) => {
                match words
                    .iter()
                    .position(|word| matches!(*word, "--command" | "-c"))
                {
                    Some(index) => index + 1,
                    None => break,
                }
            }
            // `pnpm lint` and `npm run build` are the command themselves.
            "pnpm" | "npm" | "yarn" | "bun" => {
                match words
                    .iter()
                    .position(|word| matches!(*word, "exec" | "dlx"))
                {
                    Some(index) => index + 1,
                    None => break,
                }
            }
            _ => break,
        };
        if skip >= words.len() {
            break;
        }
        words.drain(..skip);
    }

    let Some((head, rest)) = words.split_first() else {
        return SegmentKind::Noop;
    };
    let positional = |rest: &[&str]| -> Option<String> {
        rest.iter()
            .find(|word| !word.starts_with('-') && !word.chars().all(|ch| ch.is_ascii_digit()))
            .map(|word| strip_matching_quotes(word).to_string())
    };
    let paths = |rest: &[&str]| -> Vec<String> {
        rest.iter()
            .filter(|word| !word.starts_with('-'))
            // A bare number is a flag's value (`tail -n 100 log.txt`).
            .filter(|word| !word.chars().all(|ch| ch.is_ascii_digit()))
            .map(|word| strip_matching_quotes(word).to_string())
            .filter(|word| !is_unexpanded_variable(word))
            .collect()
    };

    match *head {
        "sleep" => SegmentKind::Wait {
            seconds: rest.first().and_then(|word| word.parse().ok()),
        },
        "gh" => {
            let mut positionals = rest.iter().filter(|word| !word.starts_with('-'));
            let operation = positionals.next().copied().unwrap_or("gh");
            let action = positionals.next().copied();
            SegmentKind::GitHub {
                operation: match action {
                    Some(action) => format!("{operation} {action}"),
                    None => operation.to_string(),
                },
                target: positionals
                    .next()
                    .map(|word| strip_matching_quotes(word).to_string()),
            }
        }
        "rm" => SegmentKind::Destructive {
            operation: DestructiveOperation::Delete,
            paths: paths(rest),
        },
        "mv" => SegmentKind::Destructive {
            operation: DestructiveOperation::Move,
            paths: paths(rest),
        },
        "chmod" | "chown" => SegmentKind::Destructive {
            operation: DestructiveOperation::ChangePermissions,
            paths: paths(rest).into_iter().skip(1).collect(),
        },
        // `sh -c 'pytest -q tests'` ran pytest; a multi-line payload is a
        // script and falls through.
        "sh" | "bash" | "zsh" | "dash" | "ksh"
            if inline_code(rest).is_some_and(|code| !code.contains('\n')) =>
        {
            let inner = inline_code(rest).unwrap_or_default();
            classify_segment(&inner)
        }
        "python" | "python3" | "node" | "bash" | "sh" | "zsh" | "ruby" | "perl"
            if inline_code(rest).is_some() && !is_in_place(rest) =>
        {
            SegmentKind::InlineScript {
                interpreter: (*head).to_string(),
                code: inline_code(rest).unwrap_or_default(),
            }
        }
        // `echo` is a printed divider (a redirect into a file was caught
        // earlier), and `wait` waits on jobs that are the real work.
        "cd" | "pushd" | "popd" | "export" | "true" | ":" | "exit" | "set" | "unset" | "shift"
        | "local" | "declare" | "typeset" | "readonly" | "alias" | "unalias" | "trap" | "shopt"
        | "done" | "fi" | "esac" | "for" | "while" | "until" | "if" | "case" | "then" | "elif"
        | "do" | "return" | "break" | "continue" | "echo" | "printf" | "wait" => SegmentKind::Noop,
        "sed" | "perl" if is_in_place(rest) => SegmentKind::EditInPlace {
            paths: in_place_edit_paths(head, rest),
        },
        // `perl -ne '…' file` reads the file like awk; plain `perl -e` is a
        // script.
        "perl"
            if rest.iter().any(|word| {
                word.starts_with('-') && !word.starts_with("--") && word.contains(['n', 'p'])
            }) =>
        {
            SegmentKind::Read {
                paths: paths(rest).into_iter().skip(1).collect(),
                lines: None,
                revision: None,
            }
        }
        "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "ag" => SegmentKind::Search {
            query: positional(rest),
        },
        "fd" | "find" => SegmentKind::Search {
            query: positional(rest),
        },
        "git" => {
            let subcommand = rest.iter().find(|word| !word.starts_with('-')).copied();
            let after = |name: &str| -> Option<String> {
                rest.iter()
                    .skip_while(|word| **word != name)
                    .nth(1)
                    .filter(|word| !word.starts_with('-'))
                    .map(|word| strip_matching_quotes(word).to_string())
            };
            if subcommand == Some("show")
                && let Some((revision, path)) = after("show").as_deref().and_then(revision_and_path)
            {
                return SegmentKind::Read {
                    paths: vec![path],
                    lines: None,
                    revision: Some(revision),
                };
            }

            match subcommand {
                Some("grep") => SegmentKind::Search {
                    query: after("grep"),
                },
                Some(name @ ("diff" | "show")) => SegmentKind::Git {
                    operation: GitOperation::ReadChanges,
                    target: after(name),
                },
                Some("log") if rest.contains(&"-p") || rest.contains(&"--patch") => {
                    SegmentKind::Git {
                        operation: GitOperation::ReadChanges,
                        target: None,
                    }
                }
                Some(
                    name @ ("status" | "log" | "branch" | "blame" | "remote" | "config"
                    | "ls-files" | "rev-parse" | "describe" | "shortlog" | "reflog"
                    | "whatchanged"),
                ) => SegmentKind::Git {
                    operation: GitOperation::Inspect,
                    target: (name != "status").then(|| after(name)).flatten(),
                },
                Some("reset") if rest.contains(&"--hard") => SegmentKind::Destructive {
                    operation: DestructiveOperation::DiscardChanges,
                    paths: Vec::new(),
                },
                Some("clean") => SegmentKind::Destructive {
                    operation: DestructiveOperation::DiscardChanges,
                    paths: Vec::new(),
                },
                Some("restore") | Some("checkout") if rest.contains(&"--") => {
                    SegmentKind::Destructive {
                        operation: DestructiveOperation::DiscardChanges,
                        paths: rest
                            .iter()
                            .skip_while(|word| **word != "--")
                            .skip(1)
                            .map(|word| strip_matching_quotes(word).to_string())
                            .collect(),
                    }
                }
                Some(name) => SegmentKind::Git {
                    operation: GitOperation::Modify,
                    target: Some(name.to_string()),
                },
                None => SegmentKind::Git {
                    operation: GitOperation::Inspect,
                    target: None,
                },
            }
        }
        "cat" | "head" | "tail" | "bat" | "less" | "more" => {
            let paths = paths(rest);
            // Tailing a command's captured output says nothing about the
            // project.
            if !paths.is_empty() && paths.iter().all(|path| is_scratch_path(path)) {
                SegmentKind::Noop
            } else {
                SegmentKind::Read {
                    paths,
                    lines: None,
                    revision: None,
                }
            }
        }
        "which" | "type" | "hash" | "whereis" => SegmentKind::Lookup {
            program: positional(rest),
        },
        "command" => SegmentKind::Lookup {
            program: positional(rest),
        },
        "wc" => SegmentKind::CountLines { paths: paths(rest) },
        "ls" | "tree" | "du" => SegmentKind::ListDirectory {
            path: positional(rest),
        },
        "sed" if !rest.iter().any(|word| word.starts_with("-i")) => {
            let script = rest.iter().find(|word| !word.starts_with('-')).copied();
            SegmentKind::Read {
                paths: paths(rest).into_iter().skip(1).collect(),
                lines: script.and_then(|script| sed_line_range(script)),
                revision: None,
            }
        }
        "awk" if !rest.contains(&"-i") => SegmentKind::Read {
            paths: paths(rest).into_iter().skip(1).collect(),
            lines: None,
            revision: None,
        },
        // The payload has to look like code: `docker run -e HOST=x` stays a run.
        program
            if !is_in_place(rest)
                && inline_code(rest).is_some_and(|code| looks_like_code(&code)) =>
        {
            SegmentKind::InlineScript {
                interpreter: program.to_string(),
                code: inline_code(rest).unwrap_or_default(),
            }
        }
        program => SegmentKind::Run {
            program: program.to_string(),
            argument: positional(rest),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(command: &str) -> Vec<SegmentKind> {
        parse_command(command)
            .segments
            .into_iter()
            .map(|segment| segment.kind)
            .collect()
    }

    fn summary(command: &str) -> Option<String> {
        summarize_command(&parse_command(command))
    }

    fn short_labels(command: &str) -> Vec<String> {
        parse_command(command)
            .segments
            .iter()
            .filter(|segment| segment.kind.is_worth_naming())
            .map(|segment| segment.short_label())
            .collect()
    }

    fn languages(command: &str) -> Vec<Option<&'static str>> {
        parse_command(command)
            .segments
            .iter()
            .filter(|segment| !segment.kind.is_noop())
            .map(|segment| segment.language())
            .collect()
    }

    fn environments(command: &str) -> Vec<String> {
        parse_command(command)
            .segments
            .iter()
            .filter(|segment| segment.kind.is_worth_naming())
            .map(|segment| match &segment.environment {
                Some(environment) => format!("{} in {environment}", segment.short_label()),
                None => segment.short_label(),
            })
            .collect()
    }

    #[test]
    fn sed_and_perl_in_place_edits_are_named_after_their_files() {
        let edited = |command: &str| -> Vec<Vec<String>> {
            parse_command(command)
                .segments
                .into_iter()
                .filter_map(|segment| match segment.kind {
                    SegmentKind::EditInPlace { paths } => Some(paths),
                    _ => None,
                })
                .collect()
        };
        for (command, expected) in [
            // BSD `sed -i ''` takes its empty backup suffix as its own word.
            (
                "sed -i '' '46{/^parking_lot = { workspace = true }$/d;}' arcade/Cargo.toml",
                vec!["arcade/Cargo.toml"],
            ),
            ("sed -i .bak 's/a/b/' notes.md", vec!["notes.md"]),
            ("sed -i 's/a/b/' f", vec!["f"]),
            (
                "sed -i.bak -e 's/a/b/' -e 's/c/d/' f.txt g.txt",
                vec!["f.txt", "g.txt"],
            ),
            ("sed -Ei -e 's/a/b/' src/main.rs", vec!["src/main.rs"]),
            ("sed -i -f fix.sed a.txt", vec!["a.txt"]),
            ("perl -pi -e 's|x/y|z|' f.txt", vec!["f.txt"]),
            (
                "perl -i.bak -pe 's/a/b/' lib/thing.rb",
                vec!["lib/thing.rb"],
            ),
        ] {
            let expected: Vec<String> = expected.into_iter().map(String::from).collect();
            assert_eq!(edited(command), vec![expected], "{command}");
        }
        assert_eq!(
            summary("perl -pi -e 's/Line2\\.from\\(/Line2.fromLine(/g' a.ts b.ts c.tsx").as_deref(),
            Some("Edited 3 files")
        );
    }

    #[test]
    fn short_labels_name_what_ran() {
        for (command, expected) in [
            ("cargo test -p acp_thread", vec!["cargo test acp_thread"]),
            (
                "cargo clippy -p geom -p geom-macro -p wall-wasm --all-targets",
                vec!["cargo clippy geom +2"],
            ),
            (
                "cargo check --package sidebar --all-targets",
                vec!["cargo check sidebar"],
            ),
            ("cargo check --workspace", vec!["cargo check"]),
            ("cargo clippy --fix", vec!["cargo clippy"]),
            ("npm run build", vec!["npm run"]),
            ("python3 scripts/generate.py", vec!["python3 generate.py"]),
            ("htop", vec!["htop"]),
            // Double quotes keep BRE's `\|` as written.
            (
                "grep -n \"example\\|shape\" aot_compile.py",
                vec!["example\\|shape"],
            ),
            (
                "python3 gen.py && cargo test && rg TODO src",
                vec!["python3 gen.py", "cargo test", "TODO"],
            ),
            // A jq filter's `//` is an operator; the file names the chip.
            (
                "jq -r '.index[$id|tostring] | [(.trait.resolved_path.path // \"inherent\")] \
                 | @tsv' target/doc/nalgebra.json | head -80",
                vec!["jq nalgebra.json"],
            ),
            (
                "for h in w3aa w3z w3bb; do \
                 (grpcurl -plaintext -max-time 4 $h:5001 list >/dev/null 2>&1 \
                 && echo \"$h REACHABLE\" || echo \"$h -\") & done; wait",
                vec!["grpcurl $h:5001"],
            ),
            (
                "set +e\n\
                 run_rpc() {\n\
                   err=$(grpcurl -plaintext -d \"$3\" pisa16:5001 \"$2\" 2>&1 >/dev/null)\n\
                   if [ \"$?\" -eq 0 ]; then echo \"OK  $1\"; else echo \"ERR $1\"; fi\n\
                 }\n\
                 run_rpc GetCameraType \"$svc/GetCameraType\" '{}' 10\n\
                 run_rpc Preview \"$svc/Preview\" '{\"maxWidth\":320}' 20",
                vec!["run_rpc GetCameraType", "run_rpc Preview"],
            ),
            (
                "cd /tmp/wrist && gh pr view 12104 --json state 2>/dev/null\n\
                 deadline=$(( $(date +%s) + 14400 ))\n\
                 declare -A retries\n\
                 while [ \"$(date +%s)\" -lt \"$deadline\" ]; do\n\
                 # Babysit: rerun infra-failed runs on the current head SHA\n\
                 case \"$concl\" in\n\
                 failure|cancelled)\n\
                 gh run rerun \"$id\" --failed >/dev/null 2>&1 && { retries[$id]=1; \
                 echo \"requeued $wf\"; }\n\
                 ;;\n\
                 esac\n\
                 sleep 180\n\
                 done\n\
                 echo \"TIMEOUT\"; gh pr checks 12104 2>/dev/null | awk '{print $1, $2}'",
                vec!["gh pr view", "gh run rerun", "gh pr checks"],
            ),
        ] {
            assert_eq!(short_labels(command), expected, "{command}");
        }
    }

    #[test]
    fn segments_carry_their_toolchain() {
        assert_eq!(languages("cargo fmt"), [Some("rust")]);
        assert_eq!(languages("pytest -q"), [Some("python")]);
        assert_eq!(
            languages("python3 -c 'import sys; print(sys.path)'"),
            [Some("python")]
        );
        assert_eq!(languages("./configure"), [None]);
    }

    #[test]
    fn a_devshell_is_where_it_ran_not_what_ran() {
        let command = "nix develop --quiet .#vision-dev --command ruff check \
            vision/apps/run_vide.py vision/terra/services/vide/server.py \
            && nix develop --quiet .#vision-dev --command ty check vision/apps/run_vide.py \
            && nix develop --quiet .#vision-dev --command pytest -q \
            vision/apps/test_run_vide.py vision/terra/services/vide/tests/test_wrist_grpc.py";
        assert_eq!(
            short_labels(command),
            ["ruff check", "ty check", "pytest test_run_vide.py"]
        );
        assert_eq!(
            languages(command),
            [Some("python"), Some("python"), Some("python")]
        );
        assert_eq!(
            parse_command(command).environment.as_deref(),
            Some("vision-dev")
        );

        // Opening a shell runs nothing.
        assert_eq!(parse_command("nix develop .#vision-dev").environment, None);
        assert_eq!(short_labels("nix develop .#vision-dev"), ["nix develop"]);
        assert_eq!(
            parse_command("nix develop -c pytest")
                .environment
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn a_shell_handed_a_command_line_ran_those_commands() {
        let command = "nix develop .#mapper --command bash -c \"cd arcade && \
            cargo check --quiet -p mapper-service --message-format short 2>&1 \
            | grep -E 'error' | head -30\"";
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("mapper"));
        assert_eq!(short_labels(command), ["cargo check mapper-service"]);
        assert!(
            parsed
                .segments
                .iter()
                .all(|segment| segment.environment.as_deref() == Some("mapper"))
        );

        assert_eq!(
            short_labels(
                "nix develop --quiet .#vision-dev --command sh -c \
                 'python -m pytest -q vision/terra/services/wrist_localizer 2>&1 | tail -3'"
            ),
            ["pytest wrist_localizer"]
        );
        // A payload of several lines is a script.
        assert!(matches!(
            kinds("bash -c 'set -e\ncargo build\ncargo test'").as_slice(),
            [SegmentKind::InlineScript { .. }]
        ));
    }

    #[test]
    fn a_summary_names_the_work_not_the_wrapper() {
        let command = "nix develop ..#mapper -c cargo check --quiet \
            -p mapper-service -p photogrammetry --all-targets";
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("mapper"));
        assert_eq!(
            summarize_command(&parsed).as_deref(),
            Some("cargo check --quiet -p mapper-service -p")
        );
        assert_eq!(short_labels(command), ["cargo check mapper-service +1"]);

        assert_eq!(
            summary("ssh box 'cargo build --release'").as_deref(),
            Some("cargo build --release")
        );
    }

    #[test]
    fn a_line_only_half_inside_a_devshell_still_names_it() {
        let command = "cd .. && nix develop .#mapper --command bash -c \
            'cd arcade && cargo fmt && cargo check' ; echo RUST_CLEAN; \
            cd portico && pnpm typecheck && pnpm lint";
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("mapper"));
        assert!(parsed.environment_is_partial());
        assert_eq!(
            environments(command),
            [
                "cargo fmt in mapper",
                "cargo check in mapper",
                "pnpm typecheck",
                "pnpm lint",
            ],
        );

        let whole = parse_command("nix develop .#mapper -c bash -c 'cargo fmt && cargo check'");
        assert_eq!(whole.environment.as_deref(), Some("mapper"));
        assert!(!whole.environment_is_partial());

        let mixed = parse_command(
            "nix develop .#mapper -c cargo check && nix develop .#vision-dev -c ruff check",
        );
        assert_eq!(mixed.environment, None);
        assert!(!mixed.environment_is_partial());
    }

    #[test]
    fn a_package_manager_hands_over_to_the_tool_it_execs() {
        let command = "cd /Users/x/Code/portico && CI=1 pnpm exec vitest run \
            src/robotics/plans/BuildPlan.test.ts 2>&1 | grep -E \"✓ src|× |Tests \" | head -3 \
            && pnpm typecheck 2>&1 | tail -3 \
            && pnpm lint 2>&1 | tail -2";
        assert_eq!(
            short_labels(command),
            ["vitest run", "pnpm typecheck", "pnpm lint"]
        );
        assert_eq!(
            languages(command),
            [Some("typescript"), Some("javascript"), Some("javascript")]
        );
        assert_eq!(parse_command(command).environment, None);
    }

    #[test]
    fn a_grep_over_a_builds_output_is_still_the_build() {
        assert_eq!(
            short_labels("cargo check --workspace 2>&1 | grep -E '^error' -A5 | head -20"),
            ["cargo check"]
        );
        assert_eq!(
            classify_command("cargo check --workspace | grep -E '^error'"),
            CommandClass::Other
        );

        let command = "python3 - <<'PYEOF'\n\
            import pathlib\n\
            pathlib.Path('geom/src/mat3.rs').write_text('x')\n\
            PYEOF\n\
            cargo check --quiet --workspace 2>&1 | grep -E '^error' -A5 | head -20; \
            echo \"EXIT:$?\"";
        assert_eq!(short_labels(command), ["python3 script", "cargo check"]);
        assert_eq!(languages(command), [Some("python"), Some("rust")]);
    }

    #[test]
    fn a_search_over_an_inspection_command_is_a_search() {
        let command = "ps -o pid,etime,state,command -ax | rg 'cargo (test|check)|rustc.*mock_standalone' | head -10";
        assert_eq!(classify_command(command), CommandClass::Search);
        assert_eq!(
            kinds(command),
            vec![SegmentKind::Search {
                query: Some("cargo (test|check)|rustc.*mock_standalone".into()),
            }]
        );
        assert_eq!(classify_command("env | grep CARGO"), CommandClass::Search);
    }

    #[test]
    fn a_perl_one_liner_over_a_file_reads_that_file() {
        let command = "perl -ne 'if (/error TS(\\d+)/) {$p{$1}++} END {for (keys %p) \
            {print \"$p{$_}\\t$_\\n\"}}' /tmp/portico-typecheck.txt | head -80";
        assert_eq!(
            kinds(command),
            vec![SegmentKind::Read {
                paths: vec!["/tmp/portico-typecheck.txt".into()],
                lines: None,
                revision: None,
            }]
        );
        assert_eq!(short_labels(command), ["portico-typecheck.txt"]);
        assert!(matches!(
            kinds("perl -e 'print 1'").as_slice(),
            [SegmentKind::InlineScript { .. }]
        ));
    }

    #[test]
    fn a_heredoc_body_is_data_even_when_it_looks_like_shell() {
        let command = "curl -s 'https://clickhouse.monumental.build' --data-binary @- <<'SQL'\n\
            SELECT system, count() AS n\n\
            FROM telemetry.traces\n\
            WHERE Timestamp >= toDateTime64('2026-08-07 00:00:00', 9)\n\
            GROUP BY system\n\
            SQL";
        assert!(
            !matches!(
                kinds(command).as_slice(),
                [SegmentKind::WriteFile { .. }, ..]
            ),
            "{:?}",
            kinds(command)
        );
        assert_eq!(short_labels(command), ["curl clickhouse.monumental.build"]);
    }

    #[test]
    fn a_function_definition_is_syntax_not_work() {
        let command = "show() { echo \"=== $1:$2-$3 ===\"; sed -n \"$2,$3 p\" \"$1\"; }\n\
            show src/hull/comps/OrientedBoundingBoxesComp.ts 70 80\n\
            show src/robotics/plans/check-marker/Plan.ts 50 58";
        assert_eq!(
            short_labels(command),
            ["show OrientedBoundingBoxesComp.ts", "show Plan.ts"]
        );
        assert!(
            parse_command(command)
                .segments
                .iter()
                .all(|segment| !segment.text.contains("$1") || segment.kind == SegmentKind::Noop),
        );
    }

    #[test]
    fn look_arounds_summarize_to_one_phrase() {
        for (command, class, expected) in [
            (
                "git status --short && sed -n '1,240p' arcade/src/lib.rs \
                 && sed -n '430,930p' arcade/src/lib.rs",
                CommandClass::Read,
                Some("Checked git, read lib.rs"),
            ),
            (
                "sed -n '120,430p' a/lib.rs && sed -n '700,1080p' a/lib.rs \
                 && rg -n \"packed|register\" arcade portico/src",
                CommandClass::Search,
                Some("Searched \"packed|register\", read lib.rs"),
            ),
            (
                "rg -n 'packed' a/lib.rs; sed -n '550,610p' a/geom/bspline.rs; \
                 sed -n '1,45p' a/geom/lib.rs; sed -n '1,70p' a/wasm/lib.rs; \
                 sed -n '105,155p' p/Boundary.test.ts",
                CommandClass::Search,
                Some("Searched \"packed\", read 4 files"),
            ),
            (
                "command -v node || true\n\
                 command -v corepack || true\n\
                 command -v pnpm || true\n\
                 ls -l ~/.local/share/pnpm/pnpm 2>/dev/null || true",
                CommandClass::Read,
                Some("Listed 1 directory, checked for node, corepack, pnpm"),
            ),
            (
                "git show origin/master:portico/src/geom/Vec2.ts | sed -n '1,240p'\n\
                 git show origin/master:portico/src/geom/Vec3.ts | sed -n '1,260p'",
                CommandClass::Read,
                Some("Read 2 files at origin/master"),
            ),
            (
                "git diff --numstat | sort -nr | head -n 35; \
                 git diff --name-status | tail -n 30; git diff -- a/lib.rs | sed -n '1,320p'",
                CommandClass::ReadDiff,
                Some("Read 3 diffs"),
            ),
            (
                "pnpm lint > /tmp/wasm-lint.txt 2>&1; code=$?; \
                 tail -n 100 /tmp/wasm-lint.txt; exit $code",
                CommandClass::Other,
                Some("pnpm lint"),
            ),
            // Two searches with known queries show one chip per query.
            (
                "grep -rn \"class .*PickQuality\\|IMAGE_SIZE\" vision/ml/brick_quality.py | head; \
                 echo \"=== example_inputs ===\"; \
                 grep -n \"example\\|shape\" vision/ml/aot_compile.py | head -50",
                CommandClass::Search,
                None,
            ),
        ] {
            assert_eq!(classify_command(command), class, "{command}");
            assert_eq!(summary(command).as_deref(), expected, "{command}");
        }
    }

    #[test]
    fn a_captured_command_is_what_the_line_was_looking_for() {
        let command = "f=$(grep -rln \"export function withTarget\" \
            portico/src --include='*.ts' | head -1)\n\
            n=$(grep -n \"export function withTarget\" \"$f\" | cut -d: -f1)\n\
            echo \"$f\"\n\
            sed -n \"$((n-6)),$((n+25))p\" \"$f\"";
        assert_eq!(
            short_labels(command),
            [
                "export function withTarget",
                "export function withTarget",
                "read",
            ],
        );
        // An expanded variable names no file, an arithmetic range no lines.
        assert!(
            matches!(
                kinds(command).as_slice(),
                [
                    SegmentKind::Search { .. },
                    SegmentKind::Search { .. },
                    SegmentKind::Noop,
                    SegmentKind::Read { paths, lines, .. },
                ] if paths.is_empty() && lines.is_none()
            ),
            "{:?}",
            kinds(command)
        );

        assert_eq!(kinds("t=$(date +%s)"), [SegmentKind::Noop]);
        assert_eq!(
            kinds("deadline=$(( $(date +%s) + 14400 ))"),
            [SegmentKind::Noop]
        );
        assert_eq!(
            short_labels("out=$(cd portico && rg -n TODO src); echo done"),
            ["TODO"]
        );
    }

    #[test]
    fn a_heredoc_an_interpreter_reads_is_a_script() {
        let command = "T=$(python3 -c \"import time;print(int(time.time()))\")\n\
            for m in Shmem Mapped; do\n\
            curl -s --get \"https://metrics.example/api/v1/query\" \
            --data-urlencode \"query=max_over_time(node_memory_${m}_bytes[6h])\" -o /tmp/$m.json\n\
            done\n\
            python3 - <<'EOF'\n\
            import json, statistics\n\
            print(statistics.median([1, 2, 3]))\n\
            EOF";
        let parsed = parse_command(command);
        assert_eq!(summarize_command(&parsed), None);
        assert_eq!(
            short_labels(command),
            ["curl metrics.example", "python3 script"]
        );
        let scripts = command_scripts(&parsed);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].label, "python3");
        assert_eq!(scripts[0].language.as_deref(), Some("python"));
        assert!(scripts[0].code.contains("import json, statistics"));

        assert!(matches!(
            kinds("cat <<'EOF'\nhello\nEOF").as_slice(),
            [SegmentKind::Read { paths, .. }] if paths.is_empty()
        ));
        assert!(matches!(
            kinds("kubectl apply -f - <<'YAML'\nkind: Pod\nYAML").as_slice(),
            [SegmentKind::Run { program, .. }] if program == "kubectl"
        ));
    }

    #[test]
    fn looking_for_a_program_is_not_running_one() {
        assert_eq!(
            kinds("command -v node || true"),
            vec![
                SegmentKind::Lookup {
                    program: Some("node".into())
                },
                SegmentKind::Noop,
            ]
        );
        assert_eq!(
            kinds("which pnpm"),
            vec![SegmentKind::Lookup {
                program: Some("pnpm".into())
            }]
        );
        assert_eq!(
            kinds("command cargo build"),
            vec![SegmentKind::Run {
                program: "cargo".into(),
                argument: Some("build".into()),
            }]
        );
    }

    #[test]
    fn showing_files_at_a_revision_reads_them() {
        assert_eq!(
            kinds("git show HEAD:src/lib.rs | sed -n '1,240p'"),
            vec![SegmentKind::Read {
                paths: vec!["src/lib.rs".into()],
                lines: Some(1..240),
                revision: Some("HEAD".into()),
            }]
        );
        assert!(matches!(
            kinds("git show HEAD~2").as_slice(),
            [SegmentKind::Git {
                operation: GitOperation::ReadChanges,
                ..
            }]
        ));
    }

    #[test]
    fn destructive_commands_are_their_own_kind() {
        assert_eq!(
            kinds("rm -rf build/ dist/"),
            vec![SegmentKind::Destructive {
                operation: DestructiveOperation::Delete,
                paths: vec!["build/".into(), "dist/".into()],
            }]
        );
        for command in ["git reset --hard HEAD~1", "git checkout -- src/lib.rs"] {
            assert!(
                matches!(
                    kinds(command).as_slice(),
                    [SegmentKind::Destructive {
                        operation: DestructiveOperation::DiscardChanges,
                        ..
                    }]
                ),
                "{command}"
            );
        }
    }

    #[test]
    fn scripts_are_found_even_when_they_ran_elsewhere() {
        let parsed = parse_command(
            "ssh build-box 'cat > /tmp/run.py <<PY\nimport os\nprint(os.getcwd())\nPY'",
        );
        assert_eq!(parsed.host.as_deref(), Some("build-box"));
        let scripts = command_scripts(&parsed);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].label, "/tmp/run.py");
        assert_eq!(scripts[0].language.as_deref(), Some("python"));
        assert!(scripts[0].code.contains("import os"));

        let parsed = parse_command("ssh box \"node -e 'console.log(1)'\"");
        let scripts = command_scripts(&parsed);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].language.as_deref(), Some("javascript"));
    }

    #[test]
    fn environment_runners_hand_over_to_what_they_ran() {
        let command = "direnv exec . pnpm --dir portico exec vite-node \
            --config vite-node.config.ts -e \"import { Bounds3 } from './src/geom/Bounds3'; \
            const b = Bounds3.from({min:{x:0,y:0,z:0}}); console.log(b, b.mid?.z)\"";
        let parsed = parse_command(command);
        assert!(
            matches!(
                parsed.segments.as_slice(),
                [CommandSegment {
                    kind: SegmentKind::InlineScript { interpreter, .. },
                    ..
                }] if interpreter == "vite-node"
            ),
            "{:?}",
            parsed.segments
        );
        assert_eq!(
            summarize_command(&parsed).as_deref(),
            Some("vite-node script")
        );
        let scripts = command_scripts(&parsed);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].language.as_deref(), Some("typescript"));
        assert!(scripts[0].code.contains("Bounds3.from"));

        assert_eq!(summary("pnpm lint").as_deref(), Some("pnpm lint"));
        assert_eq!(
            kinds("npx tsc --noEmit"),
            vec![SegmentKind::Run {
                program: "tsc".into(),
                argument: None,
            }]
        );
        assert!(matches!(
            kinds("docker run -e HOST=example redis").as_slice(),
            [SegmentKind::Run { .. }]
        ));
    }

    #[test]
    fn segment_kinds_carry_their_details() {
        for (command, expected) in [
            (
                "python3 -c 'import sys; print(sys.path)'",
                SegmentKind::InlineScript {
                    interpreter: "python3".into(),
                    code: "import sys; print(sys.path)".into(),
                },
            ),
            (
                "gh pr view 1234 --json state",
                SegmentKind::GitHub {
                    operation: "pr view".into(),
                    target: Some("1234".into()),
                },
            ),
            ("sleep 30", SegmentKind::Wait { seconds: Some(30) }),
            (
                "sed -n '120,430p' arcade/src/lib.rs",
                SegmentKind::Read {
                    paths: vec!["arcade/src/lib.rs".into()],
                    lines: Some(120..430),
                    revision: None,
                },
            ),
            (
                "git commit -m 'hello'",
                SegmentKind::Git {
                    operation: GitOperation::Modify,
                    target: Some("commit".into()),
                },
            ),
            (
                "ls src/",
                SegmentKind::ListDirectory {
                    path: Some("src/".into()),
                },
            ),
            (
                "wc -l a.rs b.rs",
                SegmentKind::CountLines {
                    paths: vec!["a.rs".into(), "b.rs".into()],
                },
            ),
            (
                "echo 'hello' > greeting.txt",
                SegmentKind::WriteFile {
                    path: "greeting.txt".into(),
                    contents: None,
                },
            ),
            ("cd crates/foo", SegmentKind::Noop),
        ] {
            assert_eq!(kinds(command), vec![expected], "{command}");
        }
    }

    #[test]
    fn splits_on_real_separators_only() {
        let parsed = parse_command("cargo build && cargo test; echo done");
        assert_eq!(
            parsed
                .segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["cargo build", "cargo test", "echo done"]
        );
        assert_eq!(parse_command("rg 'a && b' src/").segments.len(), 1);
        assert_eq!(parse_command("echo 'one; two'").segments.len(), 1);

        let parsed = parse_command("cargo build \\\n  --release \\\n  --features x");
        assert_eq!(parsed.segments.len(), 1);
        assert_eq!(
            parsed.segments[0].text,
            "cargo build --release --features x"
        );
    }

    #[test]
    fn heredoc_bodies_are_never_split() {
        let command = "cat > /tmp/run.py <<'PY'\nimport os\nif x:\n    print('a; b && c')\nPY";
        let parsed = parse_command(command);
        assert_eq!(parsed.segments.len(), 1, "{:?}", parsed.segments);
        match &parsed.segments[0].kind {
            SegmentKind::WriteFile { path, contents } => {
                assert_eq!(path, "/tmp/run.py");
                let contents = contents.as_ref().expect("the body is right there");
                assert!(contents.contains("import os"));
                assert!(contents.contains("print('a; b && c')"));
            }
            other => panic!("expected a file write, got {other:?}"),
        }

        let kinds = kinds("cat > /tmp/x.py <<PY\nprint(1)\nPY\npython /tmp/x.py");
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert!(matches!(kinds[0], SegmentKind::WriteFile { .. }));
        assert_eq!(
            kinds[1],
            SegmentKind::Run {
                program: "python".into(),
                argument: Some("/tmp/x.py".into()),
            }
        );
    }

    #[test]
    fn ssh_is_recognized_per_segment() {
        let parsed = parse_command("ssh -p 22 build-box 'rg needle /srv'");
        assert_eq!(parsed.host.as_deref(), Some("build-box"));
        assert!(matches!(
            parsed.segments.as_slice(),
            [CommandSegment {
                kind: SegmentKind::Search { .. },
                ..
            }]
        ));
        assert_eq!(
            classify_command("ssh box 'cat /etc/hosts'"),
            CommandClass::Read
        );

        let parsed = parse_command("cd /srv && ssh build-box 'cargo test'");
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].host, None);
        assert_eq!(parsed.segments[1].host.as_deref(), Some("build-box"));
        assert_eq!(parsed.host.as_deref(), Some("build-box"));

        let parsed = parse_command("cargo build && ssh box ./deploy.sh");
        assert_eq!(parsed.host, None);
        assert_eq!(parsed.segments[1].host.as_deref(), Some("box"));

        let parsed = parse_command("ssh box cat /etc/hosts");
        assert_eq!(parsed.host.as_deref(), Some("box"));
        assert!(matches!(parsed.segments[0].kind, SegmentKind::Read { .. }));
    }

    #[test]
    fn classification() {
        for (command, expected) in [
            ("rg foo | head -20", CommandClass::Search),
            ("cat x.rs | grep foo", CommandClass::Search),
            ("cat x.rs | wc -l", CommandClass::Read),
            ("cd src && cat a.rs", CommandClass::Read),
            ("cd src && cargo build", CommandClass::Other),
            (
                "sed -n '1,5p' a\nsed -n '1,5p' b\nsed -n '1,5p' c",
                CommandClass::Read,
            ),
            ("sed -n '1,5p' a; cargo check", CommandClass::Other),
            (
                "tail -n 140 /tmp/direct-core9.txt; rg -c 'error TS' /tmp/direct-core9.txt || true",
                CommandClass::Search,
            ),
            (
                "for spec in 'a.ts:660,674' 'b.ts:548,770'; do f=${spec%%:*}; \
                 r=${spec#*:}; sed -n \"${r}p\" \"$f\"; done",
                CommandClass::Read,
            ),
            ("sleep 5; rg -n 'ready' /tmp/log.txt", CommandClass::Search),
            ("ls -la src/", CommandClass::Read),
            ("wc -l foo.rs", CommandClass::Read),
            ("echo hi > log.txt", CommandClass::Other),
            ("rg foo 2>/dev/null", CommandClass::Search),
            ("cat foo.rs 2>&1", CommandClass::Read),
            ("git diff", CommandClass::ReadDiff),
            ("git diff HEAD~1", CommandClass::ReadDiff),
            ("git show abc123", CommandClass::ReadDiff),
            ("git log -p", CommandClass::ReadDiff),
            ("git status", CommandClass::GitInfo),
            ("git log --oneline", CommandClass::GitInfo),
            ("git branch", CommandClass::GitInfo),
            ("git commit -m x", CommandClass::Other),
            ("git push", CommandClass::Other),
            ("git checkout main", CommandClass::Other),
            ("gh pr view 123", CommandClass::GitHub),
            ("gh pr list", CommandClass::GitHub),
            ("gh pr checks", CommandClass::GitHub),
            ("gh run list", CommandClass::GitHub),
            ("gh run watch 42", CommandClass::GitHub),
            ("gh issue view 7", CommandClass::GitHub),
            ("gh pr diff", CommandClass::GitHub),
            ("gh pr create", CommandClass::Other),
            ("gh pr merge 123", CommandClass::Other),
            ("gh pr comment 1 -b x", CommandClass::Other),
            ("gh pr checkout 5", CommandClass::Other),
            ("gh api /repos/x/y", CommandClass::Other),
            ("gh pr", CommandClass::Other),
            ("gh pr view 1 && git status", CommandClass::GitInfo),
            ("ps -ax", CommandClass::Inspect),
            ("df -h", CommandClass::Inspect),
            ("uname -a", CommandClass::Inspect),
            ("jq .version package.json", CommandClass::Inspect),
            ("basename /a/b.rs", CommandClass::Inspect),
            ("cargo test", CommandClass::Other),
            ("pytest -q", CommandClass::Other),
            ("yq .a f.yaml", CommandClass::Other),
            ("ps -ax | rg cargo", CommandClass::Search),
            ("cargo test 2>&1 | rg FAILED", CommandClass::Other),
            (
                "cat Cargo.toml && jq .name package.json",
                CommandClass::Read,
            ),
        ] {
            assert_eq!(classify_command(command), expected, "{command}");
        }
    }
}
