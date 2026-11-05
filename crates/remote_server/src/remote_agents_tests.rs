//! quiet-ui: a thread asking for an agent the box has not listed yet waits
//! for the list instead of failing with "not registered".

use crate::remote_editing_tests::init_test;
use fs::FakeFs;
use gpui::TestAppContext;
use project::AgentId;
use serde_json::json;
use settings::SettingsStore;
use util::path;

#[gpui::test]
async fn test_a_thread_waits_for_the_box_to_list_its_agent(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/project"), json!({})).await;
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
    let agent = AgentId::new("claude-acp");

    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let waiting = cx.spawn({
        let store = store.clone();
        let project = project.clone();
        let agent = agent.clone();
        let done = done.clone();
        async move |mut cx| {
            agent_servers::wait_until_registered(&store, &project, &agent, &mut cx).await;
            done.set(true);
        }
    });
    cx.run_until_parked();
    assert!(!done.get(), "the box has not listed it yet");

    server_cx.update_global::<SettingsStore, _>(|store, cx| {
        store
            .set_server_settings(
                &json!({ "agent_servers": { "claude-acp": { "type": "custom", "command": "claude-acp" } } })
                    .to_string(),
                cx,
            )
            .unwrap();
    });
    server_cx.run_until_parked();
    cx.run_until_parked();
    waiting.await;
    assert!(done.get());
    assert!(
        store.update(cx, |store, _| store.get_external_agent(&agent).is_some()),
        "the wait ends once the agent is listed, and the launch goes ahead"
    );
}
