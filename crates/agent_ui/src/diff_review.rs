//! Sends diff review comments to the workspace's agent thread as one message.

use crate::AgentPanel;
use acp_thread::ThreadStatus;
use agent_client_protocol::schema::v2 as acp_v2;
use editor::{Editor, TakenReviewComment};
use gpui::{App, Entity, SharedString, Task, Window};
use language::Point;
use multi_buffer::ToPoint as _;
use util::ResultExt as _;
use util::paths::PathStyle;
use workspace::Workspace;

fn workspace_review_editors(workspace: &Entity<Workspace>, cx: &App) -> Vec<Entity<Editor>> {
    let mut editors: Vec<Entity<Editor>> = Vec::new();
    for item in workspace.read(cx).items(cx) {
        let Some(editor) = item.act_as::<Editor>(cx) else {
            continue;
        };
        if editor.read(cx).total_review_comment_count() == 0 {
            continue;
        }
        if editors
            .iter()
            .any(|existing| existing.entity_id() == editor.entity_id())
        {
            continue;
        }
        editors.push(editor);
    }
    editors
}

pub(crate) fn pending_review_comment_count(workspace: &Entity<Workspace>, cx: &App) -> usize {
    workspace_review_editors(workspace, cx)
        .iter()
        .map(|editor| editor.read(cx).total_review_comment_count())
        .sum()
}

/// One block per editor, to attach to the next outgoing message. Empties the
/// editors' comment state.
pub(crate) fn take_pending_review_blocks(
    workspace: &Entity<Workspace>,
    cx: &mut App,
) -> Vec<acp_v2::ContentBlock> {
    let editors = workspace_review_editors(workspace, cx);
    let path_style = workspace.read(cx).project().read(cx).path_style(cx);
    let mut blocks = Vec::new();
    for editor in editors {
        let comments = editor.update(cx, |editor, cx| editor.take_review_comments(cx));
        if comments.is_empty() {
            continue;
        }
        let message = compose_review_message(&editor, &comments, "", path_style, cx);
        blocks.push(acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
            message,
        )));
    }
    blocks
}

pub(crate) fn clear_pending_review_comments(workspace: &Entity<Workspace>, cx: &mut App) {
    for editor in workspace_review_editors(workspace, cx) {
        editor.update(cx, |editor, cx| {
            editor.take_review_comments(cx);
        });
    }
}

pub(crate) fn send_review_to_agent(
    editor: &Entity<Editor>,
    summary: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
        return;
    };
    let Some(thread) = panel.read(cx).active_agent_thread(cx) else {
        workspace.update(cx, |workspace, cx| {
            struct NoActiveAgentThreadToast;
            workspace.show_toast(
                workspace::Toast::new(
                    workspace::notifications::NotificationId::unique::<NoActiveAgentThreadToast>(),
                    "No active agent thread to send the review to",
                )
                .autohide(),
                cx,
            );
        });
        return;
    };

    let comments = editor.update(cx, |editor, cx| editor.take_review_comments(cx));
    if comments.is_empty() {
        return;
    }

    let path_style = thread.read(cx).project().read(cx).path_style(cx);
    let message = compose_review_message(editor, &comments, summary.trim(), path_style, cx);
    let block = acp_v2::ContentBlock::Text(acp_v2::TextContent::new(message));

    // The thread's view queues instead of interrupting a running turn.
    if let Some(thread_view) = panel.read(cx).active_thread_view(cx) {
        let is_generating = thread.read(cx).status() != ThreadStatus::Idle;
        thread_view.update(cx, |thread_view, cx| {
            if is_generating {
                thread_view.add_to_queue(vec![block], Vec::new(), window, cx);
            } else {
                thread_view.send_content(
                    Task::ready(Ok(Some((vec![block], Vec::new())))),
                    false,
                    window,
                    cx,
                );
            }
        });
    } else {
        let send = thread.update(cx, |thread, cx| thread.send(vec![block], cx));
        cx.spawn(async move |_cx| {
            send.await.log_err();
        })
        .detach();
    }
}

