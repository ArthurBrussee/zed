//! A small shell-command reader.
//!
//! Agents hand us command lines as opaque strings, and the UI wants to say
//! something better than "a command ran": which files were read, what was
//! searched for, which file a heredoc wrote, whether it all happened on
//! another machine. That means actually parsing the line rather than pattern
//! matching on it, because the interesting cases are exactly the ones naive
//! splitting gets wrong:
//!
//! - a heredoc body (`cat > x.py <<'PY' … PY`) is data, not commands, and
//!   splitting it produces one chip per line of someone's Python file;
//! - a backslash-newline continuation is one command, not two;
//! - `;`, `&&`, `|` inside quotes are characters, not separators;
//! - `ssh host 'cmd'` is a command running somewhere else, and the part worth
//!   reading is inside the quotes.
//!
//! The parser is deliberately shallow: it understands separators, quoting,
//! heredocs, and redirects, and leaves everything else as opaque argument
//! text. It never executes anything and never resolves paths.

use std::ops::Range;

/// One command line, split into the pipelines that actually run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommand {
    /// The machine this runs on, when the whole line is an ssh invocation.
    pub host: Option<String>,
    /// The devshell the line's work ran in, when the segments that used one
    /// agree on which. A line may be only partly inside it; see
    /// [`ParsedCommand::environment_is_partial`].
    pub environment: Option<String>,
    pub segments: Vec<CommandSegment>,
}

/// One pipeline (`a | b | c`) of a command line, with its source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSegment {
    /// The segment's own text, verbatim, so the UI can show a real substring
    /// of what ran rather than a reconstruction.
    pub text: String,
    pub kind: SegmentKind,
    /// The machine this segment ran on, when it was wrapped in ssh. A line can
    /// mix local and remote work (`cargo build && ssh box ./deploy`), so this
    /// belongs to the segment rather than the line.
    pub host: Option<String>,
    /// The Nix devshell this segment ran in (`nix develop .#vision-dev
    /// --command ruff check`), by name. The wrapper is not the work, but which
    /// environment the work happened in is worth a word.
    pub environment: Option<String>,
}

/// What a pipeline is for. This is the vocabulary the UI renders: each kind
/// carries the little bit of data a richer widget needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// Changing directory, setting variables: real syntax, no work.
    Noop,
    /// Reading file contents (`cat`, `sed -n`, `head`). A `sed -n '12,80p'`
    /// says which lines it wanted, which is enough to open the file there.
    Read {
        paths: Vec<String>,
        lines: Option<Range<u32>>,
        /// The revision the contents came from, for `git show rev:path`. The
        /// working copy reads as `None`.
        revision: Option<String>,
    },
    /// Looking for something (`rg`, `grep`, `find`).
    Search { query: Option<String> },
    /// Listing a directory (`ls`, `tree`).
    ListDirectory { path: Option<String> },
    /// Asking whether a program exists (`command -v`, `which`, `type`). The
    /// answer shapes what runs next; the asking is not itself work.
    Lookup { program: Option<String> },
    /// Counting rather than reading (`wc`): still just looking at a file.
    CountLines { paths: Vec<String> },
    /// A git operation. Reading changes, asking about state, and rewriting
    /// history are very different acts, so the operation is kept rather than
    /// flattened to "ran git".
    Git {
        operation: GitOperation,
        target: Option<String>,
    },
    /// Writing a file, either by redirect or by heredoc. `contents` is
    /// present for heredocs, where the body is right there in the command.
    WriteFile {
        path: String,
        contents: Option<String>,
    },
    /// Rewriting files in place (`sed -i`, `perl -pi -e`). It reads like a
    /// read, but it changes every file it names.
    EditInPlace { paths: Vec<String> },
    /// Removing, moving, or re-permissioning files. Never quiet: these are the
    /// ones worth catching in a wall of chips.
    Destructive {
        operation: DestructiveOperation,
        paths: Vec<String>,
    },
    /// Code handed to an interpreter on the command line (`python -c`,
    /// `node -e`, `bash -c`): the payload is right there, like a heredoc body.
    InlineScript { interpreter: String, code: String },
    /// Waiting. Folds into whatever the line was really doing.
    Wait { seconds: Option<u32> },
    /// A GitHub CLI operation.
    GitHub {
        operation: String,
        target: Option<String>,
    },
    /// Running a script or program by name (`python x.py`, `cargo test`).
    Run {
        program: String,
        argument: Option<String>,
    },
}

/// What a destructive command does, so the chip can say it plainly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestructiveOperation {
    Delete,
    Move,
    ChangePermissions,
    /// Throwing away work in git (`reset --hard`, `clean -fd`, `checkout --`).
    DiscardChanges,
}

/// The git verb, as far as the UI needs to care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitOperation {
    /// Showing changes: `diff`, `show`, `log -p`.
    ReadChanges,
    /// Asking about state without changing anything: `status`, `log`,
    /// `branch`, `blame`.
    Inspect,
    /// Changing the repository: `commit`, `push`, `checkout`, `rebase`, ...
    Modify,
}

impl SegmentKind {
    /// Whether this segment is worth a chip of its own, or is bookkeeping
    /// that only exists to set up the next one.
    pub fn is_noop(&self) -> bool {
        matches!(self, SegmentKind::Noop)
    }

    /// Whether this segment is worth naming when several share a chip.
    /// Waiting is real (a `sleep` in a polling loop is why the line took an
    /// hour) but it is not one of the things the line did, so it keeps its
    /// place in the classification and stays off the chip.
    pub fn is_worth_naming(&self) -> bool {
        !self.is_noop() && !matches!(self, SegmentKind::Wait { .. })
    }

    /// Whether this segment only looked at something. A search downstream of
    /// one of these is the point of the line rather than a filter on work:
    /// `ps -ax | rg cargo` is a search the same way `cat x | grep y` is, while
    /// `cargo check | grep '^error'` stays a build whose output got filtered.
    fn is_just_looking(&self) -> bool {
        match self {
            SegmentKind::Noop
            | SegmentKind::Read { .. }
            | SegmentKind::ListDirectory { .. }
            | SegmentKind::Lookup { .. }
            | SegmentKind::CountLines { .. } => true,
            // `ls` and `du` have their own kind; the rest of the inspection
            // commands carry nothing worth a kind of their own and fall
            // through the classification table to `Run`.
            SegmentKind::Run { program, .. } => INSPECTION_PROGRAMS.contains(&program.as_str()),
            _ => false,
        }
    }
}

/// Programs that only report the machine's state. They change nothing and
/// their whole output exists to be read, so a filter over one is reading the
/// state rather than trimming the output of work.
const INSPECTION_PROGRAMS: &[&str] = &[
    "date",
    "df",
    "env",
    "free",
    "hostname",
    "id",
    "printenv",
    "ps",
    "top",
    "uname",
    "uptime",
    "who",
    "whoami",
];

/// Programs whose first argument is a subcommand rather than a file, so both
/// words together name what ran. `cargo` alone says nothing; `cargo test` and
/// `cargo fmt` are different acts.
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

