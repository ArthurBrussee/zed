//! The fork's own sidebar tests, kept beside upstream's file so rebases do not
//! conflict on appends.

use super::*;
// `super`'s glob makes `assert_eq` ambiguous against the prelude's.
use pretty_assertions::assert_eq;

fn test_thread_entry(title: &str, folder: &str, updated_at: DateTime<Utc>) -> ThreadEntry {
    ThreadEntry {
        metadata: Arc::new(ThreadMetadata {
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
            archived: false,
        }),
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
        highlight_positions: Vec::new(),
        worktrees: Vec::new(),
        diff_stats: DiffStats::default(),
        running_work: RunningWork::default(),
        solo_worktree: None,
        under_worktree_header: false,
    }
}

fn archived(mut entry: ThreadEntry) -> ThreadEntry {
    Arc::make_mut(&mut entry.metadata).archived = true;
    entry
}

fn open_sent_thread(panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext) -> ThreadId {
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(panel, connection, cx);
    send_message(panel, cx);
    cx.run_until_parked();
    active_thread_id(panel, cx)
}

async fn add_workspace_with_panel(
    path: &str,
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> (Entity<Workspace>, Entity<AgentPanel>) {
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree(path, serde_json::json!({ "src": {} }))
        .await;
    let project = project::Project::test(fs, [path.as_ref()], cx).await;
    let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project, window, cx)
    });
    let panel = add_agent_panel(&workspace, cx);
    cx.run_until_parked();
    (workspace, panel)
}

fn active_row_ids(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) -> Vec<ThreadId> {
    sidebar.read_with(cx, |sidebar, _cx| {
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
            .collect()
    })
}

fn row_of(
    sidebar: &Entity<Sidebar>,
    thread_id: ThreadId,
    cx: &mut gpui::VisualTestContext,
) -> usize {
    sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id)
            })
            .expect("the thread should have a row")
    })
}

/// A project at `/project` with one linked worktree that Zed made, and a thread
/// saved in that worktree.
async fn setup_worktree_thread(
    cx: &mut TestAppContext,
) -> (
    Arc<FakeFs>,
    Entity<project::Project>,
    Entity<MultiWorkspace>,
    Entity<Sidebar>,
    &mut gpui::VisualTestContext,
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
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    (fs, main_project, multi_workspace, sidebar, cx)
}

#[gpui::test]
async fn test_a_rebuild_that_changes_nothing_stops_early(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("a-thread")),
        Some("A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, cx| {
        sidebar.skipped_rebuilds = 0;
        sidebar.update_entries(cx);
        assert_eq!(
            sidebar.skipped_rebuilds, 1,
            "an identical rebuild should stop after the comparison"
        );
    });

    // The comparison must never swallow a real change.
    sidebar.update(cx, |sidebar, _| sidebar.skipped_rebuilds = 0);
    save_thread_metadata(
        acp::SessionId::new(Arc::from("a-thread")),
        Some("A Renamed Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 1, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar.contents.all_entries.iter().any(|entry| matches!(
                entry,
                crate::ListEntry::Thread(thread)
                    if thread.metadata.display_title().as_ref() == "A Renamed Thread"
            )),
            "a rename must reach the list, not be skipped as a no-op"
        );
    });

    sidebar.update(cx, |sidebar, cx| {
        sidebar.skipped_rebuilds = 0;
        sidebar.update_entries(cx);
        assert_eq!(sidebar.skipped_rebuilds, 1);
    });
}

#[gpui::test]
async fn test_subscribing_to_a_workspace_twice_costs_one_set(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let before = cx.update(|_, cx| cx.callback_counts());

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.subscribe_to_workspace(&workspace, window, cx);
    });
    cx.run_until_parked();

    let after = cx.update(|_, cx| cx.callback_counts());
    assert_eq!(
        after.observers, before.observers,
        "asking again for a workspace already watched must not add observers"
    );
    assert_eq!(
        after.event_listeners, before.event_listeners,
        "nor event listeners"
    );
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(
            sidebar.workspace_subscriptions.len(),
            1,
            "one workspace, one set of subscriptions"
        );
    });
}

