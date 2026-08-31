use super::*;
use acp_thread::{AcpThread, PermissionOptions, StubAgentConnection};
use agent::ThreadStore;
use agent_ui::{
    ThreadId,
    terminal_thread_metadata_store::{
        TerminalThreadMetadata, TerminalThreadMetadataStore, TestTerminalMetadataDbName,
    },
    test_support::{
        active_session_id, active_thread_id, open_thread_with_connection,
        open_thread_with_custom_connection, send_message,
    },
    thread_metadata_store::{ThreadMetadata, WorktreePaths},
    threads_archive_view::format_age,
};
use chrono::DateTime;
use fs::{FakeFs, Fs};
use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use project::AgentId;
use settings::SettingsStore;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{path_list::PathList, rel_path::rel_path};

fn use_unique_metadata_databases(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DATABASE: AtomicUsize = AtomicUsize::new(0);
    let test_database_id = NEXT_TEST_DATABASE.fetch_add(1, Ordering::SeqCst);
    cx.update(|cx| {
        cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
            format!("SIDEBAR_THREAD_METADATA_{test_database_id}"),
        ));
        cx.set_global(TestTerminalMetadataDbName(format!(
            "SIDEBAR_TERMINAL_THREAD_METADATA_{test_database_id}"
        )));
    });
}

fn init_test(cx: &mut TestAppContext) {
    use_unique_metadata_databases(cx);
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        // Use an isolated DB so parallel tests can't see each other's
        // persisted records (e.g. created-worktree records).
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        TerminalThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
}

#[track_caller]
fn assert_active_thread(sidebar: &Sidebar, session_id: &acp::SessionId, msg: &str) {
    let active = sidebar.active_entry.as_ref();
    let matches = active.is_some_and(|entry| {
        matches!(entry, ActiveEntry::Thread { session_id: Some(active_session_id), .. } if active_session_id == session_id)
            || sidebar.contents.entries.iter().any(|list_entry| {
                matches!(list_entry, ListEntry::Thread(t)
                    if t.metadata.session_id.as_ref() == Some(session_id)
                        && entry.matches_entry(list_entry))
            })
    });
    assert!(
        matches,
        "{msg}: expected active_entry for session {session_id:?}, got {:?}",
        active,
    );
}

#[track_caller]
fn is_active_session(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    let thread_id = sidebar
        .contents
        .entries
        .iter()
        .find_map(|entry| match entry {
            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id) => {
                Some(t.metadata.thread_id)
            }
            _ => None,
        });
    match thread_id {
        Some(tid) => {
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { thread_id, .. }) if *thread_id == tid)
        }
        // Thread not in sidebar entries — can't confirm it's active.
        None => false,
    }
}

#[track_caller]
fn assert_active_draft(sidebar: &Sidebar, workspace: &Entity<Workspace>, msg: &str) {
    assert!(
        matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == workspace),
        "{msg}: expected active_entry to be Draft for workspace {:?}, got {:?}",
        workspace.entity_id(),
        sidebar.active_entry,
    );
}

fn has_thread_entry(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    sidebar
        .contents
        .entries
        .iter()
        .any(|entry| matches!(entry, ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id)))
}

#[track_caller]
// The merged history model has no per-project headers; this now asserts
// whether any thread or terminal row is listed at all.
fn assert_sidebar_has_thread_rows(
    sidebar: &Entity<Sidebar>,
    expected_has_threads: bool,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        let has_threads = sidebar
            .contents
            .entries
            .iter()
            .any(|entry| matches!(entry, ListEntry::Thread(_) | ListEntry::Terminal(_)));

        assert_eq!(
            has_threads, expected_has_threads,
            "expected sidebar has_threads={expected_has_threads}, got {has_threads}"
        );
    });
}

#[track_caller]
// The merged history model has no project headers; the invariant left is
// that exactly the two expected threads stay listed throughout the flicker.
fn assert_remote_project_integration_sidebar_state(
    sidebar: &mut Sidebar,
    main_thread_id: &acp::SessionId,
    remote_thread_id: &acp::SessionId,
) {
    let mut saw_main_thread = false;
    let mut saw_remote_thread = false;
    for entry in &sidebar.contents.entries {
        match entry {
            ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => {}
            ListEntry::Thread(thread)
                if thread.metadata.session_id.as_ref() == Some(main_thread_id) =>
            {
                saw_main_thread = true;
            }
            ListEntry::Thread(thread)
                if thread.metadata.session_id.as_ref() == Some(remote_thread_id) =>
            {
                saw_remote_thread = true;
            }
            ListEntry::Thread(thread) => {
                let title = thread.metadata.display_title();
                panic!(
                    "unexpected sidebar thread while simulating remote project integration flicker: title=`{}`",
                    title
                );
            }
            ListEntry::Terminal(terminal) => {
                panic!(
                    "unexpected sidebar terminal while simulating remote project integration flicker: title=`{}`",
                    terminal.metadata.title
                );
            }
        }
    }

    assert!(
        saw_main_thread,
        "expected the sidebar to keep showing `Main Thread` under `project`"
    );
    assert!(
        saw_remote_thread,
        "expected the sidebar to keep showing `Worktree Thread` under `project`"
    );
}

async fn init_test_project(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

fn setup_sidebar(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let sidebar = setup_sidebar_closed(multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    sidebar
}

fn setup_sidebar_closed(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let multi_workspace = multi_workspace.clone();
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(cx, |mw, cx| {
        mw.register_sidebar(sidebar.clone(), cx);
    });
    cx.run_until_parked();
    sidebar
}

async fn save_n_test_threads(
    count: u32,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    for i in 0..count {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(format!("thread-{}", i))),
            Some(format!("Thread {}", i + 1).into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, i).unwrap(),
            None,
            None,
            project,
            cx,
        )
    }
    cx.run_until_parked();
}

async fn save_test_thread_metadata(
    session_id: &acp::SessionId,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    save_thread_metadata(
        session_id.clone(),
        Some("Test".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    )
}

async fn save_named_thread_metadata(
    session_id: &str,
    title: &str,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    save_thread_metadata(
        acp::SessionId::new(Arc::from(session_id)),
        Some(SharedString::from(title.to_string())),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    );
    cx.run_until_parked();
}

/// Seeds a pre-built [`ThreadMetadata`] into the global store so tests can
/// exercise flows that resolve a thread by id.
fn seed_thread_metadata(metadata: ThreadMetadata, cx: &mut TestAppContext) {
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

/// Spins up a fresh remote project backed by a headless server sharing
/// `server_fs`, opens the given worktree path on it, and returns the
/// project together with the headless entity (which the caller must keep
/// alive for the duration of the test) and the `RemoteConnectionOptions`
/// used for the fake server. Passing those options back into
/// `reuse_opts` on a subsequent call makes the new project share the
/// same `RemoteConnectionIdentity`, matching how Zed treats multiple
/// projects on the same SSH host.
async fn start_remote_project(
    server_fs: &Arc<FakeFs>,
    worktree_path: &Path,
    app_state: &Arc<workspace::AppState>,
    reuse_opts: Option<&remote::RemoteConnectionOptions>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<project::Project>,
    Entity<remote_server::HeadlessProject>,
    remote::RemoteConnectionOptions,
) {
    // Bare `_` on the guard so it's dropped immediately; holding onto it
    // would deadlock `connect_mock` below since the client waits on the
    // guard before completing the mock handshake.
    let (opts, server_session) = match reuse_opts {
        Some(existing) => {
            let (session, _) = remote::RemoteClient::fake_server_with_opts(existing, cx, server_cx);
            (existing.clone(), session)
        }
        None => {
            let (opts, session, _) = remote::RemoteClient::fake_server(cx, server_cx);
            (opts, session)
        }
    };

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let fs = server_fs.clone();
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_path, true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    (project, headless, opts)
}

fn save_thread_metadata(
    session_id: acp::SessionId,
    title: Option<SharedString>,
    updated_at: DateTime<Utc>,
    created_at: Option<DateTime<Utc>>,
    interacted_at: Option<DateTime<Utc>>,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let worktree_paths = project.read(cx).worktree_paths(cx);
        let remote_connection = project.read(cx).remote_connection_options(cx);
        let thread_id = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new);
        let metadata = ThreadMetadata {
            thread_id,
            session_id: Some(session_id),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title,
            title_override: None,
            updated_at,
            created_at,
            interacted_at,
            worktree_paths,
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn save_thread_metadata_with_main_paths(
    session_id: &str,
    title: &str,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    let session_id = acp::SessionId::new(Arc::from(session_id));
    let title = SharedString::from(title.to_string());
    let thread_id = cx.update(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new)
    });
    let metadata = ThreadMetadata {
        thread_id,
        session_id: Some(session_id),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some(title),
        title_override: None,
        updated_at,
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        archived: false,
        remote_connection: None,
    };
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn save_draft_metadata_with_main_paths(
    title: Option<SharedString>,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) -> ThreadId {
    let thread_id = ThreadId::new();
    let metadata = ThreadMetadata {
        thread_id,
        session_id: None,
        agent_id: agent::ZED_AGENT_ID.clone(),
        title,
        title_override: None,
        updated_at,
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        archived: false,
        remote_connection: None,
    };
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
    thread_id
}

fn focus_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
}

fn enter_renamed_title(
    sidebar: &Entity<Sidebar>,
    target: RenameTarget,
    renamed_title: &str,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.rename_target, Some(target));
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();
}

fn request_test_tool_authorization(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    option_id: &str,
    cx: &mut gpui::VisualTestContext,
) {
    let tool_call_id = acp::ToolCallId::new(tool_call_id);
    let label = format!("Tool {tool_call_id}");
    let option_id = acp::PermissionOptionId::new(option_id);
    let _authorization_task = cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            thread
                .request_tool_call_authorization(
                    acp::ToolCall::new(tool_call_id, label)
                        .kind(acp::ToolKind::Edit)
                        .into(),
                    PermissionOptions::Flat(vec![acp::PermissionOption::new(
                        option_id,
                        "Allow",
                        acp::PermissionOptionKind::AllowOnce,
                    )]),
                    acp_thread::AuthorizationKind::PermissionGrant,
                    cx,
                )
                .unwrap()
        })
    });
    cx.run_until_parked();
}

fn format_linked_worktree_chips(worktrees: &[ThreadItemWorktreeInfo]) -> String {
    let mut seen = Vec::new();
    let mut chips = Vec::new();
    for wt in worktrees {
        if wt.kind == ui::WorktreeKind::Main {
            continue;
        }
        let Some(name) = wt.worktree_name.as_ref() else {
            continue;
        };
        if !seen.contains(name) {
            seen.push(name.clone());
            chips.push(format!("{{{}}}", name));
        }
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!(" {}", chips.join(", "))
    }
}

fn visible_entries_as_strings(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let selected = if sidebar.selection == Some(ix) {
                    "  <== selected"
                } else {
                    ""
                };
                match entry {
                    // Headers are presentation: every section groups its rows
                    // by workspace, so a header above them says nothing about
                    // the rows a test is pinning. `entry_shape_strings` is
                    // where that structure is asserted.
                    ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => String::new(),
                    ListEntry::Thread(thread) => {
                        let title = thread.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&thread.worktrees);

                        {
                            let live = if thread.is_live { " *" } else { "" };
                            let status_str = match thread.status {
                                AgentThreadStatus::Running => " (running)",
                                AgentThreadStatus::Error => " (error)",
                                AgentThreadStatus::WaitingForConfirmation => " (waiting)",
                                _ => "",
                            };
                            let notified = if sidebar
                                .contents
                                .is_thread_notified(&thread.metadata.thread_id)
                            {
                                " (!)"
                            } else {
                                ""
                            };
                            let archived = if thread.metadata.archived {
                                " (archived)"
                            } else {
                                ""
                            };
                            format!(
                                "  {title}{worktree}{live}{status_str}{notified}{archived}{selected}"
                            )
                        }
                    }
                    ListEntry::Terminal(terminal) => {
                        let title = terminal.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&terminal.worktrees);
                        format!("  {title}{worktree}{selected}")
                    }
                }
            })
            .filter(|line| !line.is_empty())
            .collect()
    })
}

#[gpui::test]
// Rewritten for the merged history model: sticky project headers are gone,
// but a same-shape metadata update must still preserve the measured bounds
// of unrelated rows.
async fn test_thread_metadata_update_preserves_list_measurements(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    add_test_project("/project-b", &fs, &multi_workspace, cx).await;

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );
    save_thread_metadata_with_main_paths(
        "project-b-thread",
        "Project B Thread",
        PathList::new(&[PathBuf::from("/project-b")]),
        PathList::new(&[PathBuf::from("/project-b")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    // The last row is the oldest thread (Project A Thread); its measurement
    // must survive a same-shape rename of that thread.
    let last_row_ix = sidebar.read_with(cx, |sidebar, _| sidebar.contents.entries.len() - 1);

    let bounds_before = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(last_row_ix)
            .expect("row should be measured before metadata update")
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Renamed Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 1, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );

    let bounds_after = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(last_row_ix)
            .expect("same-shape metadata update should preserve row measurements")
    });
    assert_eq!(bounds_before, bounds_after);
}

#[gpui::test]
async fn test_thread_status_update_does_not_reset_list_measurements(cx: &mut TestAppContext) {
    // When a thread's status changes (e.g. Running -> Completed after sending a message), the
    // shape sequence is unchanged, so `update_entries` should not reset the underlying
    // `ListState`. Resetting throws away measured item bounds for one frame, which makes the
    // sticky project header flicker between its pushed-off and fully-on-screen positions.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let before = sidebar.read_with(cx, |sidebar, _app| {
        sidebar.entry_shapes().collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, _app| {
        sidebar.entry_shapes().collect::<Vec<_>>()
    });

    assert_eq!(
        before, after,
        "a no-op rebuild should produce an identical shape sequence"
    );
}

#[gpui::test]
// Rewritten for the merged history model: collapsing is gone, so the shape
// change trigger is removing a thread from the list.
async fn test_thread_removal_changes_entry_shape(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let before = sidebar.read_with(cx, |sidebar, _app| {
        sidebar.entry_shapes().collect::<Vec<_>>()
    });
    let thread_id = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                _ => None,
            })
            .expect("thread entry should exist")
    });
    cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete(thread_id, cx));
    });
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, _app| {
        sidebar.entry_shapes().collect::<Vec<_>>()
    });

    assert_ne!(
        before, after,
        "removing a thread should change the shape sequence so the list resets"
    );
}

#[gpui::test]
async fn test_serialization_round_trip(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    // Set a custom width.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.run_until_parked();

    // Capture the serialized state from the first sidebar.
    let serialized = sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx));
    let serialized = serialized.expect("serialized_state should return Some");

    // Create a fresh sidebar and restore into it.
    let sidebar2 =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();

    sidebar2.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();

    // Assert all serialized fields match.
    let width1 = sidebar.read_with(cx, |s, _| s.width);
    let width2 = sidebar2.read_with(cx, |s, _| s.width);

    assert_eq!(width1, width2);
    assert_eq!(width1, px(420.0));
}

#[gpui::test]
// Rewritten for the merged history model: the separate archive view is gone.
// Restoring serialized state from a build that still recorded an active
// archive view must be tolerated (the unknown field is ignored).
async fn test_restore_serialized_archive_view_does_not_panic(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.update(|_window, cx| {
        AgentRegistryStore::init_test_global(cx, vec![]);
    });

    let serialized = r#"{"width":400.0,"active_view":"History"}"#;

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        if let Some(sidebar) = multi_workspace.sidebar() {
            sidebar.restore_serialized_state(serialized, window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.width, px(400.0));
    });
}

#[gpui::test]
async fn test_entities_released_on_window_close(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let weak_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().downgrade());
    let weak_sidebar = sidebar.downgrade();
    let weak_multi_workspace = multi_workspace.downgrade();

    drop(sidebar);
    drop(multi_workspace);
    cx.update(|window, _cx| window.remove_window());
    cx.run_until_parked();

    weak_multi_workspace.assert_released();
    weak_sidebar.assert_released();
    weak_workspace.assert_released();
}

#[gpui::test]
async fn test_single_workspace_no_threads(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (_sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_eq!(
        visible_entries_as_strings(&_sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_single_workspace_with_saved_threads(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix crash in project panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-2")),
        Some("Add inline diff view".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix crash in project panel",
            "  Add inline diff view",
        ]
    );
}

#[gpui::test]
async fn test_workspace_lifecycle(cx: &mut TestAppContext) {
    let project = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Single workspace with a thread
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-a1")),
        Some("Thread A1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Thread A1",
        ]
    );

    // Add a second workspace
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Thread A1",
        ]
    );
}

#[gpui::test]
// Rewritten for the merged history model: project groups (and collapsing)
// are gone. Archiving now keeps the thread in the list, rendered muted.
async fn test_archived_thread_stays_in_list(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);

    let thread_id = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                _ => None,
            })
            .expect("thread entry should exist")
    });

    cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.archive(thread_id, None, cx));
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 1 (archived)"],
        "archived threads stay in the merged history list"
    );

    cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.unarchive(thread_id, cx));
    });
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);
}

#[gpui::test]
// Rewritten for the merged history model: there is no per-group collapse
// state anymore. The invariant that remains is that threads stay visible
// when the project's group key changes (a worktree is added).
async fn test_threads_survive_worktree_key_change(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 2", "  Thread 1"]
    );

    // Add a second worktree; the project group key changes from [/project-a]
    // to [/project-a, /project-b].
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 2", "  Thread 1"]
    );
}

#[gpui::test]
async fn test_visible_entries_as_strings(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    sidebar.update_in(cx, |s, _window, _cx| {
        let notified_thread_id = ThreadId::new();
        s.contents.notified_threads.insert(notified_thread_id);
        s.contents.entries = vec![
            // Section headers are skipped by visible_entries_as_strings.
            ListEntry::SectionHeader(SidebarSection::OpenInZed),
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-1"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Completed thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_title_generating: false,
                draft: None,
                draft_leaves_workspace: false,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
                solo_worktree: None,
                under_worktree_header: false,
            })),
            // Active thread with Running status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-2"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Running thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Running,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_title_generating: false,
                draft: None,
                draft_leaves_workspace: false,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
                solo_worktree: None,
                under_worktree_header: false,
            })),
            // Active thread with Error status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-3"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Error thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Error,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_title_generating: false,
                draft: None,
                draft_leaves_workspace: false,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
                solo_worktree: None,
                under_worktree_header: false,
            })),
            // Thread with WaitingForConfirmation status, not active
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-4"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Waiting thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::WaitingForConfirmation,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_title_generating: false,
                draft: None,
                draft_leaves_workspace: false,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
                solo_worktree: None,
                under_worktree_header: false,
            })),
            // Background thread that completed (should show notification)
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: notified_thread_id,
                    session_id: Some(acp::SessionId::new(Arc::from("t-5"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Notified thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_title_generating: false,
                draft: None,
                draft_leaves_workspace: false,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
                solo_worktree: None,
                under_worktree_header: false,
            })),
            ListEntry::SectionHeader(SidebarSection::Archived),
        ];

        // Select the Running thread (index 2)
        s.selection = Some(2);
    });

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Completed thread",
            "  Running thread * (running)  <== selected",
            "  Error thread * (error)",
            "  Waiting thread (waiting)",
            "  Notified thread * (!)",
        ]
    );

    // A selection pointing at a section header renders no marker: headers are
    // presentation-only rows.
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = Some(6);
    });

    assert!(
        visible_entries_as_strings(&sidebar, cx)
            .iter()
            .all(|entry| !entry.contains("<== selected"))
    );

    // Clear selection
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = None;
    });

    // No entry should have the selected marker
    let entries = visible_entries_as_strings(&sidebar, cx);
    for entry in &entries {
        assert!(
            !entry.contains("<== selected"),
            "unexpected selection marker in: {}",
            entry
        );
    }
}

#[gpui::test]
async fn test_keyboard_select_next_and_previous(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Entries: [Active header, All Threads header, worktree header, thread3,
    // thread2, thread1]. Headers are not selectable, so navigation skips
    // indices 0 through 2.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at the first thread (index 3)
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // Move down through remaining entries
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(4));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(5));

    // At the end, wraps back to the first thread
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // Navigate back to the end
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(4));
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(5));

    // Move back up
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(4));

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    // At the top, selection clears (focus returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);
}

#[gpui::test]
async fn test_keyboard_select_first_and_last(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // SelectLast jumps to the end
    cx.dispatch_action(SelectLast);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(5));

    // SelectFirst jumps to the first thread; the section and worktree headers
    // above it are not selectable.
    cx.dispatch_action(SelectFirst);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));
}

#[gpui::test]
async fn test_keyboard_focus_in_does_not_set_selection(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Initially no selection
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // Open the sidebar so it's rendered, then focus it to trigger focus_in.
    // focus_in no longer sets a default selection.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // Manually set a selection, blur, then refocus — selection should be preserved
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.update(|window, cx| {
        window.blur(cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
// Rewritten for the merged history model: bucket headers replaced project
// headers and are inert, so Confirm on one is a no-op.
async fn test_keyboard_confirm_on_bucket_header_is_noop(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);

    // Force the selection onto the bucket header (index 0) and confirm.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);
}

#[gpui::test]
// Rewritten for the merged history model: there are no collapsible groups,
// so the SelectParent/SelectChild expand/collapse actions are no longer
// handled and the list stays unchanged when they are dispatched.
async fn test_keyboard_expand_and_collapse_are_noops(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);

    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 1  <== selected"]
    );

    cx.dispatch_action(menu::SelectParent);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 1  <== selected"]
    );

    cx.dispatch_action(menu::SelectChild);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread 1  <== selected"]
    );
}

#[gpui::test]
async fn test_keyboard_navigation_on_empty_list(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/empty-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // With no threads there are no rows at all: the merged history list has
    // no per-project headers.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );

    // Focus sidebar — focus_in does not set a selection
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // Navigation on an empty list keeps the selection cleared
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);
}

#[gpui::test]
async fn test_new_entry_noops_without_open_project(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    assert!(
        !sidebar.read_with(cx, |sidebar, _cx| sidebar.contents.has_open_projects),
        "empty workspaces should be treated as having no open projects"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_entry(&workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(
            panel.active_conversation_view().is_none(),
            "sidebar should not create an agent thread without an open project"
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_selection_clamps_after_entry_removal(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), navigate down to the thread
    // (index 2; 0 and 1 are the Active and All Threads headers. One thread
    // in the worktree, so the thread's own row is it — no separate worktree
    // row.)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));

    // Delete the thread, which removes it (and its bucket header) from the
    // list. Collapsing no longer exists in the merged history model.
    let thread_id = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                _ => None,
            })
            .expect("thread entry should exist")
    });
    cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete(thread_id, cx));
    });
    cx.run_until_parked();

    // Selection should not point past the end of the list
    let selection = sidebar.read_with(cx, |s, _| s.selection);
    let entry_count = sidebar.read_with(cx, |s, _| s.contents.entries.len());
    assert!(
        selection.unwrap_or(0) <= entry_count,
        "selection {} should be within bounds (entries: {})",
        selection.unwrap_or(0),
        entry_count,
    );
}

async fn init_test_project_with_agent_panel(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    use_unique_metadata_databases(cx);
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

fn add_agent_panel(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<AgentPanel> {
    workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    })
}

fn setup_sidebar_with_agent_panel(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> (Entity<Sidebar>, Entity<AgentPanel>) {
    let sidebar = setup_sidebar(multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let panel = add_agent_panel(&workspace, cx);
    (sidebar, panel)
}

#[gpui::test]
async fn test_agent_panel_terminals_appear_in_sidebar_and_search(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server"]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. }) if *active_terminal_id == terminal_id),
            "expected active terminal entry, got {:?}",
            sidebar.active_entry,
        );
        assert!(
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id && terminal.metadata.display_title().as_ref() == "Dev Server")
            }),
            "expected the inserted terminal to appear in sidebar contents",
        );
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        let metadata = store
            .entry(terminal_id)
            .expect("terminal metadata should be persisted");
        assert_eq!(metadata.title.as_ref(), "");
        assert_eq!(
            metadata.custom_title.as_ref().map(|title| title.as_ref()),
            Some("Dev Server")
        );
        assert_eq!(metadata.display_title().as_ref(), "Dev Server");
        assert!(
            metadata
                .folder_paths()
                .paths()
                .iter()
                .any(|path| path.as_path() == Path::new("/my-project"))
        );
    });

    type_in_search(&sidebar, "server", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server  <== selected"]
    );

    type_in_search(&sidebar, "missing", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_closing_last_agent_panel_terminal_restores_empty_header(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_sidebar_has_thread_rows(&sidebar, false, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_sidebar_has_thread_rows(&sidebar, true, cx);

    let (terminal_metadata, terminal_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id => {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&terminal_metadata, &terminal_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(!panel.has_terminal(terminal_id));
        assert!(
            panel.active_view_is_new_draft(cx),
            "closing the active terminal should leave the panel on its empty draft"
        );
    });
    // Closing the terminal drops the user back onto the panel's empty
    // draft. The sidebar mirrors that with a "New {agent}" placeholder row,
    // in Active, since that is what the panel is showing.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  New thread"]
    );
}

#[gpui::test]
async fn test_agent_panel_terminal_metadata_remains_visible_after_panel_is_removed(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).is_none()
    }));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server"]
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
}

#[gpui::test]
async fn test_terminal_metadata_is_deduped_across_project_groups(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Original", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace_a.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    let now = Utc::now();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Dev Server".into(),
        custom_title: None,
        created_at: now,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project-a")]),
            PathList::new(&[PathBuf::from("/project-b")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };

    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar
                .contents
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        ListEntry::Terminal(terminal)
                            if terminal.metadata.terminal_id == terminal_id
                    )
                })
                .count(),
            1
        );
    });
}

#[gpui::test]
async fn test_agent_panel_terminal_shows_project_and_linked_worktree(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&worktree_workspace, cx);

    panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server {wt-feature-a}"]
    );

    type_in_search(&sidebar, "wt-feature-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server {wt-feature-a}  <== selected"]
    );
}

#[gpui::test]
async fn test_terminal_close_event_on_archived_linked_worktree_removes_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);

    let archived_session_id = acp::SessionId::new(Arc::from("archived-wt-thread"));
    save_thread_metadata(
        archived_session_id.clone(),
        Some("Archived Worktree Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );
    let archived_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&archived_session_id)
            .expect("archived thread metadata should exist")
            .thread_id
    });
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.archive(archived_thread_id, None, cx);
        });
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        assert!(
            agent_ui::draft_prompt_store::read(empty_draft_id, cx).is_none(),
            "empty draft should not have persisted prompt content"
        );
    });

    let terminal_id = worktree_panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "should start with main and linked worktree workspaces"
    );
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before
            .iter()
            .any(|entry| entry.contains("Dev Server") && entry.contains('{')),
        "expected linked worktree terminal before closing, got: {entries_before:?}"
    );

    worktree_panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    for _ in 0..4 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after close"
    );
    let empty_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_none()
    });
    assert!(
        empty_draft_metadata_deleted,
        "empty draft metadata should be deleted before archiving the linked worktree"
    );
    let unarchived_worktree_threads = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries_for_path(&worktree_folder_paths, None)
            .count()
    });
    assert_eq!(
        unarchived_worktree_threads, 0,
        "closing the terminal must not create a fallback draft for the removed worktree"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "linked worktree workspace should be removed after closing its last terminal"
    );
    // Only the archived row may still carry the worktree chip.
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_after
            .iter()
            .all(|entry| !entry.contains('{') || entry.contains("(archived)")),
        "only archived rows may reference the archived worktree, got: {entries_after:?}"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after closing its last terminal"
    );
}

#[gpui::test]
async fn test_terminal_close_event_deletes_empty_draft_when_linked_worktree_has_no_archive_root(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    fs.insert_branches(Path::new("/project/.git"), &["main", "feature-a"]);
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/external-worktree"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/external-worktree".as_ref()], cx).await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    let worktree_folder_paths = PathList::new(&[PathBuf::from("/external-worktree")]);
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );

    let terminal_id = worktree_panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    worktree_panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    for _ in 0..4 {
        cx.run_until_parked();
    }

    let empty_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_none()
    });
    assert!(
        empty_draft_metadata_deleted,
        "empty draft metadata should be deleted when removing the linked worktree workspace"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should be removed after closing its last terminal"
    );
    assert!(
        fs.is_dir(Path::new("/external-worktree")).await,
        "external linked worktree directory should remain on disk when no archive root is produced"
    );
}

#[gpui::test]
async fn test_terminal_close_event_keeps_linked_worktree_workspace_with_live_editor_draft(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Worktree Draft".into()),
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );

    worktree_panel.update_in(cx, |panel, window, cx| {
        panel.load_agent_thread(
            Agent::Stub,
            draft_id,
            Some(worktree_folder_paths.clone()),
            None,
            false,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
    cx.run_until_parked();
    let editor_text =
        worktree_panel.read_with(cx, |panel, cx| panel.editor_text_if_in_memory(draft_id, cx));
    assert_eq!(
        editor_text,
        Some(None),
        "draft should be in memory with empty editor text before editing"
    );

    agent_ui::test_support::type_draft_prompt(&worktree_panel, "keep this draft", cx);

    let terminal_id = worktree_panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();
    let live_blocks = worktree_panel.read_with(cx, |panel, cx| {
        panel.draft_prompt_blocks_if_in_memory(draft_id, cx)
    });
    assert!(
        matches!(
            live_blocks.as_deref(),
            Some([acp::ContentBlock::Text(text)]) if text.text == "keep this draft"
        ),
        "edited draft should still be readable from the panel after opening the terminal"
    );

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "should start with main and linked worktree workspaces"
    );

    worktree_panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    for _ in 0..4 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after close"
    );
    let unarchived_worktree_threads = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries_for_path(&worktree_folder_paths, None)
            .count()
    });
    assert_eq!(
        unarchived_worktree_threads, 1,
        "edited draft should remain as a worktree thread reference"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_some(),
        "linked worktree workspace should stay open while an edited draft references it"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should remain on disk while an edited draft references it"
    );
}

#[gpui::test]
async fn test_archive_selected_draft_archives_linked_worktree_after_last_draft(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    add_agent_panel(&worktree_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let first_draft_id = save_draft_metadata_with_main_paths(
        Some("First Draft".into()),
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    let second_draft_id = save_draft_metadata_with_main_paths(
        Some("Second Draft".into()),
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 4, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            first_draft_id,
            &[acp::ContentBlock::Text(acp::TextContent::new(
                "first draft",
            ))],
            cx,
        )
    })
    .await
    .expect("first draft prompt should persist");
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            second_draft_id,
            &[acp::ContentBlock::Text(acp::TextContent::new(
                "second draft",
            ))],
            cx,
        )
    })
    .await
    .expect("second draft prompt should persist");
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let first_draft_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == first_draft_id
                )
            })
            .expect("first draft should be visible in sidebar")
    });
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(first_draft_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..4 {
        cx.run_until_parked();
    }

    let first_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(first_draft_id)
            .is_none()
    });
    assert!(
        first_draft_metadata_deleted,
        "first discarded draft metadata should be deleted"
    );
    let second_draft_metadata_kept = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(second_draft_id)
            .is_some()
    });
    assert!(
        second_draft_metadata_kept,
        "remaining contentful draft should still block worktree archival"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_some(),
        "linked worktree workspace should remain while another draft references it"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should remain while another draft references it"
    );

    let second_draft_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == second_draft_id
                )
            })
            .expect("second draft should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(second_draft_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let second_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(second_draft_id)
            .is_none()
    });
    assert!(
        second_draft_metadata_deleted,
        "last discarded draft metadata should be deleted"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should be removed after closing its last draft"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after closing its last draft"
    );
}