/// The toolchain a program belongs to, named as a language. Callers use it to
/// show whose logo the command wears: `cargo` is Rust, `pytest` is Python.
/// A program that is not part of any one language's toolchain has none.
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
    /// The part of the segment that is the work, with the wrappers that only
    /// arranged for it peeled off. Where a command ran (`ssh box …`) and which
    /// devshell it ran in (`nix develop .#mapper -c …`) are reported on their
    /// own, so a label that repeats them says the same thing twice and spends
    /// its width on the saying.
    pub fn work_text(&self) -> &str {
        let local = unwrap_ssh(&self.text).command;
        match nix_devshell_command(local) {
            Some((_, inner)) => inner,
            None => local,
        }
    }

    /// A compact name for what this segment did, for when several segments
    /// share one chip and there is no room for the text itself. Falls back to
    /// the opening words, which is what the reader would have seen anyway.
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
            // Never abbreviated to a verb: which files were about to go is the
            // whole point of noticing one of these.
            SegmentKind::Destructive { .. } => first_words(self.work_text(), 4),
            SegmentKind::Run { program, argument } => {
                let name = base_name(program);
                match argument.as_deref() {
                    // `cargo test`, `npm run`: the subcommand is the verb, and
                    // for cargo the packages say which crates it was about.
                    Some(argument) if SUBCOMMAND_PROGRAMS.contains(&name) => {
                        match package_arguments(self.work_text()).as_slice() {
                            [] => format!("{name} {argument}"),
                            [only] => format!("{name} {argument} {only}"),
                            [first, rest @ ..] => {
                                format!("{name} {argument} {first} +{}", rest.len())
                            }
                        }
                    }
                    // A URL is named by the machine it addresses, a path by the
                    // file at the end of it: `curl api.example`, `python
                    // gen.py`. A bare word is whatever the program calls its
                    // first argument, which for anything the parser has never
                    // heard of is the only thing distinguishing one call from
                    // the next: a script's own `run_rpc GetCameraType` and
                    // `run_rpc Preview` are two different acts.
                    //
                    // A program is the exception: a jq filter names nothing, so
                    // the line's own paths are asked instead, and `jq '…'
                    // doc.json` is about doc.json.
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

    /// The language whose logo this segment should wear, when it has one.
    pub fn language(&self) -> Option<&'static str> {
        match &self.kind {
            SegmentKind::Run { program, .. } => program_language(program),
            SegmentKind::InlineScript { interpreter, .. } => program_language(interpreter),
            _ => None,
        }
    }
}

/// How a whole command reads, for the chip-collapsing rules: a command that
/// only reads is a read, one that only searches is a search, and anything
/// doing real work is neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    Search,
    Read,
    ReadDiff,
    /// Asking git about state: repeated `git status` is the same kind of quiet
    /// noise as repeated file reads.
    GitInfo,
    Other,
}

pub fn parse_command(command: &str) -> ParsedCommand {
    let segments: Vec<CommandSegment> = split_segments(command)
        .into_iter()
        .flat_map(|text| {
            // A segment may itself be an ssh invocation; what it really does
            // is whatever it asked the remote machine to do.
            let unwrapped = unwrap_ssh(&text);
            let host = unwrapped.host.clone();
            let environment = nix_devshell(unwrapped.command);

            // A devshell only arranges where a command runs; what it handed
            // off to (`nix develop .#mapper --command bash -c "…"`) is what a
            // shell payload should be read from.
            let local = match nix_devshell_command(unwrapped.command) {
                Some((_, inner)) => inner,
                None => unwrapped.command,
            };

            // A shell handed a command line runs that line, and a line can hold
            // several commands: `bash -c "cd x && cargo check | grep error"` is
            // a change of directory and a build, not one opaque thing. Reading
            // it as one thing loses the build, since the first word is `cd` and
            // the only stage left with an opinion is the grep.
            let inner = shell_payload(local)
                .map(|payload| split_segments(&payload))
                .filter(|segments| segments.len() > 1);
            if let Some(inner) = inner {
                return inner
                    .into_iter()
                    .map(|text| CommandSegment {
                        kind: classify_segment(&text),
                        text,
                        // Where the shell was reached from is where everything
                        // it ran was reached from.
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

    // The line as a whole ran remotely only when every working segment did, on
    // the same machine: half a line on another box has no single answer, so it
    // gets none.
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

    // A devshell does not read the same way. Half a line inside
    // `nix develop .#mapper` is exactly what explains why that half has a
    // toolchain the other does not, and saying nothing loses it, so the badge
    // names the devshell whenever the segments that used one agree on which.
    let environment = shared_environment(&segments);

    ParsedCommand {
        host,
        environment,
        segments,
    }
}

/// The devshell a line's working segments agree on, ignoring the ones that ran
/// in none. `None` when nothing used one, or when two segments used different
/// ones and there is no single answer to give.
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
    /// Whether only part of the line ran in [`ParsedCommand::environment`]. The
    /// chip says so rather than implying the whole line had that toolchain, and
    /// marks the acts the devshell covered.
    pub fn environment_is_partial(&self) -> bool {
        self.environment.is_some()
            && self
                .segments
                .iter()
                .any(|segment| segment.kind.is_worth_naming() && segment.environment.is_none())
    }
}

/// The Nix devshell a command ran in, when it is wrapped in one:
/// `nix develop .#vision-dev --command ruff check` ran in `vision-dev`. The
/// wrapper without an inner command opens a shell rather than running
/// anything, and has no devshell to report.
fn nix_devshell(command: &str) -> Option<String> {
    nix_devshell_command(command).map(|(shell, _)| shell)
}

/// The same wrapper, read for both halves: the devshell it named, and the
/// command it was asked to run in there.
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
            // A lone argument after the flag is a quoted command line, and the
            // quotes are the wrapper's, not the command's.
            let inner = if tokens.len() == index + 2 {
                strip_matching_quotes(inner)
            } else {
                inner
            };
            // A flake reference names the shell after `#`; a bare one (or
            // none at all) is the default shell of whatever flake is here.
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

/// How the command reads as a whole. A single real command anywhere makes it
/// `Other`: the agent is doing something, not just looking around.
pub fn classify_command(command: &str) -> CommandClass {
    let parsed = parse_command(command);
    let mut any_search = false;
    let mut any_read = false;
    let mut any_diff = false;
    let mut any_git_info = false;
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
            // Waiting is never what a line was for.
            SegmentKind::Wait { .. } => {}
            SegmentKind::WriteFile { .. }
            | SegmentKind::EditInPlace { .. }
            | SegmentKind::Destructive { .. }
            | SegmentKind::InlineScript { .. }
            | SegmentKind::GitHub { .. }
            | SegmentKind::Run { .. } => {
                return CommandClass::Other;
            }
        }
    }
    // Searching is the loudest thing a look-around does, then reading changes,
    // then reading files, then merely asking git how things stand.
    if any_search {
        CommandClass::Search
    } else if any_diff {
        CommandClass::ReadDiff
    } else if any_read {
        CommandClass::Read
    } else if any_git_info {
        CommandClass::GitInfo
    } else {
        CommandClass::Other
    }
}

/// A script a command carried with it: a heredoc body it wrote, or code it
/// handed an interpreter. Worth showing in full, since it is the actual work
/// rather than a reference to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandScript {
    /// What to call it: the file it was written to, or the interpreter it was
    /// fed to.
    pub label: String,
    /// A markdown language tag, for highlighting.
    pub language: Option<String>,
    pub code: String,
}

/// Every script the line carries, in the order they appear. ssh is already
/// unwrapped by the parser, so a script sent to another machine is found the
/// same way a local one is.
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

/// A short description of what a command line did, for a chip label: the acts
/// it performed rather than the text it was written as. `None` when the line
/// is better shown verbatim (several unrelated real commands).
///
/// This is what makes a wall of chained `sed`/`rg`/`git` read as one thing:
/// the line is a look-around, and this says how much of one.
pub fn summarize_command(parsed: &ParsedCommand) -> Option<String> {
    let mut searches: Vec<&str> = Vec::new();
    let mut read_paths: Vec<&str> = Vec::new();
    // One entry per reading segment, so a line that read everything at one
    // revision can say so.
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
            // Waiting says nothing about what the line was for.
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
            // The text of a line carrying a script is mostly the script; name
            // what ran it instead, and let the chip show the code.
            SegmentKind::InlineScript { interpreter, .. } => format!("{interpreter} script"),
            _ => first_words(runs[0].work_text(), 6),
        });
    }

    // Several real commands have no summary: a chain of them is not one act,
    // and "python3, cargo" says less than the chain itself. The UI shows each
    // act with its own glyph and name instead.
    if !runs.is_empty() {
        return None;
    }

    // Several searches and nothing else: each query is known, and a chip per
    // query says what "Ran 2 searches" cannot. The chain renders them.
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
    // `git show origin/master:a.ts` and its siblings all read one revision;
    // saying which is most of what makes those lines legible.
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
    // Naming what was looked for is the whole content of a lookup.
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
    // The first clause leads, so it is capitalized.
    let mut chars = label.chars();
    if let Some(first) = chars.next() {
        label = first.to_uppercase().collect::<String>() + chars.as_str();
    }
    Some(label)
}

