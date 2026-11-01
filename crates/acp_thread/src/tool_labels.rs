//! quiet-ui: what a tool call is, read from its label: waits, tool lookups,
//! compaction, label-less edits and command lines, and agent error payloads.

use super::*;

impl ToolCall {
    /// Polling or sleeping rather than doing work.
    pub fn is_wait(&self, cx: &App) -> bool {
        is_wait_call(
            self.tool_name.as_deref(),
            &self.label.read(cx).source(),
            self.kind(),
        )
    }

    /// The agent looking up its own tools, not working on the project.
    pub fn is_tool_lookup(&self, cx: &App) -> bool {
        is_tool_lookup_call(self.tool_name.as_deref(), &self.label.read(cx).source())
    }

    /// An empty stdin write: an agent poking an interactive command.
    pub fn is_empty_stdin_write(&self, cx: &App) -> bool {
        is_empty_stdin_write_call(
            self.tool_name.as_deref(),
            &self.label.read(cx).source(),
            self.raw_input.as_ref(),
        )
    }

    /// Some agents report compaction as an ordinary tool call.
    pub fn is_compaction(&self, cx: &App) -> bool {
        if self
            .tool_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("compact"))
        {
            return true;
        }
        // Asked of every entry on every frame, so no lowercased copy of what
        // may be a whole command.
        let label = self.label.read(cx).source();
        let first_line = label.lines().next().unwrap_or("");
        if !contains_ignore_ascii_case(first_line, "compact") {
            return false;
        }
        contains_ignore_ascii_case(first_line, "context")
            || contains_ignore_ascii_case(first_line, "conversation")
    }
}

/// Edit labels that name no file (Codex sends these).
pub(crate) const GENERIC_EDIT_LABELS: &[&str] = &[
    "edit",
    "edits",
    "edit file",
    "edit files",
    "editing",
    "editing file",
    "editing files",
    "file edit",
    "file edits",
    "apply patch",
    "applying patch",
    "patch",
    "write",
    "writing",
    "writing file",
    "writing files",
    "update file",
    "update files",
    "updating file",
    "updating files",
];

pub(crate) fn is_generic_edit_label(title: &str) -> bool {
    let normalized = title
        .trim()
        .trim_end_matches(['.', '…', '"', '\''])
        .to_lowercase();
    normalized.is_empty() || GENERIC_EDIT_LABELS.contains(&normalized.as_str())
}

/// A label that names no file is replaced by the files the call touches.
pub(crate) fn edit_label_source(title: &str, locations: &[ToolCallLocation]) -> String {
    if !is_generic_edit_label(title) {
        return MarkdownEscaped(title).to_string();
    }

    let mut file_names: Vec<String> = Vec::new();
    for location in locations {
        let Some(name) = location.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_names.iter().any(|existing| existing == name) {
            file_names.push(name.to_string());
        }
    }

    match file_names.as_slice() {
        [] => MarkdownEscaped(title).to_string(),
        [name] => MarkdownEscaped(name).to_string(),
        names => format!("{} files", names.len()),
    }
}

pub(crate) fn is_tool_lookup_call(tool_name: Option<&str>, title: &str) -> bool {
    let names_lookup = |name: &str| {
        let name = name.trim().to_lowercase().replace(['-', ' '], "_");
        name == "toolsearch" || name.starts_with("tool_search") || name.starts_with("search_tools")
    };
    match tool_name {
        Some(tool_name) => names_lookup(tool_name),
        None => names_lookup(title),
    }
}

pub(crate) fn is_empty_stdin_write_call(
    tool_name: Option<&str>,
    title: &str,
    raw_input: Option<&serde_json::Value>,
) -> bool {
    let names_stdin = |name: &str| name.trim().to_lowercase().contains("stdin");
    let writes_stdin = match tool_name {
        Some(tool_name) => names_stdin(tool_name),
        None => names_stdin(title),
    };
    if !writes_stdin {
        return false;
    }
    // An unreadable payload is not an empty one.
    match raw_input {
        Some(input) => input
            .get("chars")
            .is_some_and(|chars| chars.as_str().is_some_and(|chars| chars.is_empty())),
        None => true,
    }
}