#[gpui::test]
async fn test_archive_selected_draft_archives_closed_linked_worktree(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Closed Worktree Draft".into()),
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            draft_id,
            &[acp::ContentBlock::Text(acp::TextContent::new(
                "closed draft",
            ))],
            cx,
        )
    })
    .await
    .expect("draft prompt should persist");
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let draft_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id
                )
            })
            .expect("closed worktree draft should be visible in sidebar")
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        match &sidebar.contents.entries[draft_index] {
            ListEntry::Thread(thread) => match &thread.workspace {
                ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                    assert_eq!(folder_paths, &worktree_folder_paths);
                }
                ThreadEntryWorkspace::Open(_) => {
                    panic!("linked worktree draft should start closed")
                }
            },
            _ => panic!("expected draft row"),
        }
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(draft_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(draft_id)
            .is_none()
    });
    assert!(
        draft_metadata_deleted,
        "discarded closed worktree draft metadata should be deleted"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "temporary linked worktree workspace should be removed after discarding its last draft"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "discarding a closed linked worktree draft should leave only the main workspace"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after discarding its last draft"
    );
}

#[gpui::test]
async fn test_terminal_close_event_closes_sidebar_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Dev Server"]
    );

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        assert!(
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entry(terminal_id)
                .is_none(),
            "terminal metadata should be deleted when the terminal requests close"
        );
    });
}

#[gpui::test]
async fn test_terminal_close_event_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(server_terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        // Workspace headers group THREADS; a workspace holding only terminals
        // has nothing to head, so the remaining terminal stands alone.
        vec!["  Build"]
    );
}

#[gpui::test]
async fn test_agent_panel_terminal_notifications_update_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(server_terminal_id));
    });

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_bell(build_terminal_id, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(sidebar.has_notifications(cx));
        assert!(sidebar.contents.notified_terminals.contains(&build_terminal_id));
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == build_terminal_id && terminal.has_notification)
        }));
    });

    panel.update_in(cx, |panel, window, cx| {
        panel.activate_terminal(build_terminal_id, true, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(!sidebar.has_notifications(cx));
        assert!(
            !sidebar
                .contents
                .notified_terminals
                .contains(&build_terminal_id)
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_can_activate_agent_panel_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    let (entry_terminal_ids, selected_terminal_id) = sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let switcher = switcher.read(cx);
        let entry_terminal_ids = switcher
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .terminal_id()
                    .expect("expected terminal switcher entry")
            })
            .collect::<Vec<_>>();
        let selected_terminal_id = switcher
            .selected_entry()
            .expect("switcher should have selected entry")
            .terminal_id()
            .expect("expected selected terminal switcher entry");
        (entry_terminal_ids, selected_terminal_id)
    });

    assert_eq!(entry_terminal_ids.len(), 2);
    assert!(entry_terminal_ids.contains(&build_terminal_id));
    assert!(entry_terminal_ids.contains(&server_terminal_id));

    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(selected_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == selected_terminal_id),
            "expected selected terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_includes_terminal_metadata_for_open_project_group(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-newer")),
        Some("Newer Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-older")),
        Some("Older Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            PathList::new(&[PathBuf::from("/project-feature")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(
            switcher
                .read(cx)
                .entries()
                .iter()
                .any(|entry| entry.terminal_id() == Some(terminal_id)),
            "terminal metadata row should be included like a closed thread row"
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_preserves_closed_terminal_linked_worktree_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should start closed"
    );

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        match switcher
            .read(cx)
            .selected_entry()
            .expect("switcher should select the terminal row by default")
        {
            ThreadSwitcherEntry::Terminal(entry) => {
                assert_eq!(entry.metadata.terminal_id, terminal_id);
                match &entry.workspace {
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        assert_eq!(folder_paths, &worktree_folder_paths);
                        assert_eq!(
                            project_group_key.path_list(),
                            &PathList::new(&[PathBuf::from("/project")])
                        );
                    }
                    ThreadEntryWorkspace::Open(_) => {
                        panic!("closed terminal row should retain its linked worktree target")
                    }
                }
            }
            ThreadSwitcherEntry::Thread(_) => {
                panic!("terminal row should be selected by default")
            }
        }
    });
}

#[gpui::test]
async fn test_archive_selected_terminal_archives_closed_linked_worktree(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        assert!(
            agent_ui::draft_prompt_store::read(empty_draft_id, cx).is_none(),
            "empty draft should not have persisted prompt content"
        );
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        match &sidebar.contents.entries[terminal_index] {
            ListEntry::Terminal(terminal) => match &terminal.workspace {
                ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                    assert_eq!(folder_paths, &worktree_folder_paths);
                }
                ThreadEntryWorkspace::Open(_) => {
                    panic!("linked worktree terminal should start closed")
                }
            },
            _ => panic!("expected terminal row"),
        }
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after closing from the sidebar"
    );
    let empty_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_none()
    });
    assert!(
        empty_draft_metadata_deleted,
        "empty draft metadata should be deleted before archiving the linked worktree"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "temporary linked worktree workspace should be removed after archiving"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "closing a closed linked worktree terminal should leave only the main workspace"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after closing its terminal"
    );
}

#[gpui::test]
async fn test_archive_selected_thread_archives_closed_linked_worktree(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        assert!(
            agent_ui::draft_prompt_store::read(empty_draft_id, cx).is_none(),
            "empty draft should not have persisted prompt content"
        );
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let thread_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(thread) if thread.metadata.session_id.as_ref() == Some(&worktree_session_id)))
            .expect("worktree thread should be visible in sidebar")
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        match &sidebar.contents.entries[thread_index] {
            ListEntry::Thread(thread) => match &thread.workspace {
                ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                    assert_eq!(folder_paths, &worktree_folder_paths);
                }
                ThreadEntryWorkspace::Open(_) => {
                    panic!("linked worktree thread should start closed")
                }
            },
            _ => panic!("expected thread row"),
        }
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(thread_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let thread_archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(
        thread_archived,
        Some(true),
        "thread metadata should remain archived after worktree archival"
    );
    let empty_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_none()
    });
    assert!(
        empty_draft_metadata_deleted,
        "empty draft metadata should be deleted before archiving the linked worktree"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "temporary linked worktree workspace should be removed after archiving"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "archiving a closed linked worktree thread should leave only the main workspace"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after archiving its thread"
    );
}

#[gpui::test]
async fn test_archive_selected_thread_deletes_empty_draft_when_linked_worktree_has_no_archive_root(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    fs.insert_branches(Path::new("/project/.git"), &["main", "feature-a"]);
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/external-worktree"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_session_id = acp::SessionId::new(Arc::from("external-worktree-thread"));
    let worktree_folder_paths = PathList::new(&[PathBuf::from("/external-worktree")]);
    save_thread_metadata_with_main_paths(
        "external-worktree-thread",
        "External Worktree Thread",
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let thread_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(thread) if thread.metadata.session_id.as_ref() == Some(&worktree_session_id)))
            .expect("worktree thread should be visible in sidebar")
    });
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(thread_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let thread_archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(
        thread_archived,
        Some(true),
        "thread metadata should remain archived after workspace removal"
    );
    let empty_draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_none()
    });
    assert!(
        empty_draft_metadata_deleted,
        "empty draft metadata should be deleted when removing the linked worktree workspace"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should be removed after archiving its last thread"
    );
    assert!(
        fs.is_dir(Path::new("/external-worktree")).await,
        "external linked worktree directory should remain on disk when no archive root is produced"
    );
}

#[gpui::test]
async fn test_archive_selected_thread_closes_selected_agent_panel_terminal(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(terminal_id).is_none(),
            "terminal metadata should be deleted when closing from the sidebar"
        );
    });
}

#[gpui::test]
async fn test_closing_active_agent_panel_terminal_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    let (server_metadata, server_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal)
                    if terminal.metadata.terminal_id == server_terminal_id =>
                {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("server terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&server_metadata, &server_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Build"]);
}

#[gpui::test]
async fn test_parallel_threads_shown_with_live_status(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread A and keep it generating.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection.clone(), cx);
    send_message(&panel, cx);

    let session_id_a = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_a, &project, cx).await;

    cx.update(|_, cx| {
        connection.send_update(
            session_id_a.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("working...".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // Open thread B (idle, default response) — thread A goes to background.
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id_b = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_b, &project, cx).await;

    cx.run_until_parked();

    // Both threads share a title and timestamp; sort for determinism. Both
    // are open (each has a tab), so both sit in Active and neither is
    // repeated in All Threads.
    let mut entries = visible_entries_as_strings(&sidebar, cx);
    entries.sort();
    assert_eq!(
        entries,
        vec![
            //
            "  Hello *",
            "  Hello * (running)",
            // Two threads in one workspace group under its header.
        ]
    );
}

#[gpui::test]
async fn test_subagent_permission_request_marks_parent_sidebar_thread_waiting(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new().with_supports_load_session(true);
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let parent_session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&parent_session_id, &project, cx).await;

    let subagent_session_id = acp::SessionId::new("subagent-session");
    cx.update(|_, cx| {
        let parent_thread = panel.read(cx).active_agent_thread(cx).unwrap();
        parent_thread.update(cx, |thread: &mut AcpThread, cx| {
            thread.subagent_spawned(subagent_session_id.clone(), cx);
        });
    });
    cx.run_until_parked();

    let subagent_thread = panel.read_with(cx, |panel, cx| {
        panel
            .active_conversation_view()
            .and_then(|conversation| conversation.read(cx).thread_view(&subagent_session_id))
            .map(|thread_view| thread_view.read(cx).thread.clone())
            .expect("Expected subagent thread to be loaded into the conversation")
    });
    request_test_tool_authorization(&subagent_thread, "subagent-tool-call", "allow-subagent", cx);

    let parent_status = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&parent_session_id) =>
                {
                    Some(thread.status)
                }
                _ => None,
            })
            .expect("Expected parent thread entry in sidebar")
    });

    assert_eq!(parent_status, AgentThreadStatus::WaitingForConfirmation);
}

#[gpui::test]
async fn test_background_thread_completion_triggers_notification(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread on workspace A and keep it generating.
    let connection_a = StubAgentConnection::new();
    open_thread_with_connection(&panel_a, connection_a.clone(), cx);
    send_message(&panel_a, cx);

    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    cx.update(|_, cx| {
        connection_a.send_update(
            session_id_a.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("chunk".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // Add a second workspace and activate it (making workspace A the background).
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    let project_b = project::Project::test(fs, [], cx).await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    // Thread A is still running; no notification yet. It's open, so its one
    // row is in Active.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Hello * (running)"]
    );

    // Complete thread A's turn (transition Running → Completed).
    connection_a.end_turn(session_id_a.clone(), acp::StopReason::EndTurn);
    cx.run_until_parked();

    // The completed background thread shows a notification indicator, still
    // as the one Active row: finishing a turn does not close it.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Hello * (!)"]
    );
}

fn type_in_search(sidebar: &Entity<Sidebar>, query: &str, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        window.focus(&sidebar.filter_editor.focus_handle(cx), cx);
        sidebar.filter_editor.update(cx, |editor, cx| {
            editor.set_text(query, window, cx);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_search_narrows_visible_threads_to_matches(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in project panel", 3),
        ("t-2", "Add inline diff view", 2),
        ("t-3", "Refactor settings module", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        );
    }
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix crash in project panel",
            "  Add inline diff view",
            "  Refactor settings module",
        ]
    );

    // User types "diff" in the search box — only the matching thread remains,
    // with its workspace header preserved for context.
    type_in_search(&sidebar, "diff", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Add inline diff view  <== selected",
        ]
    );

    // User changes query to something with no matches — list is empty.
    type_in_search(&sidebar, "nonexistent", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_search_matches_regardless_of_case(cx: &mut TestAppContext) {
    // Scenario: A user remembers a thread title but not the exact casing.
    // Search should match case-insensitively so they can still find it.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix Crash In Project Panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Lowercase query matches mixed-case title.
    type_in_search(&sidebar, "fix crash", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix Crash In Project Panel  <== selected",
        ]
    );

    // Uppercase query also matches the same title.
    type_in_search(&sidebar, "FIX CRASH", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix Crash In Project Panel  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_escape_from_search_focuses_first_thread(cx: &mut TestAppContext) {
    // Scenario: A user searches, finds what they need, then presses Escape
    // in the search field to hand keyboard control back to the thread list.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [("t-1", "Alpha thread", 2), ("t-2", "Beta thread", 1)] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    // Confirm the full list is showing.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Alpha thread",
            "  Beta thread",
        ]
    );

    // User types a search query to filter down.
    focus_sidebar(&sidebar, cx);
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Alpha thread  <== selected",
        ]
    );

    // First Escape clears the search text, restoring the full list.
    // Focus stays on the filter editor.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Alpha thread",
            "  Beta thread",
        ]
    );
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.filter_editor.read(cx).is_focused(window));
        assert!(!sidebar.focus_handle.is_focused(window));
    });

    // Second Escape moves focus from the empty search field to the thread list.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert_eq!(sidebar.selection, Some(3));
        assert!(sidebar.focus_handle.is_focused(window));
        assert!(!sidebar.filter_editor.read(cx).is_focused(window));
    });
}

#[gpui::test]
async fn test_search_only_shows_workspace_headers_with_matches(cx: &mut TestAppContext) {
    let project_a = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a second workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    // History clusters by worktree, newest cluster first, and each cluster is
    // itself newest-first.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout",
            "  Fix typo in README",
            "  Fix bug in sidebar",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches a thread in each workspace.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout  <== selected",
            "  Fix bug in sidebar",
        ]
    );

    // "typo" only matches a thread in the second workspace.
    type_in_search(&sidebar, "typo", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix typo in README  <== selected",
        ]
    );

    // "project-a" matches the first workspace's folder name, surfacing its
    // threads.
    type_in_search(&sidebar, "project-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
async fn test_search_matches_workspace_name(cx: &mut TestAppContext) {
    let project_a = init_test_project("/alpha-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a second workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    // "alpha" matches the workspace name "alpha-project" but no thread titles.
    // The workspace header should appear with all child threads included.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches thread titles in both workspaces; the merged list
    // interleaves them by recency.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout  <== selected",
            "  Fix bug in sidebar",
        ]
    );

    // A query that matches thread titles in both workspaces. In the merged
    // model all matching rows show regardless of which workspace owns them.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix bug in sidebar  <== selected",
            "  Fix typo in README",
        ]
    );

    // A query that matches a workspace name AND a thread in that same workspace.
    // Both the header (highlighted) and all child threads should appear.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // Now search for something that matches only a workspace name when there
    // are also threads with matching titles — the non-matching workspace's
    // threads should still appear if their titles match.
    type_in_search(&sidebar, "alp", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
// Rewritten for the merged history model: there are no collapsed groups
// anymore; search simply matches against all rows.
async fn test_search_finds_threads(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Important thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // User types a search; the thread is matched by title.
    type_in_search(&sidebar, "important", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Important thread  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_search_then_keyboard_navigate_and_confirm(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in panel", 3),
        ("t-2", "Fix lint warnings", 2),
        ("t-3", "Add new feature", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // User types "fix" — two threads match.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );

    // Selection starts on the first matching thread. User presses
    // SelectNext to move to the second match.
    cx.dispatch_action(SelectNext);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix crash in panel",
            "  Fix lint warnings  <== selected",
        ]
    );

    // User can also jump back with SelectPrevious.
    cx.dispatch_action(SelectPrevious);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_activates_workspace(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let (workspace_0, workspace_1) = multi_workspace.read_with(cx, |mw, _| {
        (
            mw.workspaces().next().unwrap().clone(),
            mw.workspaces().nth(1).unwrap().clone(),
        )
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Historical Thread",
        ]
    );

    // Switch to workspace 1 so we can verify the confirm switches back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_1
    );

    // Confirm on the historical (non-live) thread, found by shape rather than
    // a hard-coded index (the section headers above it are not the point).
    // Before a previous fix, the workspace field was Option<usize> and
    // historical threads had None, so activate_thread early-returned
    // without switching the workspace.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let thread_ix = sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_)))
            .expect("the historical thread should be listed");
        sidebar.selection = Some(thread_ix);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_0
    );
}

#[gpui::test]
async fn test_closing_a_thread_clears_its_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .expect("thread metadata should exist")
            .thread_id
    });

    // Select the open thread's row, the way clicking or arrowing to it does.
    sidebar.update(cx, |sidebar, _cx| {
        sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
            matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id)
        });
        assert!(sidebar.selection.is_some(), "the open thread is listed");
    });

    panel.update_in(cx, |panel, window, cx| {
        panel.test_close_thread_tab(thread_id, window, cx);
    });
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar.selection, None,
            "closing the thread leaves nothing selected"
        );
    });
}

// Clicking a thread in the sidebar whose tab has been closed must reopen it.
// The stale-active_entry fast path used to trust that active_entry still
// pointed at an open tab and early-return without loading anything, so the
// thread never reopened.
#[gpui::test]
async fn test_reopen_closed_thread_from_history(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a real thread and persist its metadata so it appears in history.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let metadata = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    let thread_id = metadata.thread_id;

    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.open_thread_tab_ids(cx).contains(&thread_id),
            "the thread should start open as a tab"
        );
    });

    // Close its tab: the metadata stays on disk but no tab hosts the thread,
    // the same shape the sidebar sees right after a session restore.
    panel.update_in(cx, |panel, window, cx| {
        panel.test_close_thread_tab(thread_id, window, cx);
    });
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.open_thread_tab_ids(cx).contains(&thread_id),
            "the tab should be closed"
        );
    });

    // Force the stale precondition the fix targets: active_entry still points
    // at the (now tab-less) thread. This is the shape of a restored session
    // (active_entry persisted, no ConversationView rehydrated) or a
    // stuck-pending activation, which the auto-created draft otherwise papers
    // over in a single-window test.
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    sidebar.update(cx, |sidebar, _cx| {
        sidebar.set_stale_thread_active_entry_for_test(
            metadata.thread_id,
            metadata.session_id.clone(),
            workspace.clone(),
        );
    });

    // Click the now-closed thread in the sidebar: it must reopen as a tab.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(metadata.clone(), &workspace, false, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.open_thread_tab_ids(cx).contains(&thread_id),
            "clicking a closed historical thread should reopen its tab"
        );
        assert_eq!(
            panel.active_thread_id(cx),
            Some(thread_id),
            "the reopened thread should be the active one"
        );
    });
}

// Active-section rows are open as tabs and get an X that closes the tab (the
// same effect as closing it in the thread pane). Archived rows are never in the
// Active section, so they expose no close affordance.
#[gpui::test]
async fn test_close_tab_from_active_row(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;

    // A separate archived thread, which must never sit in the Active section.
    let archived_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_test_thread_metadata(&archived_session_id, &project, cx).await;
    let archived_thread_id = cx.update(|_window, cx| {
        let thread_id = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&archived_session_id)
            .expect("archived thread metadata should exist")
            .thread_id;
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.archive(thread_id, None, cx));
        thread_id
    });
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let metadata = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    let thread_id = metadata.thread_id;
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let row_section = |sidebar: &Entity<Sidebar>,
                       target: ThreadId,
                       cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            let ix = sidebar
                .contents
                .entries
                .iter()
                .position(|entry| {
                    matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == target)
                })
                .expect("the thread row should be present");
            sidebar.section_of_entry(ix)
        })
    };

    assert_eq!(
        row_section(&sidebar, thread_id, cx),
        Some(SidebarSection::OpenInZed),
        "an open thread's row should be in the Active section"
    );
    assert_ne!(
        row_section(&sidebar, archived_thread_id, cx),
        Some(SidebarSection::OpenInZed),
        "an archived thread's row is never in the Active section, so it has no close affordance"
    );
    panel.read_with(cx, |panel, cx| {
        assert!(panel.open_thread_tab_ids(cx).contains(&thread_id));
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_thread_tab(thread_id, &workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.open_thread_tab_ids(cx).contains(&thread_id),
            "closing the tab from the row should close the tab"
        );
    });
}

#[gpui::test]
async fn test_confirm_on_historical_thread_preserves_historical_timestamp_and_order(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let newer_session_id = acp::SessionId::new(Arc::from("newer-historical-thread"));
    let newer_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 2, 0, 0, 0).unwrap();
    save_thread_metadata(
        newer_session_id,
        Some("Newer Historical Thread".into()),
        newer_timestamp,
        Some(newer_timestamp),
        None,
        &project,
        cx,
    );

    let older_session_id = acp::SessionId::new(Arc::from("older-historical-thread"));
    let older_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap();
    save_thread_metadata(
        older_session_id.clone(),
        Some("Older Historical Thread".into()),
        older_timestamp,
        Some(older_timestamp),
        None,
        &project,
        cx,
    );

    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let historical_entries_before: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_before,
        vec![
            "  Newer Historical Thread".to_string(),
            "  Older Historical Thread".to_string(),
        ],
        "expected the sidebar to sort historical threads by their saved timestamp before activation"
    );

    let older_entry_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&older_session_id))
            })
            .expect("expected Older Historical Thread to appear in the sidebar")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(older_entry_index);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    let older_metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&older_session_id)
            .cloned()
            .expect("expected metadata for Older Historical Thread after activation")
    });
    assert_eq!(
        older_metadata.created_at,
        Some(older_timestamp),
        "activating a historical thread should not rewrite its saved created_at timestamp"
    );

    // Activation opens a conversation view for just this thread, which puts
    // it — and only it — in the Active section. Its sibling never had a tab
    // opened for it, so it does not follow along — it stays behind as All
    // Threads' only row. The confirmed thread leaves All Threads rather than
    // appearing there a second time. What this test still guards is that the
    // activation did not rewrite the saved created_at timestamp (asserted
    // above), which is what would move a row once it is closed again.
    let historical_entries_after: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_after,
        vec![
            // Selection follows the row's identity through the reshuffle: it
            // stays on the thread the user confirmed, which is now the one
            // row that thread has.
            "  Older Historical Thread  <== selected".to_string(),
            "  Newer Historical Thread".to_string(),
        ],
        "confirming a thread moves only that thread to Active; its sibling stays behind in All Threads"
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_in_new_project_group_opens_real_thread(
    cx: &mut TestAppContext,
) {
    use workspace::ProjectGroup;

    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let project_b_key = project_b.read_with(cx, |project, cx| project.project_group_key(cx));
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(ProjectGroup {
            key: project_b_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    let session_id = acp::SessionId::new(Arc::from("historical-new-project-group"));
    save_thread_metadata(
        session_id.clone(),
        Some("Historical Thread in New Group".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_b,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries_before,
        vec!["  Historical Thread in New Group",],
        "expected the closed project group to show the historical thread before first open"
    );

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start without an open workspace for the new project group"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        let thread_ix = sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_)))
            .expect("historical thread row should exist");
        sidebar.selection = Some(thread_ix);
        sidebar.confirm(&Confirm, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "confirming the historical thread should open a workspace for the new project group"
    );

    let workspace_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == project_b_key.path_list().clone()
            })
            .cloned()
            .expect("expected workspace for project-b after opening the historical thread")
    });

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "opening the historical thread should activate the new project's workspace"
    );

    let panel = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected first-open activation to bootstrap the agent panel")
    });

    let expected_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("metadata should still map session id to thread id")
    });

    assert_eq!(
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(expected_thread_id),
        "expected the agent panel to activate the real historical thread rather than a draft"
    );

    let entries_after = visible_entries_as_strings(&sidebar, cx);
    let matching_rows: Vec<_> = entries_after
        .iter()
        .filter(|entry| entry.contains("Historical Thread in New Group") || entry.contains("Draft"))
        .cloned()
        .collect();
    // Opening the thread makes it active, so it is one row in Active — the
    // real thread, and no draft.
    assert_eq!(
        matching_rows.len(),
        1,
        "expected the real thread to appear as a single Active row after first open into a new project group, got entries: {entries_after:?}"
    );
    assert!(
        matching_rows
            .iter()
            .all(|row| row.contains("Historical Thread in New Group")),
        "expected both surviving rows to be the real historical thread, got entries: {entries_after:?}"
    );
    assert!(
        matching_rows.iter().all(|row| !row.contains("Draft")),
        "expected no draft row after first open into a new project group, got entries: {entries_after:?}"
    );
}

#[gpui::test]
async fn test_click_clears_selection_and_focus_in_restores_it(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("t-1")),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    save_thread_metadata(
        acp::SessionId::new(Arc::from("t-2")),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Thread A",
            "  Thread B",
        ]
    );

    // Keyboard confirm preserves selection.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(1);
        sidebar.confirm(&Confirm, window, cx);
    });
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.selection),
        Some(1)
    );

    // Click handlers clear selection to None so no highlight lingers
    // after a click regardless of focus state. The hover style provides
    // visual feedback during mouse interaction instead.
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = None;
    });
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.selection), None);

    // When the user tabs back into the sidebar, focus_in no longer
    // restores selection — it stays None.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.focus_in(window, cx);
    });
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.selection), None);
}

#[gpui::test]
async fn test_thread_title_update_propagates_to_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Open, so its one row is in Active.
    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Hello *"]);

    // Simulate the agent generating a title. The notification chain is:
    // AcpThread::set_title emits TitleUpdated →
    // ConnectionView::handle_thread_event calls cx.notify() →
    // AgentPanel observer fires and emits AgentPanelEvent →
    // Sidebar subscription calls update_entries / rebuild_contents.
    //
    // Before the fix, handle_thread_event did NOT call cx.notify() for
    // TitleUpdated, so the AgentPanel observer never fired and the
    // sidebar kept showing the old title.
    let thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    thread.update(cx, |thread, cx| {
        thread
            .set_title("Friendly Greeting with AI".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Friendly Greeting with AI *"]
    );
}

#[gpui::test]
async fn test_typing_a_rename_does_not_end_it(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id, title) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((
                    ix,
                    thread.metadata.thread_id,
                    thread.metadata.display_title(),
                )),
                ListEntry::SectionHeader(_)
                | ListEntry::WorkspaceHeader(_)
                | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_entry(
            entry_ix,
            RenameTarget::Thread(thread_id),
            title,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    // One character, as though typed. The rename is still in progress: the
    // editor keeps the focus, and nothing has been written yet.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text("F", window, cx);
        });
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        assert_eq!(
            sidebar.rename_target,
            Some(RenameTarget::Thread(thread_id)),
            "a keystroke must not end the rename"
        );
        assert!(
            sidebar.rename_editor.focus_handle(cx).is_focused(window),
            "the rename editor must keep the focus while it is being typed into"
        );
    });
    let written = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .and_then(|metadata| metadata.title_override.clone())
    });
    assert_eq!(written, None, "the title is written when the rename ends");

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();

    let written = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .and_then(|metadata| metadata.title_override.clone())
    });
    assert_eq!(written.as_deref(), Some("F"));
}

#[gpui::test]
async fn test_rename_thread_from_sidebar_updates_title_override(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id, title) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((
                    ix,
                    thread.metadata.thread_id,
                    thread.metadata.display_title(),
                )),
                ListEntry::SectionHeader(_)
                | ListEntry::WorkspaceHeader(_)
                | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    let renamed_title = "abcdefghijklmnopqrstuvwxyé renamed";
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_entry(entry_ix, RenameTarget::Thread(thread_id), title, window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
    // Open, so it is one row, in Active, and it is the selected one.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  abcdefghijklmnopqrstuvwxyé renamed *  <== selected"]
    );

    let active_thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    assert_eq!(
        active_thread.read_with(cx, |thread, _| thread.title()),
        Some(renamed_title.into())
    );
    let active_thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
    let title_editor_text =
        active_thread_view.read_with(cx, |view, cx| view.title_editor.read(cx).text(cx));
    assert_eq!(title_editor_text, renamed_title);

    active_thread.update(cx, |thread, cx| {
        thread
            .set_title("abcdefghijklmnopqrstuvwxyz0".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  abcdefghijklmnopqrstuvwxyé renamed *  <== selected"]
    );

    type_in_search(&sidebar, "0", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );

    type_in_search(&sidebar, "é", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  abcdefghijklmnopqrstuvwxyé renamed *  <== selected"]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        let thread = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread),
                ListEntry::SectionHeader(_)
                | ListEntry::WorkspaceHeader(_)
                | ListEntry::Terminal(_) => None,
            })
            .expect("renamed thread should match the search");
        let title = thread.metadata.display_title();
        assert!(
            thread
                .highlight_positions
                .iter()
                .all(|position| { title.is_char_boundary(*position) })
        );
    });
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_selected_thread(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((ix, thread.metadata.thread_id)),
                ListEntry::SectionHeader(_)
                | ListEntry::WorkspaceHeader(_)
                | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed via action";
    enter_renamed_title(&sidebar, RenameTarget::Thread(thread_id), renamed_title, cx);

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    let entry_ix = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Terminal(terminal)
                        if terminal.metadata.terminal_id == terminal_id
                )
            })
            .expect("sidebar should have a terminal entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed Terminal";
    enter_renamed_title(
        &sidebar,
        RenameTarget::Terminal(terminal_id),
        renamed_title,
        cx,
    );

    panel.read_with(cx, |panel, cx| {
        let terminal = panel
            .terminals(cx)
            .into_iter()
            .find(|terminal| terminal.id == terminal_id)
            .expect("terminal should remain open after renaming");
        assert_eq!(terminal.custom_title.as_deref(), Some(renamed_title));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let metadata = TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .cloned()
            .expect("renamed terminal metadata should exist");
        assert_eq!(metadata.custom_title.as_deref(), Some(renamed_title));
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Renamed Terminal  <== selected"]
    );
}

#[gpui::test]
async fn test_focused_thread_tracks_user_intent(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Save a thread so it appears in the list.
    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, connection_a, cx);
    send_message(&panel_a, cx);
    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    // Add a second workspace with its own agent panel.
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let workspace_a =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().next().unwrap().clone());

    // ── 1. Initial state: focused thread derived from active panel ─────
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "The active panel's thread should be focused on startup",
        );
    });

    let thread_metadata_a = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_a)
            .cloned()
            .expect("session_id_a should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_a, &workspace_a, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "After clicking a thread, it should be the focused thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The clicked thread should be present in the entries"
        );
    });

    workspace_a.read_with(cx, |workspace, cx| {
        assert!(
            workspace.panel::<AgentPanel>(cx).is_some(),
            "Agent panel should exist"
        );
        // Threads live as tabs in the agent panel's own pane; clicking a
        // thread opens the dock and adds a tab to the panel's thread pane.
        let dock = workspace.left_dock().read(cx);
        assert!(
            dock.is_open(),
            "Clicking a thread should open the agent panel dock"
        );
        let panel = workspace
            .panel::<AgentPanel>(cx)
            .expect("agent panel should exist");
        assert!(
            panel
                .read(cx)
                .thread_pane()
                .read(cx)
                .items_of_type::<agent_ui::thread_tab::ThreadTab>()
                .next()
                .is_some(),
            "Clicking a thread should open it as a tab in the panel's thread pane"
        );
    });

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Thread B".into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b, cx);
    send_message(&panel_b, cx);
    let session_id_b = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b, &project_b, cx).await;
    cx.run_until_parked();

    // Workspace A is currently active. Click a thread in workspace B,
    // which also triggers a workspace switch.
    let thread_metadata_b = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_b)
            .cloned()
            .expect("session_id_b should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_b, &workspace_b, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b,
            "Clicking a thread in another workspace should focus that thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b),
            "The cross-workspace thread should be present in the entries"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The seeded thread should be present in the entries"
        );
    });

    let connection_b2 = StubAgentConnection::new();
    connection_b2.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new(DEFAULT_THREAD_TITLE.into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b2, cx);
    send_message(&panel_b, cx);
    let session_id_b2 = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b2, &project_b, cx).await;
    cx.run_until_parked();

    // Panel B is not the active workspace's panel (workspace A is
    // active), so opening a thread there should not change focused_thread.
    // This prevents running threads in background workspaces from causing
    // the selection highlight to jump around.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Opening a thread in a non-active panel should not change focused_thread",
        );
    });

    workspace_b.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Defocusing the sidebar should not change focused_thread",
        );
    });

    // Switching workspaces via the multi_workspace (simulates clicking
    // a workspace header) should clear focused_thread.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().find(|w| *w == &workspace_b).cloned();
        if let Some(workspace) = workspace {
            mw.activate(workspace, None, window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The seeded thread should be present in the entries"
        );
    });

    // ── 8. Focusing the agent panel thread keeps focused_thread ────
    // Workspace B still has session_id_b2 loaded in the agent panel.
    // Clicking into the thread (simulated by focusing its view) should
    // keep focused_thread since it was already seeded on workspace switch.
    panel_b.update_in(cx, |panel, window, cx| {
        if let Some(thread_view) = panel.active_conversation_view() {
            thread_view.read(cx).focus_handle(cx).focus(window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Focusing the agent panel thread should set focused_thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The focused thread should be present in the entries"
        );
    });
}