/// Whether a word is a path rather than something that merely contains a
/// slash or a dot. A jq program is full of both (`.inner.impl.path //
/// "inherent"`), and treating one as a path names the chip after whatever
/// follows its last slash.
fn looks_like_path(word: &str) -> bool {
    if word.is_empty() || word.contains(char::is_whitespace) {
        return false;
    }
    if word.contains('/') || word.contains('\\') {
        // A path may hold dots, but a slash inside quotes usually means an
        // expression: real paths do not carry brackets or pipes.
        return !word.contains(['|', '[', ']', '(', ')', '"']);
    }
    // Otherwise it needs a plausible extension: `main.rs`, not `.inner.impl`.
    word.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && (1..=6).contains(&extension.len())
            && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
    })
}

/// The first argument on the line that is actually a path.
fn path_argument(text: &str) -> Option<String> {
    split_tokens(text)
        .into_iter()
        .skip(1)
        .map(|range| strip_matching_quotes(&text[range]))
        .find(|word| !word.starts_with('-') && looks_like_path(word))
        .map(str::to_string)
}

/// The packages a cargo-style command names with `-p`/`--package`. Which
/// crates were checked is most of what a `cargo check` chip has to say.
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

/// The host of a URL, when the argument is one. Everything past the host is
/// detail; the machine is the part worth a chip's width.
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

