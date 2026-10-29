//! quiet-ui: review comments on a diff go to a thread as one message, in front
//! of the next one typed there or on their own from "Send Review to Agent".

use std::fmt::Write as _;

use acp_thread::ThreadStatus;
use agent_client_protocol::schema::v2 as acp_v2;
use editor::{Editor, EditorEvent, TakenReviewComment, actions::SendReviewToAgent};
use gpui::{AnyElement, App, Context, Entity, Task, Window};
use ui::{Tooltip, prelude::*};
use util::paths::PathStyle;
use workspace::{ItemHandle, Toast, Workspace, notifications::NotificationId};

use crate::{AgentPanel, conversation_view::ThreadView};

pub(crate) fn send_review_to_agent(
    workspace: &mut Workspace,
    _: &SendReviewToAgent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((_, editor)) = review_editor(workspace, cx) else {
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
    let path_style = workspace.project().read(cx).path_style(cx);
    let Some(block) = review_block(&comments, path_style) else {
        return;
    };
    let thread_id = thread_view.read(cx).root_thread_id;
    workspace.focus_panel::<AgentPanel>(window, cx);
    // Sending reads the workspace, which is leased here.
    window.defer(cx, move |window, cx| {
        thread_view.update(cx, |thread_view, cx| {
            deliver_review(thread_view, block, window, cx);
        });
        panel.update(cx, |panel, cx| {
            panel.activate_thread_tab(thread_id, true, window, cx);
        });
    });
}

/// The diff in front, else any open editor holding comments.
fn review_editor(workspace: &Workspace, cx: &App) -> Option<(Box<dyn ItemHandle>, Entity<Editor>)> {
    let with_comments = |item: Box<dyn ItemHandle>| {
        let editor = item.act_as::<Editor>(cx)?;
        (editor.read(cx).total_review_comment_count() > 0).then_some((item, editor))
    };
    workspace
        .active_item(cx)
        .and_then(with_comments)
        .or_else(|| {
            workspace
                .items(cx)
                .find_map(|item| with_comments(item.boxed_clone()))
        })
}

/// Review comments waiting on a diff, for the thread a review would go to.
pub(crate) struct PendingReview {
    item: Box<dyn ItemHandle>,
    editor: Entity<Editor>,
    pub(crate) count: usize,
}

/// Re-renders the review's target thread when comments are added or taken, so
/// its message box shows how many are waiting.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|_: &mut Editor, _, cx: &mut Context<Editor>| {
        cx.subscribe_self(|editor, event, cx| {
            if !matches!(event, EditorEvent::ReviewCommentsChanged { .. }) {
                return;
            }
            let target = editor
                .workspace()
                .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                .and_then(|panel| panel.read(cx).review_target(cx));
            if let Some(thread_view) = target {
                thread_view.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    })
    .detach();
}

/// The comments the next message from this thread would carry: those on a
/// diff in its workspace, when it is the thread `review_target` picks.
pub(crate) fn pending_review(
    thread_view: &ThreadView,
    cx: &Context<ThreadView>,
) -> Option<PendingReview> {
    let workspace = thread_view.workspace.upgrade()?;
    let workspace = workspace.read(cx);
    let target = workspace
        .panel::<AgentPanel>(cx)?
        .read(cx)
        .review_target(cx)?;
    if target != cx.entity() {
        return None;
    }
    let (item, editor) = review_editor(workspace, cx)?;
    let count = editor.read(cx).total_review_comment_count();
    (thread_view.dismissed_review != Some((editor.entity_id(), count))).then_some(PendingReview {
        item,
        editor,
        count,
    })
}

/// Takes the pending comments off the diff as one message block.
pub(crate) fn take_pending_review(
    thread_view: &ThreadView,
    cx: &mut Context<ThreadView>,
) -> Option<acp_v2::ContentBlock> {
    let review = pending_review(thread_view, cx)?;
    let comments = review
        .editor
        .update(cx, |editor, cx| editor.take_review_comments(cx));
    let path_style = thread_view.project.upgrade()?.read(cx).path_style(cx);
    review_block(&comments, path_style)
}

fn review_block(
    comments: &[TakenReviewComment],
    path_style: PathStyle,
) -> Option<acp_v2::ContentBlock> {
    (!comments.is_empty()).then(|| {
        acp_v2::ContentBlock::Text(acp_v2::TextContent::new(compose_review_message(
            comments, path_style,
        )))
    })
}

/// Sends the pending comments as a message of their own, once nothing is
/// being updated.
pub(crate) fn send_pending_review(window: &mut Window, cx: &mut Context<ThreadView>) {
    cx.defer_in(window, |thread_view, window, cx| {
        if let Some(review) = take_pending_review(thread_view, cx) {
            deliver_review(thread_view, review, window, cx);
        }
    });
}

/// Sends a review as a message of its own, queued behind a running turn.
pub(crate) fn deliver_review(
    thread_view: &mut ThreadView,
    block: acp_v2::ContentBlock,
    window: &mut Window,
    cx: &mut Context<ThreadView>,
) {
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
}

/// "N review comments" in the message box: a click opens the diff, the x
/// leaves them out of the next message (the diff keeps them).
pub(crate) fn render_review_chip(
    thread_view: &mut ThreadView,
    cx: &Context<ThreadView>,
) -> Option<AnyElement> {
    let review = pending_review(thread_view, cx);
    thread_view.review_chip_shown = review.is_some();
    let review = review?;
    let count = review.count;
    let editor_id = review.editor.entity_id();
    let workspace = thread_view.workspace.clone();
    let item = review.item;
    let label = if count == 1 {
        "1 review comment".to_string()
    } else {
        format!("{count} review comments")
    };
    Some(
        h_flex()
            .gap_0p5()
            .child(
                Button::new("pending-review", label)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Chat).size(IconSize::Small))
                    .tooltip(Tooltip::text(
                        "Sent with your next message. Click to open the diff.",
                    ))
                    .on_click(move |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.activate_item(item.as_ref(), true, true, window, cx);
                            })
                            .ok();
                    }),
            )
            .child(
                IconButton::new("dismiss-pending-review", IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Leave the Review Comments Out"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dismissed_review = Some((editor_id, count));
                        cx.notify();
                    })),
            )
            .into_any_element(),
    )
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
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use std::path::Path;
    use workspace::MultiWorkspace;

    struct Review {
        panel: Entity<AgentPanel>,
        editor: Entity<Editor>,
        connection: StubAgentConnection,
    }

    fn reply_done(connection: &StubAgentConnection) {
        connection.set_next_prompt_updates(vec![
            agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new("Done".into()),
            ),
        ]);
    }

    /// A thread that has run a turn, and two comments on `src/main.rs`.
    async fn thread_with_two_comments(cx: &mut TestAppContext) -> (Review, VisualTestContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            super::init(cx);
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
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        let cx = &mut cx;
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let connection = StubAgentConnection::new();
        reply_done(&connection);
        open_thread_with_connection(&panel, connection.clone(), cx);
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
        // The chip is drawn, and so known about, only in a thread on screen.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<AgentPanel>(window, cx);
        });
        cx.run_until_parked();
        (
            Review {
                panel,
                editor,
                connection,
            },
            cx.clone(),
        )
    }

    fn last_user_message(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> String {
        panel.read_with(cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread
                .read(cx)
                .entries()
                .iter()
                .rev()
                .find(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
                .map(|entry| entry.to_markdown(cx))
                .unwrap()
        })
    }

    fn assert_carries_both_comments(message: &str) {
        assert!(message.contains("`src/main.rs` line 1:"), "{message}");
        assert!(message.contains("fn main() {"), "{message}");
        assert!(message.contains("Name this function"), "{message}");
        assert!(message.contains("`src/main.rs` lines 2-3:"), "{message}");
        assert!(message.contains("println!(\"{x}\");"), "{message}");
        assert!(message.contains("Inline x"), "{message}");
    }

    fn thread_view(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> Entity<ThreadView> {
        panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap())
    }

    /// Typing in the thread and sending puts the comments in front of the
    /// text, as one message, and takes them off the diff.
    #[gpui::test]
    async fn a_typed_message_carries_the_pending_comments(cx: &mut TestAppContext) {
        let (Review { panel, editor, .. }, mut cx) = thread_with_two_comments(cx).await;
        let cx = &mut cx;
        let thread_view = thread_view(&panel, cx);
        thread_view.read_with(cx, |thread_view, _| {
            assert!(thread_view.review_chip_shown, "the message box shows them")
        });

        let message_editor = thread_view.read_with(cx, |view, _| view.message_editor.clone());
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Address these, then run the tests", window, cx);
        });
        thread_view.update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.total_review_comment_count(), 0, "sending takes them")
        });
        let message = last_user_message(&panel, cx);
        assert_carries_both_comments(&message);
        let typed = message
            .find("Address these, then run the tests")
            .expect(&message);
        assert!(
            message.find("Name this function").unwrap() < typed,
            "the comments come first: {message}"
        );
        thread_view.read_with(cx, |thread_view, _| {
            assert!(!thread_view.review_chip_shown, "and the chip is gone")
        });
    }

    /// Sending with nothing typed sends the comments alone; dismissing the chip
    /// leaves them on the diff and out of the next message.
    #[gpui::test]
    async fn an_empty_send_sends_the_comments_and_a_dismissed_chip_keeps_them(
        cx: &mut TestAppContext,
    ) {
        let (
            Review {
                panel,
                editor,
                connection,
            },
            mut cx,
        ) = thread_with_two_comments(cx).await;
        let cx = &mut cx;
        let thread_view = thread_view(&panel, cx);
        let editor_id = editor.entity_id();
        thread_view.update(cx, |thread_view, cx| {
            thread_view.dismissed_review = Some((editor_id, 2));
            cx.notify();
        });
        cx.run_until_parked();
        let message_editor = thread_view.read_with(cx, |view, _| view.message_editor.clone());
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Only this", window, cx);
        });
        reply_done(&connection);
        thread_view.update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();
        assert!(!last_user_message(&panel, cx).contains("Name this function"));
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.total_review_comment_count(),
                2,
                "dismissed, not taken"
            )
        });

        thread_view.update(cx, |thread_view, cx| {
            thread_view.dismissed_review = None;
            cx.notify();
        });
        cx.run_until_parked();
        thread_view.read_with(cx, |thread_view, cx| {
            assert!(thread_view.review_chip_shown);
            assert!(thread_view.message_editor.read(cx).is_empty(cx));
            assert_eq!(thread_view.thread.read(cx).status(), ThreadStatus::Idle);
        });
        reply_done(&connection);
        thread_view.update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();
        assert_carries_both_comments(&last_user_message(&panel, cx));
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.total_review_comment_count(), 0)
        });
    }

    #[gpui::test]
    async fn sending_a_review_carries_every_comment_to_the_thread(cx: &mut TestAppContext) {
        let (Review { panel, editor, .. }, mut cx) = thread_with_two_comments(cx).await;
        let cx = &mut cx;
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
        assert_carries_both_comments(&last_user_message(&panel, cx));
    }
}