#[gpui::test]
async fn test_new_thread_button_works_after_adding_folder(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let fs = cx.update(|cx| <dyn fs::Fs>::global(cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Start a thread and send a message so it has history.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Verify the thread appears in the sidebar, as one Active row: it is
    // open, which is what takes it out of All Threads.
    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Hello *"]);

    // The "New Thread" button should NOT be in "active/draft" state
    // because the panel has a thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "Panel has a thread with messages, so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Now add a second folder to the workspace, changing the path_list.
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The workspace path_list is now [project-a, project-b]. The active
    // thread's metadata was re-saved with the new paths by the agent panel's
    // project subscription. The old [project-a] key is replaced by the new
    // key since no other workspace claims it.
    let entries = visible_entries_as_strings(&sidebar, cx);
    // After adding a worktree, the thread migrates to the new group key.
    // A reconciliation draft may appear during the transition.
    assert!(
        entries.contains(&"  Hello *".to_string()),
        "thread should still be present after adding folder: {entries:?}"
    );

    // The "New Thread" button must still be clickable (not stuck in
    // "active/draft" state). Verify that `active_thread_is_draft` is
    // false — the panel still has the old thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "After adding a folder the panel still has a thread with messages, \
                 so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Actually click "New Thread" by calling create_new_thread and
    // verify a new draft is created.
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace, window, cx);
    });
    cx.run_until_parked();

    // After creating a new thread, the panel should now be in draft
    // state (no messages on the new thread).
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "After creating a new thread active_entry should be Draft",
        );
    });
}

#[gpui::test]
async fn test_draft_title_updates_from_editor_text(cx: &mut TestAppContext) {
    // When the user types into a draft, the draft entry's title in the
    // sidebar should reflect the editor's text, both while the draft's
    // `ConversationView` is still open as a tab (source: live message
    // editor) and after its tab is closed (source: kvp draft prompt
    // store, the same path used when drafts are restored from disk).
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an ephemeral draft via a stub connection so the conversation
    // view reaches Connected synchronously and the panel's draft_thread
    // pointer is populated.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Type into the (active) draft's message editor. The helper drains the
    // kvp-write debounce, so by the time it returns the prompt is on disk
    // — important for Phase 2 below, which exercises the kvp fallback.
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    // Pressing Cmd-N while the draft has content leaves it open as a
    // background tab (there is no parked cache anymore) and creates a
    // fresh ephemeral draft.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let draft_title = |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread)
                        if thread.draft.is_some() && thread.metadata.thread_id == draft_id =>
                    {
                        Some(thread.metadata.display_title())
                    }
                    _ => None,
                })
                .expect("typed draft entry should be present")
        })
    };

    // Phase 1: the ConversationView is still open as a background tab;
    // the title comes from its live message editor.
    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "typed draft title should match its editor text while loaded"
    );
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.open_thread_tab_ids(cx).contains(&draft_id),
            "typed draft should stay open as a tab"
        );
    });

    // Phase 2: close the draft's tab (without deleting metadata),
    // mirroring the state the sidebar sees immediately after a process
    // restart: the metadata row and the kvp draft prompt are on disk, but
    // no ConversationView has been rehydrated yet.
    panel.update_in(cx, |panel, window, cx| {
        panel.test_close_thread_tab(draft_id, window, cx);
    });
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "typed draft title should still come from the kvp draft prompt store \
         even after its tab is closed"
    );
}

#[gpui::test]
async fn test_thread_switcher_includes_parked_draft(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-existing")),
        Some("Existing Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id)
        }));
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(switcher.read(cx).entries().iter().any(|entry| {
            matches!(entry.thread_id(), Some(thread_id) if thread_id == draft_id)
        }));
    });
}

#[gpui::test]
async fn test_plus_button_reuses_empty_draft(cx: &mut TestAppContext) {
    // Clicking `+` when an empty draft is already active should focus it
    // instead of creating and parking a new one.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an initial draft against a stub so it connects synchronously.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();

    let first_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should be active after open_draft_with_connection")
    });

    // Cmd-N with an empty draft should reuse it.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should still be active after Cmd-N")
    });
    assert_eq!(
        first_id, second_id,
        "an empty draft should be reused, not replaced"
    );
    // The active empty draft is surfaced in the sidebar as a "New {agent}
    // Thread" placeholder that mirrors the panel: one row, in Active, since
    // that is where the thread the panel is showing lives.
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        1,
        "active empty draft should appear once, in Active"
    );
    for row in &draft_rows {
        assert_eq!(
            row.draft,
            Some(DraftKind::Empty),
            "the row should be the empty-draft placeholder"
        );
        assert_eq!(row.metadata.thread_id, first_id);
    }
}

#[gpui::test]
async fn test_plus_button_parks_nonempty_draft(cx: &mut TestAppContext) {
    // Clicking `+` while the current draft has content should park the
    // current draft (surface it as a sidebar row) and create a new empty
    // draft as active.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft via a stub so the ConversationView reaches Connected and
    // we can type into its editor.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let first_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "something the user typed", cx);

    // Cmd-N parks the first draft and creates a new empty draft.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    assert_ne!(
        first_id, second_id,
        "non-empty draft should be parked and a fresh draft activated"
    );

    // Both drafts now appear as sidebar rows: the parked one with its
    // editor-derived title (real user state), and the newly-created empty
    // draft as a "New {agent} Thread" placeholder. The placeholder mirrors
    // the panel's current view; the parked row preserves typed content.
    // Parking keeps the first draft as a background tab rather than closing
    // it, so both drafts are open, which puts both in Active and neither in
    // All Threads: two rows, not four.
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        2,
        "expected two draft rows (parked + new empty placeholder), each shown once, in Active, got {:?}",
        draft_rows
            .iter()
            .map(|t| t.metadata.display_title())
            .collect::<Vec<_>>()
    );
    let parked: Vec<_> = draft_rows
        .iter()
        .filter(|t| t.metadata.thread_id == first_id)
        .collect();
    assert_eq!(
        parked.len(),
        1,
        "the parked draft should appear once, in Active"
    );
    for row in &parked {
        assert_eq!(
            row.draft,
            Some(DraftKind::WithContent),
            "the parked draft has user content and is not an empty placeholder"
        );
        assert_eq!(
            row.metadata.display_title().as_ref(),
            "something the user typed"
        );
    }
    let new_empty: Vec<_> = draft_rows
        .iter()
        .filter(|t| t.metadata.thread_id == second_id)
        .collect();
    assert_eq!(
        new_empty.len(),
        1,
        "the new empty draft should appear once, in Active"
    );
    for row in &new_empty {
        assert_eq!(
            row.draft,
            Some(DraftKind::Empty),
            "the freshly-created draft should be an empty placeholder"
        );
    }

    // Reproduce the real-world inversion deterministically: parking
    // re-saves the filled draft, which can leave its display time newer
    // than the brand-new empty draft's. Force that here by pushing the
    // parked draft's `updated_at` into the future.
    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let mut parked_meta = store
            .read(cx)
            .entry(first_id)
            .expect("parked draft metadata should exist")
            .clone();
        parked_meta.interacted_at = None;
        parked_meta.updated_at = Utc::now() + chrono::Duration::hours(1);
        store.update(cx, |store, cx| store.save(parked_meta, cx));
    });
    cx.run_until_parked();

    // The empty-draft placeholder must still sort ABOVE the parked draft
    // despite the parked draft's newer timestamp — it's pinned to the top.
    let (empty_ix, parked_ix) = sidebar.read_with(cx, |sidebar, _| {
        let position = |id: ThreadId| {
            sidebar.contents.entries.iter().position(
                |entry| matches!(entry, ListEntry::Thread(t) if t.metadata.thread_id == id),
            )
        };
        (
            position(second_id).expect("empty draft row should be present"),
            position(first_id).expect("parked draft row should be present"),
        )
    });
    assert!(
        empty_ix < parked_ix,
        "the new empty draft (ix {empty_ix}) should sort above the parked filled draft (ix {parked_ix})"
    );
}

#[gpui::test]
async fn test_remove_draft_deletes_metadata_row(cx: &mut TestAppContext) {
    // The close-draft button deletes the metadata row and the kvp draft prompt,
    // and the draft disappears from the sidebar.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft with content, park it by pressing Cmd-N.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "will be discarded", cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    // The parked draft is visible.
    let draft_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
            .expect("parked draft should be visible before removal")
    });

    // Select the parked draft and dispatch the action a real user would
    // (Shift-Backspace, bound to `ArchiveSelectedThread`). The handler
    // routes to `remove_draft` for parked drafts.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(draft_index);
        sidebar.archive_selected_thread(&agent_ui::ArchiveSelectedThread, window, cx);
    });
    cx.run_until_parked();

    // Metadata row and persisted draft prompt should both be gone.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(draft_id).is_none(),
            "removed draft metadata should be deleted"
        );
        assert!(
            agent_ui::draft_prompt_store::read(draft_id, cx).is_none(),
            "removed draft's kvp prompt should also be deleted"
        );
    });
    // And the row should be gone from the sidebar.
    let still_visible = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .any(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
    });
    assert!(
        !still_visible,
        "removed draft should no longer appear in the sidebar"
    );
}

#[gpui::test]
async fn test_sending_message_from_draft_promotes_in_place(cx: &mut TestAppContext) {
    // Sending a message from a draft should keep the same ThreadId, set the
    // session_id on its metadata row, and clear the `draft_thread` pointer.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (_sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("ok".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Before sending: draft metadata row exists with session_id = None.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("draft metadata row");
        assert!(entry.is_draft(), "expected draft row before sending");
    });

    send_message(&panel, cx);
    cx.run_until_parked();

    // After sending: draft_thread is cleared, metadata row has a session_id.
    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.active_thread_is_draft(cx),
            "should no longer be a draft after send"
        );
        assert!(
            panel.ephemeral_draft_thread_id(cx).is_none(),
            "ephemeral draft pointer should be cleared after promotion"
        );
        assert_eq!(
            panel.active_thread_id(cx),
            Some(draft_id),
            "ThreadId stays the same across promotion"
        );
    });
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("promoted metadata row");
        assert!(
            !entry.is_draft(),
            "promoted thread should have a session_id"
        );
    });
}

#[gpui::test]
async fn test_cmd_n_shows_new_thread_entry(cx: &mut TestAppContext) {
    // When the user presses Cmd-N (NewThread action) while viewing a
    // non-empty thread, the panel should switch to the draft thread and
    // the sidebar should surface a "New {agent} Thread" placeholder row
    // that mirrors the active empty draft.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create a non-empty thread (has messages).
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Open and unarchived, so its one row is in Active.
    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Hello *"]);

    // Simulate cmd-n: it always goes to the new-thread slot, creating the
    // draft row, even with a live thread tab open.
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        // Switching to the draft leaves the still-running thread with an
        // unseen-activity marker. Both the draft and the still-open thread
        // are open, so both sit in Active and neither is repeated in All
        // Threads (draft pinned to the top). The draft defaults to a NEW
        // worktree, so it does not group under this workspace.
        vec!["  New thread", "  Hello * (!)"],
        "Cmd-N should show a fresh draft row above the live thread"
    );
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should show the draft after Cmd-N",
        );
    });

    // The deliberate additional-agent path still creates a draft, which the
    // sidebar surfaces as a placeholder row above the real thread.
    panel.update_in(cx, |panel, window, cx| {
        panel.activate_additional_new_thread(
            true,
            agent_ui::AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  New thread", "  Hello * (!)"],
        "The additional-agent path should show a placeholder row for the active empty draft"
    );

    // The panel should be on the draft and active_entry should track it.
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should be showing the draft after Cmd-N",
        );
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "active_entry should be Draft after Cmd-N",
        );
    });
}

#[gpui::test]
async fn test_cmd_n_shows_new_thread_entry_in_absorbed_worktree(cx: &mut TestAppContext) {
    // When the active workspace is an absorbed git worktree, cmd-n
    // should activate the draft thread in the panel and the sidebar
    // should surface a placeholder row for the active empty draft.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkout pointing back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Switch to the worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    // Create a non-empty thread in the worktree workspace.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&worktree_panel, connection, cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;
    cx.run_until_parked();

    // Open and unarchived, so its one row is in Active.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Hello {wt-feature-a} *"]
    );

    // Simulate Cmd-N in the worktree workspace: it always creates the draft
    // row, even with a live thread tab open.
    worktree_panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    worktree_workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();

    // Both the draft and the still-open thread are open, so both sit in
    // Active and neither is repeated in All Threads.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  New thread {wt-feature-a}",
            "  Hello {wt-feature-a} * (!)",
        ],
        "Cmd-N should show a fresh draft row above the live thread"
    );

    worktree_panel.update_in(cx, |panel, window, cx| {
        panel.activate_additional_new_thread(
            true,
            agent_ui::AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    // The sidebar surfaces the active empty draft as a placeholder row. Its
    // worktree chip identifies which workspace it belongs to (the linked
    // worktree).
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  New thread {wt-feature-a}",
            "  Hello {wt-feature-a} * (!)",
        ],
        "The additional-agent path should show a placeholder row for the active empty draft"
    );

    // The panel should be on the draft and active_entry should track it.
    worktree_panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should be showing the draft after Cmd-N",
        );
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &worktree_workspace,
            "active_entry should be Draft after Cmd-N",
        );
    });
}

#[gpui::test]
async fn test_only_actively_viewed_empty_draft_is_visible_in_sidebar(cx: &mut TestAppContext) {
    // The sidebar surfaces an empty-draft placeholder row only for the
    // draft that the *active workspace's panel* is currently viewing.
    // Specifically:
    //   1. Empty ephemeral drafts in non-active workspaces (e.g. a
    //      sibling linked-worktree panel) are hidden.
    //   2. An empty ephemeral that is parked in its slot while the user
    //      is viewing a real thread is hidden (it's not the active view).
    //   3. When the active workspace switches, the placeholder follows
    //      the new active panel's current view.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, main_panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    // `mw.workspace()` returns the *currently active* workspace, so we
    // capture the main one here before adding the worktree workspace
    // (which would make it the active one).
    let main_workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);
    cx.run_until_parked();

    // Give the main panel a real thread we can park the draft behind
    // later. Send a message to promote the draft→real thread.
    let real_connection = StubAgentConnection::new();
    real_connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&main_panel, real_connection, cx);
    agent_ui::test_support::send_message(&main_panel, cx);
    let main_real_thread_id =
        main_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    cx.run_until_parked();

    // Now open a fresh ephemeral draft in the main panel.
    agent_ui::test_support::open_draft_with_connection(&main_panel, StubAgentConnection::new(), cx);
    cx.run_until_parked();

    // And an ephemeral draft in the worktree panel as well.
    agent_ui::test_support::open_draft_with_connection(
        &worktree_panel,
        StubAgentConnection::new(),
        cx,
    );
    cx.run_until_parked();

    // `open_draft_with_connection` focuses the panel it's called on,
    // which makes that workspace active. Explicitly re-activate the main
    // workspace so the baseline assertions below describe the
    // "main-workspace-is-active" case independently of call order above.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(main_workspace.clone(), None, window, cx);
    });
    cx.run_until_parked();

    // The invariant under test is: at most one empty draft is a candidate
    // row at a time, and it's the one that corresponds to the active
    // workspace's panel's currently-active draft — though since it's open,
    // it shows up as two rows (Active and All Threads), not one. Counting
    // `is_empty_draft` rows is more robust than tracking specific
    // thread_ids because draft creation flows can leave behind orphan
    // ephemeral metadata that's also hidden by the filter.
    let empty_draft_rows =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> Vec<ThreadId> {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .contents
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        ListEntry::Thread(t) if t.draft == Some(DraftKind::Empty) => {
                            Some(t.metadata.thread_id)
                        }
                        _ => None,
                    })
                    .collect()
            })
        };
    let active_panel_draft_id =
        |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| -> Option<ThreadId> {
            panel.read_with(cx, |panel, cx| {
                panel
                    .active_thread_id(cx)
                    .filter(|_| panel.active_thread_is_draft(cx))
            })
        };

    // Baseline: main workspace active, main panel viewing its draft. Only
    // that one draft is visible, matching the main panel's draft, and it is
    // one row: the panel is showing it, so it is Active and nothing else.
    let main_active_draft =
        active_panel_draft_id(&main_panel, cx).expect("main panel should be viewing a draft");
    let visible = empty_draft_rows(&sidebar, cx);
    assert_eq!(
        visible,
        vec![main_active_draft],
        "exactly the main panel's active empty draft should be visible, as one Active row"
    );

    // Navigate the main panel AWAY from its draft to the real thread.
    // The draft is no longer the active view of its panel, so its
    // placeholder must disappear from the sidebar.
    main_panel.update_in(cx, |panel, window, cx| {
        panel.load_agent_thread(
            agent_ui::Agent::NativeAgent,
            main_real_thread_id,
            None,
            None,
            false,
            agent_ui::AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    main_panel.read_with(cx, |panel, cx| {
        assert_eq!(
            panel.active_thread_id(cx),
            Some(main_real_thread_id),
            "main panel should now be viewing the real thread"
        );
    });
    assert!(
        empty_draft_rows(&sidebar, cx).is_empty(),
        "no placeholder should be visible: main panel is on a real thread and worktree workspace is inactive"
    );

    // Switch the active workspace to the worktree. Now the worktree
    // panel's draft is the active view, so its placeholder appears.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let worktree_active_draft = active_panel_draft_id(&worktree_panel, cx)
        .expect("worktree panel should be viewing a draft");
    let visible = empty_draft_rows(&sidebar, cx);
    assert_eq!(
        visible,
        vec![worktree_active_draft],
        "exactly the worktree panel's active empty draft should be visible after switching workspaces, as one Active row"
    );
}

async fn init_test_project_with_git(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> (Entity<project::Project>, Arc<dyn fs::Fs>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [worktree_path.as_ref()], cx).await;
    (project, fs)
}

#[gpui::test]
async fn test_search_matches_worktree_name(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/rosewood"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt/rosewood".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_named_thread_metadata("main-t", "Unrelated Thread", &project, cx).await;
    save_named_thread_metadata("wt-t", "Fix Bug", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Search for "rosewood" — should match the worktree name, not the title.
    type_in_search(&sidebar, "rosewood", cx);

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix Bug {rosewood}  <== selected",
        ],
    );
}

#[gpui::test]
async fn test_git_worktree_added_live_updates_sidebar(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt/rosewood".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread against a worktree path with the correct main
    // worktree association (as if the git state had been resolved).
    save_thread_metadata_with_main_paths(
        "wt-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from("/wt/rosewood")]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Thread is visible because its main_worktree_paths match the group.
    // The chip name is derived from the path even before git discovery.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Worktree Thread {rosewood}"]
    );

    // Now add the worktree to the git state and trigger a rescan.
    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/rosewood"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Worktree Thread {rosewood}",
        ]
    );
}

#[gpui::test]
async fn test_two_worktree_workspaces_absorbed_when_main_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Create the main repo directory (not opened as a workspace yet).
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
            },
            "src": {},
        }),
    )
    .await;

    // Two worktree checkouts whose .git files point back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;

    project_a.update(cx, |p, cx| p.git_scans_complete(cx)).await;
    project_b.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    // Open both worktrees as workspaces — no main repo yet.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-a")),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-b")),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        None,
        None,
        &project_b,
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Without the main repo, each worktree has its own header.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Thread B {wt-feature-b}",
            "  Thread A {wt-feature-a}",
        ]
    );

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(main_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Both worktree workspaces should now be absorbed under the main
    // repo header, with worktree chips.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Thread B {wt-feature-b}",
            "  Thread A {wt-feature-a}",
        ]
    );
}

#[gpui::test]
async fn test_threadless_workspace_shows_new_thread_with_worktree_chip(cx: &mut TestAppContext) {
    // When a group has two workspaces — one with threads and one
    // without — the threadless workspace should appear as a
    // "New Thread" button with its worktree chip.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Main repo with two linked worktrees.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace A: worktree feature-a (has threads).
    let project_a = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    project_a.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    // Workspace B: worktree feature-b (no threads).
    let project_b = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;
    project_b.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Only save a thread for workspace A.
    save_named_thread_metadata("thread-a", "Thread A", &project_a, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Workspace A's thread appears normally. Workspace B (threadless)
    // appears as a "New Thread" button with its worktree chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Thread A {wt-feature-a}",]
    );
}

#[gpui::test]
async fn test_multi_worktree_thread_shows_multiple_chips(cx: &mut TestAppContext) {
    // A thread created in a workspace with roots from different git
    // worktrees should show a chip for each distinct worktree name.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Two main repos.
    fs.insert_tree(
        "/project_a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/project_b",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkouts.
    for repo in &["project_a", "project_b"] {
        let git_path = format!("/{repo}/.git");
        for branch in &["olivetti", "selectric"] {
            fs.add_linked_worktree_for_repo(
                Path::new(&git_path),
                false,
                git::repository::Worktree {
                    path: std::path::PathBuf::from(format!("/worktrees/{repo}/{branch}/{repo}")),
                    ref_name: Some(format!("refs/heads/{branch}").into()),
                    sha: "aaa".into(),
                    is_main: false,
                    is_bare: false,
                },
            )
            .await;
        }
    }

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Open a workspace with the worktree checkout paths as roots
    // (this is the workspace the thread was created in).
    let project = project::Project::test(
        fs.clone(),
        [
            "/worktrees/project_a/olivetti/project_a".as_ref(),
            "/worktrees/project_b/selectric/project_b".as_ref(),
        ],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread under the same paths as the workspace roots.
    save_named_thread_metadata("wt-thread", "Cross Worktree Thread", &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Should show two distinct worktree chips.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Cross Worktree Thread {project_a:olivetti}, {project_b:selectric}",
        ]
    );
}

#[gpui::test]
async fn test_same_named_worktree_chips_are_deduplicated(cx: &mut TestAppContext) {
    // When a thread's roots span multiple repos but share the same
    // worktree name (e.g. both in "olivetti"), only one chip should
    // appear.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project_a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/project_b",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    for repo in &["project_a", "project_b"] {
        let git_path = format!("/{repo}/.git");
        fs.add_linked_worktree_for_repo(
            Path::new(&git_path),
            false,
            git::repository::Worktree {
                path: std::path::PathBuf::from(format!("/worktrees/{repo}/olivetti/{repo}")),
                ref_name: Some("refs/heads/olivetti".into()),
                sha: "aaa".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
    }

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(
        fs.clone(),
        [
            "/worktrees/project_a/olivetti/project_a".as_ref(),
            "/worktrees/project_b/olivetti/project_b".as_ref(),
        ],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Thread with roots in both repos' "olivetti" worktrees.
    save_named_thread_metadata("wt-thread", "Same Branch Thread", &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Both worktree paths have the name "olivetti", so only one chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Same Branch Thread {olivetti}",
        ]
    );
}

#[gpui::test]
async fn test_absorbed_worktree_running_thread_shows_live_status(cx: &mut TestAppContext) {
    // When a worktree workspace is absorbed under the main repo, a
    // running thread in the worktree's agent panel should still show
    // live status (spinner + "(running)") in the sidebar.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkout pointing back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Create the MultiWorkspace with both projects.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Add an agent panel to the worktree workspace so we can run a
    // thread inside it.
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Switch back to the main workspace before setting up the sidebar.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    // Start a thread in the worktree workspace's panel and keep it
    // generating (don't resolve it).
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);

    // Save metadata so the sidebar knows about this thread.
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;

    // Keep the thread generating by sending a chunk without ending
    // the turn.
    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("working...".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // The worktree thread should be absorbed under the main project
    // and show live running status. It is running, so it is open, which puts
    // its one row in Active.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries, vec!["  Hello {wt-feature-a} * (running)"]);
}

#[gpui::test]
async fn test_absorbed_worktree_completion_triggers_notification(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;

    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("working...".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // Running, so open: its one row is in Active.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Hello {wt-feature-a} * (running)"]
    );

    connection.end_turn(session_id, acp::StopReason::EndTurn);
    cx.run_until_parked();

    // Still open (the panel still shows it), so still one Active row.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Hello {wt-feature-a} * (!)"]
    );
}

#[gpui::test]
async fn test_clicking_worktree_thread_opens_workspace_when_none_exists(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only open the main repo — no workspace for the worktree.
    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread for the worktree path (no workspace for it).
    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Thread should appear under the main repo with a worktree chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  WT Thread {wt-feature-a}",
        ],
    );

    // Only 1 workspace should exist.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
    );

    // Focus the sidebar and select the worktree thread (found by shape, not by
    // a hard-coded index: the headers around it are not the point here).
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        let thread_ix = sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_)))
            .expect("the worktree thread should be listed");
        sidebar.selection = Some(thread_ix);
    });

    // Confirm to open the worktree thread.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    // A new workspace should have been created for the worktree path.
    let new_workspace = multi_workspace.read_with(cx, |mw, _| {
        assert_eq!(
            mw.workspaces().count(),
            2,
            "confirming a worktree thread without a workspace should open one",
        );
        mw.workspaces().nth(1).unwrap().clone()
    });

    let new_path_list =
        new_workspace.read_with(cx, |_, cx| workspace_path_list(&new_workspace, cx));
    assert_eq!(
        new_path_list,
        PathList::new(&[std::path::PathBuf::from("/wt-feature-a")]),
        "the new workspace should have been opened for the worktree path",
    );
}

#[gpui::test]
async fn test_clicking_worktree_thread_does_not_briefly_render_as_separate_project(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  WT Thread {wt-feature-a}",
        ],
    );

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(1); // index 0 is header, 1 is the thread
    });

    // The merged history model has no project headers; assert that exactly
    // the expected worktree thread stays listed throughout.
    let assert_sidebar_state = |sidebar: &mut Sidebar, _cx: &mut Context<Sidebar>| {
        let mut saw_expected_thread = false;
        for entry in &sidebar.contents.entries {
            match entry {
                ListEntry::SectionHeader(_) | ListEntry::WorkspaceHeader(_) => {}
                ListEntry::Thread(thread)
                    if thread.metadata.title.as_ref().map(|t| t.as_ref()) == Some("WT Thread")
                        && thread
                            .worktrees
                            .first()
                            .and_then(|wt| wt.worktree_name.as_ref().map(|n| n.as_ref()))
                            == Some("wt-feature-a") =>
                {
                    saw_expected_thread = true;
                }
                ListEntry::Thread(thread) => {
                    let title = thread.metadata.display_title();
                    let worktree_name = thread
                        .worktrees
                        .first()
                        .and_then(|wt| wt.worktree_name.as_ref().map(|n| n.as_ref()))
                        .unwrap_or("<none>");
                    panic!(
                        "unexpected sidebar thread while opening linked worktree thread: title=`{}`, worktree=`{}`",
                        title, worktree_name
                    );
                }
                ListEntry::Terminal(terminal) => {
                    panic!(
                        "unexpected sidebar terminal while opening linked worktree thread: title=`{}`",
                        terminal.metadata.title
                    );
                }
            }
        }

        assert!(
            saw_expected_thread,
            "expected the sidebar to keep showing `WT Thread {{wt-feature-a}}` under `project`"
        );
    };

    sidebar
        .update(cx, |_, cx| cx.observe_self(assert_sidebar_state))
        .detach();

    let window = cx.windows()[0];
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(Confirm.boxed_clone(), cx);
    })
    .unwrap();

    cx.run_until_parked();

    sidebar.update(cx, assert_sidebar_state);
}

#[gpui::test]
async fn test_clicking_absorbed_worktree_thread_activates_worktree_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Activate the main workspace before setting up the sidebar.
    let main_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace.clone(), None, window, cx);
        workspace
    });

    save_named_thread_metadata("thread-main", "Main Thread", &main_project, cx).await;
    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // The worktree workspace should be absorbed under the main repo.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries.len(), 2);
    assert!(entries.contains(&"  Main Thread".to_string()));
    assert!(entries.contains(&"  WT Thread {wt-feature-a}".to_string()));

    // Index into the real entries list (which includes bucket headers, in
    // contrast to the string dump).
    let wt_thread_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread)
                        if thread.metadata.display_title().contains("WT Thread")
                )
            })
            .expect("should find the worktree thread entry")
    });

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        main_workspace,
        "main workspace should be active initially"
    );

    // Focus the sidebar and select the absorbed worktree thread.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(wt_thread_index);
    });

    // Confirm to activate the worktree thread.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    // The worktree workspace should now be active, not the main one.
    let active_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert_eq!(
        active_workspace, worktree_workspace,
        "clicking an absorbed worktree thread should activate the worktree workspace"
    );
}