/// States the count so a sent review can be recognized and rendered as a chip.
fn review_message_header(comment_count: usize) -> String {
    format!(
        "I reviewed the changes and left {comment_count} review comment{} below. Please address them.",
        if comment_count == 1 { "" } else { "s" }
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewComment {
    pub path: SharedString,
    /// 1-based inclusive line range, when the comment names one.
    pub line_range: Option<(u32, u32)>,
    pub quoted_code: SharedString,
    pub comment: SharedString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedReview {
    pub comments: Vec<ReviewComment>,
}

/// Detected purely by the counted header, so sent and queued messages alike.
pub(crate) fn review_comment_blocks(chunks: &[acp_v2::ContentBlock]) -> Vec<ParsedReview> {
    chunks
        .iter()
        .filter_map(|chunk| parse_review_message(block_text(chunk)?))
        .collect()
}

fn block_text(block: &acp_v2::ContentBlock) -> Option<&str> {
    match block {
        acp_v2::ContentBlock::Text(text) => Some(&text.text),
        _ => None,
    }
}


/// Shared by compose and parse so they stay exact inverses.
fn comment_header_line(path: &str, line_range: Option<(u32, u32)>) -> String {
    match line_range {
        Some((start, end)) if start != end => format!("`{path}` lines {start}-{end}:"),
        Some((start, _)) => format!("`{path}` line {start}:"),
        None => format!("`{path}`:"),
    }
}

fn parse_comment_header(line: &str) -> Option<(SharedString, Option<(u32, u32)>)> {
    let line = line.trim_end();
    let rest = line.strip_prefix('`')?;
    let close = rest.find('`')?;
    let path = &rest[..close];
    if path.is_empty() {
        return None;
    }
    let after = rest[close + 1..].trim_start();
    let line_range = if after == ":" {
        None
    } else if let Some(spec) = after
        .strip_prefix("lines ")
        .and_then(|s| s.strip_suffix(':'))
    {
        let (start, end) = spec.split_once('-')?;
        Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
    } else if let Some(spec) = after
        .strip_prefix("line ")
        .and_then(|s| s.strip_suffix(':'))
    {
        let row = spec.trim().parse().ok()?;
        Some((row, row))
    } else {
        return None;
    };
    Some((SharedString::from(path.to_string()), line_range))
}

/// A trailing summary has no header to delimit it, so it folds into the last
/// comment's text.
pub(crate) fn parse_review_message(text: &str) -> Option<ParsedReview> {
    review_block_comment_count(text)?;

    let lines: Vec<&str> = text.lines().collect();
    let header_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        // The first line is the counted header, never a comment header.
        .filter(|(index, line)| *index != 0 && parse_comment_header(line).is_some())
        .map(|(index, _)| index)
        .collect();

    let mut comments = Vec::new();
    for (nth, &start) in header_indices.iter().enumerate() {
        let (path, line_range) = parse_comment_header(lines[start])?;
        let end = header_indices.get(nth + 1).copied().unwrap_or(lines.len());
        let section = &lines[start + 1..end];

        let mut quoted_code = String::new();
        let mut comment_lines: &[&str] = section;
        if section.first() == Some(&"```") {
            if let Some(close) = section[1..].iter().position(|line| *line == "```") {
                quoted_code = section[1..1 + close].join("\n");
                comment_lines = &section[1 + close + 1..];
            }
        }

        comments.push(ReviewComment {
            path,
            line_range,
            quoted_code: SharedString::from(quoted_code),
            comment: SharedString::from(comment_lines.join("\n").trim().to_string()),
        });
    }

    Some(ParsedReview { comments })
}

pub(crate) fn without_review_blocks(
    chunks: Vec<acp_v2::ContentBlock>,
) -> Vec<acp_v2::ContentBlock> {
    chunks
        .into_iter()
        .filter(|chunk| {
            block_text(chunk).is_none_or(|text| review_block_comment_count(text).is_none())
        })
        .collect()
}

fn review_block_comment_count(text: &str) -> Option<usize> {
    let first_line = text.lines().next()?.trim();
    let count = first_line
        .strip_prefix("I reviewed the changes and left ")?
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()?;
    (first_line == review_message_header(count)).then_some(count)
}

fn review_comment_from_taken(
    comment: &TakenReviewComment,
    snapshot: &multi_buffer::MultiBufferSnapshot,
    path_style: PathStyle,
) -> ReviewComment {
    let start_point = comment.range.start.to_point(snapshot);
    let end_point = comment.range.end.to_point(snapshot);
    let line_start = Point::new(start_point.row, 0);
    let line_end = Point::new(
        end_point.row,
        snapshot.line_len(multi_buffer::MultiBufferRow(end_point.row)),
    );

    let mut buffer_rows = None;
    let mut quoted_code = String::new();
    for (buffer_snapshot, range, _) in snapshot.range_to_buffer_ranges(line_start..line_end) {
        let start_row = buffer_snapshot.offset_to_point(range.start.0).row;
        let end_row = buffer_snapshot.offset_to_point(range.end.0).row;
        buffer_rows.get_or_insert((start_row, end_row)).1 = end_row;
        quoted_code.extend(buffer_snapshot.text_for_range(range.start.0..range.end.0));
    }

    ReviewComment {
        path: SharedString::from(comment.file_path.display(path_style).to_string()),
        // Serialize as 1-based inclusive rows.
        line_range: buffer_rows.map(|(start_row, end_row)| (start_row + 1, end_row + 1)),
        quoted_code: SharedString::from(quoted_code),
        comment: SharedString::from(comment.comment.clone()),
    }
}

/// Inverse of `parse_review_message`.
fn serialize_review(comments: &[ReviewComment], summary: &str) -> String {
    let mut message = review_message_header(comments.len());
    message.push('\n');

    for comment in comments {
        message.push('\n');
        message.push_str(&comment_header_line(&comment.path, comment.line_range));
        message.push('\n');
        if !comment.quoted_code.trim().is_empty() {
            message.push_str("```\n");
            message.push_str(comment.quoted_code.trim_end_matches('\n'));
            message.push_str("\n```\n");
        }
        message.push_str(&comment.comment);
        message.push('\n');
    }

    if !summary.is_empty() {
        message.push('\n');
        message.push_str(summary);
        message.push('\n');
    }

    message
}

fn compose_review_message(
    editor: &Entity<Editor>,
    comments: &[TakenReviewComment],
    summary: &str,
    path_style: PathStyle,
    cx: &App,
) -> String {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let review_comments: Vec<ReviewComment> = comments
        .iter()
        .map(|comment| review_comment_from_taken(comment, &snapshot, path_style))
        .collect();
    serialize_review(&review_comments, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::{path, rel_path::RelPath};

    #[gpui::test]
    async fn test_compose_review_message(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/test"), json!({"file1": "abc\ndef\nghi\njkl"}))
            .await;
        let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
        let buffer_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("test/file1", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(buffer_path, cx))
            .await
            .unwrap();

        let (editor, cx) = cx.add_window_view(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });

        let (comments, path_style) = editor.read_with(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range =
                snapshot.anchor_before(Point::new(1, 0))..snapshot.anchor_after(Point::new(2, 3));
            let comments = vec![TakenReviewComment {
                file_path: RelPath::new(
                    std::path::Path::new("file1"),
                    util::paths::PathStyle::Unix,
                )
                .expect("a plain file name is a valid relative path")
                .into_arc(),
                range,
                comment: "Prefer uppercase here".to_string(),
            }];
            (comments, project.read(cx).path_style(cx))
        });

        let message = cx.update(|_, cx| {
            compose_review_message(&editor, &comments, "Looks fine otherwise", path_style, cx)
        });

        assert_eq!(
            message,
            "I reviewed the changes and left 1 review comment below. \
             Please address them.\n\
             \n\
             `file1` lines 2-3:\n\
             ```\n\
             def\nghi\n\
             ```\n\
             Prefer uppercase here\n\
             \n\
             Looks fine otherwise\n"
        );

        let user_text = acp_v2::ContentBlock::Text(acp_v2::TextContent::new("please fix"));
        let review = acp_v2::ContentBlock::Text(acp_v2::TextContent::new(message.clone()));
        let chunks = vec![user_text.clone(), review];

        assert_eq!(review_block_comment_count(&message), Some(1));
        assert_eq!(review_block_comment_count("please fix"), None);
        assert_eq!(
            review_block_comment_count(
                "I reviewed the changes and left the following review comments. Please address them."
            ),
            None,
            "only the counted header is a review block"
        );


        // The pending-review pipeline composes with an empty summary.
        let pipeline_message =
            cx.update(|_, cx| compose_review_message(&editor, &comments, "", path_style, cx));
        let pipeline_chunks = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
            pipeline_message,
        ))];
        let parsed = review_comment_blocks(&pipeline_chunks);
        assert_eq!(parsed.len(), 1);

        assert_eq!(
            parsed[0].comments,
            vec![ReviewComment {
                path: "file1".into(),
                line_range: Some((2, 3)),
                quoted_code: "def\nghi".into(),
                comment: "Prefer uppercase here".into(),
            }]
        );

        assert_eq!(
            without_review_blocks(chunks),
            vec![user_text],
            "the sent message keeps the user's own text and drops the review block"
        );
    }

    #[test]
    fn test_parse_review_message_variants() {
        let comments = vec![
            ReviewComment {
                path: "src/a.rs".into(),
                line_range: Some((10, 10)),
                quoted_code: "let x = 1;".into(),
                comment: "rename x".into(),
            },
            ReviewComment {
                path: "src/b.rs".into(),
                line_range: Some((4, 6)),
                quoted_code: "fn f() {\n    todo!()\n}".into(),
                comment: "implement this\nover two lines".into(),
            },
            ReviewComment {
                path: "src/c.rs".into(),
                line_range: None,
                quoted_code: "".into(),
                comment: "whole file note".into(),
            },
        ];

        let message = serialize_review(&comments, "");
        assert_eq!(review_block_comment_count(&message), Some(3));

        let parsed = parse_review_message(&message).expect("valid review block");

        assert_eq!(parsed.comments, comments);

        assert!(parse_review_message("just a message").is_none());
    }
}