#[gpui::test]
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

    // The last row is the oldest thread (Project A Thread).
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
async fn test_stored_rows_share_the_store_allocation(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("thread-1"));
    save_thread_metadata(
        session_id.clone(),
        Some("Fix crash in project panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let stored = cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let store = store.read(cx);
        let thread_id = store
            .entry_by_session(&session_id)
            .expect("the seeded thread should be stored")
            .thread_id;
        store
            .entry_arc(thread_id)
            .cloned()
            .expect("the seeded thread should be stored")
    });

    let row_metadata = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread) => Some(thread.metadata.clone()),
                    _ => None,
                })
                .expect("the stored thread should have a row")
        })
    };

    assert!(
        Arc::ptr_eq(&stored, &row_metadata(cx)),
        "a row should share the store's metadata rather than copy it"
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert!(
        Arc::ptr_eq(&stored, &row_metadata(cx)),
        "a rebuild should keep sharing the store's metadata"
    );
}

#[gpui::test]
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

    // The project group key changes from [/project-a] to [/project-a, /project-b].
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
async fn test_keyboard_confirm_on_bucket_header_is_noop(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(visible_entries_as_strings(&sidebar, cx), vec!["  Thread 1"]);
}

#[gpui::test]
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

/// A `+` worktree belongs to its thread, sent or not, until that is archived.
#[gpui::test]
async fn test_only_a_spare_worktree_is_reclaimed(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/reclaim",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "unsent": { "commondir": "../../", "HEAD": "ref: refs/heads/unsent" },
                    "spare": { "commondir": "../../", "HEAD": "ref: refs/heads/spare" },
                    "in-use": { "commondir": "../../", "HEAD": "ref: refs/heads/in-use" },
                },
            },
            "src": {},
        }),
    )
    .await;
    for name in ["unsent", "spare", "in-use"] {
        fs.insert_tree(
            format!("/worktrees/reclaim/{name}/reclaim"),
            serde_json::json!({
                ".git": format!("gitdir: /reclaim/.git/worktrees/{name}"),
                "src": {},
            }),
        )
        .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/reclaim/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from(format!("/worktrees/reclaim/{name}/reclaim")),
                ref_name: Some(format!("refs/heads/{name}").into()),
                sha: "aaa".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
        let path = format!("/worktrees/reclaim/{name}/reclaim");
        if name == "spare" {
            agent_ui::test_support::record_zed_created_spare_worktree(
                fs.as_ref(),
                Path::new(&path),
                None,
                cx,
            )
            .await;
        } else {
            agent_ui::test_support::record_zed_created_worktree(
                fs.as_ref(),
                Path::new(&path),
                None,
                cx,
            )
            .await;
        }
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/reclaim".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let unsent_paths = PathList::new(&[PathBuf::from("/worktrees/reclaim/unsent/reclaim")]);
    let unsent_thread_id = save_draft_metadata_with_main_paths(
        None,
        unsent_paths.clone(),
        PathList::new(&[PathBuf::from("/reclaim")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    save_thread_metadata_with_main_paths(
        "in-use-thread",
        "In Use Thread",
        PathList::new(&[PathBuf::from("/worktrees/reclaim/in-use/reclaim")]),
        PathList::new(&[PathBuf::from("/reclaim")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert!(
        !fs.is_dir(Path::new("/worktrees/reclaim/spare/reclaim"))
            .await,
        "a spare from an earlier session, which nobody was handed, is reclaimed"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/reclaim/unsent/reclaim"))
            .await,
        "a worktree whose thread has sent nothing is still that thread's worktree"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/reclaim/in-use/reclaim"))
            .await,
        "a worktree a thread is using must be left alone"
    );
    let unsent_kept = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(unsent_thread_id)
            .is_some()
    });
    assert!(
        unsent_kept,
        "and the thread that holds it keeps its row, so it can still be archived"
    );
}

/// Archiving is a flag on metadata: the row moves before the worktree's closed
/// workspace is built to plan the removal.
#[gpui::test]
async fn test_archiving_a_closed_worktree_thread_does_not_wait_for_its_workspace(
    cx: &mut TestAppContext,
) {
    let (fs, main_project, multi_workspace, sidebar, cx) = setup_worktree_thread(cx).await;
    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
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

    // Nothing runs after this, so the workspace cannot have been opened yet.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread_by_session(&worktree_session_id, window, cx);
    });
    let archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(
        archived,
        Some(true),
        "the thread should be archived before its workspace is built"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "the worktree's workspace should not be open yet"
    );

    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk once the plan is built"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "the workspace opened to plan the removal should not be left behind"
    );
}

#[gpui::test]
async fn test_unarchiving_during_the_deferred_plan_leaves_the_worktree_alone(
    cx: &mut TestAppContext,
) {
    let (fs, _main_project, _multi_workspace, sidebar, cx) = setup_worktree_thread(cx).await;
    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let thread_id = thread_id_for(&worktree_session_id, cx);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread_by_session(&worktree_session_id, window, cx);
    });
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.unarchive(thread_id, cx));
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert!(
        fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "a thread that is live again must keep its worktree"
    );
}