// Reproduces the core of the user-reported bug: a thread belonging to
// a multi-root workspace that mixes a standalone project and a linked
// git worktree can become invisible in the sidebar when its stored
// `main_worktree_paths` don't match the workspace's project group
// key. The metadata still exists and Thread History still shows it,
// but the sidebar rebuild's lookups all miss.
//
// Real-world setup: a single multi-root workspace whose roots are
// `[/cloud, /worktrees/zed/wt_a/zed]`, where:
//   - `/cloud` is a standalone git repo (main == folder).
//   - `/worktrees/zed/wt_a/zed` is a linked worktree of `/zed`.
//
// Once git scans complete the project group key is
// `[/cloud, /zed]` — the main paths of the two roots. A thread
// created in this workspace is written with
// `main=[/cloud, /zed], folder=[/cloud, /worktrees/zed/wt_a/zed]`
// and the sidebar finds it via `entries_for_main_worktree_path`.
//
// If some other code path (stale data on reload, a path-less archive
// restored via the project picker, a legacy write …) persists the
// thread with `main == folder` instead, the stored
// `main_worktree_paths` is
// `[/cloud, /worktrees/zed/wt_a/zed]` ≠ `[/cloud, /zed]`. The three
// lookups in `rebuild_contents` all miss:
//
//   1. `entries_for_main_worktree_path([/cloud, /zed])` — the
//      thread's stored main doesn't equal the group key.
//   2. `entries_for_path([/cloud, /zed])` — the thread's folder paths
//      don't equal the group key either.
//   3. The linked-worktree fallback iterates the group's workspaces'
//      `linked_worktrees()` snapshots. Those yield *sibling* linked
//      worktrees of the repo, not the workspace's own roots, so the
//      thread's folder `/worktrees/zed/wt_a/zed` doesn't match.
//
// The row falls out of the sidebar entirely — matching the user's
// symptom of a thread visible in the agent panel but missing from
// the sidebar. It only reappears once something re-writes the
// thread's metadata in the good shape (e.g. `handle_conversation_event`
// firing after the user sends a message).
//
// We directly persist the bad shape via `store.save(...)` rather
// than trying to reproduce the original writer. The bug is
// ultimately about the sidebar's tolerance for any stale row whose
// folder paths correspond to an open workspace's roots, regardless
// of how that row came to be in the store.
#[gpui::test]
async fn test_sidebar_keeps_multi_root_thread_with_stale_main_paths(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Standalone repo — one of the workspace's two roots, main
    // worktree of its own .git.
    fs.insert_tree(
        "/cloud",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Separate /zed repo whose linked worktree will form the second
    // workspace root. /zed itself is NOT opened as a workspace root.
    fs.insert_tree(
        "/zed",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/zed/wt_a/zed",
        serde_json::json!({
            ".git": "gitdir: /zed/.git/worktrees/wt_a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/zed/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/worktrees/zed/wt_a/zed"),
            ref_name: Some("refs/heads/wt_a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Single multi-root project with both /cloud and the linked
    // worktree of /zed.
    let project = project::Project::test(
        fs.clone(),
        ["/cloud".as_ref(), "/worktrees/zed/wt_a/zed".as_ref()],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _panel = add_agent_panel(&workspace, cx);
    cx.run_until_parked();

    // Sanity-check the shapes the rest of the test depends on.
    let group_key = workspace.read_with(cx, |ws, cx| ws.project_group_key(cx));
    let expected_main_paths = PathList::new(&[PathBuf::from("/cloud"), PathBuf::from("/zed")]);
    assert_eq!(
        group_key.path_list(),
        &expected_main_paths,
        "expected the multi-root workspace's project group key to normalize to \
         [/cloud, /zed] (main of the standalone repo + main of the linked worktree)"
    );

    let folder_paths = PathList::new(&[
        PathBuf::from("/cloud"),
        PathBuf::from("/worktrees/zed/wt_a/zed"),
    ]);
    let workspace_root_paths = workspace.read_with(cx, |ws, cx| PathList::new(&ws.root_paths(cx)));
    assert_eq!(
        workspace_root_paths, folder_paths,
        "expected the workspace's root paths to equal [/cloud, /worktrees/zed/wt_a/zed]"
    );

    let session_id = acp::SessionId::new(Arc::from("multi-root-stale-paths"));
    let thread_id = ThreadId::new();

    // Persist the thread in the "bad" shape that the bug manifests as:
    // main == folder for every root. Any stale row where
    // `main_worktree_paths` no longer equals the group key produces
    // the same user-visible symptom; this is the concrete shape
    // produced by `WorktreePaths::from_folder_paths` on the workspace
    // roots.
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Stale Multi-Root Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&folder_paths),
                    archived: false,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    let visible = sidebar.read_with(cx, |sidebar, _cx| has_thread_entry(sidebar, &session_id));

    // If this assert fails, we've reproduced the bug: the sidebar's
    // rebuild queries can't locate the thread under the current
    // project group, even though the metadata is intact and the
    // thread's folder paths exactly equal the open workspace's roots.
    assert!(
        visible,
        "thread disappeared from the sidebar when its main_worktree_paths \
         ({folder_paths:?}) diverged from the project group key ({expected_main_paths:?}); \
         sidebar entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_with_saved_paths_activates_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has saved metadata in ThreadStore. A matching workspace is
    // already open. Expected: activates the matching workspace.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Save a thread with path_list pointing to project-b.
    let session_id = acp::SessionId::new(Arc::from("archived-1"));
    save_test_thread_metadata(&session_id, &project_b, cx).await;

    // Ensure workspace A is active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // Call activate_archived_thread – should resolve saved paths and
    // switch to the workspace for project-b.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Archived Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have switched to the workspace matching the saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_cwd_fallback_with_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has no saved metadata but session_info has cwd. A matching
    // workspace is open. Expected: uses cwd to find and activate it.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Start with workspace A active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // No thread saved to the store – cwd is the only path hint.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("unknown-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("CWD Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                    std::path::PathBuf::from("/project-b"),
                ])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have activated the workspace matching the cwd"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_no_paths_no_cwd_uses_active_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has no saved metadata and no cwd. Expected: falls back to
    // the currently active workspace.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });

    // Activate workspace B (index 1) to make it the active one.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b
    );

    // No saved thread, no cwd – should fall back to the active workspace.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("no-context-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Contextless Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have stayed on the active workspace when no path info is available"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_saved_paths_opens_new_workspace(cx: &mut TestAppContext) {
    // Thread has saved metadata pointing to a path with no open workspace.
    // Expected: opens a new workspace for that path.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread with path_list pointing to project-b – which has no
    // open workspace.
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let session_id = acp::SessionId::new(Arc::from("archived-new-ws"));

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with one workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("New WS Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a second workspace for the archived thread's saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window"));

    sidebar.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Cross Window Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window_with_target_sidebar(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b.clone(), window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);
    let workspace_b = multi_workspace_b_entity.read_with(cx_b, |mw, _| mw.workspace().clone());
    let _panel_b = add_agent_panel(&workspace_b, cx_b);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window-with-sidebar"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Cross Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-b",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
    sidebar_b.read_with(cx_b, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "target window's sidebar should eagerly focus the activated archived thread",
        );
    });
}

#[gpui::test]
async fn test_activate_archived_thread_prefers_current_window_for_matching_paths(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_b = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));
    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-current-window"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Current Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-a",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_a,
        "should keep activation in the current window when it already has a matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "current window's sidebar should eagerly focus the activated archived thread",
        );
    });
    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "current window should continue reusing its existing workspace"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "other windows should not be activated just because they also match the saved paths"
    );
}

#[gpui::test]
async fn test_archive_thread_uses_next_threads_own_workspace(cx: &mut TestAppContext) {
    // Regression test: archive_thread previously always loaded the next thread
    // through group_workspace (the main workspace's ProjectHeader), even when
    // the next thread belonged to an absorbed linked-worktree workspace. That
    // caused the worktree thread to be loaded in the main panel, which bound it
    // to the main project and corrupted its stored folder_paths.
    //
    // The fix: use next.workspace (ThreadEntryWorkspace::Open) when available,
    // falling back to group_workspace only for Closed workspaces.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Activate main workspace so the sidebar tracks the main panel.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let main_panel = add_agent_panel(&main_workspace, cx);
    let _worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Open Thread 2 in the main panel and keep it running.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&main_panel, connection.clone(), cx);
    send_message(&main_panel, cx);

    let thread2_session_id = active_session_id(&main_panel, cx);

    cx.update(|_, cx| {
        connection.send_update(
            thread2_session_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("working...".into())),
            cx,
        );
    });

    // Save thread 2's metadata with a newer timestamp so it sorts above thread 1.
    save_thread_metadata(
        thread2_session_id.clone(),
        Some("Thread 2".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    // Save thread 1's metadata with the worktree path and an older timestamp so
    // it sorts below thread 2. archive_thread will find it as the "next" candidate.
    let thread1_session_id = acp::SessionId::new(Arc::from("thread1-worktree-session"));
    save_thread_metadata(
        thread1_session_id,
        Some("Thread 1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    cx.run_until_parked();

    // Verify the sidebar absorbed thread 1 under [project] with the worktree chip.
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before.iter().any(|s| s.contains("{wt-feature-a}")),
        "Thread 1 should appear with the linked-worktree chip before archiving: {:?}",
        entries_before
    );

    // The sidebar should track T2 as the focused thread (derived from the
    // main panel's active view).
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &thread2_session_id,
            "focused thread should be Thread 2 before archiving",
        );
    });

    // Archive thread 2.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread2_session_id, window, cx);
    });

    cx.run_until_parked();

    // Archiving T2 closes its tab (tabs are what "open" means), so the
    // main panel falls back to a fresh draft. The regression this test
    // guards is that the linked-worktree thread T1 must NOT be loaded
    // into the main panel; before the fix, archive_thread used
    // group_workspace instead of next.workspace, causing T1 to be loaded
    // in the wrong panel and corrupting its folder_paths.
    let main_active = main_panel.read_with(cx, |panel, cx| {
        panel
            .active_agent_thread(cx)
            .map(|t| t.read(cx).session_id().clone())
    });
    assert_ne!(
        main_active,
        Some(acp::SessionId::new(Arc::from("thread1-worktree-session"))),
        "main panel should not have been taken over by loading the linked-worktree thread T1"
    );
    main_panel.read_with(cx, |panel, cx| {
        assert!(
            panel
                .active_agent_thread(cx)
                .is_none_or(|thread| thread.read(cx).is_draft_thread()),
            "archiving the active thread should close its tab and fall back to a draft"
        );
    });

    // Thread 1 should still appear in the sidebar with its worktree chip
    // (Thread 2 was archived so it is gone from the list).
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_after.iter().any(|s| s.contains("{wt-feature-a}")),
        "T1 should still carry its linked-worktree chip after archiving T2: {:?}",
        entries_after
    );
}

#[gpui::test]
async fn test_archive_last_worktree_thread_removes_workspace(cx: &mut TestAppContext) {
    // When the last non-archived thread for a linked worktree is archived,
    // the linked worktree workspace should be removed from the multi-workspace.
    // The main worktree workspace should remain (it's always reachable via
    // the project header).
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let _worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Save a thread for the main project.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    // Save a thread for the linked worktree.
    let wt_thread_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata(
        wt_thread_id.clone(),
        Some("Worktree Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );
    cx.run_until_parked();

    let remote_host =
        remote::RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 99 });
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(workspace::ProjectGroup {
            key: ProjectGroupKey::new(
                Some(remote_host.clone()),
                PathList::new(&[PathBuf::from("/remote/project")]),
            ),
            workspaces: Vec::new(),
            expanded: true,
        });
    });
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(acp::SessionId::new(Arc::from("remote-thread"))),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Remote Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/remote/project",
            )])),
            archived: false,
            remote_connection: Some(remote_host),
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Should have 2 workspaces.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should start with 2 workspaces (main + linked worktree)"
    );

    // Archive the worktree thread (the only thread for /wt-feature-a).
    sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
        sidebar.archive_thread(&wt_thread_id, window, cx);
    });

    // archive_thread spawns a multi-layered chain of tasks (workspace
    // removal → git persist → disk removal), each of which may spawn
    // further background work. Each run_until_parked() call drives one
    // layer of pending work.

    cx.run_until_parked();

    // The linked worktree workspace should have been removed.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "linked worktree workspace should be removed after archiving its last thread"
    );

    multi_workspace.read_with(cx, |mw, _| {
        assert_eq!(
            mw.workspace(),
            &main_workspace,
            "archiving the worktree's last thread should activate its own project, not the remote one"
        );
    });

    // The linked worktree checkout directory should also be removed from disk.
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after archiving its last thread"
    );

    // The main thread should still be visible.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Main Thread")),
        "main thread should still be visible: {entries:?}"
    );
    // In the merged history model the archived thread stays listed, muted.
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Worktree Thread") && e.contains("(archived)")),
        "archived worktree thread should stay listed as archived: {entries:?}"
    );

    // The archived thread must retain its folder_paths so it can be
    // restored to the correct workspace later.
    let wt_thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&wt_thread_id)
            .unwrap()
            .thread_id
    });
    let archived_paths = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(wt_thread_id)
            .unwrap()
            .folder_paths()
            .clone()
    });
    assert_eq!(
        archived_paths.paths(),
        &[PathBuf::from("/worktrees/project/feature-a/project")],
        "archived thread must retain its folder_paths for restore"
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch has moved
    // to a different SHA since archival. The worktree stays in detached
    // HEAD and the moved branch is left untouched.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Move the branch to a different SHA.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state
            .refs
            .insert("refs/heads/feature-a".into(), "moved-sha".into());
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-a"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-a".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed even when branch has moved: {:?}",
        result.err()
    );

    // The moved branch ref should be completely untouched.
    let branch_sha = fs
        .with_git_state(Path::new("/project/.git"), false, |state| {
            state.refs.get("refs/heads/feature-a").cloned()
        })
        .unwrap();
    assert_eq!(
        branch_sha.as_deref(),
        Some("moved-sha"),
        "the moved branch ref should not be modified by the restore"
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_not_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch still
    // points at the same SHA as at archive time.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-b",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-b",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // refs/heads/feature-b already points at "original-sha" (set by
    // add_linked_worktree_for_repo), matching original_commit_hash.

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-b"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-b".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch has not moved: {:?}",
        result.err()
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_does_not_exist(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch no longer
    // exists (e.g. it was deleted while the thread was archived). The
    // code should attempt to recreate the branch.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-d": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-d",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-d",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-d",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-d"),
            ref_name: Some("refs/heads/feature-d".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-d".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Remove the branch ref so change_branch will fail.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state.refs.remove("refs/heads/feature-d");
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-d"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-d".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch does not exist: {:?}",
        result.err()
    );
}

#[gpui::test]
async fn test_restore_worktree_thread_uses_main_repo_project_group_key(cx: &mut TestAppContext) {
    // Activating an archived linked worktree thread whose directory has
    // been deleted should reuse the existing main repo workspace, not
    // create a new one. The provisional ProjectGroupKey must be derived
    // from main_worktree_paths so that find_or_create_local_workspace
    // matches the main repo workspace when the worktree path is absent.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-c": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-c",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-c",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-c",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-c"),
            ref_name: Some("refs/heads/feature-c".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-c".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Save thread metadata for the linked worktree.
    let wt_session_id = acp::SessionId::new(Arc::from("wt-thread-c"));
    save_thread_metadata(
        wt_session_id.clone(),
        Some("Worktree Thread C".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );
    cx.run_until_parked();

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&wt_session_id)
            .unwrap()
            .thread_id
    });

    // Archive the thread without creating ArchivedGitWorktree records.
    let store = cx.update(|_window, cx| ThreadMetadataStore::global(cx));
    cx.update(|_window, cx| {
        store.update(cx, |store, cx| store.archive(thread_id, None, cx));
    });
    cx.run_until_parked();

    // Remove the worktree workspace and delete the worktree from disk.
    let remove_task = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.remove(
            vec![worktree_workspace],
            RemovalIntent::KeepProject,
            window,
            cx,
        )
    });
    remove_task.await.ok();
    cx.run_until_parked();
    cx.run_until_parked();
    fs.remove_dir(
        Path::new("/wt-feature-c"),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: true,
        },
    )
    .await
    .unwrap();

    let workspace_count_before = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(
        workspace_count_before, 1,
        "should have only the main workspace"
    );

    // Activate the archived thread. The worktree path is missing from
    // disk, so find_or_create_local_workspace falls back to the
    // provisional ProjectGroupKey to find a matching workspace.
    let metadata = cx.update(|_window, cx| store.read(cx).entry(thread_id).unwrap().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The provisional key should use [/project] (the main repo),
    // which matches the existing main workspace. If it incorrectly
    // used [/wt-feature-c] (the linked worktree path), no workspace
    // would match and a spurious new one would be created.
    let workspace_count_after = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(
        workspace_count_after, 1,
        "restoring a linked worktree thread should reuse the main repo workspace, \
         not create a new one (workspace count went from {workspace_count_before} to \
         {workspace_count_after})"
    );
}

#[gpui::test]
async fn test_archive_last_worktree_thread_not_blocked_by_remote_thread_at_same_path(
    cx: &mut TestAppContext,
) {
    // A remote thread at the same path as a local linked worktree thread
    // should not prevent the local workspace from being removed when the
    // local thread is archived (the last local thread for that worktree).
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let _worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Save a thread for the main project.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    // Save a local thread for the linked worktree.
    let wt_thread_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata(
        wt_thread_id.clone(),
        Some("Local Worktree Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    // Save a remote thread at the same /wt-feature-a path but on a
    // different host. This should NOT count as a remaining thread for
    // the local linked worktree workspace.
    let remote_host =
        remote::RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 99 });
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(acp::SessionId::new(Arc::from("remote-wt-thread"))),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Remote Worktree Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/wt-feature-a",
            )])),
            archived: false,
            remote_connection: Some(remote_host),
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should start with 2 workspaces (main + linked worktree)"
    );

    // The merged history lists threads from every host, so the remote
    // thread appears alongside the local ones.
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before
            .iter()
            .any(|e| e.contains("Remote Worktree Thread")),
        "remote thread should appear in the merged history: {entries_before:?}"
    );

    // Archive the local worktree thread.
    sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
        sidebar.archive_thread(&wt_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The linked worktree workspace should be removed because the
    // only *local* thread for it was archived. The remote thread at
    // the same path should not have prevented removal.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "linked worktree workspace should be removed; the remote thread at the same path \
         should not count as a remaining local thread"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Main Thread")),
        "main thread should still be visible: {entries:?}"
    );
    // In the merged history model the archived thread stays listed, muted,
    // and remote threads are part of the single history list too.
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Local Worktree Thread") && e.contains("(archived)")),
        "archived local worktree thread should stay listed as archived: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Remote Worktree Thread")),
        "remote threads are listed in the merged history: {entries:?}"
    );
}

#[gpui::test]
async fn test_linked_worktree_threads_not_duplicated_across_groups(cx: &mut TestAppContext) {
    // When a multi-root workspace (e.g. [/other, /project]) shares a
    // repo with a single-root workspace (e.g. [/project]), linked
    // worktree threads from the shared repo should only appear under
    // the dedicated group [project], not under [other, project].
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
    let fs = FakeFs::new(cx.executor());

    // Two independent repos, each with their own git history.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/other",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Register the linked worktree in the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace 1: just /project.
    let project_only = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    project_only
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Workspace 2: /other and /project together (multi-root).
    let multi_root =
        project::Project::test(fs.clone(), ["/other".as_ref(), "/project".as_ref()], cx).await;
    multi_root
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Save a thread under the linked worktree path BEFORE setting up
    // the sidebar and panels, so that reconciliation sees the [project]
    // group as non-empty and doesn't create a spurious draft there.
    let wt_session_id = acp::SessionId::new(Arc::from("wt-thread"));
    save_thread_metadata(
        wt_session_id,
        Some("Worktree Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_only.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let multi_root_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(multi_root.clone(), window, cx)
    });
    add_agent_panel(&multi_root_workspace, cx);
    cx.run_until_parked();

    // The thread should appear only under [project] (the dedicated
    // group for the /project repo), not under [other, project].
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Worktree Thread {wt-feature-a}",
        ]
    );
}

fn thread_id_for(session_id: &acp::SessionId, cx: &mut TestAppContext) -> ThreadId {
    cx.read(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .map(|m| m.thread_id)
            .expect("thread metadata should exist")
    })
}

#[gpui::test]
async fn test_thread_switcher_ordering(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let switcher_ids =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> Vec<ThreadId> {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                switcher
                    .read(cx)
                    .entries()
                    .iter()
                    .map(|entry| entry.thread_id().expect("expected thread switcher entry"))
                    .collect()
            })
        };

    let switcher_selected_id =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> ThreadId {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                let s = switcher.read(cx);
                s.selected_entry()
                    .expect("should have selection")
                    .thread_id()
                    .expect("expected selected thread entry")
            })
        };

    // ── Setup: create three threads with distinct created_at times ──────
    // Thread C (oldest), Thread B, Thread A (newest) — by created_at.
    // We send messages in each so they also get last_message_sent_or_queued timestamps.
    let connection_c = StubAgentConnection::new();
    connection_c.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done C".into()),
    )]);
    open_thread_with_connection(&panel, connection_c, cx);
    send_message(&panel, cx);
    let session_id_c = active_session_id(&panel, cx);
    let thread_id_c = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_c.clone(),
        Some("Thread C".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done B".into()),
    )]);
    open_thread_with_connection(&panel, connection_b, cx);
    send_message(&panel, cx);
    let session_id_b = active_session_id(&panel, cx);
    let thread_id_b = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_b.clone(),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done A".into()),
    )]);
    open_thread_with_connection(&panel, connection_a, cx);
    send_message(&panel, cx);
    let session_id_a = active_session_id(&panel, cx);
    let thread_id_a = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_a.clone(),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    // All three threads are now live. Thread A was opened last, so it's
    // the one being viewed. Opening each thread called record_thread_access,
    // so all three have last_accessed_at set.
    // Access order is: A (most recent), B, C (oldest).

    // ── 1. Open switcher: threads sorted by last_accessed_at ─────────────────
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // All three have last_accessed_at, so they sort by access time.
    // A was accessed most recently (it's the currently viewed thread),
    // then B, then C.
    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_b, thread_id_c,],
    );
    // First ctrl-tab selects the second entry (B).
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_b);

    // Dismiss the switcher without confirming.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 2. Confirm on Thread C: it becomes most-recently-accessed ──────
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Cycle twice to land on Thread C (index 2).
    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        assert_eq!(switcher.read(cx).selected_index(), 1);
    });
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar
            .thread_switcher
            .as_ref()
            .unwrap()
            .update(cx, |s, cx| s.cycle_selection(cx));
    });
    cx.run_until_parked();
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_c);

    assert!(sidebar.update(cx, |sidebar, _cx| sidebar.thread_last_accessed.is_empty()));

    // Confirm on Thread C.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    // Switcher should be dismissed after confirm.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.thread_switcher.is_none(),
            "switcher should be dismissed"
        );
    });

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 1);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(
            is_active_session(&sidebar, &session_id_c),
            "active_entry should be Thread({session_id_c:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_c, thread_id_a, thread_id_b],
    );

    // Confirm on Thread A.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 2);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(
            is_active_session(&sidebar, &session_id_a),
            "active_entry should be Thread({session_id_a:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_c, thread_id_b,],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        switcher.update(cx, |switcher, cx| switcher.cycle_selection(cx));
    });
    cx.run_until_parked();

    // Confirm on Thread B.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 3);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(last_accessed.contains(&thread_id_b));
        assert!(
            is_active_session(&sidebar, &session_id_b),
            "active_entry should be Thread({session_id_b:?})"
        );
    });

    // ── 3. Add a historical thread (no last_accessed_at, no message sent) ──
    // This thread was never opened in a panel — it only exists in metadata.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-historical")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Historical Thread has no last_accessed_at and no last_message_sent_or_queued,
    // so it falls to tier 3 (sorted by created_at). It should appear after all
    // accessed threads, even though its created_at (June 2024) is much later
    // than the others.
    //
    // But the live threads (A, B, C) each had send_message called which sets
    // last_message_sent_or_queued. So for the accessed threads (tier 1) the
    // sort key is last_accessed_at; for Historical Thread (tier 3) it's created_at.
    let session_id_hist = acp::SessionId::new(Arc::from("thread-historical"));
    let thread_id_hist = thread_id_for(&session_id_hist, cx);

    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![thread_id_b, thread_id_a, thread_id_c, thread_id_hist],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 4. Add another historical thread with older created_at ─────────
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-old-historical")),
        Some("Old Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Both historical threads have no access or message times. They should
    // appear after accessed threads, sorted by created_at (newest first).
    let session_id_old_hist = acp::SessionId::new(Arc::from("thread-old-historical"));
    let thread_id_old_hist = thread_id_for(&session_id_old_hist, cx);
    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![
            thread_id_b,
            thread_id_a,
            thread_id_c,
            thread_id_hist,
            thread_id_old_hist,
        ],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
// Rewritten for the merged history model: archived threads stay in the
// sidebar list (rendered muted) instead of being hidden.
async fn test_archive_thread_keeps_metadata_and_stays_listed(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-to-archive")),
        Some("Thread To Archive".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Thread To Archive")),
        "expected thread to be visible before archiving, got: {entries:?}"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(
            &acp::SessionId::new(Arc::from("thread-to-archive")),
            window,
            cx,
        );
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Thread To Archive") && e.contains("(archived)")),
        "expected thread to stay listed (archived) after archiving, got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "thread-to-archive"
        );
        assert!(archived[0].archived);
    });
}

// Rewritten from test_archive_thread_drops_retained_conversation_view:
// there is no retained cache anymore; archiving must close the thread's
// tab, which is what "open in Zed" means.
#[gpui::test]
async fn test_archive_thread_closes_its_tab(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    let thread_id = active_thread_id(&panel, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(sidebar, &session_id),
            "expected the newly created thread to be active before archiving",
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.open_thread_tab_ids(cx).contains(&thread_id),
            "archiving a thread must close its tab, but the archived thread \
             id {thread_id:?} is still open",
        );
    });
}

#[gpui::test]
async fn test_archive_thread_active_entry_management(cx: &mut TestAppContext) {
    // Tests two archive scenarios:
    // 1. Archiving a thread in a non-active workspace leaves active_entry
    //    as the current draft.
    // 2. Archiving the thread the user is looking at falls back to a draft
    //    on the same workspace.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // --- Scenario 1: archive a thread in the non-active workspace ---

    // Create a thread in project-a (non-active — project-b is active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // active_entry should still be a draft on workspace_b (the active one).
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected Draft(workspace_b) after archiving non-active thread, got: {:?}",
            sidebar.active_entry,
        );
    });

    // --- Scenario 2: archive the thread the user is looking at ---

    // Create a thread in project-b (the active workspace) and verify it
    // becomes the active entry.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let thread_b = agent_ui::test_support::active_session_id(&panel_b, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(&sidebar, &thread_b),
            "expected active_entry to be Thread({thread_b}), got: {:?}",
            sidebar.active_entry,
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_b, window, cx);
    });
    cx.run_until_parked();

    // Archiving the active thread activates a draft on the same workspace
    // (via clear_base_view → activate_draft). The draft is not shown as a
    // sidebar row but active_entry tracks it.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected draft on workspace_b after archiving active thread, got: {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_unarchive_only_shows_restored_thread(cx: &mut TestAppContext) {
    // Full flow: create a thread, archive it (removing the workspace),
    // then unarchive. Only the restored thread should appear — no
    // leftover drafts or previously-serialized threads.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's a real thread.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hello".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive it.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Grab metadata for unarchive.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    // Unarchive it — the draft should be replaced by the restored thread.
    let restored_title = metadata.display_title();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The restored thread should be visible. A fresh draft may also be
    // visible as a sidebar row: archive_thread auto-activates one via
    // clear_base_view, and the unarchive then parks it by pushing the
    // restored thread into the base view.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains(restored_title.as_ref())),
        "expected the restored thread to be visible, got entries: {entries:?}"
    );
    let thread_count = entries
        .iter()
        .filter(|e| !e.starts_with("v ") && !e.starts_with("> "))
        .count();
    assert!(
        thread_count <= 2,
        "expected at most the restored thread plus a parked draft, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_first_thread_in_group_does_not_create_spurious_draft(
    cx: &mut TestAppContext,
) {
    // When a thread is unarchived into a project group that has no open
    // workspace, the sidebar opens a new workspace and loads the thread.
    // No spurious draft should appear alongside the unarchived thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    // Save an archived thread whose folder_paths point to project-b,
    // which has no open workspace.
    let session_id = acp::SessionId::new(Arc::from("archived-thread"));
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    // Verify no workspace for project-b exists yet.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with only the project-a workspace"
    );

    // Un-archive the thread — should open project-b workspace and load it.
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // A second workspace should have been created for project-b.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a workspace for the unarchived thread"
    );

    // The sidebar should show the unarchived thread without a spurious draft
    // in the project-b group.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    // project-a gets a draft (it's the active workspace with no threads),
    // but project-b should NOT have one — only the unarchived thread.
    assert!(
        draft_count <= 1,
        "expected at most one draft (for project-a), got entries: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Unarchived Thread")),
        "expected unarchived thread to appear, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_new_workspace_does_not_create_duplicate_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("restore-into-new-workspace"));
    let path_list_b = PathList::new(&[PathBuf::from("/project-b")]);
    let original_thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id: original_thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist before unarchive")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "expected unarchive to open the target workspace"
    );

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| PathList::new(&workspace.read(cx).root_paths(cx)) == path_list_b)
            .cloned()
            .expect("expected restored workspace for unarchived thread")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected unarchive to install an agent panel in the new workspace")
    });

    let restored_thread_id = restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
    assert_eq!(
        restored_thread_id,
        Some(original_thread_id),
        "expected the new workspace's agent panel to target the restored archived thread id"
    );

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for restored session after opening a new workspace, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected restore into a new workspace to reuse the original thread id"
    );
    assert!(
        !session_entries[0].archived,
        "expected restored thread metadata to be unarchived, got: {:?}",
        session_entries[0]
    );

    let mapped_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
    });
    assert_eq!(
        mapped_thread_id,
        Some(original_thread_id),
        "expected session mapping to remain stable after opening the new workspace"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        .count();
    // Restoring opens the thread, which puts its one row in Active and takes
    // it out of All Threads.
    assert_eq!(
        real_thread_rows, 1,
        "expected the restored thread as a single Active row after restore into a new workspace, got entries: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.contains("Unarchived Thread")),
        "expected restored thread row to be visible, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_existing_workspace_replaces_draft(cx: &mut TestAppContext) {
    // When a workspace already exists with an empty draft and a thread
    // is unarchived into it, the draft should be replaced — not kept
    // alongside the loaded thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's no longer a draft.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive the thread — the group is left empty (no draft created).
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Un-archive the thread.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The draft should be gone — only the unarchived thread remains.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    assert_eq!(
        draft_count, 0,
        "expected no drafts after unarchiving, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_inactive_existing_workspace_does_not_leave_active_draft(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("unarchive-into-inactive-existing-workspace"));
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Restored In Inactive Workspace".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                        PathBuf::from("/project-b"),
                    ])),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should exist before restore")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    let panel_b_before_settle = workspace_b.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).expect(
            "target workspace should still have an agent panel immediately after activation",
        )
    });
    let immediate_active_thread_id =
        panel_b_before_settle.read_with(cx, |panel, cx| panel.active_thread_id(cx));

    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "unarchiving into an inactive existing workspace should end on the restored thread",
        );
    });

    let panel_b = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("target workspace should still have an agent panel")
    });
    assert_eq!(
        panel_b.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(thread_id),
        "expected target panel to activate the restored thread id"
    );
    assert!(
        immediate_active_thread_id.is_none() || immediate_active_thread_id == Some(thread_id),
        "expected immediate panel state to be either still loading or already on the restored thread, got active_thread_id={immediate_active_thread_id:?}"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let target_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains("Restored In Inactive Workspace") || entry.contains("Draft"))
        .cloned()
        .collect();
    // Restoring opens the thread, so its one row is in Active, and no draft
    // survives alongside it.
    assert_eq!(
        target_rows.len(),
        1,
        "expected the restored row as a single Active row, and no surviving draft in the target group, got entries: {entries:?}"
    );
    assert!(
        target_rows
            .iter()
            .all(|row| row.contains("Restored In Inactive Workspace")),
        "expected the remaining rows to be the restored thread, got entries: {entries:?}"
    );
    assert!(
        !target_rows[0].contains("Draft"),
        "expected no surviving draft row after unarchive into inactive existing workspace, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_after_removing_parent_project_group_restores_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id, &project_b, cx).await;
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });

    cx.run_until_parked();

    let archived_metadata = cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let thread_id = store
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("archived thread should still exist in metadata store");
        let metadata = store
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should still exist after archive");
        assert!(
            metadata.archived,
            "thread should be archived before project removal"
        );
        metadata
    });

    let group_key_b =
        project_b.read_with(cx, |project, cx| ProjectGroupKey::from_project(project, cx));
    let remove_task = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.remove_project_group(&group_key_b, window, cx)
    });
    remove_task
        .await
        .expect("remove project group task should complete");
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "removing the archived thread's parent project group should remove its workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(archived_metadata.clone(), window, cx);
    });
    cx.run_until_parked();

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == PathList::new(&[PathBuf::from("/project-b")])
            })
            .cloned()
            .expect("expected unarchive to recreate the removed project workspace")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected restored workspace to bootstrap an agent panel")
    });

    let restored_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("session should still map to restored thread id")
    });
    assert_eq!(
        restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(restored_thread_id),
        "expected unarchive after project removal to activate the restored real thread"
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "expected sidebar active entry to track the restored thread after project removal",
        );
    });

    let entries = visible_entries_as_strings(&sidebar, cx);
    let restored_title = archived_metadata.display_title().to_string();
    let matching_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains(&restored_title) || entry.contains("Draft"))
        .cloned()
        .collect();
    // Unarchiving opens the thread, so its one row is in Active, and no draft
    // survives alongside it.
    assert_eq!(
        matching_rows.len(),
        1,
        "expected the restored row as a single Active row, and no surviving draft after unarchive following project removal, got entries: {entries:?}"
    );
    assert!(
        matching_rows.iter().all(|row| !row.contains("Draft")),
        "expected no draft row after unarchive following project removal, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_does_not_create_duplicate_real_thread_metadata(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    let original_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store before archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist after archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });

    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for the restored session, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected unarchive to reuse the original thread id instead of creating a duplicate row"
    );
    assert!(
        session_entries[0].session_id.is_some(),
        "expected restored metadata to be a real thread, got: {:?}",
        session_entries[0]
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        // Parked drafts render with the default title until the user types.
        .filter(|entry| !entry.contains(DEFAULT_THREAD_TITLE))
        .count();
    // Unarchiving opens the thread, which puts its one row in Active and takes
    // it out of All Threads.
    assert_eq!(
        real_thread_rows, 1,
        "expected the restored thread as a single Active row after unarchive, got entries: {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry.contains("Draft")),
        "expected no draft rows after restoring, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_switch_to_workspace_with_archived_thread_shows_no_active_entry(
    cx: &mut TestAppContext,
) {
    // When a thread is archived while the user is in a different workspace,
    // clear_base_view creates a draft on the archived workspace's panel.
    // Switching back to that workspace shows the draft as active_entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Create a thread in project-a's panel (currently non-active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    // Archive it while project-b is active.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // Switch back to project-a. Its panel was cleared during archiving
    // (clear_base_view activated a draft), so active_entry should point
    // to the draft on workspace_a.
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert_active_draft(
            sidebar,
            &workspace_a,
            "after switching to workspace with archived thread, active_entry should be the draft",
        );
    });
}