/// Recognized by a reported name (`wait`, `wait_for_task`), else by a title
/// that only says it is waiting. Calls that run or touch files never are.
pub(crate) fn is_wait_call(tool_name: Option<&str>, title: &str, kind: &acp_v2::ToolKind) -> bool {
    if matches!(
        kind,
        acp_v2::ToolKind::Execute
            | acp_v2::ToolKind::Edit
            | acp_v2::ToolKind::Delete
            | acp_v2::ToolKind::Move
    ) {
        return false;
    }

    if let Some(tool_name) = tool_name {
        let name = tool_name.trim().to_lowercase();
        return name == "wait" || name.starts_with("wait_") || name.starts_with("wait-");
    }

    let title = title
        .trim()
        .trim_matches('`')
        .trim_end_matches(['.', '…', '!'])
        .to_lowercase();
    let Some(rest) = title
        .strip_prefix("waiting")
        .or_else(|| title.strip_prefix("wait"))
    else {
        return false;
    };
    // Not "Waitlist".
    rest.is_empty() || rest.starts_with(' ')
}

/// A bash-tagged fence for shell highlighting, reusing the body of any fence
/// the agent already sent.
pub(crate) fn execute_command_label_source(title: &str) -> String {
    let command = title
        .strip_prefix("```")
        .and_then(|after_fence| after_fence.split_once('\n'))
        .map(|(_tag, body)| body.strip_suffix("\n```").unwrap_or(body))
        .unwrap_or(title);
    let command = unquote_command(command);
    format!("```bash\n{command}\n```")
}

/// Strips a JSON string literal or single quotes wrapped around a whole
/// command, which would otherwise highlight as one string.
pub(crate) fn unquote_command(command: &str) -> Cow<'_, str> {
    let trimmed = command.trim();
    if trimmed.len() < 2 {
        return Cow::Borrowed(command);
    }

    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        // serde rejects `"a" && "b"` and unescapes `\"`.
        if let Ok(unquoted) = serde_json::from_str::<String>(trimmed)
            && !unquoted.trim().is_empty()
        {
            return Cow::Owned(unquoted);
        }
        return Cow::Borrowed(command);
    }

    if trimmed.starts_with('\'')
        && trimmed.ends_with('\'')
        && let Some(inner) = trimmed
            .strip_prefix('\'')
            .and_then(|rest| rest.strip_suffix('\''))
        && !inner.contains('\'')
        && !inner.trim().is_empty()
    {
        return Cow::Owned(inner.to_string());
    }

    Cow::Borrowed(command)
}

/// A verbatim one-line prefix of the command, with an ellipsis when anything
/// is omitted.
pub fn command_display_prefix(command: &str, max_chars: usize) -> String {
    let trimmed = command.trim();
    let first_line = trimmed.lines().next().unwrap_or("").trim_end();
    let more_lines = trimmed.lines().nth(1).is_some();
    let mut prefix: String = first_line.chars().take(max_chars).collect();
    if more_lines || first_line.chars().count() > max_chars {
        prefix.truncate(prefix.trim_end().len());
        prefix.push('…');
    }
    prefix
}

/// A command too long for its chip, shortened by dropping leading directories from
/// its longest path rather than cutting the end off: `wc …/flushcut-review.diff`
/// keeps the file name, which is the part worth reading. Falls back to
/// [`command_display_prefix`] when there is no path left to shorten.
pub fn command_display_label(command: &str, max_chars: usize) -> String {
    let trimmed = command.trim();
    let first_line = trimmed.lines().next().unwrap_or("").trim_end();
    let more_lines = trimmed.lines().nth(1).is_some();
    if !more_lines && first_line.chars().count() <= max_chars {
        return first_line.to_string();
    }

    let mut words: Vec<String> = first_line.split(' ').map(str::to_owned).collect();
    while words
        .iter()
        .map(|word| word.chars().count() + 1)
        .sum::<usize>()
        > max_chars + 1
    {
        // The longest path first: shortening it buys the most room.
        let Some(longest) = words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.contains('/'))
            .max_by_key(|(_, word)| word.chars().count())
            .map(|(index, _)| index)
        else {
            break;
        };
        let word = words[longest]
            .trim_start_matches('…')
            .trim_start_matches('/')
            .to_owned();
        let Some((_, rest)) = word.split_once('/') else {
            break;
        };
        words[longest] = format!("…/{rest}");
    }

    let shortened = words.join(" ");
    if shortened.chars().count() > max_chars {
        // Nothing left to shorten: cut the end off, which says so with its own ellipsis.
        command_display_prefix(&shortened, max_chars)
    } else if more_lines {
        format!("{shortened}…")
    } else {
        shortened
    }
}