#[gpui::test]
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
async fn test_closing_a_thread_clears_its_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let thread_id = open_sent_thread(&panel, cx);
    let ix = row_of(&sidebar, thread_id, cx);
    sidebar.update(cx, |sidebar, _cx| sidebar.selection = Some(ix));

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

// The stale-active_entry fast path used to early-return without reopening.
#[gpui::test]
async fn test_reopen_closed_thread_from_history(cx: &mut TestAppContext) {
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

    // The shape of a restored session: active_entry still points at the thread.
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    sidebar.update(cx, |sidebar, _cx| {
        sidebar.set_stale_thread_active_entry_for_test(
            metadata.thread_id,
            metadata.session_id.clone(),
            workspace.clone(),
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(Arc::new(metadata.clone()), &workspace, false, window, cx);
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

// One thread per worktree, so every row a drag can reach belongs to another
// workspace. A real mouse drag: calling the drop handler passed while the drag
// stayed broken.
#[gpui::test]
async fn test_dragging_a_row_across_worktrees_moves_its_whole_group(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Three worktrees with one thread each, and one with two.
    let thread_a = open_sent_thread(&panel_a, cx);
    let (_, panel_b) = add_workspace_with_panel("/project-b", &multi_workspace, cx).await;
    let thread_b = open_sent_thread(&panel_b, cx);
    let (_, panel_c) = add_workspace_with_panel("/project-c", &multi_workspace, cx).await;
    let thread_c = open_sent_thread(&panel_c, cx);
    let (_, panel_d) = add_workspace_with_panel("/project-d", &multi_workspace, cx).await;
    let thread_d1 = open_sent_thread(&panel_d, cx);
    let thread_d2 = open_sent_thread(&panel_d, cx);

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let name = move |id: ThreadId| {
        if id == thread_a {
            "a"
        } else if id == thread_b {
            "b"
        } else if id == thread_c {
            "c"
        } else if id == thread_d1 {
            "d1"
        } else if id == thread_d2 {
            "d2"
        } else {
            "?"
        }
    };
    let active_rows = |cx: &mut gpui::VisualTestContext| {
        active_row_ids(&sidebar, cx)
            .into_iter()
            .map(name)
            .collect::<Vec<_>>()
    };
    let strip = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
        panel
            .read_with(cx, |panel, cx| panel.thread_tab_ids_in_pane_order(cx))
            .into_iter()
            .map(name)
            .collect::<Vec<_>>()
    };
    // Tall enough that every row is measured.
    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(1600.)),
            |_, _| sidebar.clone().into_any_element(),
        );
    };
    let bounds_of = |ix: usize, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .list_state
                .bounds_for_item(ix)
                .expect("sidebar row should be measured")
        })
    };
    let drag_ix = |from_ix: usize, onto_ix: usize, cx: &mut gpui::VisualTestContext| {
        draw(cx);
        let from = bounds_of(from_ix, cx).center();
        let to = bounds_of(onto_ix, cx).center();
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw(cx);
        // The first move crosses the threshold that starts the drag, the second
        // carries it over the target so the drop has somewhere to land.
        cx.simulate_mouse_move(
            from + gpui::point(px(0.), px(8.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        draw(cx);
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw(cx);
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
    };
    let drag_row =
        |from_thread: ThreadId, onto_thread: ThreadId, cx: &mut gpui::VisualTestContext| {
            let from_ix = row_of(&sidebar, from_thread, cx);
            let onto_ix = row_of(&sidebar, onto_thread, cx);
            drag_ix(from_ix, onto_ix, cx);
        };

    assert_eq!(
        active_rows(cx),
        vec!["a", "b", "c", "d1", "d2"],
        "the threads start in the order they were opened"
    );

    // The case that did nothing: two solo worktrees, dragged upward.
    drag_row(thread_c, thread_a, cx);
    assert_eq!(
        active_rows(cx),
        vec!["c", "a", "b", "d1", "d2"],
        "a row dropped on another worktree's row lands there"
    );
    assert_eq!(
        strip(&panel_b, cx),
        vec!["c", "a", "b", "d1", "d2"],
        "and every window's strip carries the same order, so it is the published one"
    );

    // A two-thread group moved by one of its rows, upward: it arrives in one
    // piece and keeps its own order.
    drag_row(thread_d1, thread_b, cx);
    assert_eq!(
        active_rows(cx),
        vec!["c", "a", "d1", "d2", "b"],
        "dropping a grouped row on another worktree moves the whole group"
    );

    // Downward, past a group of two.
    drag_row(thread_a, thread_d2, cx);
    assert_eq!(
        active_rows(cx),
        vec!["c", "d1", "d2", "a", "b"],
        "a row dropped below lands after the group it was dropped on"
    );

    // Inside one worktree the single thread moves, not the group.
    drag_row(thread_d2, thread_d1, cx);
    assert_eq!(
        active_rows(cx),
        vec!["c", "d2", "d1", "a", "b"],
        "a drop inside a worktree reorders that worktree only"
    );
    assert_eq!(
        strip(&panel_a, cx),
        vec!["c", "d2", "d1", "a", "b"],
        "every pane agrees after all of it"
    );
    assert_eq!(strip(&panel_c, cx), vec!["c", "d2", "d1", "a", "b"]);

    // A worktree header is the handle for its whole group.
    let header_row = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::WorkspaceHeader(_)))
            .expect("the worktree with two threads should have a header")
    });
    let onto_ix = row_of(&sidebar, thread_b, cx);
    drag_ix(header_row, onto_ix, cx);
    assert_eq!(
        active_rows(cx),
        vec!["c", "a", "b", "d2", "d1"],
        "dragging a worktree header moves its group, keeping the group's own order"
    );
}