#[gpui::test]
// Rewritten for the merged history model: archived threads are included in
// the sidebar list, marked archived, instead of being excluded.
async fn test_archived_threads_included_in_sidebar_entries(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("visible-thread")),
        Some("Visible Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let archived_thread_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_thread_metadata(
        archived_thread_session_id.clone(),
        Some("Archived Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            let thread_id = store
                .entries()
                .find(|e| e.session_id.as_ref() == Some(&archived_thread_session_id))
                .map(|e| e.thread_id)
                .unwrap();
            store.archive(thread_id, None, cx)
        })
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Visible Thread")),
        "expected visible thread in sidebar, got: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Archived Thread") && e.contains("(archived)")),
        "expected archived thread to stay listed (archived), got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let all: Vec<_> = store.read(cx).entries().collect();
        assert_eq!(
            all.len(),
            2,
            "expected 2 total entries in the store, got: {}",
            all.len()
        );

        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "archived-thread"
        );
    });
}

#[gpui::test]
async fn test_archive_last_thread_on_linked_worktree_does_not_create_new_thread_on_worktree(
    cx: &mut TestAppContext,
) {
    // When a linked worktree has a single thread and that thread is archived,
    // the sidebar must NOT create a new thread on the same worktree (which
    // would prevent the worktree from being cleaned up on disk). Instead,
    // archive_thread switches to a sibling thread on the main workspace (or
    // creates a draft there) before archiving the metadata.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Set up both workspaces with agent panels.
    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Activate the linked worktree workspace so the sidebar tracks it.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });

    // Open a thread in the linked worktree panel and send a message
    // so it becomes the active thread.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let worktree_thread_id = active_session_id(&worktree_panel, cx);

    // Give the thread a response chunk so it has content.
    cx.update(|_, cx| {
        connection.send_update(
            worktree_thread_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("done".into())),
            cx,
        );
    });

    // Save the worktree thread's metadata.
    save_thread_metadata(
        worktree_thread_id.clone(),
        Some("Ochre Drift Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    // Also save a thread on the main project so there's a sibling in the
    // group that can be selected after archiving.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-project-thread")),
        Some("Main Project Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    cx.run_until_parked();

    // Verify the linked worktree thread appears with its chip.
    // The live thread title comes from the message text ("Hello"), not
    // the metadata title we saved.
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before
            .iter()
            .any(|s| s.contains("{wt-ochre-drift}")),
        "expected worktree thread with chip before archiving, got: {entries_before:?}"
    );
    assert!(
        entries_before
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "expected main project thread before archiving, got: {entries_before:?}"
    );

    // Confirm the worktree thread is the active entry.
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &worktree_thread_id,
            "worktree thread should be active before archiving",
        );
    });

    // Archive the worktree thread — it's the only thread using ochre-drift.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The archived thread stays in the merged history list, marked archived.
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_after
            .iter()
            .any(|s| s.contains("Ochre Drift Thread") && s.contains("(archived)")),
        "archived thread should stay listed as archived, got: {entries_after:?}"
    );

    // No "+ New Thread" entry should appear with the ochre-drift worktree
    // chip — that would keep the worktree alive and prevent cleanup. Only
    // the archived row itself may carry the chip.
    assert!(
        entries_after
            .iter()
            .all(|s| !s.contains("{wt-ochre-drift}") || s.contains("(archived)")),
        "only the archived row may reference the archived worktree, got: {entries_after:?}"
    );

    // The main project thread should still be visible.
    assert!(
        entries_after
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "main project thread should still be visible, got: {entries_after:?}"
    );
}

#[gpui::test]
async fn test_archive_last_thread_on_linked_worktree_with_no_siblings_leaves_group_empty(
    cx: &mut TestAppContext,
) {
    // When a linked worktree thread is the ONLY thread in the project group
    // (no threads on the main repo either), archiving it should leave the
    // group empty with no active entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Activate the linked worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });

    // Open a thread on the linked worktree — this is the ONLY thread.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let worktree_thread_id = active_session_id(&worktree_panel, cx);

    cx.update(|_, cx| {
        connection.send_update(
            worktree_thread_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("done".into())),
            cx,
        );
    });

    save_thread_metadata(
        worktree_thread_id.clone(),
        Some("Ochre Drift Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    cx.run_until_parked();

    // Archive it — there are no other threads in the group.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_thread_id, window, cx);
    });

    cx.run_until_parked();

    let entries_after = visible_entries_as_strings(&sidebar, cx);

    // The archived thread stays listed (with its worktree chip); only the
    // archived row may reference the linked worktree.
    assert!(
        entries_after
            .iter()
            .all(|s| !s.contains("{wt-ochre-drift}") || s.contains("(archived)")),
        "only the archived row may reference the archived worktree, got: {entries_after:?}"
    );

    // The active entry should be None — no draft is created.
    sidebar.read_with(cx, |s, _| {
        assert!(
            s.active_entry.is_none(),
            "expected no active entry after archiving the last thread, got: {:?}",
            s.active_entry,
        );
    });
}

#[gpui::test]
async fn test_unarchive_linked_worktree_thread_into_project_group_shows_only_restored_real_thread(
    cx: &mut TestAppContext,
) {
    // When an archived thread belongs to a linked worktree whose main repo is
    // already open, unarchiving should reopen the linked workspace into the
    // same project group and show only the restored real thread row.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-ochre-drift",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/ochre-drift",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("linked-worktree-unarchive"));
    let original_thread_id = ThreadId::new();
    let main_paths = PathList::new(&[PathBuf::from("/project")]);
    let folder_paths = PathList::new(&[PathBuf::from("/wt-ochre-drift")]);

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id: original_thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Linked Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_path_lists(
                        main_paths.clone(),
                        folder_paths.clone(),
                    )
                    .expect("main and folder paths should be well-formed"),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("archived linked-worktree metadata should exist before restore")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "expected unarchive to open the linked worktree workspace into the project group"
    );

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for restored linked worktree session, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected unarchive to reuse the original linked worktree thread id"
    );
    assert!(
        !session_entries[0].archived,
        "expected restored linked worktree metadata to be unarchived, got: {:?}",
        session_entries[0]
    );

    let assert_no_extra_rows = |entries: &[String]| {
        let real_thread_rows = entries
            .iter()
            .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
            .filter(|entry| !entry.contains("Draft"))
            .count();
        // Unarchiving opens the thread, which puts its one row in Active and
        // takes it out of All Threads.
        assert_eq!(
            real_thread_rows, 1,
            "expected the restored thread as a single Active row after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            !entries.iter().any(|entry| entry.contains("Draft")),
            "expected no draft rows after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.contains(DEFAULT_THREAD_TITLE)),
            "expected no default-titled real placeholder row after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.contains("Unarchived Linked Thread")),
            "expected restored linked worktree thread row to be visible, got entries: {entries:?}"
        );
    };

    let entries_after_restore = visible_entries_as_strings(&sidebar, cx);
    assert_no_extra_rows(&entries_after_restore);

    // The reported bug may only appear after an extra scheduling turn.
    cx.run_until_parked();

    let entries_after_extra_turns = visible_entries_as_strings(&sidebar, cx);
    assert_no_extra_rows(&entries_after_extra_turns);
}

#[gpui::test]
async fn test_archive_thread_on_linked_worktree_selects_sibling_thread(cx: &mut TestAppContext) {
    // When a linked worktree thread is archived but the group has other
    // threads (e.g. on the main project), archive_thread should select
    // the nearest sibling.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Activate the linked worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });

    // Open a thread on the linked worktree.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let worktree_thread_id = active_session_id(&worktree_panel, cx);

    cx.update(|_, cx| {
        connection.send_update(
            worktree_thread_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("done".into())),
            cx,
        );
    });

    save_thread_metadata(
        worktree_thread_id.clone(),
        Some("Ochre Drift Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    // Save a sibling thread on the main project.
    let main_thread_id = acp::SessionId::new(Arc::from("main-project-thread"));
    save_thread_metadata(
        main_thread_id,
        Some("Main Project Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    cx.run_until_parked();

    // Confirm the worktree thread is active.
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &worktree_thread_id,
            "worktree thread should be active before archiving",
        );
    });

    // Archive the worktree thread.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The worktree workspace was removed. Only the archived row may still
    // reference the linked worktree.
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_after
            .iter()
            .all(|s| !s.contains("{wt-ochre-drift}") || s.contains("(archived)")),
        "only the archived row may reference the archived worktree, got: {entries_after:?}"
    );

    // The main project thread should still be visible.
    assert!(
        entries_after
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "main project thread should still be visible, got: {entries_after:?}"
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_shows_main_worktree_threads(cx: &mut TestAppContext) {
    // When only a linked worktree workspace is open (not the main repo),
    // threads saved against the main repo should still appear in the sidebar.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Create the main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        std::path::Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only open the linked worktree as a workspace — NOT the main repo.
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread against the MAIN repo path.
    save_named_thread_metadata("main-thread", "Main Repo Thread", &main_project, cx).await;

    // Save a thread against the linked worktree path.
    save_named_thread_metadata("wt-thread", "Worktree Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Both threads should be visible: the worktree thread by direct lookup,
    // and the main repo thread because the workspace is a linked worktree
    // and we also query the main repo path.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Main Repo Thread")),
        "expected main repo thread to be visible in linked worktree workspace, got: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Worktree Thread")),
        "expected worktree thread to be visible, got: {entries:?}"
    );
}

async fn init_multi_project_test(
    paths: &[&str],
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
    let fs = FakeFs::new(cx.executor());
    for path in paths {
        fs.insert_tree(path, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [paths[0].as_ref()], cx).await;
    (fs, project)
}

async fn add_test_project(
    path: &str,
    fs: &Arc<FakeFs>,
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Workspace> {
    let project = project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [path.as_ref()], cx).await;
    let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project, window, cx)
    });
    cx.run_until_parked();
    workspace
}

#[gpui::test]
async fn test_workspace_lifecycle_retains_projects_when_sidebar_is_closed(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert!(!multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_a));

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));
}

#[gpui::test]
async fn test_workspaces_remain_retained_after_sidebar_closes(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(
        &["/project-a", "/project-b", "/project-c", "/project-d"],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a, None, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| mw.close_sidebar(window, cx));
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));

    let workspace_d = add_test_project("/project-d", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        4
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_d));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_c)));
}

#[gpui::test]
async fn test_toggle_from_inside_the_sidebar_closes_it(cx: &mut TestAppContext) {
    let project = init_test_project("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));

    // The header's toggle button dispatches the action from wherever focus
    // happens to be, which is inside the sidebar once it has been opened.
    cx.update(|window, cx| {
        let handle = sidebar.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(workspace::ToggleWorkspaceSidebar);
    cx.run_until_parked();

    assert!(
        !multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()),
        "expected the toggle action to close the sidebar"
    );
}

#[gpui::test]
async fn test_clicking_the_header_toggle_closes_the_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    setup_sidebar(&multi_workspace, cx);
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(1200.), px(800.)),
        |_, _| gpui::Empty,
    );
    cx.run_until_parked();

    let bounds = cx
        .debug_bounds("ICON-ThreadsSidebarLeftOpen")
        .expect("the sidebar header should show its collapse button");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert!(
        !multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()),
        "expected clicking the header toggle to close the sidebar"
    );
}

#[gpui::test]
async fn test_header_toggle_is_present_without_open_projects(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    setup_sidebar(&multi_workspace, cx);
    cx.draw(
        gpui::Point::default(),
        gpui::size(px(1200.), px(800.)),
        |_, _| gpui::Empty,
    );
    cx.run_until_parked();

    let bounds = cx
        .debug_bounds("ICON-ThreadsSidebarLeftOpen")
        .expect("an empty sidebar still needs a way to collapse itself");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert!(
        !multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()),
        "expected clicking the header toggle to close the sidebar"
    );
}

#[gpui::test]
async fn test_sidebar_opening_keeps_existing_retained_workspaces(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
}

#[gpui::test]
async fn test_legacy_thread_with_canonical_path_opens_main_repo_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only a linked worktree workspace is open — no workspace for /project.
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a legacy thread: folder_paths = main repo, main_worktree_paths = empty.
    let legacy_session = acp::SessionId::new(Arc::from("legacy-main-thread"));
    cx.update(|_, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(legacy_session.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Legacy Main Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            archived: false,
            remote_connection: None,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // The legacy thread should appear in the sidebar under the project group.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Legacy Main Thread")),
        "legacy thread should be visible: {entries:?}",
    );

    // Verify only 1 workspace before clicking.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
    );

    // Focus and select the legacy thread, then confirm.
    focus_sidebar(&sidebar, cx);
    let thread_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|e| e.session_id().is_some_and(|id| id == &legacy_session))
            .expect("legacy thread should be in entries")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(thread_index);
    });
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    let new_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let new_path_list =
        new_workspace.read_with(cx, |_, cx| workspace_path_list(&new_workspace, cx));
    assert_eq!(
        new_path_list,
        PathList::new(&[PathBuf::from("/project")]),
        "the new workspace should be for the main repo, not the linked worktree",
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_reachable_after_adding_unrelated_project(
    cx: &mut TestAppContext,
) {
    // Regression test for a property-test finding:
    //   AddLinkedWorktree { project_group_index: 0 }
    //   AddProject { use_worktree: true }
    //   AddProject { use_worktree: false }
    // After these three steps, the linked-worktree workspace was not
    // reachable from any sidebar entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);

        cx.observe_new(
            |workspace: &mut Workspace,
             window: Option<&mut Window>,
             cx: &mut gpui::Context<Workspace>| {
                if let Some(window) = window {
                    let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                    workspace.add_panel(panel, window, cx);
                }
            },
        )
        .detach();
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/my-project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/my-project".as_ref()], cx).await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Step 1: Create a linked worktree for the main project.
    let worktree_name = "wt-0";
    let worktree_path = "/worktrees/wt-0";

    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": "gitdir: /my-project/.git/worktrees/wt-0",
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/my-project/.git/worktrees/wt-0",
        serde_json::json!({
            "commondir": "../../",
            "HEAD": "ref: refs/heads/wt-0",
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/my-project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from(worktree_path),
            ref_name: Some(format!("refs/heads/{}", worktree_name).into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    let main_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let main_project = main_workspace.read_with(cx, |ws, _| ws.project().clone());
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    cx.run_until_parked();

    // Step 2: Open the linked worktree as its own workspace.
    let worktree_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [worktree_path.as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });
    cx.run_until_parked();

    // Step 3: Add an unrelated project.
    fs.insert_tree(
        "/other-project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    let other_project = project::Project::test(
        fs.clone() as Arc<dyn fs::Fs>,
        ["/other-project".as_ref()],
        cx,
    )
    .await;
    other_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(other_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Force a full sidebar rebuild with all groups expanded.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        if let Some(mw) = sidebar.multi_workspace.upgrade() {
            mw.update(cx, |mw, _cx| mw.test_expand_all_groups());
        }
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    // The merged history model dropped project headers, so workspaces
    // without rows are not reachable from the sidebar (the recent-projects
    // menu covers navigation). The invariant that remains is that no sidebar
    // entry references a workspace the multi-workspace doesn't know.
    let worktree_ws_id = worktree_workspace.entity_id();
    let (all_ids, reachable_ids) = sidebar.read_with(cx, |sidebar, cx| {
        let mw = multi_workspace.read(cx);

        let all: HashSet<gpui::EntityId> = mw.workspaces().map(|ws| ws.entity_id()).collect();
        let reachable: HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(mw, cx))
            .map(|ws| ws.entity_id())
            .collect();
        (all, reachable)
    });

    let dangling = &reachable_ids - &all_ids;

    assert!(
        dangling.is_empty(),
        "sidebar entries reference unknown workspaces: {:?}\n\
         (linked-worktree workspace id: {:?})",
        dangling,
        worktree_ws_id,
    );
}

#[gpui::test]
async fn test_startup_failed_restoration_shows_no_draft(cx: &mut TestAppContext) {
    // Empty project groups no longer auto-create drafts via reconciliation.
    // A fresh startup with no restorable thread should show only the header.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let _workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries,
        Vec::<String>::new(),
        "a fresh startup with no restorable thread should show no rows"
    );
}

#[gpui::test]
async fn test_startup_successful_restoration_no_spurious_draft(cx: &mut TestAppContext) {
    // Rule 5: When the app starts and the AgentPanel successfully loads
    // a thread, no spurious draft should appear.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create and send a message to make a real thread.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Should show the thread, NOT a spurious draft. It's open, so its one
    // row is in Active.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries, vec!["  Hello *"]);

    // active_entry should be Thread, not Draft.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &session_id, "should be on the thread, not a draft");
    });
}

#[gpui::test]
async fn test_project_header_click_restores_last_viewed(cx: &mut TestAppContext) {
    // Rule 9: Clicking a project header should restore whatever the
    // user was last looking at in that group, not create new drafts
    // or jump to the first entry.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create two threads in project-a.
    let conn1 = StubAgentConnection::new();
    conn1.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn1, cx);
    send_message(&panel_a, cx);
    let thread_a1 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a1, &project_a, cx).await;

    let conn2 = StubAgentConnection::new();
    conn2.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn2, cx);
    send_message(&panel_a, cx);
    let thread_a2 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a2, &project_a, cx).await;
    cx.run_until_parked();

    // The user is now looking at thread_a2.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &thread_a2, "should be on thread_a2");
    });

    // Add project-b and switch to it.
    let fs = cx.update(|_window, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Now switch BACK to project-a by activating its workspace.
    let workspace_a = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|ws| {
                ws.read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .any(|wt| {
                        wt.read(cx)
                            .abs_path()
                            .to_string_lossy()
                            .contains("project-a")
                    })
            })
            .unwrap()
            .clone()
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    // The panel should still show thread_a2 (the last thing the user
    // was viewing in project-a), not a draft or thread_a1.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &thread_a2,
            "switching back to project-a should restore thread_a2",
        );
    });

    // No spurious draft entries should have been created. The merged
    // history list has no per-project sections, so check the whole list
    // (project-b may have an empty-draft placeholder, which renders as a
    // "New ... Thread" row, not a "Draft" one).
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_rows = entries.iter().filter(|e| e.contains("Draft")).count();
    assert_eq!(
        draft_rows, 0,
        "switching back to project-a should not create drafts: {entries:?}"
    );
}

#[gpui::test]
async fn test_activating_workspace_with_draft_does_not_create_extras(cx: &mut TestAppContext) {
    // When a workspace has a draft (from the panel's load fallback)
    // and the user activates it (e.g. by clicking the placeholder or
    // the project header), no extra drafts should be created.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let _panel_a = add_agent_panel(&workspace_a, cx);
    cx.run_until_parked();

    // Add project-b with its own workspace and agent panel.
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // Count project-b's drafts.
    let count_b_drafts = |cx: &mut gpui::VisualTestContext| {
        let entries = visible_entries_as_strings(&sidebar, cx);
        entries
            .iter()
            .skip_while(|e| !e.contains("project-b"))
            .take_while(|e| !e.starts_with("v ") || e.contains("project-b"))
            .filter(|e| e.contains("Draft"))
            .count()
    };
    let drafts_before = count_b_drafts(cx);

    // Switch away from project-b, then back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_b.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let drafts_after = count_b_drafts(cx);
    assert_eq!(
        drafts_before, drafts_after,
        "activating workspace should not create extra drafts"
    );

    // The draft should be highlighted as active after switching back.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_draft(
            sidebar,
            &workspace_b,
            "draft should be active after switching back to its workspace",
        );
    });
}

#[gpui::test]
async fn test_non_archive_thread_paths_migrate_on_worktree_add_and_remove(cx: &mut TestAppContext) {
    // Historical threads (not open in any agent panel) should have their
    // worktree paths updated when a folder is added to or removed from the
    // project.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save two threads directly into the metadata store (not via the agent
    // panel), so they are purely historical — no open views hold them.
    // Use different timestamps so sort order is deterministic.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical 1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-2")),
        Some("Historical 2".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // Sanity-check: both threads exist under the initial key [/project-a].
    let old_key_paths = PathList::new(&[PathBuf::from("/project-a")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key before worktree add"
        );
    });

    // Add a second worktree to the project.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The historical threads should now be indexed under the new combined
    // key [/project-a, /project-b].
    let new_key_paths = PathList::new(&[PathBuf::from("/project-a"), PathBuf::from("/project-b")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under old key after worktree add"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under new key after worktree add"
        );
    });

    // Sidebar should show threads under the new header.
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Historical 2", "  Historical 1",]
    );

    // Now remove the second worktree.
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .find(|wt| wt.read(cx).abs_path().as_ref() == Path::new("/project-b"))
            .map(|wt| wt.read(cx).id())
            .expect("should find project-b worktree")
    });
    project.update(cx, |project, cx| {
        project.remove_worktree(worktree_id, cx);
    });
    cx.run_until_parked();

    // Historical threads should migrate back to the original key.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under new key after worktree remove"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key after worktree remove"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Historical 2", "  Historical 1",]
    );
}