/// "Context compacted to fit the model's context window." The length cap keeps
/// prose about compaction from matching.
pub(crate) fn is_context_compaction_notice(text: &str) -> bool {
    const MAX_NOTICE_LEN: usize = 200;

    let text = text.trim();
    if text.is_empty() || text.len() > MAX_NOTICE_LEN {
        return false;
    }

    let text = text.to_lowercase();
    text.starts_with("context compacted")
        || (text.contains("compacted") && text.contains("context window"))
}

/// Agents report usage failures as a JSON blob, often behind a prefix:
/// `Internal error: { "message": "…", "codexErrorInfo": "usageLimitExceeded" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentErrorPayload {
    pub message: String,
    pub code: Option<String>,
}

pub fn parse_agent_error_payload(raw: &str) -> Option<AgentErrorPayload> {
    payload_from_json(&extract_json_value(raw)?)
}

pub(crate) fn extract_json_value(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(value);
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(trimmed.get(start..=end)?).ok()
}

pub(crate) fn payload_from_json(value: &serde_json::Value) -> Option<AgentErrorPayload> {
    const MESSAGE_KEYS: [&str; 5] = [
        "message",
        "error_message",
        "errorMessage",
        "detail",
        "description",
    ];
    const CODE_KEYS: [&str; 7] = [
        "codexErrorInfo",
        "error_code",
        "errorCode",
        "code",
        "kind",
        "type",
        "status",
    ];
    const NESTED_KEYS: [&str; 4] = ["error", "data", "body", "payload"];

    let object = value.as_object()?;

    let code = CODE_KEYS
        .iter()
        .find_map(|key| object.get(*key).and_then(json_scalar_to_string));

    let message = MESSAGE_KEYS
        .iter()
        .find_map(|key| object.get(*key).and_then(|value| value.as_str()));

    let Some(message) = message else {
        // Shapes like `{"error": {"message": …}}` carry the message one level down.
        return NESTED_KEYS.iter().find_map(|key| {
            let mut nested = payload_from_json(object.get(*key)?)?;
            nested.code = nested.code.or_else(|| code.clone());
            Some(nested)
        });
    };

    // Double-encoded payloads.
    if let Some(mut nested) = parse_agent_error_payload(message) {
        nested.code = nested.code.or(code);
        return Some(nested);
    }

    Some(AgentErrorPayload {
        message: message.trim().to_string(),
        code,
    })
}

pub(crate) fn json_scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Wraps bare `http(s)` URLs in markdown links, leaving existing links alone.
pub fn linkify_urls(text: &str) -> String {
    const SCHEMES: [&str; 2] = ["https://", "http://"];

    let mut linkified = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find("http") {
        let (before, from) = rest.split_at(index);
        linkified.push_str(before);

        let Some(scheme) = SCHEMES
            .iter()
            .find(|scheme| from.starts_with(**scheme))
            .copied()
        else {
            linkified.push_str("http");
            rest = &from["http".len()..];
            continue;
        };

        let end = from.find(char::is_whitespace).unwrap_or(from.len());
        let (candidate, remainder) = from.split_at(end);
        rest = remainder;

        if before.ends_with("](") || before.ends_with('<') {
            linkified.push_str(candidate);
            continue;
        }

        let url = candidate.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '}', '\'']);
        if url.len() > scheme.len() {
            linkified.push_str(&format!("[{url}]({url})"));
        } else {
            linkified.push_str(url);
        }
        linkified.push_str(&candidate[url.len()..]);
    }

    linkified.push_str(rest);
    linkified
}