#[gpui::test]
async fn test_rows_without_a_tab_are_not_draggable(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("historical-thread"));
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        let ix = sidebar
            .contents
            .entries
            .iter()
            .position(|entry| entry.session_id() == Some(&session_id))
            .expect("the historical thread's row should be present");
        assert_ne!(
            sidebar.section_of_entry(ix),
            Some(SidebarSection::OpenInZed),
            "a thread that was never opened is history, not Active"
        );
        let ListEntry::Thread(thread) = &sidebar.contents.entries[ix] else {
            unreachable!()
        };
        assert!(
            sidebar.draggable_thread_row(ix, thread).is_none(),
            "a row with no tab has nothing to move"
        );
    });
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
        sidebar.start_renaming_entry(entry_ix, RenameTarget::Thread(thread_id), title, window, cx);
    });
    cx.run_until_parked();

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
async fn test_every_open_empty_draft_keeps_its_sidebar_row(cx: &mut TestAppContext) {
    // An open thread keeps its row whatever state it is in: the sidebar is the
    // only tab strip there is.
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
    let main_workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);
    cx.run_until_parked();

    // A sent thread and an empty draft in the main panel, and a draft in the
    // worktree panel.
    let main_real_thread_id = open_sent_thread(&main_panel, cx);
    agent_ui::test_support::open_draft_with_connection(&main_panel, StubAgentConnection::new(), cx);
    cx.run_until_parked();
    agent_ui::test_support::open_draft_with_connection(
        &worktree_panel,
        StubAgentConnection::new(),
        cx,
    );
    cx.run_until_parked();

    // `open_draft_with_connection` activated the worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(main_workspace.clone(), None, window, cx);
    });
    cx.run_until_parked();

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

    let main_active_draft =
        active_panel_draft_id(&main_panel, cx).expect("main panel should be viewing a draft");
    let worktree_draft = worktree_panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("worktree panel should have a draft");
    let visible = empty_draft_rows(&sidebar, cx);
    assert!(
        visible.contains(&main_active_draft) && visible.contains(&worktree_draft),
        "both workspaces' empty drafts should be listed, got {visible:?}"
    );

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
        empty_draft_rows(&sidebar, cx).contains(&main_active_draft),
        "the draft the main panel navigated away from is still open, so it keeps its row"
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let worktree_active_draft = active_panel_draft_id(&worktree_panel, cx)
        .expect("worktree panel should be viewing a draft");
    let visible = empty_draft_rows(&sidebar, cx);
    assert!(
        visible.contains(&worktree_active_draft) && visible.contains(&main_active_draft),
        "both drafts are still listed after switching workspaces, got {visible:?}"
    );
}