#[gpui::test]
async fn test_worktree_add_only_regroups_threads_for_changed_workspace(cx: &mut TestAppContext) {
    // When two workspaces share the same project group (same main path)
    // but have different folder paths (main repo vs linked worktree),
    // adding a worktree to the main workspace should regroup only that
    // workspace and its threads into the new project group. Threads for the
    // linked worktree workspace should remain under the original group.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature"),
            ref_name: Some("refs/heads/feature".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace A: main repo at /project.
    let main_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/project".as_ref()], cx).await;
    // Workspace B: linked worktree of the same repo (same group, different folder).
    let worktree_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/wt-feature".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Save a thread for each workspace's folder paths.
    let time_main = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap();
    let time_wt = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 2).unwrap();
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-main")),
        Some("Main Thread".into()),
        time_main,
        Some(time_main),
        None,
        &main_project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-wt")),
        Some("Worktree Thread".into()),
        time_wt,
        Some(time_wt),
        None,
        &worktree_project,
        cx,
    );
    cx.run_until_parked();

    let folder_paths_main = PathList::new(&[PathBuf::from("/project")]);
    let folder_paths_wt = PathList::new(&[PathBuf::from("/wt-feature")]);

    // Sanity-check: each thread is indexed under its own folder paths, but
    // both appear under the shared sidebar group keyed by the main worktree.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store.entries_for_path(&folder_paths_main, None).count(),
            1,
            "one thread under [/project]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_wt, None).count(),
            1,
            "one thread under [/wt-feature]"
        );
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Worktree Thread {wt-feature}", "  Main Thread",]
    );

    // Add /project-b to the main project only.
    main_project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // Main Thread (folder paths [/project]) should be regrouped to
    // [/project, /project-b]. Worktree Thread should remain under the
    // original [/project] group.
    let folder_paths_main_b =
        PathList::new(&[PathBuf::from("/project"), PathBuf::from("/project-b")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store.entries_for_path(&folder_paths_main, None).count(),
            0,
            "main thread should no longer be under old folder paths [/project]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_main_b, None).count(),
            1,
            "main thread should now be under [/project, /project-b]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_wt, None).count(),
            1,
            "worktree thread should remain unchanged under [/wt-feature]"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Worktree Thread {wt-feature}", "  Main Thread",]
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_reachable_after_adding_worktree_to_project(
    cx: &mut TestAppContext,
) {
    // When a linked worktree is opened as its own workspace and then a new
    // folder is added to the main project group, the linked worktree
    // workspace must still be reachable from some sidebar entry.
    let (_fs, project) = init_multi_project_test(&["/my-project"], cx).await;
    let fs = _fs.clone();

    // Set up git worktree infrastructure.
    fs.insert_tree(
        "/my-project/.git/worktrees/wt-0",
        serde_json::json!({
            "commondir": "../../",
            "HEAD": "ref: refs/heads/wt-0",
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/wt-0",
        serde_json::json!({
            ".git": "gitdir: /my-project/.git/worktrees/wt-0",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/my-project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/wt-0"),
            ref_name: Some("refs/heads/wt-0".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    // Re-scan so the main project discovers the linked worktree.
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Open the linked worktree as its own workspace.
    let worktree_project = project::Project::test(
        fs.clone() as Arc<dyn fs::Fs>,
        ["/worktrees/wt-0".as_ref()],
        cx,
    )
    .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Both workspaces should be reachable.
    let workspace_count = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(workspace_count, 2, "should have 2 workspaces");

    // Add a new folder to the main project, changing the project group key.
    fs.insert_tree(
        "/other-project",
        serde_json::json!({ ".git": {}, "src": {} }),
    )
    .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/other-project", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // The merged history model dropped project headers, so workspaces
    // without rows are not reachable from the sidebar (the recent-projects
    // menu covers navigation). The invariant that remains is that no sidebar
    // entry references a workspace the multi-workspace doesn't know.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let mw_workspaces: Vec<_> = multi_workspace.read_with(cx, |mw, _| {
        mw.workspaces().map(|ws| ws.entity_id()).collect()
    });
    sidebar.read_with(cx, |sidebar, cx| {
        let multi_workspace = multi_workspace.read(cx);
        let reachable: std::collections::HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(multi_workspace, cx))
            .map(|ws| ws.entity_id())
            .collect();
        let all: std::collections::HashSet<gpui::EntityId> =
            mw_workspaces.iter().copied().collect();
        let dangling = &reachable - &all;
        assert!(
            dangling.is_empty(),
            "sidebar entries reference unknown workspaces after adding folder; \
             dangling: {:?}, entries: {:?}",
            dangling,
            entries,
        );
    });
}

mod property_test {
    use super::*;
    use gpui::proptest::prelude::*;

    struct UnopenedWorktree {
        path: String,
        main_workspace_path: String,
    }

    struct TestState {
        fs: Arc<FakeFs>,
        thread_counter: u32,
        workspace_counter: u32,
        worktree_counter: u32,
        saved_thread_ids: Vec<acp::SessionId>,
        unopened_worktrees: Vec<UnopenedWorktree>,
    }

    impl TestState {
        fn new(fs: Arc<FakeFs>) -> Self {
            Self {
                fs,
                thread_counter: 0,
                workspace_counter: 1,
                worktree_counter: 0,
                saved_thread_ids: Vec::new(),
                unopened_worktrees: Vec::new(),
            }
        }

        fn next_metadata_only_thread_id(&mut self) -> acp::SessionId {
            let id = self.thread_counter;
            self.thread_counter += 1;
            acp::SessionId::new(Arc::from(format!("prop-thread-{id}")))
        }

        fn next_workspace_path(&mut self) -> String {
            let id = self.workspace_counter;
            self.workspace_counter += 1;
            format!("/prop-project-{id}")
        }

        fn next_worktree_name(&mut self) -> String {
            let id = self.worktree_counter;
            self.worktree_counter += 1;
            format!("wt-{id}")
        }
    }

    #[derive(Debug)]
    enum Operation {
        SaveThread { project_group_index: usize },
        SaveWorktreeThread { worktree_index: usize },
        ToggleAgentPanel,
        CreateDraftThread,
        AddProject { use_worktree: bool },
        ArchiveThread { index: usize },
        SwitchToThread { index: usize },
        SwitchToProjectGroup { index: usize },
        AddLinkedWorktree { project_group_index: usize },
        AddWorktreeToProject { project_group_index: usize },
        RemoveWorktreeFromProject { project_group_index: usize },
    }

    // Distribution (out of 24 slots):
    //   SaveThread:                5 slots (~21%)
    //   SaveWorktreeThread:        2 slots (~8%)
    //   ToggleAgentPanel:          1 slot  (~4%)
    //   CreateDraftThread:         1 slot  (~4%)
    //   AddProject:                1 slot  (~4%)
    //   ArchiveThread:             2 slots (~8%)
    //   SwitchToThread:            2 slots (~8%)
    //   SwitchToProjectGroup:      2 slots (~8%)
    //   AddLinkedWorktree:         4 slots (~17%)
    //   AddWorktreeToProject:      2 slots (~8%)
    //   RemoveWorktreeFromProject: 2 slots (~8%)
    const DISTRIBUTION_SLOTS: u32 = 24;

    impl TestState {
        fn generate_operation(&self, raw: u32, project_group_count: usize) -> Operation {
            let extra = (raw / DISTRIBUTION_SLOTS) as usize;

            match raw % DISTRIBUTION_SLOTS {
                0..=4 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                5..=6 if !self.unopened_worktrees.is_empty() => Operation::SaveWorktreeThread {
                    worktree_index: extra % self.unopened_worktrees.len(),
                },
                5..=6 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                7 => Operation::ToggleAgentPanel,
                8 => Operation::CreateDraftThread,
                9 => Operation::AddProject {
                    use_worktree: !self.unopened_worktrees.is_empty(),
                },
                10..=11 if !self.saved_thread_ids.is_empty() => Operation::ArchiveThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                10..=11 => Operation::AddProject {
                    use_worktree: !self.unopened_worktrees.is_empty(),
                },
                12..=13 if !self.saved_thread_ids.is_empty() => Operation::SwitchToThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                12..=13 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                14..=15 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                16..=19 if project_group_count > 0 => Operation::AddLinkedWorktree {
                    project_group_index: extra % project_group_count,
                },
                16..=19 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                20..=21 if project_group_count > 0 => Operation::AddWorktreeToProject {
                    project_group_index: extra % project_group_count,
                },
                20..=21 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                22..=23 if project_group_count > 0 => Operation::RemoveWorktreeFromProject {
                    project_group_index: extra % project_group_count,
                },
                22..=23 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                _ => unreachable!(),
            }
        }
    }

    fn save_thread_to_path_with_main(
        state: &mut TestState,
        path_list: PathList,
        main_worktree_paths: PathList,
        cx: &mut gpui::VisualTestContext,
    ) {
        let session_id = state.next_metadata_only_thread_id();
        let title: SharedString = format!("Thread {}", session_id).into();
        let updated_at = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
            .unwrap()
            + chrono::Duration::seconds(state.thread_counter as i64);
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(session_id),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some(title),
            title_override: None,
            updated_at,
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, path_list).unwrap(),
            archived: false,
            remote_connection: None,
        };
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx))
        });
        cx.run_until_parked();
    }

    async fn perform_operation(
        operation: Operation,
        state: &mut TestState,
        multi_workspace: &Entity<MultiWorkspace>,
        sidebar: &Entity<Sidebar>,
        cx: &mut gpui::VisualTestContext,
    ) {
        match operation {
            Operation::SaveThread {
                project_group_index,
            } => {
                // Find a workspace for this project group and create a real
                // thread via its agent panel.
                let (workspace, project) = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    let ws = mw
                        .workspaces_for_project_group(key, cx)
                        .first()
                        .cloned()
                        .unwrap_or_else(|| mw.workspace().clone());
                    let project = ws.read(cx).project().clone();
                    (ws, project)
                });

                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    let agent_id = AgentId::new(format!("prop-agent-{}", state.thread_counter));
                    let connection = StubAgentConnection::new().with_agent_id(agent_id.clone());
                    open_thread_with_custom_connection(&panel, connection.clone(), cx);
                    let thread_id = active_thread_id(&panel, cx);
                    let session_id = active_session_id(&panel, cx);
                    // Make the thread non-draft without exercising the prompt
                    // send path; these invariants are about sidebar state, not
                    // git checkpointing during user prompts.
                    cx.update(|_, cx| {
                        connection.send_update(
                            session_id.clone(),
                            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                "Done".into(),
                            )),
                            cx,
                        );
                    });
                    cx.run_until_parked();
                    state.saved_thread_ids.push(session_id.clone());

                    let title: SharedString = format!("Thread {}", state.thread_counter).into();
                    state.thread_counter += 1;
                    let updated_at =
                        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
                            .unwrap()
                            + chrono::Duration::seconds(state.thread_counter as i64);
                    let metadata = cx.update(|_, cx| ThreadMetadata {
                        thread_id,
                        session_id: Some(session_id),
                        agent_id,
                        title: Some(title),
                        title_override: None,
                        updated_at,
                        created_at: None,
                        interacted_at: None,
                        worktree_paths: project.read(cx).worktree_paths(cx),
                        archived: false,
                        remote_connection: project.read(cx).remote_connection_options(cx),
                    });
                    cx.update(|_, cx| {
                        ThreadMetadataStore::global(cx)
                            .update(cx, |store, cx| store.save(metadata, cx))
                    });
                    cx.run_until_parked();
                }
            }
            Operation::SaveWorktreeThread { worktree_index } => {
                let worktree = &state.unopened_worktrees[worktree_index];
                let path_list = PathList::new(&[std::path::PathBuf::from(&worktree.path)]);
                let main_worktree_paths =
                    PathList::new(&[std::path::PathBuf::from(&worktree.main_workspace_path)]);
                save_thread_to_path_with_main(state, path_list, main_worktree_paths, cx);
            }

            Operation::ToggleAgentPanel => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel_open =
                    workspace.read_with(cx, |_, cx| AgentPanel::is_visible(&workspace, cx));
                workspace.update_in(cx, |workspace, window, cx| {
                    if panel_open {
                        workspace.close_panel::<AgentPanel>(window, cx);
                    } else {
                        workspace.open_panel::<AgentPanel>(window, cx);
                    }
                });
            }
            Operation::CreateDraftThread => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    panel.update_in(cx, |panel, window, cx| {
                        panel.new_thread(&NewThread, window, cx);
                    });
                    cx.run_until_parked();
                }
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                });
            }
            Operation::AddProject { use_worktree } => {
                let path = if use_worktree {
                    // Open an existing linked worktree as a project (simulates Cmd+O
                    // on a worktree directory).
                    state.unopened_worktrees.remove(0).path
                } else {
                    // Create a brand new project.
                    let path = state.next_workspace_path();
                    state
                        .fs
                        .insert_tree(
                            &path,
                            serde_json::json!({
                                ".git": {},
                                "src": {},
                            }),
                        )
                        .await;
                    path
                };
                let project = project::Project::test(
                    state.fs.clone() as Arc<dyn fs::Fs>,
                    [path.as_ref()],
                    cx,
                )
                .await;
                project.update(cx, |p, cx| p.git_scans_complete(cx)).await;
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.test_add_workspace(project.clone(), window, cx)
                });
            }

            Operation::ArchiveThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
                    sidebar.archive_thread(&session_id, window, cx);
                });
                cx.run_until_parked();
                state.saved_thread_ids.remove(index);
            }
            Operation::SwitchToThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                // Find the thread's position in the sidebar entries and select it.
                let thread_index = sidebar.read_with(cx, |sidebar, _| {
                    sidebar.contents.entries.iter().position(|entry| {
                        matches!(
                            entry,
                            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(&session_id)
                        )
                    })
                });
                if let Some(ix) = thread_index {
                    sidebar.update_in(cx, |sidebar, window, cx| {
                        sidebar.selection = Some(ix);
                        sidebar.confirm(&Confirm, window, cx);
                    });
                    cx.run_until_parked();
                }
            }
            Operation::SwitchToProjectGroup { index } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[index];
                    mw.workspaces_for_project_group(key, cx)
                        .first()
                        .cloned()
                        .unwrap_or_else(|| mw.workspace().clone())
                });
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.activate(workspace, None, window, cx);
                });
            }
            Operation::AddLinkedWorktree {
                project_group_index,
            } => {
                // Get the main worktree path from the project group key.
                let main_path = multi_workspace.read_with(cx, |mw, _| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    key.path_list()
                        .paths()
                        .first()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                });
                let dot_git = format!("{}/.git", main_path);
                let worktree_name = state.next_worktree_name();
                let worktree_path = format!("/worktrees/{}", worktree_name);

                state.fs
                    .insert_tree(
                        &worktree_path,
                        serde_json::json!({
                            ".git": format!("gitdir: {}/.git/worktrees/{}", main_path, worktree_name),
                            "src": {},
                        }),
                    )
                    .await;

                // Also create the worktree metadata dir inside the main repo's .git
                state
                    .fs
                    .insert_tree(
                        &format!("{}/.git/worktrees/{}", main_path, worktree_name),
                        serde_json::json!({
                            "commondir": "../../",
                            "HEAD": format!("ref: refs/heads/{}", worktree_name),
                        }),
                    )
                    .await;

                let dot_git_path = std::path::Path::new(&dot_git);
                let worktree_pathbuf = std::path::PathBuf::from(&worktree_path);
                state
                    .fs
                    .add_linked_worktree_for_repo(
                        dot_git_path,
                        false,
                        git::repository::Worktree {
                            path: worktree_pathbuf,
                            ref_name: Some(format!("refs/heads/{}", worktree_name).into()),
                            sha: "aaa".into(),
                            is_main: false,
                            is_bare: false,
                        },
                    )
                    .await;

                // Re-scan the main workspace's project so it discovers the new worktree.
                let main_workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    mw.workspaces_for_project_group(key, cx)
                        .first()
                        .cloned()
                        .unwrap()
                });
                let main_project = main_workspace.read_with(cx, |ws, _| ws.project().clone());
                main_project
                    .update(cx, |p, cx| p.git_scans_complete(cx))
                    .await;

                state.unopened_worktrees.push(UnopenedWorktree {
                    path: worktree_path,
                    main_workspace_path: main_path.clone(),
                });
            }
            Operation::AddWorktreeToProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    mw.workspaces_for_project_group(key, cx).first().cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let new_path = state.next_workspace_path();
                state
                    .fs
                    .insert_tree(&new_path, serde_json::json!({ ".git": {}, "src": {} }))
                    .await;

                let result = project
                    .update(cx, |project, cx| {
                        project.find_or_create_worktree(&new_path, true, cx)
                    })
                    .await;
                if result.is_err() {
                    return;
                }
                cx.run_until_parked();
            }
            Operation::RemoveWorktreeFromProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |mw, cx| {
                    let keys = mw.project_group_keys();
                    let key = &keys[project_group_index];
                    mw.workspaces_for_project_group(key, cx).first().cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let worktree_count = project.read_with(cx, |p, cx| p.visible_worktrees(cx).count());
                if worktree_count <= 1 {
                    return;
                }

                let worktree_id = project.read_with(cx, |p, cx| {
                    p.visible_worktrees(cx).last().map(|wt| wt.read(cx).id())
                });
                if let Some(worktree_id) = worktree_id {
                    project.update(cx, |project, cx| {
                        project.remove_worktree(worktree_id, cx);
                    });
                    cx.run_until_parked();
                }
            }
        }
    }

    fn update_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
        sidebar.update_in(cx, |sidebar, _window, cx| {
            if let Some(mw) = sidebar.multi_workspace.upgrade() {
                mw.update(cx, |mw, _cx| mw.test_expand_all_groups());
            }
            sidebar.update_entries(cx);
        });
    }

    fn validate_sidebar_properties(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        verify_section_headers_are_well_formed(sidebar)?;
        verify_no_duplicate_threads(sidebar)?;
        verify_all_threads_are_shown(sidebar, cx)?;
        verify_active_state_matches_current_workspace(sidebar, cx)?;
        verify_all_workspaces_are_reachable(sidebar, cx)?;
        verify_workspace_group_key_integrity(sidebar, cx)?;
        Ok(())
    }

    /// A thread appears once in the whole list, not once per section: the
    /// three sections partition the threads between them, so there is no
    /// reading of the sidebar in which the same row turns up twice.
    fn verify_no_duplicate_threads(sidebar: &Sidebar) -> anyhow::Result<()> {
        let mut seen: HashSet<acp::SessionId> = HashSet::default();
        let mut duplicates: Vec<(acp::SessionId, String)> = Vec::new();

        for entry in &sidebar.contents.entries {
            if let Some(session_id) = entry.session_id() {
                if !seen.insert(session_id.clone()) {
                    let title = match entry {
                        ListEntry::Thread(thread) => thread.metadata.display_title().to_string(),
                        _ => "<unknown>".to_string(),
                    };
                    duplicates.push((session_id.clone(), title));
                }
            }
        }

        anyhow::ensure!(
            duplicates.is_empty(),
            "threads appear more than once in the list: {:?}",
            duplicates,
        );
        Ok(())
    }

    // The list is a flat sequence of sections: each section header is unique,
    // ordered Active, All Threads, Archived, and (while expanded) followed by
    // at least one row.
    fn verify_section_headers_are_well_formed(sidebar: &Sidebar) -> anyhow::Result<()> {
        fn section_rank(section: SidebarSection) -> usize {
            match section {
                SidebarSection::OpenInZed => 0,
                SidebarSection::AllThreads => 1,
                SidebarSection::Archived => 2,
            }
        }

        let entries = &sidebar.contents.entries;
        if !entries.is_empty() {
            anyhow::ensure!(
                matches!(entries.first(), Some(ListEntry::SectionHeader(_))),
                "a non-empty list must start with a section header"
            );
        }

        let mut last_rank: Option<usize> = None;
        for (ix, entry) in entries.iter().enumerate() {
            match entry {
                // Workspace headers are presentation rows inside a section.
                ListEntry::WorkspaceHeader(_) => {}
                ListEntry::SectionHeader(section) => {
                    let rank = section_rank(*section);
                    if let Some(last) = last_rank {
                        anyhow::ensure!(
                            rank > last,
                            "section headers must be unique and in section order"
                        );
                    }
                    last_rank = Some(rank);
                    // The Active header is exempt: it always renders, rows or
                    // not, because it carries the new-thread button.
                    if !sidebar.collapsed_sections.contains(section)
                        && !matches!(section, SidebarSection::OpenInZed)
                    {
                        anyhow::ensure!(
                            matches!(
                                entries.get(ix + 1),
                                // A worktree group opens with its own header.
                                Some(
                                    ListEntry::Thread(_)
                                        | ListEntry::Terminal(_)
                                        | ListEntry::WorkspaceHeader(_)
                                )
                            ),
                            "an expanded section header must be followed by at least one row"
                        );
                    }
                }
                ListEntry::Thread(_) | ListEntry::Terminal(_) => {}
            }
        }

        anyhow::ensure!(
            !entries
                .iter()
                .any(|entry| matches!(entry, ListEntry::Terminal(_)))
                || entries.iter().any(|entry| matches!(
                    entry,
                    ListEntry::SectionHeader(SidebarSection::AllThreads)
                )),
            "terminal rows belong to the All Threads section"
        );

        // A live thread is in Active, and the partition puts it nowhere else,
        // so anything above the All Threads header is the whole of it.
        let all_threads_ix = entries.iter().position(|entry| {
            matches!(entry, ListEntry::SectionHeader(SidebarSection::AllThreads))
        });
        let live_in_active: HashSet<acp::SessionId> = entries
            .iter()
            .take(all_threads_ix.unwrap_or(entries.len()))
            .filter_map(|entry| match entry {
                ListEntry::Thread(thread) if thread.is_live => thread.metadata.session_id.clone(),
                _ => None,
            })
            .collect();
        for entry in entries {
            if let ListEntry::Thread(thread) = entry
                && thread.is_live
                && let Some(session_id) = &thread.metadata.session_id
            {
                anyhow::ensure!(
                    live_in_active.contains(session_id),
                    "a live thread must appear in the Active section"
                );
            }
        }
        Ok(())
    }

    fn verify_all_threads_are_shown(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let thread_store = ThreadMetadataStore::global(cx);

        let sidebar_thread_ids: HashSet<acp::SessionId> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| entry.session_id().cloned())
            .collect();

        // The merged history model shows every stored thread (archived
        // included), so the sidebar's session ids must equal the store's.
        let metadata_thread_ids: HashSet<acp::SessionId> = thread_store
            .read(cx)
            .entries()
            .filter_map(|metadata| metadata.session_id.clone())
            .collect();

        anyhow::ensure!(
            sidebar_thread_ids == metadata_thread_ids,
            "sidebar threads don't match metadata store: sidebar has {:?}, store has {:?}",
            sidebar_thread_ids,
            metadata_thread_ids,
        );
        Ok(())
    }

    fn verify_active_state_matches_current_workspace(
        sidebar: &Sidebar,
        cx: &App,
    ) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let active_workspace = multi_workspace.read(cx).workspace();

        // 1. active_entry should be Some when the panel has content.
        //    It may be None when the panel is uninitialized (no drafts,
        //    no threads), which is fine.
        //    It may also temporarily point at a different workspace
        //    when the workspace just changed and the new panel has no
        //    content yet.
        let panel = active_workspace.read(cx).panel::<AgentPanel>(cx).unwrap();
        let panel_has_content = panel.read(cx).active_thread_id(cx).is_some()
            || panel.read(cx).active_conversation_view().is_some()
            || panel.read(cx).active_terminal_id().is_some();

        let Some(entry) = sidebar.active_entry.as_ref() else {
            if panel_has_content {
                anyhow::bail!("active_entry is None but panel has content");
            }
            return Ok(());
        };

        // If the entry workspace doesn't match the active workspace
        // and the panel has no content, this is a transient state that
        // will resolve when the panel gets content.
        if entry.workspace().entity_id() != active_workspace.entity_id() && !panel_has_content {
            return Ok(());
        }

        // 2. The entry's workspace must agree with the multi-workspace's
        //    active workspace.
        anyhow::ensure!(
            entry.workspace().entity_id() == active_workspace.entity_id(),
            "active_entry workspace ({:?}) != active workspace ({:?})",
            entry.workspace().entity_id(),
            active_workspace.entity_id(),
        );

        // 3. The entry must match the agent panel's current state.
        if panel.read(cx).active_thread_id(cx).is_some() {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { .. }),
                "panel shows a tracked draft but active_entry is {:?}",
                entry,
            );
        } else if let Some(thread_id) = panel
            .read(cx)
            .active_conversation_view()
            .map(|cv| cv.read(cx).parent_id())
        {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { thread_id: tid, .. } if *tid == thread_id),
                "panel has thread {:?} but active_entry is {:?}",
                thread_id,
                entry,
            );
        }

        // 4. The active_entry must be uniquely identified within each
        //    section it appears in — unless the panel is showing the
        //    new-draft slot (which is represented by the + button's active
        //    state rather than a sidebar row) or nothing at all.
        // Active terminals must still match a row, so don't treat the absence
        // of a conversation view as "new-draft" when a terminal is active.
        let hidden_from_sidebar = panel.read(cx).active_terminal_id().is_none()
            && (panel.read(cx).active_view_is_new_draft(cx)
                || panel.read(cx).active_conversation_view().is_none());
        if hidden_from_sidebar {
            return Ok(());
        }
        // A thread the panel is showing has an open tab, so under the
        // Active/All-Threads model it belongs to both sections at once —
        // one matching row each. A terminal only ever lives in All
        // Threads, so it keeps a single match. Either way, per-section
        // uniqueness (no more than one match per section) is what
        // verify_no_duplicate_threads already guards; this just bounds the
        // total.
        let max_matches = match entry {
            ActiveEntry::Thread { .. } => 2,
            ActiveEntry::Terminal { .. } => 1,
        };
        let matching_count = sidebar
            .contents
            .entries
            .iter()
            .filter(|e| entry.matches_entry(e))
            .count();
        if matching_count == 0 || matching_count > max_matches {
            let thread_entries: Vec<_> = sidebar
                .contents
                .entries
                .iter()
                .filter_map(|e| match e {
                    ListEntry::Thread(t) => Some(format!(
                        "tid={:?} sid={:?}",
                        t.metadata.thread_id, t.metadata.session_id
                    )),
                    _ => None,
                })
                .collect();
            let store = agent_ui::thread_metadata_store::ThreadMetadataStore::global(cx).read(cx);
            let store_entries: Vec<_> = store
                .entries()
                .map(|m| {
                    format!(
                        "tid={:?} sid={:?} archived={} paths={:?}",
                        m.thread_id,
                        m.session_id,
                        m.archived,
                        m.folder_paths()
                    )
                })
                .collect();
            anyhow::bail!(
                "expected between 1 and {} sidebar entries matching active_entry {:?}, found {}. sidebar threads: {:?}. store: {:?}",
                max_matches,
                entry,
                matching_count,
                thread_entries,
                store_entries,
            );
        }

        Ok(())
    }

    /// Every workspace in the multi-workspace should be "reachable" from
    /// the sidebar — meaning there is at least one entry (thread, draft,
    /// new-thread, or project header) that, when clicked, would activate
    /// that workspace.
    fn verify_all_workspaces_are_reachable(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let multi_workspace = multi_workspace.read(cx);

        // The merged history model dropped project headers, so workspaces
        // without any thread or terminal rows are legitimately not reachable
        // from the sidebar (the recent-projects menu covers navigation).
        // The invariant that remains is the reverse: every workspace an
        // entry points at must be a workspace the multi-workspace knows.
        let reachable_workspaces: HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(multi_workspace, cx))
            .map(|ws| ws.entity_id())
            .collect();

        let all_workspace_ids: HashSet<gpui::EntityId> = multi_workspace
            .workspaces()
            .map(|ws| ws.entity_id())
            .collect();

        let dangling = &reachable_workspaces - &all_workspace_ids;

        anyhow::ensure!(
            dangling.is_empty(),
            "The following sidebar entries reference unknown workspaces: {:?}",
            dangling,
        );

        Ok(())
    }

    fn verify_workspace_group_key_integrity(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };
        multi_workspace
            .read(cx)
            .assert_project_group_key_integrity(cx)
    }

    #[gpui::property_test(config = ProptestConfig {
        cases: 20,
        ..Default::default()
    })]
    async fn test_sidebar_invariants(
        #[strategy = gpui::proptest::collection::vec(0u32..DISTRIBUTION_SLOTS * 10, 1..10)]
        raw_operations: Vec<u32>,
        cx: &mut TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_PROPTEST_DB: AtomicUsize = AtomicUsize::new(0);

        let test_db_id = NEXT_PROPTEST_DB.fetch_add(1, Ordering::SeqCst);
        cx.update(|cx| {
            cx.set_global(TestTerminalMetadataDbName(format!(
                "PROPTEST_TERMINAL_THREAD_METADATA_{test_db_id}"
            )));
        });

        agent_ui::test_support::init_test(cx);
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
                format!("PROPTEST_THREAD_METADATA_{test_db_id}"),
            ));

            ThreadStore::init_global(cx);
            ThreadMetadataStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            prompt_store::init(cx);

            // Auto-add an AgentPanel to every workspace so that implicitly
            // created workspaces (e.g. from thread activation) also have one.
            cx.observe_new(
                |workspace: &mut Workspace,
                 window: Option<&mut Window>,
                 cx: &mut gpui::Context<Workspace>| {
                    if let Some(window) = window {
                        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                        workspace.add_panel(panel, window, cx);
                    }
                },
            )
            .detach();
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/my-project",
            serde_json::json!({
                ".git": {},
                "src": {},
            }),
        )
        .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project =
            project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/my-project".as_ref()], cx)
                .await;
        project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let sidebar = setup_sidebar(&multi_workspace, cx);

        let mut state = TestState::new(fs);
        let mut executed: Vec<String> = Vec::new();

        for &raw_op in &raw_operations {
            let project_group_count =
                multi_workspace.read_with(cx, |mw, _| mw.project_group_keys().len());
            let operation = state.generate_operation(raw_op, project_group_count);
            executed.push(format!("{:?}", operation));
            perform_operation(operation, &mut state, &multi_workspace, &sidebar, cx).await;
            cx.run_until_parked();

            update_sidebar(&sidebar, cx);
            cx.run_until_parked();

            let result =
                sidebar.read_with(cx, |sidebar, cx| validate_sidebar_properties(sidebar, cx));
            if let Err(err) = result {
                let log = executed.join("\n  ");
                panic!(
                    "Property violation after step {}:\n{err}\n\nOperations:\n  {log}",
                    executed.len(),
                );
            }
        }
    }
}

#[gpui::test]
async fn test_remote_project_integration_does_not_briefly_render_as_separate_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    // Set up the remote server side.
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));

    // Create the linked worktree checkout path on the remote server,
    // but do not yet register it as a git-linked worktree. The real
    // regrouping update in this test should happen only after the
    // sidebar opens the closed remote thread.
    server_fs
        .insert_tree(
            "/project-wt-1",
            serde_json::json!({
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let (original_opts, server_session, _) = remote::RemoteClient::fake_server(cx, server_cx);

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let _headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    // Connect the client side and build a remote project.
    let remote_client = remote::RemoteClient::connect_mock(original_opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    // Open the remote worktree.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/project"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    // Verify the project is remote.
    project.read_with(cx, |project, cx| {
        assert!(!project.is_local(), "project should be remote");
        assert!(
            project.remote_connection_options(cx).is_some(),
            "project should have remote connection options"
        );
    });

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    // Create MultiWorkspace with the remote project.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    cx.run_until_parked();

    // Save a thread for the main remote workspace (folder_paths match
    // the open workspace, so it will be classified as Open).
    let main_thread_id = acp::SessionId::new(Arc::from("main-thread"));
    save_thread_metadata(
        main_thread_id.clone(),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Save a thread whose folder_paths point to a linked worktree path
    // that doesn't have an open workspace ("/project-wt-1"), but whose
    // main_worktree_paths match the project group key so it appears
    // in the sidebar under the same remote group. This simulates a
    // linked worktree workspace that was closed.
    let remote_thread_id = acp::SessionId::new(Arc::from("remote-thread"));
    let (main_worktree_paths, remote_connection) = project.read_with(cx, |p, cx| {
        (
            p.project_group_key(cx).path_list().clone(),
            p.remote_connection_options(cx),
        )
    });
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(remote_thread_id.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Worktree Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(
                main_worktree_paths,
                PathList::new(&[PathBuf::from("/project-wt-1")]),
            )
            .unwrap(),
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
            matches!(
                entry,
                ListEntry::Thread(thread) if thread.metadata.session_id.as_ref() == Some(&remote_thread_id)
            )
        });
    });

    let saw_separate_project_header = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_separate_project_header_for_observer = saw_separate_project_header.clone();

    sidebar
        .update(cx, |_, cx| {
            // The merged history model has no project headers. The flicker
            // this guards against manifested as the expected threads
            // disappearing mid-integration, so track that instead. Count
            // distinct sessions, not rows, so the signal stays "exactly
            // these two threads" however the sections divide them up.
            cx.observe_self(move |sidebar, _cx| {
                let thread_count = sidebar
                    .contents
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        ListEntry::Thread(thread) => thread.metadata.session_id.clone(),
                        _ => None,
                    })
                    .collect::<HashSet<_>>()
                    .len();
                if thread_count != 2 {
                    saw_separate_project_header_for_observer
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            })
        })
        .detach();

    multi_workspace.update(cx, |multi_workspace, cx| {
        let workspace = multi_workspace.workspace().clone();
        workspace.update(cx, |workspace: &mut Workspace, cx| {
            let remote_client = workspace
                .project()
                .read(cx)
                .remote_client()
                .expect("main remote project should have a remote client");
            remote_client.update(cx, |remote_client: &mut remote::RemoteClient, cx| {
                remote_client.force_server_not_running(cx);
            });
        });
    });
    cx.run_until_parked();

    let (server_session_2, connect_guard_2) =
        remote::RemoteClient::fake_server_with_opts(&original_opts, cx, server_cx);
    let _headless_2 = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session_2,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    drop(connect_guard_2);

    let window = cx.windows()[0];
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(Confirm.boxed_clone(), cx);
    })
    .unwrap();

    cx.run_until_parked();

    let new_workspace = multi_workspace.read_with(cx, |mw, _| {
        assert_eq!(
            mw.workspaces().count(),
            2,
            "confirming a closed remote thread should open a second workspace"
        );
        mw.workspaces()
            .find(|workspace| workspace.entity_id() != mw.workspace().entity_id())
            .unwrap()
            .clone()
    });

    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: PathBuf::from("/project-wt-1"),
                ref_name: Some("refs/heads/feature-wt".into()),
                sha: "abc123".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    server_cx.run_until_parked();
    cx.run_until_parked();
    server_cx.run_until_parked();
    cx.run_until_parked();

    let entries_after_update = visible_entries_as_strings(&sidebar, cx);
    let group_after_update = new_workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).project_group_key(cx)
    });

    assert_eq!(
        group_after_update,
        project.read_with(cx, |project, cx| ProjectGroupKey::from_project(project, cx)),
        "expected the remote worktree workspace to be grouped under the main remote project after the real update; \
         final sidebar entries: {:?}",
        entries_after_update,
    );

    sidebar.update(cx, |sidebar, _cx| {
        assert_remote_project_integration_sidebar_state(
            sidebar,
            &main_thread_id,
            &remote_thread_id,
        );
    });

    assert!(
        !saw_separate_project_header.load(std::sync::atomic::Ordering::SeqCst),
        "sidebar briefly rendered the remote worktree as a separate project during the real remote open/update sequence; \
         final group: {:?}; final sidebar entries: {:?}",
        group_after_update,
        entries_after_update,
    );
}

#[gpui::test]
async fn test_archive_removes_worktree_even_when_workspace_paths_diverge(cx: &mut TestAppContext) {
    // When the thread's folder_paths don't exactly match any workspace's
    // root paths (e.g. because a folder was added to the workspace after
    // the thread was created), workspace_to_remove is None. But the linked
    // worktree workspace still needs to be removed so that its worktree
    // entities are released, allowing git worktree removal to proceed.
    //
    // With the fix, archive_thread scans roots_to_archive for any linked
    // worktree workspaces and includes them in the removal set, even when
    // the thread's folder_paths don't match the workspace's root paths.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {
                "main.rs": "fn main() {}",
            },
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Save thread metadata using folder_paths that DON'T match the
    // workspace's root paths. This simulates the case where the workspace's
    // paths diverged (e.g. a folder was added after thread creation).
    // This causes workspace_to_remove to be None because
    // workspace_for_paths can't find a workspace with these exact paths.
    let wt_thread_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[
            PathBuf::from("/worktrees/project/feature-a/project"),
            PathBuf::from("/nonexistent"),
        ]),
        PathList::new(&[PathBuf::from("/project"), PathBuf::from("/nonexistent")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    // Also save a main thread so the sidebar has something to show.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should start with 2 workspaces (main + linked worktree)"
    );

    // Archive the worktree thread.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&wt_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The linked worktree workspace should have been removed, even though
    // workspace_to_remove was None (paths didn't match).
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "linked worktree workspace should be removed after archiving, \
         even when folder_paths don't match workspace root paths"
    );

    // The thread should still be archived (not unarchived due to an error).
    let still_archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&wt_thread_id)
            .map(|t| t.archived)
    });
    assert_eq!(
        still_archived,
        Some(true),
        "thread should still be archived (not rolled back due to error)"
    );

    // The linked worktree directory should be removed from disk.
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk"
    );
}

