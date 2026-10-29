//! What each agent last said a prompt may carry, remembered across restarts, so
//! a thread whose agent has not started yet already takes what that agent
//! takes: images, above all. An agent's prompt capabilities come from its
//! `initialize` response, so they belong to the agent rather than a session.

use agent_client_protocol::schema::v2 as acp_v2;
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Global};
use project::AgentId;
use util::ResultExt as _;

const KEY: &str = "quiet_ui_prompt_capabilities";

#[derive(Default)]
struct Remembered(Option<HashMap<String, acp_v2::PromptCapabilities>>);

impl Global for Remembered {}

fn remembered(cx: &mut App) -> &mut HashMap<String, acp_v2::PromptCapabilities> {
    let kvp = KeyValueStore::global(cx);
    cx.default_global::<Remembered>().0.get_or_insert_with(|| {
        kvp.read_kvp(KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default()
    })
}

pub fn recall(agent: &AgentId, cx: &mut App) -> Option<acp_v2::PromptCapabilities> {
    remembered(cx).get(agent.0.as_ref()).cloned()
}

pub fn remember(agent: &AgentId, capabilities: &acp_v2::PromptCapabilities, cx: &mut App) {
    let remembered = remembered(cx);
    if remembered.get(agent.0.as_ref()) == Some(capabilities) {
        return;
    }
    remembered.insert(agent.0.to_string(), capabilities.clone());
    let Some(json) = serde_json::to_string(remembered).log_err() else {
        return;
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move { kvp.write_kvp(KEY.to_string(), json).await.log_err() })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AgentPanel,
        conversation_view::tests::init_test,
        message_editor::MessageEditorEvent,
        test_support::{draft_message_editor, open_draft_with_connection},
    };
    use acp_thread::{AgentThreadEntry, StubAgentConnection};
    use base64::Engine as _;
    use editor::{SelectionEffects, actions::Paste};
    use fs::FakeFs;
    use gpui::{ClipboardItem, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use std::{io::Write as _, path::Path};
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn test_a_new_thread_takes_an_image_in_its_first_message(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        let cx = &mut cx;
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        let accepts_images: acp_v2::PromptCapabilities =
            serde_json::from_value(json!({ "image": {} })).unwrap();
        let (_sender, receiver) = watch::channel(accepts_images);
        let connection = StubAgentConnection::new().with_prompt_capabilities(receiver);
        open_draft_with_connection(&panel, connection, cx);
        assert!(
            panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).is_none()),
            "the draft has not started its thread"
        );

        let png = base64::prelude::BASE64_STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
            .unwrap();
        let mut image = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        image.write_all(&png).unwrap();
        cx.write_to_clipboard(ClipboardItem {
            entries: vec![gpui::ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                vec![image.path().to_path_buf()].into(),
            ))],
        });
        let editor = draft_message_editor(&panel, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("look ", window, cx);
            editor.editor().update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    s.select_ranges([
                        multi_buffer::MultiBufferOffset(5)..multi_buffer::MultiBufferOffset(5)
                    ])
                });
            });
            editor.paste(&Paste, window, cx);
        });
        cx.run_until_parked();
        let name = image
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            editor
                .read_with(cx, |editor, cx| editor.text(cx))
                .contains(&name),
            "the draft took the pasted image"
        );

        editor.update(cx, |_, cx| cx.emit(MessageEditorEvent::Send));
        cx.run_until_parked();
        let thread = panel
            .read_with(cx, |panel, cx| panel.active_agent_thread(cx))
            .expect("sending started the thread");
        let first = thread.read_with(cx, |thread, _| match thread.entries().first() {
            Some(AgentThreadEntry::UserMessage(message)) => {
                message.content.source_blocks().to_vec()
            }
            other => panic!("the first entry is the message, not {other:?}"),
        });
        assert!(
            first
                .iter()
                .any(|block| matches!(block, acp_v2::ContentBlock::Image(_))),
            "the image went with the first message: {first:?}"
        );
    }

    /// The first thread after a restart has nothing live to ask yet.
    #[gpui::test]
    async fn test_what_an_agent_accepts_outlives_the_app(cx: &mut TestAppContext) {
        init_test(cx);
        let agent = AgentId::new("claude-acp");
        let accepts_images: acp_v2::PromptCapabilities =
            serde_json::from_value(json!({ "image": {} })).unwrap();
        cx.update(|cx| remember(&agent, &accepts_images, cx));
        cx.run_until_parked();
        cx.update(|cx| cx.remove_global::<Remembered>());
        assert_eq!(cx.update(|cx| recall(&agent, cx)), Some(accepts_images));
        assert_eq!(cx.update(|cx| recall(&AgentId::new("other"), cx)), None);
    }
}