#[gpui::test]
async fn test_close_selected_thread_closes_the_row_under_the_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let thread_id = open_sent_thread(&panel, cx);
    let ix = row_of(&sidebar, thread_id, cx);
    sidebar.update(cx, |sidebar, _| sidebar.selection = Some(ix));
    focus_sidebar(&sidebar, cx);

    cx.dispatch_action(CloseSelectedThread);
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.open_thread_tab_ids(cx).contains(&thread_id),
            "the selected thread's tab closes"
        );
    });
    cx.update(|_, cx| {
        assert!(
            !ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .expect("a closed thread stays in history")
                .archived,
            "closing is not archiving"
        );
    });
}

#[gpui::test]
async fn test_every_thread_row_offers_a_way_out(cx: &mut TestAppContext) {
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

    // Three row kinds no panel is hosting.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("closed-workspace-thread")),
        Some("Closed Workspace Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    let archived_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_thread_metadata(
        archived_session_id.clone(),
        Some("Archived Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );
    let archived_thread = thread_id_for(&archived_session_id, cx);
    cx.update(|cx| {
        ThreadMetadataStore::global(cx)
            .update(cx, |store, cx| store.archive(archived_thread, None, cx));
    });
    let worktree_paths = PathList::new(&[PathBuf::from("/wt-feature-a")]);
    let typed_draft = save_draft_metadata_with_main_paths(
        None,
        worktree_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|cx| {
        agent_ui::draft_prompt_store::write(
            typed_draft,
            &[acp_v2::ContentBlock::from("half a thought".to_string())],
            cx,
        )
    })
    .await
    .unwrap();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, main_panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });
    add_agent_panel(&worktree_workspace, cx);
    cx.run_until_parked();

    // A sent thread, and an empty draft beside it.
    open_sent_thread(&main_panel, cx);
    agent_ui::test_support::open_draft_with_connection(&main_panel, StubAgentConnection::new(), cx);
    cx.run_until_parked();

    let rows: Vec<(SharedString, Vec<ThreadRowDisposal>, bool)> =
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .enumerate()
                .filter_map(|(ix, entry)| match entry {
                    ListEntry::Thread(thread) => Some((
                        thread.metadata.display_title(),
                        sidebar.thread_row_disposals(ix, thread),
                        sidebar.draggable_thread_row(ix, thread).is_some(),
                    )),
                    _ => None,
                })
                .collect()
        });
    assert!(
        rows.len() >= 4,
        "the fixture should produce several row kinds, got {rows:?}"
    );
    for (title, disposals, _) in &rows {
        assert!(
            !disposals.is_empty(),
            "every row offers a way out; {title:?} offered none"
        );
    }
    // Middle-click is carried by the drag's wrapper.
    assert!(
        rows.iter().any(
            |(_, disposals, draggable)| disposals.contains(&ThreadRowDisposal::Close) && *draggable
        ),
        "an open row closes from its menu and from middle-click, got {rows:?}"
    );
    assert!(
        rows.iter().any(|(_, disposals, _)| disposals
            == &[
                ThreadRowDisposal::RestoreWorktree,
                ThreadRowDisposal::DeleteWorktree
            ]),
        "the archived row keeps its own pair, got {rows:?}"
    );
    assert!(
        rows.iter().any(|(_, disposals, _)| disposals
            .contains(&ThreadRowDisposal::ArchiveWorktree)
            || disposals.contains(&ThreadRowDisposal::ArchiveThread)),
        "an unsent thread archives like any other, got {rows:?}"
    );
}