#[gpui::test]
async fn test_archive_mixed_workspace_closes_only_archived_worktree_items(cx: &mut TestAppContext) {
    // When a workspace contains both a worktree being archived and other
    // worktrees that should remain, only the editor items referencing the
    // archived worktree should be closed — the workspace itself must be
    // preserved.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/main-repo",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {
                "lib.rs": "pub fn hello() {}",
            },
        }),
    )
    .await;

    fs.insert_tree(
        "/worktrees/main-repo/feature-b/main-repo",
        serde_json::json!({
            ".git": "gitdir: /main-repo/.git/worktrees/feature-b",
            "src": {
                "main.rs": "fn main() { hello(); }",
            },
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/main-repo/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/main-repo/feature-b/main-repo"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "def".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/main-repo/feature-b/main-repo"),
        None,
        cx,
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Create a single project that contains BOTH the main repo and the
    // linked worktree — this makes it a "mixed" workspace.
    let mixed_project = project::Project::test(
        fs.clone(),
        [
            "/main-repo".as_ref(),
            "/worktrees/main-repo/feature-b/main-repo".as_ref(),
        ],
        cx,
    )
    .await;

    mixed_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(mixed_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Open editor items in both worktrees so we can verify which ones
    // get closed.
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let worktree_ids: Vec<(WorktreeId, Arc<Path>)> = workspace.read_with(cx, |ws, cx| {
        ws.project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|wt| (wt.read(cx).id(), wt.read(cx).abs_path()))
            .collect()
    });

    let main_repo_wt_id = worktree_ids
        .iter()
        .find(|(_, path)| path.as_ref() == Path::new("/main-repo"))
        .map(|(id, _)| *id)
        .expect("should find main-repo worktree");

    let feature_b_wt_id = worktree_ids
        .iter()
        .find(|(_, path)| path.as_ref() == Path::new("/worktrees/main-repo/feature-b/main-repo"))
        .map(|(id, _)| *id)
        .expect("should find feature-b worktree");

    // Open files from both worktrees.
    let main_repo_path = project::ProjectPath {
        worktree_id: main_repo_wt_id,
        path: Arc::from(rel_path("src/lib.rs")),
    };
    let feature_b_path = project::ProjectPath {
        worktree_id: feature_b_wt_id,
        path: Arc::from(rel_path("src/main.rs")),
    };

    workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_path(main_repo_path.clone(), None, true, window, cx)
        })
        .await
        .expect("should open main-repo file");
    workspace
        .update_in(cx, |ws, window, cx| {
            ws.open_path(feature_b_path.clone(), None, true, window, cx)
        })
        .await
        .expect("should open feature-b file");

    cx.run_until_parked();

    // Verify both items are open.
    let open_paths_before: Vec<project::ProjectPath> = workspace.read_with(cx, |ws, cx| {
        ws.panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.project_path(cx))
            })
            .collect()
    });
    assert!(
        open_paths_before
            .iter()
            .any(|pp| pp.worktree_id == main_repo_wt_id),
        "main-repo file should be open"
    );
    assert!(
        open_paths_before
            .iter()
            .any(|pp| pp.worktree_id == feature_b_wt_id),
        "feature-b file should be open"
    );

    // Save thread metadata for the linked worktree with deliberately
    // mismatched folder_paths to trigger the scan-based detection.
    save_thread_metadata_with_main_paths(
        "feature-b-thread",
        "Feature B Thread",
        PathList::new(&[
            PathBuf::from("/worktrees/main-repo/feature-b/main-repo"),
            PathBuf::from("/nonexistent"),
        ]),
        PathList::new(&[PathBuf::from("/main-repo"), PathBuf::from("/nonexistent")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    // Save another thread that references only the main repo (not the
    // linked worktree) so archiving the feature-b thread's worktree isn't
    // blocked by another unarchived thread referencing the same path.
    save_thread_metadata_with_main_paths(
        "other-thread",
        "Other Thread",
        PathList::new(&[PathBuf::from("/main-repo")]),
        PathList::new(&[PathBuf::from("/main-repo")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // There should still be exactly 1 workspace.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should have 1 workspace (the mixed workspace)"
    );

    // Archive the feature-b thread.
    let fb_session_id = acp::SessionId::new(Arc::from("feature-b-thread"));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&fb_session_id, window, cx);
    });

    cx.run_until_parked();

    // The workspace should still exist (it's "mixed" — has non-archived worktrees).
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "mixed workspace should be preserved"
    );

    // Only the feature-b editor item should have been closed.
    let open_paths_after: Vec<project::ProjectPath> = workspace.read_with(cx, |ws, cx| {
        ws.panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.project_path(cx))
            })
            .collect()
    });
    assert!(
        open_paths_after
            .iter()
            .any(|pp| pp.worktree_id == main_repo_wt_id),
        "main-repo file should still be open"
    );
    assert!(
        !open_paths_after
            .iter()
            .any(|pp| pp.worktree_id == feature_b_wt_id),
        "feature-b file should have been closed"
    );
}

#[gpui::test]
async fn test_discard_mixed_workspace_draft_closes_only_archived_worktree_items(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/main-repo",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {
                "lib.rs": "pub fn hello() {}",
            },
        }),
    )
    .await;

    fs.insert_tree(
        "/worktrees/main-repo/feature-b/main-repo",
        serde_json::json!({
            ".git": "gitdir: /main-repo/.git/worktrees/feature-b",
            "src": {
                "main.rs": "fn main() { hello(); }",
            },
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/main-repo/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/main-repo/feature-b/main-repo"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "def".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/main-repo/feature-b/main-repo"),
        None,
        cx,
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let mixed_project = project::Project::test(
        fs.clone(),
        [
            "/main-repo".as_ref(),
            "/worktrees/main-repo/feature-b/main-repo".as_ref(),
        ],
        cx,
    )
    .await;

    mixed_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(mixed_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    let worktree_ids: Vec<(WorktreeId, Arc<Path>)> = workspace.read_with(cx, |workspace, cx| {
        workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| (worktree.read(cx).id(), worktree.read(cx).abs_path()))
            .collect()
    });

    let main_repo_worktree_id = worktree_ids
        .iter()
        .find(|(_, path)| path.as_ref() == Path::new("/main-repo"))
        .map(|(id, _)| *id)
        .expect("should find main-repo worktree");

    let feature_b_worktree_id = worktree_ids
        .iter()
        .find(|(_, path)| path.as_ref() == Path::new("/worktrees/main-repo/feature-b/main-repo"))
        .map(|(id, _)| *id)
        .expect("should find feature-b worktree");

    let main_repo_path = project::ProjectPath {
        worktree_id: main_repo_worktree_id,
        path: Arc::from(rel_path("src/lib.rs")),
    };
    let feature_b_path = project::ProjectPath {
        worktree_id: feature_b_worktree_id,
        path: Arc::from(rel_path("src/main.rs")),
    };

    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(main_repo_path.clone(), None, true, window, cx)
        })
        .await
        .expect("should open main-repo file");
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(feature_b_path.clone(), None, true, window, cx)
        })
        .await
        .expect("should open feature-b file");

    let folder_paths = PathList::new(&[
        PathBuf::from("/main-repo"),
        PathBuf::from("/worktrees/main-repo/feature-b/main-repo"),
    ]);
    let main_worktree_paths =
        PathList::new(&[PathBuf::from("/main-repo"), PathBuf::from("/main-repo")]);
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Mixed Workspace Draft".into()),
        folder_paths,
        main_worktree_paths,
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            draft_id,
            &[acp::ContentBlock::Text(acp::TextContent::new(
                "mixed workspace draft",
            ))],
            cx,
        )
    })
    .await
    .expect("draft prompt should persist");

    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let draft_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id
                )
            })
            .expect("mixed workspace draft should be visible")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(draft_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "mixed workspace should be preserved"
    );

    let open_paths_after: Vec<project::ProjectPath> = workspace.read_with(cx, |workspace, cx| {
        workspace
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.project_path(cx))
            })
            .collect()
    });
    assert!(
        open_paths_after
            .iter()
            .any(|project_path| project_path.worktree_id == main_repo_worktree_id),
        "main-repo file should still be open"
    );
    assert!(
        !open_paths_after
            .iter()
            .any(|project_path| project_path.worktree_id == feature_b_worktree_id),
        "feature-b file should have been closed"
    );

    let draft_metadata_deleted = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(draft_id)
            .is_none()
    });
    assert!(
        draft_metadata_deleted,
        "discarded draft metadata should be deleted"
    );
}

#[test]
fn test_worktree_info_branch_names_for_main_worktrees() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> =
        [(PathBuf::from("/projects/myapp"), "feature-x".into())]
            .into_iter()
            .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, Some(SharedString::from("feature-x")));
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[test]
fn test_worktree_info_branch_names_for_linked_worktrees() {
    let main_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp-feature")]);
    let worktree_paths =
        WorktreePaths::from_path_lists(main_paths, folder_paths).expect("same length");

    let branch_by_path: HashMap<PathBuf, SharedString> = [(
        PathBuf::from("/projects/myapp-feature"),
        "feature-branch".into(),
    )]
    .into_iter()
    .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Linked);
    assert_eq!(
        infos[0].branch_name,
        Some(SharedString::from("feature-branch"))
    );
}

#[test]
fn test_worktree_info_missing_branch_returns_none() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, None);
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[gpui::test]
async fn test_remote_archive_thread_with_active_connection(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // End-to-end test of archiving a remote thread tied to a linked git
    // worktree. Archival should:
    //  1. Persist the worktree's git state via the remote repository RPCs
    //     (head_sha / create_archive_checkpoint / update_ref).
    //  2. Remove the linked worktree directory from the *remote* filesystem
    //     via the GitRemoveWorktree RPC.
    //  3. Mark the thread metadata archived and hide it from the sidebar.
    //
    // The mock remote transport only supports one live `RemoteClient` per
    // connection at a time (each client's `start_proxy` replaces the
    // previous server channel), so we can't split the main repo and the
    // linked worktree across two remote projects the way Zed does in
    // production. Opening both as visible worktrees of a single remote
    // project still exercises every interesting path of the archive flow
    // while staying within the mock's multiplexing limits.
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    // Set up the remote filesystem with a main repo and one linked worktree.
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {
                    "worktrees": {
                        "feature-a": {
                            "commondir": "../../",
                            "HEAD": "ref: refs/heads/feature-a",
                        },
                    },
                },
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;
    server_fs
        .insert_tree(
            "/worktrees/project/feature-a/project",
            serde_json::json!({
                ".git": "gitdir: /project/.git/worktrees/feature-a",
                "src": { "lib.rs": "// feature" },
            }),
        )
        .await;
    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from("/worktrees/project/feature-a/project"),
                ref_name: Some("refs/heads/feature-a".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.set_head_for_repo(
        Path::new("/project/.git"),
        &[("src/main.rs", "fn main() {}".into())],
        "head-sha",
    );

    // Open a single remote project with both the main repo and the linked
    // worktree as visible worktrees. The mock transport doesn't multiplex
    // multiple `RemoteClient`s over one pooled connection cleanly (each
    // client's `start_proxy` clobbers the previous one's server channel),
    // so we can't build two separate `Project::remote` instances in this
    // test. Folding both worktrees into one project still exercises the
    // archive flow's interesting paths: `build_root_plan` classifies the
    // linked worktree correctly, and `find_or_create_repository` finds
    // the main repo live on that same project — avoiding the temp-project
    // fallback that would also run into the multiplexing limitation.
    let (project, _headless, _opts) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(
                Path::new("/worktrees/project/feature-a/project"),
                true,
                cx,
            )
        })
        .await
        .expect("should open linked worktree on remote");
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;
    cx.run_until_parked();

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // The worktree thread's (main_worktree_path, folder_path) pair points
    // the folder at the linked worktree checkout and the main at the
    // parent repo, so `build_root_plan` targets the linked worktree
    // specifically and knows which main repo owns it.
    let remote_connection = project.read_with(cx, |p, cx| p.remote_connection_options(cx));

    // Record the worktree as Zed-created on the client, keyed by the remote
    // connection identity, with the creation time of the gitdir on the
    // *remote* filesystem (where the archive flow will re-stat it).
    agent_ui::test_support::record_zed_created_worktree(
        server_fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        remote_connection.as_ref(),
        cx,
    )
    .await;

    let wt_thread_id = acp::SessionId::new(Arc::from("worktree-thread"));
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(wt_thread_id.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Worktree Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
                .unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(
                PathList::new(&[PathBuf::from("/project")]),
                PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]),
            )
            .unwrap(),
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    assert!(
        server_fs
            .is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should exist on remote before archiving"
    );

    sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
        sidebar.archive_thread(&wt_thread_id, window, cx);
    });
    cx.run_until_parked();
    server_cx.run_until_parked();

    let is_archived = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&wt_thread_id)
            .map(|t| t.archived)
            .unwrap_or(false)
    });
    assert!(is_archived, "worktree thread should be archived");

    assert!(
        !server_fs
            .is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from remote fs \
         (the GitRemoveWorktree RPC runs `Repository::remove_worktree` \
         on the headless server, which deletes the directory via `Fs::remove_dir` \
         before running `git worktree remove --force`)"
    );

    // In the merged history model the archived thread stays listed, muted.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Worktree Thread") && e.contains("(archived)")),
        "archived worktree thread should stay listed as archived: {entries:?}"
    );
}

#[gpui::test]
async fn test_remote_linked_worktree_workspace_to_remove_uses_remote_connection(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": {},
            }),
        )
        .await;
    server_fs
        .insert_tree(
            "/external-worktree",
            serde_json::json!({
                ".git": "gitdir: /project/.git/worktrees/feature-a",
                "src": {},
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.insert_branches(Path::new("/project/.git"), &["main", "feature-a"]);
    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from("/external-worktree"),
                ref_name: Some("refs/heads/feature-a".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    let (worktree_project, _headless, remote_connection) = start_remote_project(
        &server_fs,
        Path::new("/external-worktree"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.run_until_parked();

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_session_id = acp::SessionId::new(Arc::from("remote-worktree-thread"));
    let worktree_folder_paths = PathList::new(&[PathBuf::from("/external-worktree")]);
    let main_folder_paths = PathList::new(&[PathBuf::from("/project")]);
    let worktree_thread_id = ThreadId::new();
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: worktree_thread_id,
            session_id: Some(worktree_session_id.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Remote Worktree Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(
                main_folder_paths,
                worktree_folder_paths.clone(),
            )
            .unwrap(),
            archived: false,
            remote_connection: Some(remote_connection.clone()),
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(
                    &worktree_folder_paths,
                    Some(&remote_connection),
                    cx,
                )
            })
            .is_some(),
        "remote linked-worktree workspace should be open before archiving"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "the test must exercise a remote-only workspace lookup"
    );
    assert_ne!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).project_group_key(cx)
            })
            .path_list(),
        &worktree_folder_paths,
        "remote workspace must be classified as a linked worktree under the main project"
    );

    let workspace_to_remove = sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .linked_worktree_workspace_to_remove(
                &worktree_folder_paths,
                Some(&remote_connection),
                Some(worktree_thread_id),
                None,
                &[],
                cx,
            )
            .map(|workspace| workspace.entity_id())
    });
    let active_workspace_id = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().entity_id()
    });
    assert_eq!(
        workspace_to_remove,
        Some(active_workspace_id),
        "archive helper should resolve the remote linked-worktree workspace"
    );
    assert!(
        server_fs.is_dir(Path::new("/external-worktree")).await,
        "direct helper check should not remove the linked worktree from disk"
    );
}

#[gpui::test]
async fn test_remote_archive_thread_with_disconnected_remote(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // When a remote thread has no linked-worktree state to archive (only
    // a main worktree), archival is a pure metadata operation: no RPCs
    // are issued against the remote server. This must succeed even when
    // the connection has dropped out, because losing connectivity should
    // not block users from cleaning up their thread list.
    //
    // Threads that *do* have linked-worktree state require a live
    // connection to run the git worktree removal on the server; that
    // path is covered by `test_remote_archive_thread_with_active_connection`.
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));

    let (project, _headless, _opts) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    let remote_client = project
        .read_with(cx, |project, _cx| project.remote_client())
        .expect("remote project should expose its client");

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let thread_id = acp::SessionId::new(Arc::from("remote-thread"));
    save_thread_metadata(
        thread_id.clone(),
        Some("Remote Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Sanity-check: there is nothing on the remote fs outside the main
    // repo, so archival should not need to touch the server.
    assert!(
        !server_fs.is_dir(Path::new("/worktrees")).await,
        "no linked worktrees on the server before archiving"
    );

    // Disconnect the remote connection before archiving. We don't
    // `run_until_parked` here because the disconnect itself triggers
    // reconnection work that can't complete in the test environment.
    remote_client.update(cx, |client, cx| {
        client.simulate_disconnect(cx).detach();
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_id, window, cx);
    });
    cx.run_until_parked();

    let is_archived = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&thread_id)
            .map(|t| t.archived)
            .unwrap_or(false)
    });
    assert!(
        is_archived,
        "thread should be archived even when remote is disconnected"
    );

    // In the merged history model the archived thread stays listed, muted.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries
            .iter()
            .any(|e| e.contains("Remote Thread") && e.contains("(archived)")),
        "archived thread should stay listed as archived: {entries:?}"
    );
}

#[gpui::test]
async fn test_collab_guest_move_thread_paths_is_noop(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    // Set up the sidebar while the project is local. This registers the
    // WorktreePathsChanged subscription for the project.
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("test-thread"));
    save_named_thread_metadata("test-thread", "My Thread", &project, cx).await;

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .map(|e| e.thread_id)
            .expect("thread must be in the store")
    });

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store.read(cx).entry(thread_id).unwrap();
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread must be saved with /project-a before collab"
        );
    });

    // Transition the project into collab mode. The sidebar's subscription is
    // still active from when the project was local.
    project.update(cx, |project, _cx| {
        project.mark_as_collab_for_testing();
    });

    // Adding a worktree fires WorktreePathsChanged with old_paths = {/project-a}.
    // The sidebar's subscription is still active, so move_thread_paths is called.
    // Without the is_via_collab() guard inside move_thread_paths, this would
    // update the stored thread paths from {/project-a} to {/project-a, /project-b}.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store
            .read(cx)
            .entry(thread_id)
            .expect("thread must still exist");
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread path must not change when project is via collab"
        );
    });
}

#[gpui::test]
async fn test_cmd_click_project_header_returns_to_last_active_linked_worktree_workspace(
    cx: &mut TestAppContext,
) {
    // Regression test for: cmd-clicking a project group header should return
    // the user to the workspace they most recently had active in that group,
    // including workspaces rooted at a linked worktree.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project-a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project-a/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let worktree_project_a =
        project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    main_project_a
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project_a
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // The multi-workspace starts with the main-paths workspace of group A
    // as the initially active workspace.
    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(main_project_a.clone(), window, cx));

    let _sidebar = setup_sidebar(&multi_workspace, cx);

    // Capture the initially active workspace (group A's main-paths workspace)
    // *before* registering additional workspaces, since `workspaces()` returns
    // retained workspaces in registration order — not activation order — and
    // the multi-workspace's starting workspace may not be retained yet.
    let main_workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    // Register the linked-worktree workspace (group A) and the group-B
    // workspace. Both get retained by the multi-workspace.
    let worktree_workspace_a = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project_a.clone(), window, cx)
    });
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });

    cx.run_until_parked();

    // Step 1: activate the linked-worktree workspace. The MultiWorkspace
    // records this as the last-active workspace for group A on its
    // ProjectGroupState. (We don't assert on the initial active workspace
    // because `test_add_workspace` may auto-activate newly registered
    // workspaces — what matters for this test is the explicit sequence of
    // activations below.)
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        worktree_workspace_a,
        "linked-worktree workspace should be active after step 1"
    );

    // Step 2: switch to the workspace for group B. Group A's last-active
    // workspace remains the linked-worktree one (group B getting activated
    // records *its own* last-active workspace, not group A's).
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_b.clone(), None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "group B's workspace should be active after step 2"
    );

    // Step 3: switch back to group A. Project headers (and their cmd-click
    // handler) are gone in the merged history model; the surviving invariant
    // lives on the MultiWorkspace: the last-active workspace for group A is
    // the linked-worktree one, not the main-paths one. This is what any
    // group-level navigation (e.g. the recent-projects menu) resolves to.
    let group_a_key = main_workspace_a.read_with(cx, |ws, cx| ws.project_group_key(cx));
    let last_active_for_group_a = multi_workspace.read_with(cx, |mw, cx| {
        mw.last_active_workspace_for_group(&group_a_key, cx)
    });
    assert_eq!(
        last_active_for_group_a.as_ref(),
        Some(&worktree_workspace_a),
        "group A's last-active workspace should be the linked-worktree one"
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let active_after_switch = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert_eq!(
        active_after_switch, worktree_workspace_a,
        "activating group A's last-active workspace should return to the \
         linked-worktree workspace, not the main-paths workspace"
    );
    assert_ne!(
        active_after_switch, main_workspace_a,
        "navigation must not fall back to the main-paths workspace when a \
         linked-worktree workspace was the last-active one for the group"
    );
}

#[gpui::test]
async fn test_new_worktree_draft_creates_worktree_on_first_send(cx: &mut TestAppContext) {
    // New model: picking "new worktree" starts a fresh draft IN PLACE (no
    // worktree, no workspace, no dummy thread). The git worktree and its
    // workspace are created only on first send, with the composed message in
    // hand, and the message lands in the new workspace's own thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (_sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let source_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    // Give any workspace created during the flow an agent panel, and wire the
    // destination pull that production runs on `ActiveWorkspaceChanged`, so the
    // switch carries the source draft's message into the new workspace's draft.
    cx.update(|window, cx| {
        let window_handle = window.window_handle();
        cx.subscribe(
            &multi_workspace,
            move |multi_workspace, event: &workspace::MultiWorkspaceEvent, cx| match event {
                workspace::MultiWorkspaceEvent::WorkspaceAdded(workspace) => {
                    let workspace = workspace.clone();
                    window_handle
                        .update(cx, |_, window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                let panel =
                                    cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                                workspace.add_panel(panel, window, cx);
                            });
                        })
                        .ok();
                }
                workspace::MultiWorkspaceEvent::ActiveWorkspaceChanged { source_workspace } => {
                    let Some(source) = source_workspace.clone() else {
                        return;
                    };
                    let active = multi_workspace.read(cx).workspace().clone();
                    window_handle
                        .update(cx, |_, window, cx| {
                            active.update(cx, |workspace, cx| {
                                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                    panel.update(cx, |panel, cx| {
                                        panel.initialize_from_source_workspace_if_needed(
                                            source.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            });
                        })
                        .ok();
                }
                _ => {}
            },
        )
        .detach();
    });

    // One click on the split plus button: a fresh in-place draft only.
    cx.update(|window, cx| {
        create_worktree_thread(
            &source_workspace,
            zed_actions::NewWorktreeBranchTarget::CurrentBranch,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    // Composing the draft creates no worktree and no second workspace.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "composing a new-worktree draft must not create a worktree/workspace"
    );
    panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("a draft should exist in the current workspace");

    // The draft's thread view connects asynchronously; tick until it appears,
    // then type into it.
    // The unstarted composer exists immediately: nothing spawns until send.
    agent_ui::test_support::type_draft_prompt(&panel, "do the thing", cx);

    // Send: the worktree, its workspace, and the thread are created now.
    agent_ui::test_support::send_draft(&panel, cx);

    // Exactly one worktree workspace was created, and it is now active.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "sending should create exactly one worktree workspace"
    );
    let new_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert_ne!(
        new_workspace, source_workspace,
        "the new worktree workspace should be active"
    );

    // The message is SUBMITTED in the new workspace's own thread. Accepting a
    // message that merely sits in the destination composer is what let the
    // "empty thread in the new worktree" bug pass.
    let new_panel = new_workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .expect("new workspace should have an agent panel");
    let user_messages = new_panel.read_with(cx, |panel, cx| {
        let view = panel
            .active_thread_view(cx)
            .expect("new workspace should have an active thread");
        view.read(cx)
            .thread
            .read(cx)
            .entries()
            .iter()
            .filter(|entry| matches!(entry, acp_thread::AgentThreadEntry::UserMessage(_)))
            .count()
    });
    assert_eq!(
        user_messages, 1,
        "the composed message is submitted exactly once in the new worktree's thread"
    );
    assert_eq!(
        new_panel.read_with(cx, |panel, cx| panel.open_thread_tab_ids(cx).len()),
        1,
        "the new worktree holds exactly one thread, with no empty dummy beside it"
    );

    // Nothing was started in the worktree the user composed in: the whole point
    // is that the send moves to the new worktree, it does not also run here.
    let source_panel = source_workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .expect("source workspace should have an agent panel");
    source_panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_agent_thread(cx).is_none(),
            "the source worktree must not start a thread of its own"
        );
    });
    let source_text = agent_ui::test_support::draft_prompt_text(&source_panel, cx);
    assert!(
        source_text.trim().is_empty(),
        "the source draft's composer is cleared, so the message cannot land twice"
    );
}

#[gpui::test]
async fn test_new_worktree_send_failure_leaves_draft_usable(cx: &mut TestAppContext) {
    // When worktree creation fails on send (here: no git repository), the draft
    // stays in place with its typed message intact and no workspace is created.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    // No `.git`: worktree creation must fail.
    fs.insert_tree(
        "/project",
        serde_json::json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (_sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let source_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.update(|window, cx| {
        create_worktree_thread(
            &source_workspace,
            zed_actions::NewWorktreeBranchTarget::CurrentBranch,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    // The unstarted composer exists immediately: nothing spawns until send.
    agent_ui::test_support::type_draft_prompt(&panel, "do the thing", cx);

    agent_ui::test_support::send_draft(&panel, cx);

    // No worktree workspace was created, and the draft keeps its message.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "a failed worktree creation must not open a workspace"
    );
    let text = agent_ui::test_support::draft_prompt_text(&panel, cx);
    assert_eq!(
        text, "do the thing",
        "the draft stays usable in place with its message intact"
    );

    // The failure must not flip the draft's worktree choice: a retry still
    // asks for a new worktree instead of silently sending here. (The reset
    // used to happen before creation, so a retry after a failure ran the
    // message in the CURRENT worktree.)
    agent_ui::test_support::send_draft(&panel, cx);
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "the retry targeted a worktree again (and failed again); nothing opened"
    );
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_agent_thread(cx).is_none(),
            "a retried new-worktree send never starts a thread in the current worktree"
        );
    });
}

#[test]
fn test_split_leading_icon_char() {
    // A leading symbol set off by whitespace is pulled out and trimmed from the
    // title.
    let (icon, title, positions) =
        split_leading_icon_char(&"✳ Implement separate config".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "✳");
    assert_eq!(title.as_ref(), "Implement separate config");
    assert_eq!(positions, Vec::<usize>::new());

    // No prefix when the title starts with a letter.
    assert!(split_leading_icon_char(&"Implement separate config".into(), &[]).is_none());

    // Leading whitespace is not treated as a prefix.
    assert!(split_leading_icon_char(&" leading space".into(), &[]).is_none());

    // An alphanumeric prefix such as a version marker is not treated as an icon.
    assert!(split_leading_icon_char(&"v1 Running".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"1 first".into(), &[]).is_none());

    // A title consisting only of a symbol (no whitespace separator) is left
    // untouched.
    assert!(split_leading_icon_char(&"✳".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"✳Thinking".into(), &[]).is_none());

    // A run of the same symbol collapses to a single glyph.
    let (icon, title, _) = split_leading_icon_char(&">>> Thinking".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), ">");
    assert_eq!(title.as_ref(), "Thinking");

    // Surrounding ASCII brackets are stripped so the inner glyph is used.
    let (icon, title, _) = split_leading_icon_char(&"[!] codex waiting".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "!");
    assert_eq!(title.as_ref(), "codex waiting");

    // A run of dots is condensed into an ellipsis.
    let (icon, title, _) = split_leading_icon_char(&"... working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[...] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[…] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    // Multi-codepoint emoji are kept intact rather than sliced mid-cluster.
    let (icon, title, _) = split_leading_icon_char(&"🇺🇸 flag".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "🇺🇸");
    assert_eq!(title.as_ref(), "flag");

    // Highlight positions are shifted to account for the stripped prefix, and
    // positions that fall inside the stripped prefix are dropped.
    let title: SharedString = "# abc".into();
    let abc_offset = title.find('a').unwrap();
    let (icon, trimmed, positions) =
        split_leading_icon_char(&title, &[0, abc_offset, abc_offset + 1]).unwrap();
    assert_eq!(icon.as_ref(), "#");
    assert_eq!(trimmed.as_ref(), "abc");
    assert_eq!(positions, vec![0, 1]);
}

#[gpui::test]
async fn test_find_or_create_workspace_returns_the_created_remote_workspace(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let local_project = init_test_project("/local", cx).await;
    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(local_project, window, cx));
    let local_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/remote-project", serde_json::json!({ "src": {} }))
        .await;
    let (opts, server_session, _) = remote::RemoteClient::fake_server(cx, server_cx);
    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let _headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor)),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;

    // Stand in for the save prompt from a concurrent workspace removal: as
    // soon as the remote workspace is activated mid-open, activate the local
    // workspace again. The open must still return the workspace it created,
    // not whichever workspace is active once it finishes.
    multi_workspace.update_in(cx, |_, window, cx| {
        let local_workspace = local_workspace.clone();
        cx.subscribe_in(&cx.entity(), window, move |this, _, event, window, cx| {
            if matches!(event, MultiWorkspaceEvent::WorkspaceAdded(_)) {
                this.activate(local_workspace.clone(), None, window, cx);
            }
        })
        .detach();
    });

    let created = multi_workspace
        .update_in(cx, |mw, window, cx| {
            let key = ProjectGroupKey::new(
                Some(opts.clone()),
                PathList::new(&[PathBuf::from("/remote-project")]),
            );
            mw.find_or_create_workspace(
                PathList::new(&[PathBuf::from("/remote-project")]),
                Some(opts),
                Some(key),
                move |_, _, _| Task::ready(Ok(Some(remote_client))),
                None,
                workspace::OpenMode::Activate,
                None,
                window,
                cx,
            )
        })
        .await
        .expect("opening the remote project should succeed");
    cx.run_until_parked();

    assert_eq!(
        created.read_with(cx, |workspace, cx| PathList::new(&workspace.root_paths(cx))),
        PathList::new(&[PathBuf::from("/remote-project")]),
        "the returned workspace should be the remote workspace that was created"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        local_workspace,
        "the local workspace should have re-activated during the open"
    );
}

#[track_caller]
fn assert_single_focused_draft_tab(
    panel: &Entity<AgentPanel>,
    cx: &mut gpui::VisualTestContext,
    msg: &str,
) {
    let tab_count = panel.read_with(cx, |panel, cx| panel.thread_pane().read(cx).items_len());
    assert_eq!(tab_count, 1, "{msg}: expected one thread tab in the pane");
    let is_thread_tab = panel.read_with(cx, |panel, cx| {
        panel
            .thread_pane()
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<agent_ui::thread_tab::ThreadTab>().is_some())
    });
    assert!(
        is_thread_tab,
        "{msg}: active pane item should be a ThreadTab"
    );

    let editor = agent_ui::test_support::draft_message_editor(panel, cx);
    cx.update(|window, cx| {
        assert!(
            editor.focus_handle(cx).contains_focused(window, cx),
            "{msg}: draft message editor should be focused"
        );
    });
}

#[gpui::test]
async fn test_sidebar_plus_opens_draft_thread_tab(cx: &mut TestAppContext) {
    // The sidebar's new-thread button must reliably produce a draft
    // ThreadTab in the panel's thread pane with the message editor focused.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace, window, cx);
    });
    cx.run_until_parked();

    assert_single_focused_draft_tab(&panel, cx, "after create_new_thread");
}

#[gpui::test]
async fn test_sidebar_new_thread_waits_for_panel_load(cx: &mut TestAppContext) {
    // Freshly opened workspaces load their agent panel asynchronously. A
    // new-thread request that arrives before the panel is registered must
    // be parked and fulfilled once the panel lands, instead of silently
    // doing nothing and leaving the panel empty.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, _panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Second workspace whose agent panel has not loaded yet.
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // No panel yet: the request must be parked rather than dropped.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.pending_new_thread_workspace.is_some(),
            "thread creation should be parked until the panel loads"
        );
    });

    // The panel registers (as it would after its async load); the parked
    // request is fulfilled.
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.pending_new_thread_workspace.is_none(),
            "parked thread creation should be consumed once the panel loads"
        );
    });
    assert_single_focused_draft_tab(&panel_b, cx, "after panel load");
}