/// The first few words of a command, for naming what ran.
fn first_words(text: &str, max: usize) -> String {
    let mut words: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        // Stop at a redirect: where the output went is not what ran.
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

/// `ssh host 'cmd'` is a command that happens elsewhere; the part worth
/// reading is the quoted payload. Options that take a value (`-p 22`) are
/// skipped so the destination is not mistaken for one of them.
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

    // Everything after the destination is the remote command; it is usually
    // one quoted argument, in which case the quotes are not part of it.
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

/// Splits a command line into its pipelines, respecting quotes, heredoc
/// bodies, and backslash-newline continuations. Chain operators (`&&`, `||`,
/// `;`) and newlines separate; a pipeline's own `|` stages stay together here
/// and are split during classification.
/// Whether a segment is shell punctuation rather than a command.
///
/// Splitting a script on its separators leaves the syntax between them behind:
/// a comment on its own line, a `case` pattern like `failure|cancelled)` (whose
/// own `|` the pipeline splitter has already taken apart), the `{` and `}` of a
/// brace group, a bare `fi`. None of it ran, and each one otherwise arrives as
/// a chip named after a fragment of the script.
fn is_shell_punctuation(segment: &str) -> bool {
    let text = segment.trim();
    if text.is_empty() || text.starts_with('#') {
        return true;
    }
    if text.chars().all(|ch| "{}()[];&|<>".contains(ch)) {
        return true;
    }
    // `pattern)` and `pattern|other)` open a `case` arm. A real command never
    // arrives as a single word ending in a close bracket.
    !text.contains(char::is_whitespace) && text.ends_with(')') && !text.contains('(')
}

/// Whether the text so far ends in a function header (`name()`), which is what
/// makes the `{` after it a definition rather than a group or an expansion.
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

fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.char_indices().peekable();
    let mut quote: Option<char> = None;
    // Heredoc delimiters collected from the current line; their bodies start
    // after the line ends.
    let mut pending_heredocs: Vec<String> = Vec::new();

    let push = |segments: &mut Vec<String>, current: &mut String| {
        let text = current.trim().to_string();
        if !text.is_empty() {
            segments.push(text);
        }
        current.clear();
    };

    while let Some((index, ch)) = chars.next() {
        // Inside quotes everything is literal except the closing quote.
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
            // A backslash-newline is a continuation: one command, not two.
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
                // `<<WORD`, `<<'WORD'`, `<<-WORD`: the body is data and must
                // survive intact, so remember the delimiter and swallow
                // everything up to it once this line ends.
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
                // Consume heredoc bodies verbatim, delimiter by delimiter.
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
            // `name() { … }` defines a function. Nothing in the body runs
            // here, and splitting it at every `;` inside the braces turns one
            // definition into a chip per line of it.
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

/// Token ranges of a single command, quote-aware. Quotes are kept in the
/// range so callers can tell a quoted argument from a bare one.
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
    // Defining a function runs none of it. The work is in the calls, which
    // read as calls of whatever the function was named.
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

    // A heredoc with nowhere to be written feeds the command's stdin:
    // `python3 - <<'PY'` is a script the same way `python3 -c` is.
    if let Some(heredoc_at) = unquoted_find(segment, "<<") {
        let prefix = &segment[..heredoc_at];
        if let Some(interpreter) = stdin_interpreter(prefix)
            && let Some(code) = heredoc_body(&segment[heredoc_at..])
        {
            return SegmentKind::InlineScript { interpreter, code };
        }
        // The body is data either way, so only the command in front of it can
        // say what happened. Reading the body as arguments invents paths.
        return classify_segment(prefix);
    }

    // A pipeline's meaning comes from where its data starts: `git diff | sort
    // | head` reads changes, and the sorting and trimming are plumbing. A
    // filter that searches can outrank the source (`cat x | grep y` is a
    // search, not a read), but only when the source was itself just looking:
    // `cargo check | grep '^error'` is a build whose output was filtered, and
    // calling it a search loses the build.
    let mut result = SegmentKind::Noop;
    for (index, stage) in split_stages(segment).into_iter().enumerate() {
        let kind = classify_stage(&stage);
        // A search only becomes the point of the line when nothing earlier
        // already said what happened: `cat x | grep y` is a search, but
        // `pnpm exec vitest run x | grep -E "..."` is a test run whose output
        // got filtered for readability, not the search itself.
        if matches!(kind, SegmentKind::Search { .. }) && result.is_just_looking() {
            return kind;
        }
        if index == 0 || result.is_noop() {
            if !kind.is_noop() {
                result = kind;
            }
            continue;
        }
        // A filter that trims to a line range says which lines the source was
        // read for: `git show rev:file | sed -n '1,240p'` wanted the first 240.
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
        // Later stages only matter when the source said nothing.
        if is_plumbing(&stage) {
            continue;
        }
    }
    result
}

/// Stages that only reshape someone else's output: they never decide what a
/// pipeline was for.
fn is_plumbing(stage: &str) -> bool {
    const FILTERS: &[&str] = &[
        "sort", "head", "tail", "uniq", "cut", "tr", "column", "wc", "cat", "tee", "jq", "sed",
        "awk", "xargs", "nl", "rev", "paste", "fold", "expand", "less", "more", "tac",
    ];
    stage
        .split_whitespace()
        .next()
        .is_some_and(|head| FILTERS.contains(&head))
}

/// Splits a pipeline into its stages on unquoted `|`.
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

/// The file a segment writes, when it redirects into one or feeds a heredoc.
/// `2>/dev/null` and `2>&1` are noise suppression, not writing.
fn write_target(segment: &str) -> Option<SegmentKind> {
    // A heredoc is checked first: in `cat > file <<EOF` the redirect comes
    // earlier in the text, but the body is the interesting part and only the
    // heredoc knows where the segment really ends.
    let heredoc_at = unquoted_find(segment, "<<");
    if let Some(heredoc_at) = heredoc_at
        && let Some(path) = redirect_path(&segment[..heredoc_at])
    {
        let contents = heredoc_body(&segment[heredoc_at..]);
        return Some(SegmentKind::WriteFile { path, contents });
    }

    // Past that, only the command matters: a heredoc body is data, and a SQL
    // statement handed to `curl --data-binary @-` is full of `>=` that is not
    // a redirect to a file called "=".
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
                    // Sending output to a scratch file is capturing it, not
                    // authoring a file: `pnpm lint > /tmp/out.txt` still just
                    // ran the linter.
                    if is_scratch_path(&path) {
                        return None;
                    }
                    // `foo > bar <<EOF` is handled above; a plain redirect
                    // has no body to show.
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

/// The byte offset of `needle` outside quotes.
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

/// Whether the flags ask for editing in place rather than printing.
fn is_in_place(rest: &[&str]) -> bool {
    rest.iter()
        .any(|word| word.starts_with("-i") || word.contains("i") && word.starts_with("-p"))
}

/// The interpreter a heredoc is being fed to, given everything before the
/// `<<`. Only programs that run what they are given: `cat <<EOF` prints text.
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

/// Whether a `-e` payload is a program rather than a value. `-e` means "env
/// var" to some tools and "evaluate this" to others, and only the payload
/// tells them apart.
fn looks_like_code(payload: &str) -> bool {
    payload.contains('(') || payload.contains(';') || payload.contains('{')
}

/// The code an interpreter was handed on the command line.
fn inline_code(rest: &[&str]) -> Option<String> {
    let index = rest
        .iter()
        .position(|word| matches!(*word, "-c" | "-e" | "--eval" | "--command"))?;
    let code = rest.get(index + 1)?;
    Some(strip_matching_quotes(code).to_string())
}

/// The command line a shell was handed, when a segment is one: `bash -c '…'`.
/// A payload of several lines is a real script rather than a command line, and
/// keeps its own chip.
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

/// Splits `origin/master:src/lib.rs` into the revision and the path. A target
/// without a colon is a commit, and one whose right side is empty or looks
/// like a flag is not a path.
fn revision_and_path(target: &str) -> Option<(String, String)> {
    let (revision, path) = target.split_once(':')?;
    if revision.is_empty() || path.is_empty() || path.starts_with('-') {
        return None;
    }
    Some((revision.to_string(), path.to_string()))
}

/// The line range a `sed -n` script asks for, when it is a simple `A,Bp`.
fn sed_line_range(script: &str) -> Option<Range<u32>> {
    let script = strip_matching_quotes(script).trim_end_matches('p');
    let (start, end) = script.split_once(',')?;
    let start = start.trim().parse::<u32>().ok()?;
    let end = end.trim().parse::<u32>().ok()?;
    (start <= end).then_some(start..end)
}

/// Whether a path is somewhere output gets parked rather than somewhere a
/// project keeps files.
fn is_scratch_path(path: &str) -> bool {
    path.starts_with("/tmp/")
        || path.starts_with("/var/tmp/")
        || path.starts_with("/var/folders/")
        || path.starts_with("/dev/")
        || path.starts_with("$TMPDIR")
}

/// The path a redirect writes to, given everything before the heredoc.
fn redirect_path(prefix: &str) -> Option<String> {
    let mut parts = prefix.rsplit('>');
    let target = parts.next()?.trim();
    if parts.next().is_none() {
        // No redirect at all: a bare heredoc feeds a command's stdin.
        return None;
    }
    let target = target.split_whitespace().next()?;
    Some(strip_matching_quotes(target).to_string())
}

/// The body of a heredoc, given text starting at `<<`.
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

/// A subshell's parentheses and a brace group's braces group commands, and a
/// trailing `&` puts the job in the background. None of it is part of what
/// ran, and a stage split out of `(a && b) &` or `&& { a; b; }` otherwise
/// starts (or, once a leading keyword like `do` is peeled off, comes to
/// start) with a bracket glued to the program name.
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

/// Whether `name` (the part of a word before its first `=`) is something bash
/// would actually assign to: a plain identifier (`retries=1`) or an array
/// element of one (`retries[$id]=1`). The subscript's own contents are not
/// validated further; anything between one matched pair of brackets is fine.
fn is_assignment_target(name: &str) -> bool {
    let ident = match name.split_once('[') {
        Some((ident, rest)) if rest.ends_with(']') => ident,
        Some(_) => return false,
        None => name,
    };
    !ident.is_empty() && ident.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// One stage of a pipeline: the program and what it is doing.
fn classify_stage(stage: &str) -> SegmentKind {
    let stage = trim_brackets(stage);

    let tokens = split_tokens(stage);
    let mut words: Vec<&str> = tokens.iter().map(|range| &stage[range.clone()]).collect();

    // Environment assignments and wrappers come before the real program.
    while let Some(word) = words.first() {
        // A keyword like `do` can sit in front of a bracket that only shows
        // up once the keyword is peeled off (`do (grpcurl …`), so this runs
        // on every pass, not just the stage's original leading edge.
        let debracketed = trim_brackets(word);
        if debracketed != *word {
            if debracketed.is_empty() {
                words.remove(0);
            } else {
                words[0] = debracketed;
            }
            continue;
        }
        let assigned = word.split_once('=').filter(|(name, _)| is_assignment_target(name));
        // Capturing a command's output into a variable is bookkeeping, like
        // `code=$?` next to it. The substitution also runs through the rest of
        // the line, so there is no program left to read here.
        if assigned.is_some_and(|(_, value)| value.starts_with("$(")) {
            return SegmentKind::Noop;
        }
        let is_assignment = assigned.is_some();
        // `command -v foo` asks whether foo exists rather than running
        // anything, so only the bare wrapper form hands over to what follows.
        if *word == "command" && words.get(1).is_some_and(|word| word.starts_with('-')) {
            break;
        }
        // `do`/`then` introduce a body; the body is the interesting part.
        if is_assignment
            || matches!(
                *word,
                "command" | "env" | "do" | "then" | "else" | "time" | "nohup"
            )
        {
            words.remove(0);
            continue;
        }
        // Runners that exist to set up an environment and then hand over:
        // `direnv exec .`, `pnpm --dir portico exec`, `uv run`, `npx`. What
        // ran is what came after them.
        let skip = match *word {
            "npx" | "pnpx" | "bunx" => 1,
            "direnv" if words.get(1) == Some(&"exec") => 3,
            "poetry" | "uv" | "pipenv" | "rye" | "hatch" if words.get(1) == Some(&"run") => 2,
            // `python -m pytest` runs pytest; the interpreter is how it got
            // there. A python given a script keeps its own name.
            "python" | "python3" if words.get(1) == Some(&"-m") => 2,
            "bundle" | "dotenv" if words.get(1) == Some(&"exec") => 2,
            // `nix develop .#shell --command cmd`: the devshell is where it
            // ran, `cmd` is what ran. Without `--command` there is no inner
            // command (`nix develop` opens a shell, `nix build` builds).
            "nix" if matches!(words.get(1).copied(), Some("develop" | "shell")) => {
                match words
                    .iter()
                    .position(|word| matches!(*word, "--command" | "-c"))
                {
                    Some(index) => index + 1,
                    None => break,
                }
            }
            // A package manager passes through only for `exec`/`dlx`;
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
    // Arguments that name files, as opposed to flags and the values flags
    // take: `tail -n 100 log.txt` reads one file, not a file called "100".
    let paths = |rest: &[&str]| -> Vec<String> {
        rest.iter()
            .filter(|word| !word.starts_with('-'))
            // A bare number is a count a flag took (`tail -n 100 log.txt`),
            // not a file.
            .filter(|word| !word.chars().all(|ch| ch.is_ascii_digit()))
            .map(|word| strip_matching_quotes(word).to_string())
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
        // A shell handed a command line is not running a script, it is running
        // that command: `sh -c 'pytest -q tests'` ran pytest. A payload with
        // several lines is a real script and falls through to the generic
        // inline-script arm below.
        "sh" | "bash" | "zsh" | "dash" | "ksh"
            if inline_code(rest).is_some_and(|code| !code.contains('\n')) =>
        {
            let inner = inline_code(rest).unwrap_or_default();
            classify_segment(&inner)
        }
        // Code on the command line is the same shape as a heredoc body.
        "python" | "python3" | "node" | "bash" | "sh" | "zsh" | "ruby" | "perl"
            if inline_code(rest).is_some() && !is_in_place(rest) =>
        {
            SegmentKind::InlineScript {
                interpreter: (*head).to_string(),
                code: inline_code(rest).unwrap_or_default(),
            }
        }
        // `echo`/`printf` between two real commands is a divider the agent
        // printed for itself. Writing one to a file is a redirect, which is
        // decided before a stage is ever classified.
        // `wait` blocks until the background jobs finish. Like `sleep`, the
        // waiting is not the work; the jobs it waits on are.
        "cd" | "pushd" | "popd" | "export" | "true" | ":" | "exit" | "set" | "unset" | "shift"
        | "local" | "declare" | "typeset" | "readonly" | "alias" | "unalias" | "trap" | "shopt"
        | "done" | "fi" | "esac" | "for" | "while" | "until" | "if" | "case" | "then" | "elif"
        | "do" | "return" | "break" | "continue" | "echo" | "printf" | "wait" => SegmentKind::Noop,
        // Rewriting files in place looks like reading but changes everything
        // it names.
        "sed" | "perl" if is_in_place(rest) => SegmentKind::EditInPlace {
            paths: paths(rest).into_iter().skip(1).collect(),
        },
        // `perl -ne '…' file` is awk with different syntax: a program run over
        // a file's lines, which reads that file. Without `-n` or `-p` there is
        // no file to loop over and `perl -e` is a script like any other.
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
            // `git show rev:path` prints a file as it was: a read of that file
            // at a revision, not a diff.
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
                // `git grep` is a search like any other.
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
                // Throwing away work is not the same as making a commit.
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
            let lines = None;
            // Tailing the file a command just wrote its output to says nothing
            // about the project.
            if !paths.is_empty() && paths.iter().all(|path| is_scratch_path(path)) {
                SegmentKind::Noop
            } else {
                SegmentKind::Read {
                    paths,
                    lines,
                    revision: None,
                }
            }
        }
        "which" | "type" | "hash" | "whereis" => SegmentKind::Lookup {
            program: positional(rest),
        },
        // `command -v foo`: the wrapper strip leaves this arm the whole line.
        "command" => SegmentKind::Lookup {
            program: positional(rest),
        },
        "wc" => SegmentKind::CountLines { paths: paths(rest) },
        "ls" | "tree" | "du" => SegmentKind::ListDirectory {
            path: positional(rest),
        },
        // `sed -n '1,50p' file` reads a range; `sed -i` edits in place.
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
        // Any other runner handed a program on the command line. The payload
        // has to look like code, so `docker run -e HOST=x` stays a run.
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


/// A determinate progress reading that a running command printed about itself,
/// as a fraction in `0.0..=1.0`.
///
/// Only a stated fraction qualifies. A wrong progress bar is worse than none,
/// so a tool that merely counts is left to the last-line fallback: `pnpm`
/// prints "resolved 2, reused 0, downloaded 2" with no denominator, so "2" is
/// not 2 of anything, and `cargo` keeps its `N/M` in a progress bar that does
/// not survive being captured.
pub fn progress_fraction(line: &str) -> Option<f32> {
    pytest_percent(line)
}

/// `pytest` ends every file's line with the run's running total, right
/// aligned: `test_mod1.py ......      [ 50%]`.
fn pytest_percent(line: &str) -> Option<f32> {
    let inside = line.trim_end().strip_suffix(']')?.rsplit_once('[')?.1.trim();
    let digits = inside.strip_suffix('%')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let percent: u32 = digits.parse().ok()?;
    (percent <= 100).then(|| percent as f32 / 100.0)
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

    /// Each named act and the devshell it ran in, so a mixed line can be read
    /// segment by segment rather than through the line's collapsed answer.
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
    fn a_subcommand_tool_is_named_by_its_subcommand() {
        // The package the crate names it was pointed at, same as
        // `cargo_names_the_crates_it_was_pointed_at` below.
        assert_eq!(
            short_labels("cargo test -p acp_thread"),
            ["cargo test acp_thread"]
        );
        assert_eq!(short_labels("cargo clippy --fix"), ["cargo clippy"]);
        assert_eq!(short_labels("npm run build"), ["npm run"]);
        // Not a subcommand tool: the script is what ran.
        assert_eq!(
            short_labels("python3 scripts/generate.py"),
            ["python3 generate.py"]
        );
        // Nothing to add to the program's own name.
        assert_eq!(short_labels("htop"), ["htop"]);
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
        // The tools are Python's, not Nix's: the wrapper is not the work.
        assert_eq!(
            languages(command),
            [Some("python"), Some("python"), Some("python")]
        );
        // One devshell for the whole line, so the chip says it once.
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("vision-dev"));

        // Opening a shell runs nothing, so there is nothing to hand over to.
        assert_eq!(parse_command("nix develop .#vision-dev").environment, None);
        assert_eq!(short_labels("nix develop .#vision-dev"), ["nix develop"]);
        // A bare reference names no shell.
        assert_eq!(
            parse_command("nix develop -c pytest")
                .environment
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn a_shell_handed_several_commands_ran_several_commands() {
        // `cd x && build | grep | head` inside `bash -c` used to reach the
        // classifier as one segment: the first stage begins with `cd`, so
        // nothing said what happened, and the grep was left to claim the line.
        let command = "nix develop .#mapper --command bash -c \"cd arcade && \
            cargo check --quiet -p mapper-service --message-format short 2>&1 \
            | grep -E 'error' | head -30\"";
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("mapper"));
        assert_eq!(
            parsed
                .segments
                .iter()
                .map(|segment| segment.kind.clone())
                .filter(|kind| !kind.is_noop())
                .collect::<Vec<_>>()
                .len(),
            1,
            "the build is the only work: {:?}",
            parsed.segments
        );
        assert_eq!(short_labels(command), ["cargo check mapper-service"]);

        // The devshell still belongs to everything the shell ran.
        assert!(
            parsed
                .segments
                .iter()
                .all(|segment| segment.environment.as_deref() == Some("mapper"))
        );

        // One command in the payload still reads as that command.
        assert_eq!(short_labels("bash -c 'pytest -q tests'"), ["pytest tests"]);
    }

    #[test]
    fn a_summary_names_the_work_not_the_wrapper() {
        // The devshell has its own badge on the chip, so the label spends its
        // width on what actually ran.
        let command = "nix develop ..#mapper -c cargo check --quiet \
            -p mapper-service -p photogrammetry --all-targets";
        let parsed = parse_command(command);
        assert_eq!(parsed.environment.as_deref(), Some("mapper"));
        assert_eq!(
            summarize_command(&parsed).as_deref(),
            Some("cargo check --quiet -p mapper-service -p")
        );
        assert_eq!(short_labels(command), ["cargo check mapper-service +1"]);

        // The same for a machine, which is named by its own badge too.
        let parsed = parse_command("ssh box 'cargo build --release'");
        assert_eq!(
            summarize_command(&parsed).as_deref(),
            Some("cargo build --release")
        );
    }

    #[test]
    fn a_line_only_half_inside_a_devshell_still_names_it() {
        // Half the work has the mapper toolchain and half does not, which is
        // the only thing that explains the line. Demanding that every segment
        // agree threw the answer away.
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
            "each act keeps its own answer: {:?}",
            parsed.segments,
        );

        // A line wholly inside one still reads as it always did.
        let whole = parse_command("nix develop .#mapper -c bash -c 'cargo fmt && cargo check'");
        assert_eq!(whole.environment.as_deref(), Some("mapper"));
        assert!(!whole.environment_is_partial());

        // Two devshells in one line have no single answer, so the badge stays
        // out of it.
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
        // `cd` is bookkeeping, the greps are plumbing, and `pnpm exec` is a
        // way of running vitest.
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
        // The build is what ran; the grep is how its output was read.
        assert_eq!(
            short_labels("cargo check --workspace 2>&1 | grep -E '^error' -A5 | head -20"),
            ["cargo check"]
        );
        assert_eq!(
            classify_command("cargo check --workspace | grep -E '^error'"),
            CommandClass::Other
        );

        // A source that was only looking still loses to the search: the point
        // of `cat x | grep y` is the grep.
        assert_eq!(
            classify_command("cat src/main.rs | grep TODO"),
            CommandClass::Search
        );

        // The shape the whole thing arrives in: a script that edits files, then
        // a build filtered for errors.
        let command = "python3 - <<'PYEOF'\n\
            import pathlib\n\
            pathlib.Path('geom/src/mat3.rs').write_text('x')\n\
            PYEOF\n\
            cargo check --quiet --workspace 2>&1 | grep -E '^error' -A5 | head -20; \
            echo \"EXIT:$?\"";
        assert_eq!(short_labels(command), ["python3 script", "cargo check"]);
        assert_eq!(
            languages(command),
            [Some("python"), Some("rust")],
            "the script is Python's, the build is Rust's"
        );
    }

    #[test]
    fn a_search_over_an_inspection_command_is_a_search() {
        // The line the complaint arrived with: `ps` exists to be read, so the
        // `rg` over it is what the line was for, not a filter on work.
        let command =
            "ps -o pid,etime,state,command -ax | rg 'cargo (test|check)|rustc.*mock_standalone' | head -10";
        assert_eq!(classify_command(command), CommandClass::Search);
        assert_eq!(
            kinds(command),
            vec![SegmentKind::Search {
                query: Some("cargo (test|check)|rustc.*mock_standalone".into()),
            }]
        );

        // The rest of the inspection commands read the same way.
        assert_eq!(classify_command("df -h | rg /dev/vda"), CommandClass::Search);
        assert_eq!(classify_command("env | grep CARGO"), CommandClass::Search);

        // A program that did work still owns its line.
        assert_eq!(
            classify_command("cargo test 2>&1 | rg FAILED"),
            CommandClass::Other
        );
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

        // Without an implicit loop there is no file to read: a bare `perl -e`
        // is a script, and keeps its own chip.
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
              AND Timestamp < toDateTime64('2026-08-08 00:00:00', 9)\n\
            GROUP BY system\n\
            SQL";
        // The `>=` in the SQL is a comparison, not a redirect to a file called
        // "=", and the line posted a query rather than writing anything.
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
    fn a_scripts_own_helper_is_named_by_what_it_was_asked_for() {
        let command = "set +e\n\
            run_rpc() {\n\
              label=\"$1\"\n\
              err=$(grpcurl -plaintext -d \"$3\" pisa16:5001 \"$2\" 2>&1 >/dev/null)\n\
              if [ \"$?\" -eq 0 ]; then echo \"OK  $label\"; else echo \"ERR $label\"; fi\n\
            }\n\
            svc=terraform.brick_quality.BrickQualityService\n\
            run_rpc GetCameraType \"$svc/GetCameraType\" '{}' 10\n\
            run_rpc Preview \"$svc/Preview\" '{\"maxWidth\":320}' 20";
        // The parser has never heard of `run_rpc`, and does not need to: the
        // calls differ by their first argument, so that is what names them.
        assert_eq!(
            short_labels(command),
            ["run_rpc GetCameraType", "run_rpc Preview"]
        );
    }

    #[test]
    fn a_shell_handed_a_command_line_ran_that_command() {
        // `sh -c` inside a devshell: three wrappers deep, and what ran is
        // pytest.
        assert_eq!(
            short_labels(
                "nix develop --quiet .#vision-dev --command sh -c \
                 'python -m pytest -q vision/terra/services/wrist_localizer 2>&1 | tail -3'"
            ),
            ["pytest wrist_localizer"]
        );
        // A payload of several lines is a script, and keeps its own chip.
        assert!(matches!(
            kinds("bash -c 'set -e\ncargo build\ncargo test'").as_slice(),
            [SegmentKind::InlineScript { .. }]
        ));
    }

    #[test]
    fn a_babysitting_script_reads_as_the_commands_in_it() {
        let command = "cd /tmp/wrist && gh pr view 12104 --json state 2>/dev/null\n\
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
            echo \"TIMEOUT\"; gh pr checks 12104 2>/dev/null | awk '{print $1, $2}'";
        // The loop, the comment, the case arm, the braces, the array-element
        // assignment, the declaration and the sleep are all scaffolding. What
        // ran is the gh commands, each named by its subcommand and action
        // (`gh_commands_name_their_operation`'s own format).
        assert_eq!(
            short_labels(command),
            ["gh pr view", "gh run rerun", "gh pr checks"],
            "only the commands, not the syntax around them"
        );
    }

    #[test]
    fn cargo_names_the_crates_it_was_pointed_at() {
        assert_eq!(
            short_labels("cargo test -p acp_thread"),
            ["cargo test acp_thread"]
        );
        assert_eq!(
            short_labels("cargo clippy -p geom -p geom-macro -p wall-wasm --all-targets"),
            ["cargo clippy geom +2"]
        );
        assert_eq!(
            short_labels("cargo check --package sidebar --all-targets"),
            ["cargo check sidebar"]
        );
        // Nothing to name: the whole workspace.
        assert_eq!(short_labels("cargo check --workspace"), ["cargo check"]);
    }

    #[test]
    fn a_script_argument_is_not_a_path() {
        // jq's `//` is an operator, not a directory separator: the chip is
        // named after the file the query ran against.
        let command = "jq -r '.index[$id|tostring] | select(.items|length > 0) \
            | [(.trait.resolved_path.path // \"inherent\")] | @tsv' \
            target/doc/nalgebra.json | head -80";
        assert_eq!(short_labels(command), ["jq nalgebra.json"]);
    }

    #[test]
    fn a_subshell_bracket_is_not_part_of_the_command() {
        let command = "for h in w3aa w3z w3bb; do \
            (grpcurl -plaintext -max-time 4 $h:5001 list >/dev/null 2>&1 \
            && echo \"$h REACHABLE\" || echo \"$h -\") & done; wait";
        // The loop, the echoes, and the wait are all scaffolding around one
        // command, which keeps its own bracket-free name; the bare-word
        // argument still names the call, same as any other unknown program's.
        assert_eq!(short_labels(command), ["grpcurl $h:5001"]);
    }

    #[test]
    fn a_function_definition_is_syntax_not_work() {
        let command = "show() { echo \"=== $1:$2-$3 ===\"; sed -n \"$2,$3 p\" \"$1\"; }\n\
            show src/hull/comps/OrientedBoundingBoxesComp.ts 70 80\n\
            show src/robotics/plans/check-marker/Plan.ts 50 58\n\
            show src/robotics/scene/localizeCameraDebugLocal.ts 94 100";
        // The definition is one segment and does nothing; the calls are the
        // work, and each names the file it was given.
        assert_eq!(
            short_labels(command),
            [
                "show OrientedBoundingBoxesComp.ts",
                "show Plan.ts",
                "show localizeCameraDebugLocal.ts",
            ]
        );
        // Nothing in the body leaks out as a read of "$1" or a stray "}".
        assert!(
            parse_command(command)
                .segments
                .iter()
                .all(|segment| !segment.text.contains("$1") || segment.kind == SegmentKind::Noop),
            "the body's commands belong to the definition"
        );
    }

    #[test]
    fn a_chained_line_names_each_act() {
        assert_eq!(
            short_labels("python3 gen.py && cargo test && rg TODO src"),
            ["python3 gen.py", "cargo test", "TODO"]
        );
    }

    #[test]
    fn a_status_then_two_reads_is_one_look_around() {
        let command = "git status --short && sed -n '1,240p' arcade/src/lib.rs \
            && sed -n '430,930p' arcade/src/lib.rs";
        assert_eq!(classify_command(command), CommandClass::Read);
        assert_eq!(
            summary(command).as_deref(),
            Some("Checked git, read lib.rs")
        );
    }

    #[test]
    fn reads_plus_a_search_is_a_search() {
        let command = "sed -n '120,430p' a/lib.rs && sed -n '700,1080p' a/lib.rs \
            && rg -n \"packed|register\" arcade portico/src";
        assert_eq!(classify_command(command), CommandClass::Search);
        assert_eq!(
            summary(command).as_deref(),
            Some("Searched \"packed|register\", read lib.rs")
        );
    }

    #[test]
    fn a_long_read_sweep_counts_its_files() {
        let command = "rg -n 'packed' a/lib.rs; sed -n '550,610p' a/geom/bspline.rs; \
            sed -n '1,45p' a/geom/lib.rs; sed -n '1,70p' a/wasm/lib.rs; \
            sed -n '105,155p' p/Boundary.test.ts";
        assert_eq!(classify_command(command), CommandClass::Search);
        let summary = summary(command).expect("a look-around summarizes");
        assert!(summary.starts_with("Searched"), "{summary}");
        assert!(summary.contains("read 4 files"), "{summary}");
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
        // Capturing a timestamp is bookkeeping; the fetch and the analysis are
        // the work. Two real commands do not collapse into one phrase, so the
        // chip shows each of them.
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

        // A heredoc going into a file is still a file being written, and one
        // going into a command that only prints it is not a script.
        assert!(matches!(
            kinds("cat > /tmp/run.py <<'PY'\nprint(1)\nPY").as_slice(),
            [SegmentKind::WriteFile { .. }]
        ));
        // A body fed to something that only prints it names no files: the
        // text after `<<` is data, not arguments.
        assert!(matches!(
            kinds("cat <<'EOF'\nhello\nEOF").as_slice(),
            [SegmentKind::Read { paths, .. }] if paths.is_empty()
        ));
        // A command that does something with the body keeps its own meaning.
        assert!(matches!(
            kinds("kubectl apply -f - <<'YAML'\nkind: Pod\nYAML").as_slice(),
            [SegmentKind::Run { program, .. }] if program == "kubectl"
        ));
    }

    #[test]
    fn looking_for_a_program_is_not_running_one() {
        // `command` is a wrapper only in front of a program; in front of a
        // flag it is asking whether one exists.
        assert_eq!(
            kinds("command -v node || true"),
            vec![
                SegmentKind::Lookup {
                    program: Some("node".into())
                },
                // `|| true` is there to keep a failed lookup from stopping the
                // line; it does nothing itself.
                SegmentKind::Noop,
            ]
        );
        assert_eq!(
            kinds("which pnpm"),
            vec![SegmentKind::Lookup {
                program: Some("pnpm".into())
            }]
        );
        // The wrapper form still hands over to what it wraps.
        assert_eq!(
            kinds("command cargo build"),
            vec![SegmentKind::Run {
                program: "cargo".into(),
                argument: Some("build".into()),
            }]
        );

        let command = "command -v node || true\n\
            command -v corepack || true\n\
            command -v pnpm || true\n\
            ls -l ~/.local/share/pnpm/pnpm 2>/dev/null || true";
        assert_eq!(classify_command(command), CommandClass::Read);
        assert_eq!(
            summary(command).as_deref(),
            Some("Listed 1 directory, checked for node, corepack, pnpm")
        );
    }

    #[test]
    fn printed_dividers_are_not_work() {
        let command = "grep -rn \"class .*PickQuality\\|IMAGE_SIZE\\|input_size\" \
            vision/terra/ml/brick_quality.py | head -30; \
            echo \"=== example_inputs in aot_compile ===\"; \
            grep -n \"example\\|torch.randn\\|shape\" vision/terra/ml/aot_compile.py | head -50";
        assert_eq!(classify_command(command), CommandClass::Search);
        // Two searches whose queries are both known summarize to nothing: the
        // chip shows one piece per query instead of counting them.
        assert_eq!(summary(command), None);
        // Double quotes do not treat `\|` as an escape, so the backslashes
        // the query used for BRE alternation survive into the label as-is.
        assert_eq!(
            short_labels(command),
            [
                "class .*PickQuality\\|IMAGE_SIZE\\|input_size",
                "example\\|torch.randn\\|shape"
            ]
        );

        // Printing into a file is still authoring one.
        assert_eq!(
            kinds("echo 'hello' > greeting.txt"),
            vec![SegmentKind::WriteFile {
                path: "greeting.txt".into(),
                contents: None,
            }]
        );
    }

    #[test]
    fn showing_files_at_a_revision_reads_them() {
        let command = "git show origin/master:portico/src/geom/Vec2.ts | sed -n '1,240p'\n\
            git show origin/master:portico/src/geom/Vec3.ts | sed -n '1,260p'\n\
            git show origin/master:portico/src/geom/unit/Angle.ts | sed -n '1,220p'\n\
            git show origin/master:portico/src/geom/Pose4dof.ts | sed -n '1,180p'";
        assert_eq!(classify_command(command), CommandClass::Read);
        assert_eq!(
            summary(command).as_deref(),
            Some("Read 4 files at origin/master")
        );
        // The filter says which lines the file was wanted for.
        assert_eq!(
            kinds("git show HEAD:src/lib.rs | sed -n '1,240p'"),
            vec![SegmentKind::Read {
                paths: vec!["src/lib.rs".into()],
                lines: Some(1..240),
                revision: Some("HEAD".into()),
            }]
        );
        // A commit with no path is still a diff.
        assert!(matches!(
            kinds("git show HEAD~2").as_slice(),
            [SegmentKind::Git {
                operation: GitOperation::ReadChanges,
                ..
            }]
        ));
    }

    #[test]
    fn diff_pipelines_read_changes() {
        // sort/head/tail are plumbing: the pipeline is still a diff.
        let command = "git diff --numstat | sort -nr | head -n 35; \
            git diff --name-status | tail -n 30; git diff -- a/lib.rs | sed -n '1,320p'";
        assert_eq!(classify_command(command), CommandClass::ReadDiff);
        assert_eq!(summary(command).as_deref(), Some("Read 3 diffs"));
    }

    #[test]
    fn capturing_output_to_a_temp_file_is_still_just_the_command() {
        let command = "pnpm lint > /tmp/wasm-lint.txt 2>&1; code=$?; \
            tail -n 100 /tmp/wasm-lint.txt; exit $code";
        assert_eq!(classify_command(command), CommandClass::Other);
        assert_eq!(summary(command).as_deref(), Some("pnpm lint"));
    }

    #[test]
    fn in_place_rewrites_are_edits() {
        let command = "perl -pi -e 's/Line2\\.from\\(/Line2.fromLine(/g' a.ts b.ts c.tsx";
        assert_eq!(classify_command(command), CommandClass::Other);
        assert_eq!(summary(command).as_deref(), Some("Edited 3 files"));
    }

    #[test]
    fn tailing_a_log_then_counting_errors_is_a_search() {
        let command =
            "tail -n 140 /tmp/direct-core9.txt; rg -c 'error TS' /tmp/direct-core9.txt || true";
        assert_eq!(classify_command(command), CommandClass::Search);
    }

    #[test]
    fn a_loop_over_specs_reads_files() {
        let command = "for spec in 'a.ts:660,674' 'b.ts:548,770'; do f=${spec%%:*}; \
            r=${spec#*:}; sed -n \"${r}p\" \"$f\"; done";
        assert_eq!(classify_command(command), CommandClass::Read);
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
        assert!(matches!(
            kinds("git reset --hard HEAD~1").as_slice(),
            [SegmentKind::Destructive {
                operation: DestructiveOperation::DiscardChanges,
                ..
            }]
        ));
        assert!(matches!(
            kinds("git checkout -- src/lib.rs").as_slice(),
            [SegmentKind::Destructive {
                operation: DestructiveOperation::DiscardChanges,
                ..
            }]
        ));
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

        // Inline code, remote as well.
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

        // Package managers only hand over for `exec`/`dlx`.
        assert_eq!(summary("pnpm lint").as_deref(), Some("pnpm lint"));
        assert_eq!(
            kinds("npx tsc --noEmit"),
            vec![SegmentKind::Run {
                program: "tsc".into(),
                argument: None,
            }]
        );
        // `-e` that names an environment variable is not a script.
        assert!(matches!(
            kinds("docker run -e HOST=example redis").as_slice(),
            [SegmentKind::Run { .. }]
        ));
    }

    #[test]
    fn inline_code_is_captured_like_a_heredoc() {
        assert_eq!(
            kinds("python3 -c 'import sys; print(sys.path)'"),
            vec![SegmentKind::InlineScript {
                interpreter: "python3".into(),
                code: "import sys; print(sys.path)".into(),
            }]
        );
    }

    #[test]
    fn gh_commands_name_their_operation() {
        assert_eq!(
            kinds("gh pr view 1234 --json state"),
            vec![SegmentKind::GitHub {
                operation: "pr view".into(),
                target: Some("1234".into()),
            }]
        );
    }

    #[test]
    fn waiting_never_decides_what_a_line_did() {
        assert_eq!(
            kinds("sleep 30"),
            vec![SegmentKind::Wait { seconds: Some(30) }]
        );
        // A poll loop is still the thing it polls for.
        assert_eq!(
            classify_command("sleep 5; rg -n 'ready' /tmp/log.txt"),
            CommandClass::Search
        );
    }

    #[test]
    fn reads_remember_the_lines_they_asked_for() {
        assert_eq!(
            kinds("sed -n '120,430p' arcade/src/lib.rs"),
            vec![SegmentKind::Read {
                paths: vec!["arcade/src/lib.rs".into()],
                lines: Some(120..430),
                revision: None,
            }]
        );
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

        // Quoted separators are characters, not separators.
        assert_eq!(parse_command("rg 'a && b' src/").segments.len(), 1);
        assert_eq!(parse_command("echo 'one; two'").segments.len(), 1);
    }

    #[test]
    fn line_continuations_stay_one_command() {
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
        assert_eq!(
            parsed.segments.len(),
            1,
            "the body is data: {:?}",
            parsed.segments
        );
        match &parsed.segments[0].kind {
            SegmentKind::WriteFile { path, contents } => {
                assert_eq!(path, "/tmp/run.py");
                let contents = contents.as_ref().expect("the body is right there");
                assert!(contents.contains("import os"));
                assert!(contents.contains("print('a; b && c')"));
            }
            other => panic!("expected a file write, got {other:?}"),
        }
    }

    #[test]
    fn heredoc_then_command_reads_as_write_then_run() {
        let command = "cat > /tmp/x.py <<PY\nprint(1)\nPY\npython /tmp/x.py";
        let kinds = kinds(command);
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
    fn ssh_reports_its_host_and_reads_the_remote_command() {
        let parsed = parse_command("ssh -p 22 build-box 'rg needle /srv'");
        assert_eq!(parsed.host.as_deref(), Some("build-box"));
        assert_eq!(parsed.segments.len(), 1);
        assert!(matches!(
            parsed.segments[0].kind,
            SegmentKind::Search { .. }
        ));
        assert_eq!(
            classify_command("ssh box 'cat /etc/hosts'"),
            CommandClass::Read
        );
    }

    #[test]
    fn pipelines_take_their_meaning_from_their_stages() {
        assert_eq!(classify_command("rg foo | head -20"), CommandClass::Search);
        assert_eq!(
            classify_command("cat x.rs | grep foo"),
            CommandClass::Search
        );
        assert_eq!(classify_command("cat x.rs | wc -l"), CommandClass::Read);
        assert_eq!(classify_command("cd src && cat a.rs"), CommandClass::Read);
        assert_eq!(
            classify_command("cd src && cargo build"),
            CommandClass::Other
        );
    }

    #[test]
    fn repeated_reads_stay_reads() {
        assert_eq!(
            classify_command("sed -n '1,5p' a\nsed -n '1,5p' b\nsed -n '1,5p' c"),
            CommandClass::Read
        );
        assert_eq!(
            classify_command("sed -n '1,5p' a; sed -n '1,5p' b; rg foo"),
            CommandClass::Search
        );
        // Doing real work anywhere disqualifies the whole line.
        assert_eq!(
            classify_command("sed -n '1,5p' a; cargo check"),
            CommandClass::Other
        );
    }

    #[test]
    fn git_operations_are_told_apart() {
        // Reading changes.
        assert_eq!(classify_command("git diff"), CommandClass::ReadDiff);
        assert_eq!(classify_command("git diff HEAD~1"), CommandClass::ReadDiff);
        assert_eq!(classify_command("git show abc123"), CommandClass::ReadDiff);
        assert_eq!(classify_command("git log -p"), CommandClass::ReadDiff);
        // Asking about state: quiet enough to fold, but not a file read.
        assert_eq!(classify_command("git status"), CommandClass::GitInfo);
        assert_eq!(classify_command("git log --oneline"), CommandClass::GitInfo);
        assert_eq!(classify_command("git branch"), CommandClass::GitInfo);
        // Changing the repository is real work.
        assert_eq!(classify_command("git commit -m x"), CommandClass::Other);
        assert_eq!(classify_command("git push"), CommandClass::Other);
        assert_eq!(classify_command("git checkout main"), CommandClass::Other);

        assert_eq!(
            kinds("git commit -m 'hello'"),
            vec![SegmentKind::Git {
                operation: GitOperation::Modify,
                target: Some("commit".into()),
            }]
        );
    }

    #[test]
    fn listing_and_counting_are_looking_around() {
        assert_eq!(classify_command("ls -la src/"), CommandClass::Read);
        assert_eq!(classify_command("wc -l foo.rs"), CommandClass::Read);
        assert_eq!(
            kinds("ls src/"),
            vec![SegmentKind::ListDirectory {
                path: Some("src/".into())
            }]
        );
        assert_eq!(
            kinds("wc -l a.rs b.rs"),
            vec![SegmentKind::CountLines {
                paths: vec!["a.rs".into(), "b.rs".into()]
            }]
        );
        // `cd` is syntax, not work, so a line that only changes directory has
        // nothing to report.
        assert_eq!(kinds("cd crates/foo"), vec![SegmentKind::Noop]);
    }

    #[test]
    fn ssh_is_recognized_per_segment() {
        // Not just at the start of the line.
        let parsed = parse_command("cd /srv && ssh build-box 'cargo test'");
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].host, None);
        assert_eq!(parsed.segments[1].host.as_deref(), Some("build-box"));
        // Every working segment ran there, so the line did.
        assert_eq!(parsed.host.as_deref(), Some("build-box"));

        // A line that is only partly remote does not claim a host.
        let parsed = parse_command("cargo build && ssh box ./deploy.sh");
        assert_eq!(parsed.host, None);
        assert_eq!(parsed.segments[1].host.as_deref(), Some("box"));

        // A bare (unquoted) remote command still classifies.
        let parsed = parse_command("ssh box cat /etc/hosts");
        assert_eq!(parsed.host.as_deref(), Some("box"));
        assert!(matches!(parsed.segments[0].kind, SegmentKind::Read { .. }));
    }

    #[test]
    fn redirects_write_but_noise_suppression_does_not() {
        assert_eq!(classify_command("echo hi > log.txt"), CommandClass::Other);
        assert_eq!(classify_command("rg foo 2>/dev/null"), CommandClass::Search);
        assert_eq!(classify_command("cat foo.rs 2>&1"), CommandClass::Read);
    }

    // Real lines, captured from `pytest` and `pnpm` runs in the container that
    // built this, not remembered.
    #[test]
    fn pytest_states_a_real_fraction() {
        assert_eq!(
            progress_fraction(
                "test_mod0.py ......                                                      [ 25%]"
            ),
            Some(0.25)
        );
        assert_eq!(
            progress_fraction(
                "test_mod1.py ......                                                      [ 50%]"
            ),
            Some(0.5)
        );
        assert_eq!(
            progress_fraction(
                "test_mod3.py ......                                                      [100%]"
            ),
            Some(1.0)
        );
    }

    #[test]
    fn a_count_without_a_denominator_is_not_progress() {
        // pnpm counts what it has done, not what it has left, so there is no
        // fraction to be had and the last line stands on its own.
        assert_eq!(
            progress_fraction("Progress: resolved 1, reused 0, downloaded 0, added 0"),
            None
        );
        assert_eq!(
            progress_fraction("Progress: resolved 2, reused 0, downloaded 2, added 2, done"),
            None
        );
        // cargo's count lives in a progress bar that captures as nothing.
        assert_eq!(progress_fraction("   Compiling gpui v0.2.2 (/home/user/zed/crates/gpui)"), None);
    }

    #[test]
    fn a_trailing_bracket_that_is_not_a_percentage_is_ignored() {
        assert_eq!(progress_fraction("running [4 tests]"), None);
        assert_eq!(progress_fraction("thread 'main' panicked at [src/lib.rs]"), None);
        assert_eq!(progress_fraction("almost [ 101%]"), None);
        assert_eq!(progress_fraction("empty [%]"), None);
        assert_eq!(progress_fraction("nothing here at all"), None);
    }

    #[test]
    fn reads_name_the_files_they_read() {
        let kinds = kinds("sed -n '1,20p' crates/foo/src/lib.rs");
        assert_eq!(
            kinds,
            vec![SegmentKind::Read {
                paths: vec!["crates/foo/src/lib.rs".into()],
                lines: Some(1..20),
                revision: None,
            }]
        );
    }

    #[test]
    fn searches_name_their_query() {
        let kinds = kinds("rg 'fn main' crates/");
        assert_eq!(
            kinds,
            vec![SegmentKind::Search {
                query: Some("fn main".into())
            }]
        );
    }
}