#[gpui::test]
async fn test_only_open_threads_are_watched_on_github(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // One thread open in the panel, and one only in history.
    let open_thread = open_sent_thread(&panel, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("history-thread")),
        Some("Closed months ago".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    let (open_rows, watched) = sidebar.read_with(cx, |sidebar, _| {
        let open_rows: Vec<ThreadId> = sidebar
            .contents
            .all_entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                _ => None,
            })
            .collect();
        (open_rows, sidebar.gh_watched_branches.clone())
    });
    assert!(
        open_rows.len() > 1,
        "both threads should have rows; only one of them is open"
    );
    assert!(
        sidebar.read_with(cx, |sidebar, _| sidebar
            .contents
            .open_threads
            .contains(&open_thread)),
        "sanity: the sent thread is the open one"
    );
    assert!(
        watched.len() <= 1,
        "only what is open is asked about on GitHub, got {watched:?}"
    );
}

#[gpui::test]
async fn test_an_unsent_thread_keeps_its_row_and_survives_a_restart(cx: &mut TestAppContext) {
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
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Three drafts nobody has open, as an earlier run would have left them.
    let project_paths = PathList::new(&[PathBuf::from("/project")]);
    let stale: Vec<ThreadId> = (0..3)
        .map(|day| {
            save_draft_metadata_with_main_paths(
                None,
                project_paths.clone(),
                project_paths.clone(),
                chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, day + 1, 0, 0, 0).unwrap(),
                cx,
            )
        })
        .collect();
    // And one with something typed into it, which is not the same thing.
    let typed = save_draft_metadata_with_main_paths(
        None,
        project_paths.clone(),
        project_paths.clone(),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 5, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|cx| {
        agent_ui::draft_prompt_store::write(
            typed,
            &[acp_v2::ContentBlock::from("half a thought".to_string())],
            cx,
        )
    })
    .await
    .unwrap();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    agent_ui::test_support::open_draft_with_connection(&panel, StubAgentConnection::new(), cx);
    cx.run_until_parked();

    let open_draft = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("the panel should be viewing a draft");

    let draft_rows: Vec<ThreadId> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(thread) if thread.draft.is_some() => {
                    Some(thread.metadata.thread_id)
                }
                _ => None,
            })
            .collect()
    });
    assert!(
        draft_rows.contains(&open_draft),
        "the open thread keeps its row, got {draft_rows:?}"
    );
    assert!(
        draft_rows.contains(&typed),
        "a thread with something typed into it keeps its row, got {draft_rows:?}"
    );
    for id in &stale {
        assert!(
            draft_rows.contains(id),
            "a thread nobody has open still keeps its row, got {draft_rows:?}"
        );
    }

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let store = store.read(cx);
        for id in &stale {
            assert!(
                store.entry(*id).is_some(),
                "a thread with nothing typed into it survives the load"
            );
        }
        assert!(
            store.entry(typed).is_some(),
            "a thread with something typed into it survives the load"
        );
        assert!(store.entry(open_draft).is_some(), "the open thread stays");
    });
}