#[gpui::test]
async fn test_thread_tabs_span_workspaces(cx: &mut TestAppContext) {
    // Each panel's thread pane mirrors the other workspaces' open threads
    // as foreign tabs; activating a foreign tab switches to its workspace
    // and focuses the real thread there.
    use agent_ui::thread_tab::{ForeignThreadTab, ThreadTab};

    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (_sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    cx.run_until_parked();

    // A thread in workspace A.
    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done A".into()),
    )]);
    open_thread_with_connection(&panel_a, connection_a, cx);
    send_message(&panel_a, cx);
    let thread_a = panel_a.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // A second workspace with its own panel and thread.
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done B".into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b, cx);
    send_message(&panel_b, cx);
    let thread_b = panel_b.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    cx.run_until_parked();

    // Both panes show both threads: their own as a real tab, the other
    // workspace's as a foreign proxy, in global insertion order.
    let tab_kinds = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
        panel.read_with(cx, |panel, cx| {
            panel
                .thread_pane()
                .read(cx)
                .items()
                .map(|item| {
                    if let Some(tab) = item.downcast::<ThreadTab>() {
                        ("real", tab.read(cx).thread_id(cx))
                    } else if let Some(proxy) = item.downcast::<ForeignThreadTab>() {
                        ("foreign", proxy.read(cx).thread_id())
                    } else {
                        panic!("unexpected item type in thread pane");
                    }
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        tab_kinds(&panel_a, cx),
        vec![("real", thread_a), ("foreign", thread_b)],
        "workspace A's pane should show its real tab plus B's thread as a foreign tab"
    );
    assert_eq!(
        tab_kinds(&panel_b, cx),
        vec![("foreign", thread_a), ("real", thread_b)],
        "workspace B's pane should show A's thread as a foreign tab plus its real tab"
    );

    // Make workspace A active, then click B's foreign tab in A's pane.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let foreign_index = panel_a.read_with(cx, |panel, cx| {
        panel
            .thread_pane()
            .read(cx)
            .items()
            .position(|item| item.downcast::<ForeignThreadTab>().is_some())
            .expect("foreign tab should exist in A's pane")
    });
    panel_a.update_in(cx, |panel, window, cx| {
        panel.thread_pane().clone().update(cx, |pane, cx| {
            // A click activates the tab with focus.
            pane.activate_item(foreign_index, true, true, window, cx);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "activating the foreign tab should switch to its workspace"
    );
    // The proxy must not stay visible in A's pane.
    panel_a.read_with(cx, |panel, cx| {
        assert!(
            panel
                .thread_pane()
                .read(cx)
                .active_item()
                .is_some_and(|item| item.downcast::<ThreadTab>().is_some()),
            "A's pane should have re-activated its own real tab"
        );
    });
    // The real thread is active and focused in workspace B's panel.
    panel_b.read_with(cx, |panel, cx| {
        assert_eq!(
            panel.active_thread_id(cx),
            Some(thread_b),
            "workspace B's panel should have its real thread active"
        );
    });
    let view_b = panel_b.read_with(cx, |panel, _| {
        panel
            .active_conversation_view()
            .expect("thread view should exist")
            .clone()
    });
    cx.update(|window, cx| {
        assert!(
            view_b.focus_handle(cx).contains_focused(window, cx),
            "the foreign thread should be focused in its home workspace"
        );
    });
}

#[gpui::test]
async fn test_closing_foreign_tab_closes_real_tab_in_home_workspace(cx: &mut TestAppContext) {
    use agent_ui::thread_tab::{ForeignThreadTab, ThreadTab};

    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (_sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done A".into()),
    )]);
    open_thread_with_connection(&panel_a, connection_a, cx);
    send_message(&panel_a, cx);
    let thread_a = panel_a.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done B".into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b, cx);
    send_message(&panel_b, cx);
    cx.run_until_parked();

    // From workspace B (active), close A's thread via its foreign tab.
    let proxy_item_id = panel_b.read_with(cx, |panel, cx| {
        panel
            .thread_pane()
            .read(cx)
            .items()
            .find(|item| item.downcast::<ForeignThreadTab>().is_some())
            .map(|item| item.item_id())
            .expect("foreign tab should exist in B's pane")
    });
    panel_b.update_in(cx, |panel, window, cx| {
        panel.thread_pane().clone().update(cx, |pane, cx| {
            pane.close_item_by_id(proxy_item_id, SaveIntent::Close, window, cx)
                .detach();
        });
    });
    cx.run_until_parked();

    // The real tab in workspace A is gone, and B keeps only its own tab.
    panel_a.read_with(cx, |panel, cx| {
        assert!(
            !panel
                .thread_pane()
                .read(cx)
                .items_of_type::<ThreadTab>()
                .any(|tab| tab.read(cx).thread_id(cx) == thread_a),
            "closing the foreign tab should close the real tab in its home workspace"
        );
    });
    panel_b.read_with(cx, |panel, cx| {
        assert!(
            !panel
                .thread_pane()
                .read(cx)
                .items()
                .filter_map(|item| item.downcast::<ForeignThreadTab>())
                .any(|proxy| proxy.read(cx).thread_id() == thread_a),
            "thread A's foreign tab should not come back after the real tab closed"
        );
    });
    // Closing never respawns a tab: workspace A's pane stays empty.
    panel_a.read_with(cx, |panel, cx| {
        assert!(
            panel
                .thread_pane()
                .read(cx)
                .items_of_type::<ThreadTab>()
                .next()
                .is_none(),
            "closing the last tab should leave workspace A's pane empty"
        );
    });
    // Closing a tab must not switch workspaces.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "closing a foreign tab should not change the active workspace"
    );
}

#[gpui::test]
async fn test_thread_pr_chips_always_show_pr_state_for_branches(cx: &mut TestAppContext) {
    init_test(cx);

    let make_entry = |worktrees: Vec<ui::ThreadItemWorktreeInfo>| ThreadEntry {
        metadata: ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(acp::SessionId::new("session")),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Thread".into()),
            title_override: None,
            updated_at: Utc::now(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            archived: false,
        },
        icon: ui::IconName::ZedAgent,
        icon_from_external_svg: None,
        status: ui::AgentThreadStatus::Completed,
        workspace: ThreadEntryWorkspace::Closed {
            folder_paths: PathList::new(&[Path::new("/repo/wt")]),
            project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
        },
        is_live: false,
        is_title_generating: false,
        draft: None,
        draft_leaves_workspace: false,
        highlight_positions: Vec::new(),
        worktrees,
        diff_stats: DiffStats::default(),
        solo_worktree: None,
                under_worktree_header: false,
    };

    // A row with a branch but no known PR always shows a muted, inert
    // "no PR" indicator.
    let entry_with_branch = make_entry(vec![ui::ThreadItemWorktreeInfo {
        worktree_name: Some("wt".into()),
        branch_name: Some("feature".into()),
        full_path: "/repo/wt".into(),
        highlight_positions: Vec::new(),
        kind: ui::WorktreeKind::Linked,
    }]);
    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&entry_with_branch, cx);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].label.as_ref(), "no PR");
        assert!(chips[0].url.is_none(), "the no-PR chip should be inert");
    });

    // A row without any branch still shows the absent PR state, so rows never
    // change shape when a branch and a PR appear.
    let entry_without_branch = make_entry(vec![ui::ThreadItemWorktreeInfo {
        worktree_name: Some("wt".into()),
        branch_name: None,
        full_path: "/repo/wt".into(),
        highlight_positions: Vec::new(),
        kind: ui::WorktreeKind::Linked,
    }]);
    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&entry_without_branch, cx);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].label.as_ref(), "no PR");
        assert!(chips[0].url.is_none(), "the no-PR chip should be inert");
    });

    // A row with no worktrees at all behaves the same.
    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&make_entry(Vec::new()), cx);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].label.as_ref(), "no PR");
    });
}

// A draft has no worktree branch of its own: its paths still resolve to the
// project's current branch, and the old code surfaced that branch's PRs on the
// draft row. A draft must resolve no branches at all, so it never adopts the
// project branch's PRs (nor watches or snapshots them).
#[gpui::test]
async fn test_draft_row_suppresses_project_branch_prs(cx: &mut TestAppContext) {
    init_test(cx);

    let make_entry = |draft: Option<DraftKind>| ThreadEntry {
        metadata: ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(acp::SessionId::new("session")),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Thread".into()),
            title_override: None,
            updated_at: Utc::now(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            archived: false,
        },
        icon: ui::IconName::ZedAgent,
        icon_from_external_svg: None,
        status: ui::AgentThreadStatus::Completed,
        workspace: ThreadEntryWorkspace::Closed {
            folder_paths: PathList::new(&[Path::new("/repo")]),
            project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
        },
        is_live: false,
        is_title_generating: false,
        draft,
        draft_leaves_workspace: false,
        highlight_positions: Vec::new(),
        // The project branch a draft would otherwise inherit.
        worktrees: vec![ui::ThreadItemWorktreeInfo {
            worktree_name: Some("repo".into()),
            branch_name: Some("main".into()),
            full_path: "/repo".into(),
            highlight_positions: Vec::new(),
            kind: ui::WorktreeKind::Main,
        }],
        diff_stats: DiffStats::default(),
        solo_worktree: None,
                under_worktree_header: false,
    };

    let draft_entry = make_entry(Some(DraftKind::WithContent));
    let live_entry = make_entry(None);

    assert!(
        Sidebar::thread_branches(&draft_entry).is_empty(),
        "a draft must not adopt its project's branch"
    );
    assert!(
        !Sidebar::thread_branches(&live_entry).is_empty(),
        "a non-draft resolves its worktree branch"
    );

    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&draft_entry, cx);
        assert!(
            chips.iter().all(|chip| chip.url.is_none()),
            "a draft row must show no clickable PR badge from the project branch"
        );
    });
}

// Sidebar rows no longer carry a branch chip (or the "no branch" pill): the row
// renders through the branch-free worktree metadata path and is still measured.
#[gpui::test]
async fn test_sidebar_row_renders_without_branch_chip(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("branchless-thread")),
        Some("Threaded work".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    let row_ix = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_)))
            .expect("the thread row should be present")
    });
    let bounds = sidebar.read_with(cx, |sidebar, _| sidebar.list_state.bounds_for_item(row_ix));
    assert!(
        bounds.is_some(),
        "the thread row should render and be measured"
    );
}

#[gpui::test]
async fn test_archived_thread_keeps_its_persisted_pr_badge(cx: &mut TestAppContext) {
    init_test(cx);

    // An archived thread: its worktree is gone from disk, so it has no branch
    // to resolve and gh_status has nothing to query.
    let thread_id = ThreadId::new();
    let entry = ThreadEntry {
        metadata: ThreadMetadata {
            thread_id,
            session_id: Some(acp::SessionId::new("archived")),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Archived".into()),
            title_override: None,
            updated_at: Utc::now(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            archived: true,
        },
        icon: ui::IconName::ZedAgent,
        icon_from_external_svg: None,
        status: ui::AgentThreadStatus::Completed,
        workspace: ThreadEntryWorkspace::Closed {
            folder_paths: PathList::new(&[Path::new("/repo/wt")]),
            project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
        },
        is_live: false,
        is_title_generating: false,
        draft: None,
        draft_leaves_workspace: false,
        highlight_positions: Vec::new(),
        worktrees: Vec::new(),
        diff_stats: DiffStats::default(),
        solo_worktree: None,
                under_worktree_header: false,
    };

    // Without a persisted snapshot the row degrades to the inert "no PR" pill.
    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&entry, cx);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].label.as_ref(), "no PR");
    });

    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.set_pr_snapshot(
                thread_id,
                ThreadPrSnapshot {
                    branches: vec!["feature".into()],
                    prs: vec![gh_status::PrStatus {
                        number: 42,
                        url: "https://github.com/org/repo/pull/42".into(),
                        title: "Ship it".into(),
                        state: gh_status::PrState::Merged,
                        checks: gh_status::ChecksState::Passing,
                        review: gh_status::ReviewState::Approved,
                        failing_checks: Vec::new(),
                        extra_failing_checks: 0,
                    }],
                },
                cx,
            );
        });
    });

    cx.update(|cx| {
        let chips = Sidebar::thread_pr_chips(&entry, cx);
        assert_eq!(
            chips.len(),
            1,
            "the persisted PR should render as one badge"
        );
        assert_eq!(chips[0].label.as_ref(), "#42");
        assert_eq!(
            chips[0].url.as_deref(),
            Some("https://github.com/org/repo/pull/42")
        );
        let detail = chips[0]
            .detail
            .as_ref()
            .expect("a real PR has a detail card");
        assert_eq!(detail.state.as_ref(), "merged");
        assert!(
            chips[0].checks.is_none(),
            "a merged PR passed by definition, so it shows no checks glyph"
        );
    });
}

#[gpui::test]
fn test_a_worktree_archive_stops_once_nothing_is_left_to_archive(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, archived: bool| {
        Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(title.to_string())),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                remote_connection: None,
                archived,
            },
            icon: ui::IconName::ZedAgent,
            icon_from_external_svg: None,
            status: ui::AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::new(&[Path::new("/repo/wt-a")]),
                project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
            },
            is_live: false,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
                under_worktree_header: false,
        })
    };

    let header_sessions = |rows: Vec<Arc<ThreadEntry>>| {
        Sidebar::group_rows_by_workspace(rows.into_iter().map(ListEntry::Thread).collect())
            .into_iter()
            .find_map(|entry| match entry {
                ListEntry::WorkspaceHeader(header) => Some(header.member_sessions.clone()),
                _ => None,
            })
            .expect("the group has a header")
    };

    // The header's archive takes the group's unarchived threads.
    assert_eq!(
        header_sessions(vec![
            make_entry("live one", false),
            make_entry("done", true)
        ])
        .len(),
        1,
        "an already-archived thread is not something the worktree archive takes"
    );
    // With every thread archived, the header has nothing left to offer.
    assert!(
        header_sessions(vec![
            make_entry("done", true),
            make_entry("also done", true)
        ])
        .is_empty(),
        "an archived group shows no worktree archive"
    );
}

#[gpui::test]
fn test_a_live_thread_does_not_pull_its_worktree_siblings_into_active(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, folder: &str, is_live: bool| {
        Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(title.to_string())),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                remote_connection: None,
                archived: false,
            },
            icon: ui::IconName::ZedAgent,
            icon_from_external_svg: None,
            status: ui::AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::new(&[Path::new(folder)]),
                project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
            },
            is_live,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
                under_worktree_header: false,
        })
    };

    // Two threads in one worktree, one of them running, plus a thread of a
    // worktree nobody is working in.
    let threads = vec![
        make_entry("running", "/repo/wt-a", true),
        make_entry("quiet sibling", "/repo/wt-a", false),
        make_entry("elsewhere", "/repo/wt-b", false),
    ];

    let mut session_ids = HashSet::default();
    let mut thread_ids = HashSet::default();
    let entries = Sidebar::sectioned_entries(
        Vec::new(),
        threads,
        &HashSet::default(),
        &HashMap::default(),
        &mut session_ids,
        &mut thread_ids,
    );

    assert_eq!(
        entry_shape_strings(&entries),
        vec![
            // Only the live thread is Active; its quiet worktree sibling
            // does not come along just for sharing a worktree. Alone in the
            // section, it is its own worktree's row and needs no header.
            "section: Active",
            "thread: running",
            // All Threads is what Active left behind, so the live thread is
            // not repeated here: only its quiet sibling and the unrelated
            // worktree's thread. That leaves one row in each worktree, and one
            // row is not what a header is for.
            "section: All Threads",
            "thread: elsewhere",
            "thread: quiet sibling",
        ],
        "a live thread does not bring the rest of its worktree into Active, and leaves All Threads itself"
    );
}

#[gpui::test]
fn test_archived_threads_go_to_their_own_bottom_section(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, archived: bool, updated_at: DateTime<Utc>| {
        Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(title.to_string())),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at,
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                remote_connection: None,
                archived,
            },
            icon: ui::IconName::ZedAgent,
            icon_from_external_svg: None,
            status: ui::AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::new(&[Path::new("/repo/wt")]),
                project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
            },
            is_live: false,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
                under_worktree_header: false,
        })
    };

    let now = Utc::now();
    // The archived thread is the most recent, so a single merged list would
    // have sorted it to the top of the history.
    let threads = vec![
        make_entry("archived", true, now),
        make_entry("live", false, now - chrono::Duration::hours(1)),
    ];

    let mut session_ids = HashSet::default();
    let mut thread_ids = HashSet::default();
    let entries = Sidebar::sectioned_entries(
        Vec::new(),
        threads,
        &HashSet::default(),
        &HashMap::default(),
        &mut session_ids,
        &mut thread_ids,
    );

    assert_eq!(
        entry_shape_strings(&entries),
        vec![
            // Active always renders: it carries the new-thread button.
            "section: Active",
            "section: All Threads",
            // Every section groups by worktree, history included, but a
            // worktree holding one thread is that thread's own row.
            "thread: live",
            "section: Archived",
            "thread: archived",
        ],
        "archived threads belong to their own section at the bottom"
    );
    assert_eq!(thread_ids.len(), 2, "both rows stay tracked");
}

#[gpui::test]
fn test_active_rows_follow_the_tab_order(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, folder: &str, minutes_old: i64| {
        Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(title.to_string())),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at: Utc::now() - chrono::Duration::minutes(minutes_old),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                remote_connection: None,
                archived: false,
            },
            icon: ui::IconName::ZedAgent,
            icon_from_external_svg: None,
            status: ui::AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::new(&[Path::new(folder)]),
                project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
            },
            is_live: false,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
            under_worktree_header: false,
        })
    };

    // Three tabs in one worktree, arranged in an order the timestamps
    // disagree with: the oldest thread sits in the first tab.
    let oldest = make_entry("oldest", "/repo/wt", 30);
    let middle = make_entry("middle", "/repo/wt", 20);
    let newest = make_entry("newest", "/repo/wt", 10);
    // Live, but in a workspace that is not showing it, so it has no tab.
    let untabbed = Arc::new(ThreadEntry {
        is_live: true,
        ..(*make_entry("untabbed", "/repo/wt", 40)).clone()
    });

    let threads = vec![newest.clone(), untabbed, oldest.clone(), middle.clone()];
    let open_thread_ids: HashSet<agent_ui::ThreadId> = [
        oldest.metadata.thread_id,
        middle.metadata.thread_id,
        newest.metadata.thread_id,
    ]
    .into_iter()
    .collect();
    let tab_positions: HashMap<agent_ui::ThreadId, usize> = [
        (oldest.metadata.thread_id, 0),
        (middle.metadata.thread_id, 1),
        (newest.metadata.thread_id, 2),
    ]
    .into_iter()
    .collect();

    let mut session_ids = HashSet::default();
    let mut thread_ids = HashSet::default();
    let entries = Sidebar::sectioned_entries(
        Vec::new(),
        threads,
        &open_thread_ids,
        &tab_positions,
        &mut session_ids,
        &mut thread_ids,
    );

    let active: Vec<String> = entry_shape_strings(&entries)
        .into_iter()
        .skip_while(|row| row != "section: Active")
        .take_while(|row| row == "section: Active" || !row.starts_with("section:"))
        .collect();

    assert_eq!(
        active,
        vec![
            "section: Active",
            "workspace: Workspace",
            // Tab order, not newest-first: the tabs are the order the user
            // arranged.
            "thread: oldest",
            "thread: middle",
            "thread: newest",
            // No tab of its own, so it falls to the back and sorts by time
            // with anything else that has none.
            "thread: untabbed",
        ],
        "Active rows sit in the order their tabs do"
    );

    // Every thread here is open, so All Threads has nothing left to list and
    // the section goes away entirely rather than standing empty.
    assert!(
        !entry_shape_strings(&entries).contains(&"section: All Threads".to_string()),
        "with everything open, All Threads has no rows and no header"
    );
}

/// Active and All Threads are two halves of one set, not two views of it: a
/// thread is in exactly one of them, and closing it moves it from the first to
/// the second at the position its age gives it.
#[gpui::test]
fn test_a_thread_is_in_active_or_in_all_threads_but_not_both(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, minutes_old: i64| {
        Arc::new(ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(title.to_string())),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at: Utc::now() - chrono::Duration::minutes(minutes_old),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                remote_connection: None,
                archived: false,
            },
            icon: ui::IconName::ZedAgent,
            icon_from_external_svg: None,
            status: ui::AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::new(&[Path::new("/repo/wt")]),
                project_group_key: ProjectGroupKey::new(None, PathList::new(&[Path::new("/repo")])),
            },
            is_live: false,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
            under_worktree_header: false,
        })
    };

    let newest = make_entry("newest", 10);
    let middle = make_entry("middle", 20);
    let oldest = make_entry("oldest", 30);
    let threads = vec![newest.clone(), middle.clone(), oldest.clone()];

    let shape = |open: &[&Arc<ThreadEntry>]| {
        let open_thread_ids: HashSet<agent_ui::ThreadId> =
            open.iter().map(|thread| thread.metadata.thread_id).collect();
        let tab_positions: HashMap<agent_ui::ThreadId, usize> = open
            .iter()
            .enumerate()
            .map(|(position, thread)| (thread.metadata.thread_id, position))
            .collect();
        let mut session_ids = HashSet::default();
        let mut thread_ids = HashSet::default();
        entry_shape_strings(&Sidebar::sectioned_entries(
            Vec::new(),
            threads.clone(),
            &open_thread_ids,
            &tab_positions,
            &mut session_ids,
            &mut thread_ids,
        ))
    };

    // One thread open. It is listed under Active and nowhere else; the two
    // still-closed threads keep All Threads to themselves, newest first.
    assert_eq!(
        shape(&[&middle]),
        vec![
            "section: Active",
            "thread: middle",
            "section: All Threads",
            "workspace: Workspace",
            "thread: newest",
            "thread: oldest",
        ],
        "an open thread appears under Active instead of twice"
    );

    // Close it. It comes back to All Threads between the two it is older and
    // newer than, rather than at the top of the section.
    assert_eq!(
        shape(&[]),
        vec![
            "section: Active",
            "section: All Threads",
            "workspace: Workspace",
            "thread: newest",
            "thread: middle",
            "thread: oldest",
        ],
        "closing a thread returns it to All Threads at its age"
    );

    // Open all three and All Threads has nothing to show. The section drops
    // out; Active keeps its header either way, since that is where the
    // new-thread button lives.
    assert_eq!(
        shape(&[&oldest, &middle, &newest]),
        vec![
            "section: Active",
            "workspace: Workspace",
            "thread: oldest",
            "thread: middle",
            "thread: newest",
        ],
        "a workspace with everything open shows no All Threads section at all"
    );
}

fn entry_shape_strings(entries: &[ListEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| match entry {
            ListEntry::SectionHeader(section) => format!("section: {}", section.label()),
            ListEntry::WorkspaceHeader(header) => format!("workspace: {}", header.label),
            ListEntry::Thread(thread) => format!("thread: {}", thread.metadata.display_title()),
            ListEntry::Terminal(terminal) => {
                format!("terminal: {}", terminal.metadata.display_title())
            }
        })
        .collect()
}

/// Seeds a history thread and an archived thread, so the list has an
/// "All Threads" section above an "Archived" section.
async fn setup_sidebar_with_two_sections(
    cx: &mut TestAppContext,
) -> (Entity<Sidebar>, &mut gpui::VisualTestContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_named_thread_metadata("history-thread", "History Thread", &project, cx).await;

    let archived_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_named_thread_metadata("archived-thread", "Archived Thread", &project, cx).await;
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            let thread_id = store
                .entries()
                .find(|entry| entry.session_id.as_ref() == Some(&archived_session_id))
                .map(|entry| entry.thread_id)
                .expect("archived thread should be saved");
            store.archive(thread_id, None, cx)
        })
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    (sidebar, cx)
}

fn sidebar_shape(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, _cx| {
        entry_shape_strings(&sidebar.contents.entries)
    })
}

#[gpui::test]
async fn test_collapsed_section_hides_its_rows(cx: &mut TestAppContext) {
    let (sidebar, cx) = setup_sidebar_with_two_sections(cx).await;

    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
            // One thread in the worktree, so the thread's own row is it.
            "thread: History Thread",
            "section: Archived",
            "thread: Archived Thread",
        ]
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.toggle_section(SidebarSection::AllThreads, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
            "section: Archived",
            "thread: Archived Thread",
        ],
        "a collapsed section keeps its header and drops its rows"
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            entry_shape_strings(&sidebar.contents.all_entries).len(),
            5,
            "the underlying rows stay tracked while collapsed"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.toggle_section(SidebarSection::AllThreads, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
            // One thread in the worktree, so the thread's own row is it.
            "thread: History Thread",
            "section: Archived",
            "thread: Archived Thread",
        ],
        "expanding restores the rows"
    );
}

#[gpui::test]
async fn test_keyboard_navigation_skips_collapsed_rows(cx: &mut TestAppContext) {
    let (sidebar, cx) = setup_sidebar_with_two_sections(cx).await;
    focus_sidebar(&sidebar, cx);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.select_first(&SelectFirst, window, cx);
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            "  History Thread  <== selected",
            "  Archived Thread (archived)",
        ]
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.toggle_section(SidebarSection::AllThreads, cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.select_first(&SelectFirst, window, cx);
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Archived Thread (archived)  <== selected"],
        "selection lands on the first row of the expanded section"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.select_next(&SelectNext, window, cx);
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Archived Thread (archived)  <== selected"],
        "the collapsed section's rows are never selectable"
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar
                .selection
                .and_then(|ix| sidebar.contents.entries.get(ix))
                .is_some_and(|entry| matches!(entry, ListEntry::Thread(thread)
                    if thread.metadata.archived)),
        );
    });
}

#[gpui::test]
async fn test_collapse_state_round_trips_through_serialization(cx: &mut TestAppContext) {
    let (sidebar, cx) = setup_sidebar_with_two_sections(cx).await;

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.toggle_section(SidebarSection::AllThreads, cx);
        sidebar.toggle_section(SidebarSection::Archived, cx);
    });
    cx.run_until_parked();

    let state = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("sidebar state should serialize");

    // A restart starts from a sidebar with nothing collapsed and replays the
    // persisted blob into it.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.collapsed_sections.clear();
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();
    assert_eq!(sidebar_shape(&sidebar, cx).len(), 5);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&state, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar.collapsed_sections,
            HashSet::from_iter([SidebarSection::AllThreads, SidebarSection::Archived]),
            "both collapsed sections survive a round trip"
        );
    });
    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
            "section: Archived",
        ],
        "restored collapse state hides the rows without a click"
    );
}

#[gpui::test]
async fn test_history_list_is_flat_and_sorted_by_age(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let now = Utc::now();
    for (session_id, title, updated_at) in [
        ("old", "Old Thread", now - chrono::Duration::days(40)),
        ("recent", "Recent Thread", now - chrono::Duration::hours(2)),
        ("middle", "Middle Thread", now - chrono::Duration::days(3)),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(session_id)),
            Some(title.into()),
            updated_at,
            None,
            None,
            &project,
            cx,
        );
    }
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
            "workspace: my-project",
            "thread: Recent Thread",
            "thread: Middle Thread",
            "thread: Old Thread",
        ],
        "threads spanning weeks render as one flat, recency-sorted list under a single header"
    );
}

#[gpui::test]
fn test_age_label_formats(_cx: &mut TestAppContext) {
    let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 7, 14, 12, 0, 0).unwrap();
    let ago = |duration: chrono::Duration| format_age(now, now - duration);

    assert_eq!(ago(chrono::Duration::seconds(5)), "1m");
    assert_eq!(ago(chrono::Duration::minutes(45)), "45m");
    assert_eq!(ago(chrono::Duration::minutes(59)), "59m");
    assert_eq!(ago(chrono::Duration::hours(2)), "2h");
    assert_eq!(ago(chrono::Duration::hours(23)), "23h");
    assert_eq!(ago(chrono::Duration::days(3)), "3d");
    assert_eq!(ago(chrono::Duration::days(6)), "6d");
    assert_eq!(ago(chrono::Duration::days(8)), "1w");
    assert_eq!(ago(chrono::Duration::days(21)), "3w");
    assert_eq!(ago(chrono::Duration::days(40)), "1mo");
    assert_eq!(ago(chrono::Duration::days(200)), "6mo");
    assert_eq!(ago(chrono::Duration::days(400)), "1y");
    // An empty draft sorts with a future timestamp; it must still read as an age.
    assert_eq!(format_age(now, now + chrono::Duration::hours(1)), "1m");
}

/// A worktree with one thread has no header, so nothing told a collapsed group
/// above it where to stop: collapsing one worktree hid every solo row that
/// followed it.
#[gpui::test]
fn test_collapsing_a_worktree_leaves_the_rows_after_it_alone(cx: &mut TestAppContext) {
    let entry = |title: &str, solo: bool| {
        let mut thread = ThreadEntry {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from(title))),
                agent_id: AgentId::new("zed-agent"),
                worktree_paths: WorktreePaths::default(),
                title: Some(title.into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: Some(Utc::now()),
                interacted_at: None,
                archived: false,
                remote_connection: None,
            },
            icon: IconName::ZedAgent,
            icon_from_external_svg: None,
            status: AgentThreadStatus::Completed,
            workspace: ThreadEntryWorkspace::Closed {
                folder_paths: PathList::default(),
                project_group_key: ProjectGroupKey::from_worktree_paths(&WorktreePaths::default(), None),
            },
            is_live: false,
            is_title_generating: false,
            draft: None,
            draft_leaves_workspace: false,
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            solo_worktree: None,
            under_worktree_header: !solo,
        };
        if solo {
            thread.solo_worktree = Some(SoloWorktree {
                workspace: None,
                is_linked_worktree: true,
                path: None,
            });
        }
        ListEntry::Thread(Arc::new(thread))
    };

    let entries = vec![
        ListEntry::WorkspaceHeader(Arc::new(WorkspaceHeaderEntry {
            label: "mapper".into(),
            lead_thread: None,
            workspace: None,
            member_sessions: Vec::new(),
            is_linked_worktree: true,
            path: None,
            key: "mapper".to_string(),
            member_count: 2,
        })),
        entry("in mapper one", false),
        entry("in mapper two", false),
        entry("a worktree of its own", true),
        entry("another one", true),
    ];

    let collapsed: HashSet<String> = ["mapper".to_string()].into_iter().collect();
    let visible = cx.update(|_| Sidebar::visible_entries(&entries, &HashSet::default(), &collapsed));

    let titles: Vec<String> = visible
        .iter()
        .map(|entry| match entry {
            ListEntry::WorkspaceHeader(header) => header.label.to_string(),
            ListEntry::Thread(thread) => thread.metadata.display_title().to_string(),
            ListEntry::Terminal(_) | ListEntry::SectionHeader(_) => "?".to_string(),
        })
        .collect();
    assert_eq!(
        titles,
        vec!["mapper", "a worktree of its own", "another one"],
        "the collapsed group hides its own rows and nothing else"
    );
}

#[gpui::test]
async fn test_active_worktree_groups_follow_the_tab_strip(cx: &mut TestAppContext) {
    // A group sits where its earliest tab sits, and inside it the rows sit in
    // their tab order. Workspace A's threads are opened first and B's last, so
    // A's group leads even though B's thread is the most recent one — which is
    // what the group order used to be decided by.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let mut open_thread = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Done".into()),
        )]);
        open_thread_with_connection(panel, connection, cx);
        send_message(panel, cx);
        cx.run_until_parked();
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap())
    };

    let thread_a1 = open_thread(&panel_a, cx);
    let thread_a2 = open_thread(&panel_a, cx);

    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let thread_b = open_thread(&panel_b, cx);

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let active_thread_ids = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .filter(|(ix, _)| sidebar.section_of_entry(*ix) == Some(SidebarSection::OpenInZed))
            .filter_map(|(_, entry)| match entry {
                ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    });

    assert_eq!(
        active_thread_ids,
        vec![thread_a1, thread_a2, thread_b],
        "the worktree opened first leads, and its rows keep their tab order, \
         even though the other worktree's thread is the newest"
    );
}
