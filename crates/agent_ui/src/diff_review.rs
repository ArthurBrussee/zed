//! quiet-ui: "Send Review to Agent" on a diff sends its review comments to a
//! thread as one message.

use std::fmt::Write as _;

use acp_thread::ThreadStatus;
use agent_client_protocol::schema::v2 as acp_v2;
use editor::{Editor, TakenReviewComment, actions::SendReviewToAgent};
use gpui::{App, Context, Entity, Task, Window};
use util::paths::PathStyle;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::AgentPanel;

pub(crate) fn send_review_to_agent(
    workspace: &mut Workspace,
    _: &SendReviewToAgent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = review_editor(workspace, cx) else {
        return;
    };
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    let Some(thread_view) = panel.read(cx).review_target(cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<SendReviewToAgent>(),
                "No thread to send the review to",
            )
            .autohide(),
            cx,
        );
        return;
    };
    let comments = editor.update(cx, |editor, cx| editor.take_review_comments(cx));
    if comments.is_empty() {
        return;
    }
    let path_style = workspace.project().read(cx).path_style(cx);
    let message = compose_review_message(&comments, path_style);
    let block = acp_v2::ContentBlock::Text(acp_v2::TextContent::new(message));
    let thread_id = thread_view.read(cx).root_thread_id;
    workspace.focus_panel::<AgentPanel>(window, cx);
    // Sending reads the workspace, which is leased here.
    window.defer(cx, move |window, cx| {
        thread_view.update(cx, |thread_view, cx| {
            if thread_view.thread.read(cx).status() == ThreadStatus::Generating {
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
        panel.update(cx, |panel, cx| {
            panel.activate_thread_tab(thread_id, true, window, cx);
        });
    });
}

/// The diff in front, else any open editor holding comments.
fn review_editor(workspace: &Workspace, cx: &App) -> Option<Entity<Editor>> {
    let has_comments = |editor: &Entity<Editor>| editor.read(cx).total_review_comment_count() > 0;
    workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
        .filter(has_comments)
        .or_else(|| {
            workspace
                .items(cx)
                .filter_map(|item| item.act_as::<Editor>(cx))
                .find(has_comments)
        })
}

fn compose_review_message(comments: &[TakenReviewComment], path_style: PathStyle) -> String {
    let mut message = String::from("Review comments on the diff:\n");
    for comment in comments {
        let (start, end) = (comment.rows.start() + 1, comment.rows.end() + 1);
        let lines = if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        };
        // A fence longer than any backtick run in the code it quotes.
        let longest_run = comment
            .code
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat(longest_run.max(2) + 1);
        write!(
            message,
            "\n`{}` {lines}:\n{fence}\n{}\n{fence}\n{}\n",
            comment.path.display(path_style),
            comment.code,
            comment.comment.trim(),
        )
        .ok();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::init_test;
    use crate::test_support::{open_thread_with_connection, send_message};
    use acp_thread::{AgentThreadEntry, StubAgentConnection};
    use editor::display_map::DisplayRow;
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use std::path::Path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn sending_a_review_carries_every_comment_to_the_thread(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree(
            "/project",
            json!({ "src": { "main.rs": "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n" } }),
        )
        .await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![
            agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new("Done".into()),
            ),
        ]);
        open_thread_with_connection(&panel, connection, cx);
        send_message(&panel, cx);

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, util::rel_path::rel_path("src/main.rs")),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        // Bottom first: an overlay is a block, which moves the display rows below it.
        for (rows, text) in [
            (DisplayRow(1)..DisplayRow(2), "Inline x"),
            (DisplayRow(0)..DisplayRow(0), "Name this function"),
        ] {
            // The overlay focuses its prompt, so typing goes to it.
            editor.update_in(cx, |editor, window, cx| {
                editor.show_diff_review_overlay(rows, window, cx);
            });
            cx.simulate_input(text);
            editor.update_in(cx, |editor, window, cx| {
                editor.submit_diff_review_comment(window, cx);
            });
        }
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.total_review_comment_count(), 2)
        });

        editor.update_in(cx, |_, window, cx| {
            window.dispatch_action(Box::new(SendReviewToAgent), cx);
        });
        cx.run_until_parked();
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.total_review_comment_count(),
                0,
                "sending takes the comments"
            )
        });

        let message = panel.read_with(cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread
                .read(cx)
                .entries()
                .iter()
                .rev()
                .find(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
                .map(|entry| entry.to_markdown(cx))
                .unwrap()
        });
        assert!(message.contains("`src/main.rs` line 1:"), "{message}");
        assert!(message.contains("fn main() {"), "{message}");
        assert!(message.contains("Name this function"), "{message}");
        assert!(message.contains("`src/main.rs` lines 2-3:"), "{message}");
        assert!(message.contains("println!(\"{x}\");"), "{message}");
        assert!(message.contains("Inline x"), "{message}");
    }
}