#[gpui::test]
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
        sidebar.archive_thread_by_session(
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

#[gpui::test]
async fn test_archive_thread_closes_its_tab(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let thread_id = open_sent_thread(&panel, cx);
    let session_id = active_session_id(&panel, cx);

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(sidebar, &session_id),
            "expected the newly created thread to be active before archiving",
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread_by_session(&session_id, window, cx);
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
async fn test_toggle_from_inside_the_sidebar_closes_it(cx: &mut TestAppContext) {
    let project = init_test_project("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));

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
async fn test_sidebar_plus_opens_draft_thread_tab(cx: &mut TestAppContext) {
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
    use agent_ui::thread_tab::{ForeignThreadTab, ThreadTab};

    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (_sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    cx.run_until_parked();

    let thread_a = open_sent_thread(&panel_a, cx);
    let (workspace_b, panel_b) = add_workspace_with_panel("/project-b", &multi_workspace, cx).await;
    let thread_b = open_sent_thread(&panel_b, cx);

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

    let thread_a = open_sent_thread(&panel_a, cx);
    let (workspace_b, panel_b) = add_workspace_with_panel("/project-b", &multi_workspace, cx).await;
    open_sent_thread(&panel_b, cx);

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
        worktrees,
        ..test_thread_entry("Thread", "/repo/wt", Utc::now())
    };

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

    // Rows never change shape when a branch and a PR appear.
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

#[gpui::test]
async fn test_draft_row_suppresses_project_branch_prs(cx: &mut TestAppContext) {
    init_test(cx);

    let make_entry = |draft: Option<DraftKind>| ThreadEntry {
        draft,
        // The project branch a draft would otherwise inherit.
        worktrees: vec![ui::ThreadItemWorktreeInfo {
            worktree_name: Some("repo".into()),
            branch_name: Some("main".into()),
            full_path: "/repo".into(),
            highlight_positions: Vec::new(),
            kind: ui::WorktreeKind::Main,
        }],
        ..test_thread_entry("Thread", "/repo", Utc::now())
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

#[gpui::test]
async fn test_archived_thread_keeps_its_persisted_pr_badge(cx: &mut TestAppContext) {
    init_test(cx);

    // Its worktree is gone from disk, so gh_status has no branch to query.
    let entry = archived(test_thread_entry("Archived", "/repo/wt", Utc::now()));
    let thread_id = entry.metadata.thread_id;

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
                agent_ui::thread_metadata_store::ThreadPrSnapshot {
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
                        merge: gh_status::MergeState::Unknown,
                    }],
                    ..Default::default()
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
    let make_entry = |title: &str, is_archived: bool| {
        let entry = test_thread_entry(title, "/repo/wt-a", Utc::now());
        Arc::new(if is_archived { archived(entry) } else { entry })
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
            is_live,
            ..test_thread_entry(title, folder, Utc::now())
        })
    };

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
            "section: Active",
            "thread: running",
            "section: All Threads",
            "thread: elsewhere",
            "thread: quiet sibling",
        ],
        "a live thread does not bring the rest of its worktree into Active, and leaves All Threads itself"
    );
}

#[gpui::test]
fn test_archived_threads_go_to_their_own_bottom_section(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, is_archived: bool, updated_at: DateTime<Utc>| {
        let entry = test_thread_entry(title, "/repo/wt", updated_at);
        Arc::new(if is_archived { archived(entry) } else { entry })
    };

    let now = Utc::now();
    // The archived thread is the most recent.
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
            "section: Active",
            "section: All Threads",
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
        Arc::new(test_thread_entry(
            title,
            folder,
            Utc::now() - chrono::Duration::minutes(minutes_old),
        ))
    };

    // A tab order the timestamps disagree with.
    let oldest = make_entry("oldest", "/repo/wt", 30);
    let middle = make_entry("middle", "/repo/wt", 20);
    let newest = make_entry("newest", "/repo/wt", 10);
    // Live, but with no tab.
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
            "thread: oldest",
            "thread: middle",
            "thread: newest",
            "thread: untabbed",
        ],
        "Active rows sit in the order their tabs do"
    );

    assert!(
        !entry_shape_strings(&entries).contains(&"section: All Threads".to_string()),
        "with everything open, All Threads has no rows and no header"
    );
}

#[gpui::test]
fn test_a_thread_is_in_active_or_in_all_threads_but_not_both(_cx: &mut TestAppContext) {
    let make_entry = |title: &str, minutes_old: i64| {
        Arc::new(test_thread_entry(
            title,
            "/repo/wt",
            Utc::now() - chrono::Duration::minutes(minutes_old),
        ))
    };

    let newest = make_entry("newest", 10);
    let middle = make_entry("middle", 20);
    let oldest = make_entry("oldest", 30);
    let threads = vec![newest.clone(), middle.clone(), oldest.clone()];

    let shape = |open: &[&Arc<ThreadEntry>]| {
        let open_thread_ids: HashSet<agent_ui::ThreadId> = open
            .iter()
            .map(|thread| thread.metadata.thread_id)
            .collect();
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

#[gpui::test]
async fn test_collapsed_section_hides_its_rows(cx: &mut TestAppContext) {
    let (sidebar, cx) = setup_sidebar_with_two_sections(cx).await;

    assert_eq!(
        sidebar_shape(&sidebar, cx),
        vec![
            "section: Active",
            "section: All Threads",
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

#[gpui::test]
fn test_collapsing_a_worktree_leaves_the_rows_after_it_alone(cx: &mut TestAppContext) {
    let entry = |title: &str, solo: bool| {
        ListEntry::Thread(Arc::new(ThreadEntry {
            under_worktree_header: !solo,
            solo_worktree: solo.then_some(SoloWorktree {
                workspace: None,
                is_linked_worktree: true,
                path: None,
            }),
            ..test_thread_entry(title, "/repo/wt", Utc::now())
        }))
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
    let visible =
        cx.update(|_| Sidebar::visible_entries(&entries, &HashSet::default(), &collapsed));

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
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let thread_a1 = open_sent_thread(&panel_a, cx);
    let thread_a2 = open_sent_thread(&panel_a, cx);
    let (_workspace_b, panel_b) =
        add_workspace_with_panel("/project-b", &multi_workspace, cx).await;
    let thread_b = open_sent_thread(&panel_b, cx);

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        active_row_ids(&sidebar, cx),
        vec![thread_a1, thread_a2, thread_b],
        "the worktree opened first leads, and its rows keep their tab order, \
         even though the other worktree's thread is the newest"
    );
}

#[gpui::test]
async fn test_dragging_a_tab_reorders_every_pane_and_the_sidebar(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let thread_a1 = open_sent_thread(&panel_a, cx);
    let thread_a2 = open_sent_thread(&panel_a, cx);
    let (_workspace_b, panel_b) =
        add_workspace_with_panel("/project-b", &multi_workspace, cx).await;
    let thread_b = open_sent_thread(&panel_b, cx);
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let name = move |id: ThreadId| {
        if id == thread_a1 {
            "a1"
        } else if id == thread_a2 {
            "a2"
        } else if id == thread_b {
            "b"
        } else {
            "?"
        }
    };
    let strip = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
        panel
            .read_with(cx, |panel, cx| panel.thread_tab_ids_in_pane_order(cx))
            .into_iter()
            .map(name)
            .collect::<Vec<_>>()
    };
    let active_rows = |cx: &mut gpui::VisualTestContext| {
        active_row_ids(&sidebar, cx)
            .into_iter()
            .map(name)
            .collect::<Vec<_>>()
    };
    let drag_tab =
        |panel: &Entity<AgentPanel>, from: usize, to: usize, cx: &mut gpui::VisualTestContext| {
            let pane = panel.read_with(cx, |panel, _cx| panel.thread_pane().clone());
            let item_id = pane.read_with(cx, |pane, _cx| pane.items().nth(from).unwrap().item_id());
            cx.update(|window, cx| {
                workspace::move_item(&pane, &pane, item_id, to, false, window, cx);
            });
            cx.run_until_parked();
        };

    assert_eq!(
        strip(&panel_a, cx),
        vec!["a1", "a2", "b"],
        "both panes start on one strip spanning the window"
    );
    assert_eq!(strip(&panel_b, cx), vec!["a1", "a2", "b"]);
    assert_eq!(active_rows(cx), vec!["a1", "a2", "b"]);

    // A drag within one worktree's own tabs.
    drag_tab(&panel_a, 1, 0, cx);
    assert_eq!(
        strip(&panel_a, cx),
        vec!["a2", "a1", "b"],
        "the dragged tab stays where it was dropped"
    );
    assert_eq!(
        strip(&panel_b, cx),
        vec!["a2", "a1", "b"],
        "the other worktree's pane mirrors it, so both read the same order"
    );
    assert_eq!(
        active_rows(cx),
        vec!["a2", "a1", "b"],
        "the rows follow the tabs without anything else rebuilding the list"
    );

    // A drag from the other worktree's pane, past that worktree's tabs.
    drag_tab(&panel_b, 2, 0, cx);
    assert_eq!(
        strip(&panel_b, cx),
        vec!["b", "a2", "a1"],
        "a tab dragged past another worktree's tabs stays there too"
    );
    assert_eq!(strip(&panel_a, cx), vec!["b", "a2", "a1"]);
    assert_eq!(
        active_rows(cx),
        vec!["b", "a2", "a1"],
        "and the worktree groups reorder with them"
    );
}
