//! The fork's own sidebar tests.
//!
//! These live beside `sidebar_tests.rs` rather than in it so that upstream's
//! file keeps upstream's shape: its tests append at the end, and so did these,
//! which made an append/append conflict out of every rebase. A child module
//! sees the parent's imports and helpers through `use super::*`, so nothing
//! had to be made public to move them here.

use super::*;
// `super`'s glob brings in `pretty_assertions::assert_eq`, which is ambiguous
// against the prelude's through a second glob; naming it here settles it.
use pretty_assertions::assert_eq;

/// Nothing in a sidebar changes four times a second, but it was being asked
/// to rebuild at that rate all session. A rebuild that produces the list that
/// is already there now stops before paying for anything downstream of it —
/// the draft passes, the list's measurements, the repaint.
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

    // A rebuild asked for when nothing has moved.
    sidebar.update(cx, |sidebar, cx| {
        sidebar.skipped_rebuilds = 0;
        sidebar.update_entries(cx);
        assert_eq!(
            sidebar.skipped_rebuilds, 1,
            "an identical rebuild should stop after the comparison"
        );
    });

    // The comparison must never swallow a real change. A rename moves the
    // row's title, which the comparison looks at, so the rebuild it causes
    // has to run all the way through.
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

    // And once it has landed, asking again is a no-op once more.
    sidebar.update(cx, |sidebar, cx| {
        sidebar.skipped_rebuilds = 0;
        sidebar.update_entries(cx);
        assert_eq!(sidebar.skipped_rebuilds, 1);
    });
}

/// Subscribing to a workspace happens more than once — at startup for the
/// ones already open, and again when `WorkspaceAdded` names one — and a
/// detached duplicate is invisible: every event it watches asks for the same
/// rebuild twice from then on, and gpui walks the longer list on every flush
/// for the rest of the session.
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

    // The cost this guards is per-rebuild, so the second one matters more
    // than the first: rebuilding must not deep-copy every stored row again.
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert!(
        Arc::ptr_eq(&stored, &row_metadata(cx)),
        "a rebuild should keep sharing the store's metadata"
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

/// Press `+`, get a worktree and an empty draft, walk away: the draft is
/// filtered out of the list, so there was never a row to archive and nothing
/// ever took the worktree off disk. One click, one worktree, forever.
#[gpui::test]
async fn test_a_worktree_left_by_an_abandoned_new_thread_is_reclaimed(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/reclaim",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "abandoned": { "commondir": "../../", "HEAD": "ref: refs/heads/abandoned" },
                    "in-use": { "commondir": "../../", "HEAD": "ref: refs/heads/in-use" },
                },
            },
            "src": {},
        }),
    )
    .await;
    for name in ["abandoned", "in-use"] {
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
        agent_ui::test_support::record_zed_created_worktree(
            fs.as_ref(),
            Path::new(&format!("/worktrees/reclaim/{name}/reclaim")),
            None,
            cx,
        )
        .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/reclaim".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    // The abandoned worktree gets the empty draft `+` leaves behind; the other
    // one carries a real thread, which is what keeps a worktree alive.
    let abandoned_paths = PathList::new(&[PathBuf::from("/worktrees/reclaim/abandoned/reclaim")]);
    let abandoned_draft_id = save_draft_metadata_with_main_paths(
        None,
        abandoned_paths.clone(),
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
        !fs.is_dir(Path::new("/worktrees/reclaim/abandoned/reclaim"))
            .await,
        "the worktree nothing ever used should be reclaimed"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/reclaim/in-use/reclaim"))
            .await,
        "a worktree a thread is using must be left alone"
    );
    let draft_gone = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(abandoned_draft_id)
            .is_none()
    });
    assert!(
        draft_gone,
        "the empty draft that held the reclaimed worktree should go with it"
    );
}

/// Archiving is a flag on metadata and has to feel like one. It used to build
/// the thread's closed workspace first — worktree scan, repositories, language
/// servers — because the disk plan needs a live project, and only moved the row
/// once that finished. The row moves first now, and the worktree still comes
/// off disk behind it.
#[gpui::test]
async fn test_archiving_a_closed_worktree_thread_does_not_wait_for_its_workspace(
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
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // Archive without letting anything run afterwards, which is the whole
    // point: the workspace has not been opened at this instant and the row has
    // already moved.
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

    // And the slow half still happens, behind the flag.
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

/// The worktree belongs to a live thread again, so it must stay on disk: the
/// user can unarchive while the workspace that plans the removal is still
/// being built.
#[gpui::test]
async fn test_unarchiving_during_the_deferred_plan_leaves_the_worktree_alone(
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
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .expect("thread metadata should exist")
            .thread_id
    });

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

// Dragging one Active row onto another arranges the list by moving the thread's
// tab. There is one order, held by the tabs, and the row moves because the tab
// did — never alongside it.
#[gpui::test]
async fn test_drag_active_row_reorders_its_tab(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let mut thread_ids = Vec::new();
    for _ in 0..2 {
        open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
        send_message(&panel, cx);
        let session_id = active_session_id(&panel, cx);
        save_test_thread_metadata(&session_id, &project, cx).await;
        thread_ids.push(cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .expect("thread metadata should exist")
                .thread_id
        }));
    }
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let (first, second) = (thread_ids[0], thread_ids[1]);
    #[track_caller]
    fn position_of(ids: &[ThreadId], target: ThreadId, what: &str) -> usize {
        ids.iter()
            .position(|id| *id == target)
            .unwrap_or_else(|| panic!("{what} should be present"))
    }
    let tabs = |cx: &mut gpui::VisualTestContext| {
        panel.read_with(cx, |panel, cx| panel.open_thread_tab_ids(cx))
    };
    let rows = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
    };

    let tabs_before = tabs(cx);
    assert!(
        position_of(&tabs_before, first, "the first thread's tab")
            < position_of(&tabs_before, second, "the second thread's tab"),
        "the thread opened first starts in front"
    );

    // Pick up the second row and drop it on the first.
    let target_ix = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == first)
            })
            .expect("the first thread's row should be present")
    });
    let dragged = sidebar.read_with(cx, |sidebar, _cx| {
        let ix = sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == second)
            })
            .expect("the second thread's row should be present");
        let ListEntry::Thread(thread) = &sidebar.contents.entries[ix] else {
            unreachable!()
        };
        sidebar
            .draggable_thread_row(ix, thread)
            .expect("an Active row hosting a tab can be picked up")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.handle_thread_row_drop(&dragged, target_ix, window, cx);
    });
    cx.run_until_parked();

    let tabs_after = tabs(cx);
    assert!(
        position_of(&tabs_after, second, "the second thread's tab")
            < position_of(&tabs_after, first, "the first thread's tab"),
        "dropping a row on the one above it moves its tab in front of that one"
    );

    let rows_after = rows(cx);
    assert!(
        position_of(&rows_after, second, "the second thread's row")
            < position_of(&rows_after, first, "the first thread's row"),
        "the row follows its tab without the sidebar keeping an order of its own"
    );
}

// The same reorder as the test above, but taken the way a user takes it:
// through the row's own `on_drag` and `on_drop`. The direct call proves the
// move; only the rendered path proves that a row can be picked up at all.
#[gpui::test]
async fn test_dragging_a_row_with_the_mouse_reorders_its_tab(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let mut thread_ids = Vec::new();
    for _ in 0..2 {
        open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
        send_message(&panel, cx);
        let session_id = active_session_id(&panel, cx);
        save_test_thread_metadata(&session_id, &project, cx).await;
        thread_ids.push(cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .expect("thread metadata should exist")
                .thread_id
        }));
    }
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let (first, second) = (thread_ids[0], thread_ids[1]);
    let row_of = |thread_id: ThreadId, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .position(
                    |entry| matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id),
                )
                .expect("both threads should have a row")
        })
    };
    let tab_position = |thread_id: ThreadId, cx: &mut gpui::VisualTestContext| {
        panel.read_with(cx, |panel, cx| {
            panel
                .open_thread_tab_ids(cx)
                .iter()
                .position(|id| *id == thread_id)
                .expect("both threads should have a tab")
        })
    };
    assert!(
        tab_position(first, cx) < tab_position(second, cx),
        "the thread opened first starts in front"
    );

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(600.)),
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

    draw(cx);
    let from = bounds_of(row_of(second, cx), cx).center();
    let to = bounds_of(row_of(first, cx), cx).center();

    cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::none());
    draw(cx);
    // Two moves: the first crosses the threshold that starts the drag, the
    // second carries it over the target so the drop has somewhere to land.
    cx.simulate_mouse_move(from + gpui::point(px(0.), px(8.)), gpui::MouseButton::Left, gpui::Modifiers::none());
    draw(cx);
    cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::none());
    draw(cx);
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();

    assert!(
        tab_position(second, cx) < tab_position(first, cx),
        "dragging a row onto the one above it moves its tab in front"
    );
}

// Arthur's setup, which is the one this feature never worked in: one thread per
// worktree, so every row a drag can reach belongs to a different workspace. The
// drop used to be confined to the dragged row's own workspace, so no row ever
// accepted it and the drag did nothing at all. Across worktrees the whole group
// moves, because the list groups by worktree and cannot show one thread sitting
// inside another group's rows.
//
// The drag here is a real one — mouse down, move, move, up, over the rendered
// rows. A test that called the drop handler would have passed while the drag
// stayed broken, which is how this was missed twice.
#[gpui::test]
async fn test_dragging_a_row_across_worktrees_moves_its_whole_group(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));

    // Three worktrees with one thread each, and one with two.
    let open_thread = async |panel: &Entity<AgentPanel>,
                                 project: &Entity<project::Project>,
                                 cx: &mut gpui::VisualTestContext| {
        open_thread_with_connection(panel, StubAgentConnection::new(), cx);
        send_message(panel, cx);
        cx.run_until_parked();
        let session_id = active_session_id(panel, cx);
        save_test_thread_metadata(&session_id, project, cx).await;
        cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .expect("thread metadata should exist")
                .thread_id
        })
    };

    let add_worktree = async |path: &str, cx: &mut gpui::VisualTestContext| {
        fs.as_fake()
            .insert_tree(path, serde_json::json!({ "src": {} }))
            .await;
        let project =
            project::Project::test(fs.clone(), [std::path::Path::new(path)], cx).await;
        let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
            mw.test_add_workspace(project.clone(), window, cx)
        });
        let panel = add_agent_panel(&workspace, cx);
        cx.run_until_parked();
        (project, panel)
    };

    let thread_a = open_thread(&panel_a, &project_a, cx).await;
    let (project_b, panel_b) = add_worktree("/project-b", cx).await;
    let thread_b = open_thread(&panel_b, &project_b, cx).await;
    let (project_c, panel_c) = add_worktree("/project-c", cx).await;
    let thread_c = open_thread(&panel_c, &project_c, cx).await;
    let (project_d, panel_d) = add_worktree("/project-d", cx).await;
    let thread_d1 = open_thread(&panel_d, &project_d, cx).await;
    let thread_d2 = open_thread(&panel_d, &project_d, cx).await;

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
        sidebar
            .read_with(cx, |sidebar, _cx| {
                sidebar
                    .contents
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(ix, _)| {
                        sidebar.section_of_entry(*ix) == Some(SidebarSection::OpenInZed)
                    })
                    .filter_map(|(_, entry)| match entry {
                        ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
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
    let row_of = |thread_id: ThreadId, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .position(
                    |entry| matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id),
                )
                .expect("every open thread should have a row")
        })
    };
    // Tall enough that every row is measured: an unmeasured row has no bounds to
    // aim a drag at.
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
    let drag_row = |from_thread: ThreadId,
                    onto_thread: ThreadId,
                    cx: &mut gpui::VisualTestContext| {
        draw(cx);
        let from = bounds_of(row_of(from_thread, cx), cx).center();
        let to = bounds_of(row_of(onto_thread, cx), cx).center();
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

    // The two-thread worktree has a header, and the header is the handle for the
    // whole group: dragging it down onto the last row moves the group, not one
    // thread out of it.
    let header_row = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::WorkspaceHeader(_)))
            .expect("the worktree with two threads should have a header")
    });
    draw(cx);
    let from = bounds_of(header_row, cx).center();
    let to = bounds_of(row_of(thread_b, cx), cx).center();
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::none());
    draw(cx);
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
    assert_eq!(
        active_rows(cx),
        vec!["c", "a", "b", "d2", "d1"],
        "dragging a worktree header moves its group, keeping the group's own order"
    );
}

// A row with no tab is not draggable: the drag moves a tab, and All Threads and
// Archived rows have none. A manual order for rows that are only history would
// be the second, disagreeing order this feature exists to avoid.
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
async fn test_every_open_empty_draft_keeps_its_sidebar_row(cx: &mut TestAppContext) {
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

    // The invariant under test is the one the tab bar's removal set: an open
    // thread keeps its row whatever state it is in, empty drafts included.
    // The sidebar is the only tab strip there is, so a draft dropped from the
    // list the moment it stopped being active would be open with nowhere to be
    // found. Counting `is_empty_draft` rows is more robust than tracking
    // specific thread_ids because draft creation flows can leave behind orphan
    // ephemeral metadata.
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

    // Baseline: both panels have an empty draft open, and both are listed —
    // one per workspace, because each is a thread someone can type into.
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

    // Navigate the main panel away from its draft to the real thread. The
    // draft is still open, so it keeps its row.
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

    // Switching the active workspace changes which draft is active, and
    // changes nothing about which are listed.
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
    // cmd-w with the sidebar focused used to walk past `ThreadsSidebar` to
    // the workspace and close a file in the editor pane, leaving the thread
    // the user was looking straight at open.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    cx.run_until_parked();
    let thread_id = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("the sent thread is open");

    let ix = sidebar
        .read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .position(|entry| matches!(entry, ListEntry::Thread(thread)
                    if thread.metadata.thread_id == thread_id))
        })
        .expect("the thread has a row");
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
    // With no tab bar, the row's context menu is the only place a thread can
    // be closed or archived from, and middle-click on the row is the other.
    // A row kind that offers neither is a thread that cannot be got rid of,
    // which is what every draft row was.
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

    // A thread in a workspace that is not open, an archived one, and a draft
    // with content: three row kinds that no panel is hosting.
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
    cx.update(|cx| {
        let archived = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&archived_session_id)
            .expect("archived thread metadata should exist")
            .thread_id;
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.archive(archived, None, cx));
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
            &[acp::ContentBlock::from("half a thought".to_string())],
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

    // A sent thread, so one row is a real conversation rather than a draft.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&main_panel, connection, cx);
    agent_ui::test_support::send_message(&main_panel, cx);
    cx.run_until_parked();
    // And an empty draft beside it.
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
    // Middle-click is carried by the same wrapper the drag uses, so an open
    // row having one is what makes middle-click reach it.
    assert!(
        rows.iter()
            .any(|(_, disposals, draggable)| disposals.contains(&ThreadRowDisposal::Close)
                && *draggable),
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
            .contains(&ThreadRowDisposal::DiscardDraft)
            || disposals.contains(&ThreadRowDisposal::ArchiveWorktree)),
        "the draft with content can be thrown away, got {rows:?}"
    );
}

#[gpui::test]
async fn test_only_open_threads_are_watched_on_github(cx: &mut TestAppContext) {
    // Every watched branch and PR is its own `gh` process and its own
    // request, so the number of them is what spends GitHub's hourly budget.
    // Watching every thread the store has ever kept made that number the size
    // of the history rather than the size of what is open.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // One thread open in the panel, and one only in history.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    cx.run_until_parked();
    let open_thread = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("the sent thread is open");

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
async fn test_stale_empty_drafts_are_hidden_and_purged(cx: &mut TestAppContext) {
    // Every workspace that restores with no thread tab is handed a fresh
    // empty draft, and one that is never typed into keeps its metadata row
    // for good, so a store that has been around for months holds one per
    // workspace per restore. None of them is open in anything. They do not
    // get a row, and they do not survive the load.
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
            &[acp::ContentBlock::from("half a thought".to_string())],
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
        "the open draft keeps its row, got {draft_rows:?}"
    );
    assert!(
        draft_rows.contains(&typed),
        "a draft with content keeps its row whether or not it is open, got {draft_rows:?}"
    );
    for id in &stale {
        assert!(
            !draft_rows.contains(id),
            "a stale empty draft should not be listed, got {draft_rows:?}"
        );
    }

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let store = store.read(cx);
        for id in &stale {
            assert!(
                store.entry(*id).is_none(),
                "a stale empty draft should be gone from the store after load"
            );
        }
        assert!(
            store.entry(typed).is_some(),
            "a draft with content is not the backlog and stays"
        );
        assert!(
            store.entry(open_draft).is_some(),
            "the open draft stays"
        );
    });
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

/// The sidebar is the only tab strip there is, so a thread that is open shows
/// in it whatever state it is in. An empty draft used to drop out of the list
/// the moment something else became active, which with no tab bar would leave
/// it open and unreachable.
#[gpui::test]
async fn test_an_empty_draft_keeps_its_row_once_something_else_is_active(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new("Hello".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let sent_thread = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("the sent thread is active");
    cx.run_until_parked();

    agent_ui::test_support::open_draft_with_connection(
        &panel,
        acp_thread::StubAgentConnection::new(),
        cx,
    );
    let draft_id = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("the draft is the active thread");
    cx.run_until_parked();

    let listed = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id)
            })
        })
    };
    assert!(listed(cx), "the draft is listed while it is active");

    // Go back to the thread that has actually been sent.
    panel.update_in(cx, |panel, window, cx| {
        panel.activate_thread_tab(sent_thread, true, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(sent_thread),
        "the sent thread is active again"
    );
    assert!(
        listed(cx),
        "and the draft keeps its row, because closing it is the only way out"
    );
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
        metadata: Arc::new(ThreadMetadata {
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
        }),
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
        highlight_positions: Vec::new(),
        worktrees,
        diff_stats: DiffStats::default(),
        running_work: RunningWork::default(),
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
        metadata: Arc::new(ThreadMetadata {
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
        }),
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
        running_work: RunningWork::default(),
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
        metadata: Arc::new(ThreadMetadata {
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
        }),
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
        highlight_positions: Vec::new(),
        worktrees: Vec::new(),
        diff_stats: DiffStats::default(),
        running_work: RunningWork::default(),
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
    let make_entry = |title: &str, archived: bool| {
        Arc::new(ThreadEntry {
            metadata: Arc::new(ThreadMetadata {
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
            }),
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
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            running_work: RunningWork::default(),
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
            metadata: Arc::new(ThreadMetadata {
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
            }),
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
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            running_work: RunningWork::default(),
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
                archived,
            }),
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
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            running_work: RunningWork::default(),
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
            metadata: Arc::new(ThreadMetadata {
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
            metadata: Arc::new(ThreadMetadata {
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
            }),
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
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            running_work: RunningWork::default(),
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
            metadata: Arc::new(ThreadMetadata {
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
            }),
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
            highlight_positions: Vec::new(),
            worktrees: Vec::new(),
            diff_stats: DiffStats::default(),
            running_work: RunningWork::default(),
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

    let open_thread = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
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

#[gpui::test]
async fn test_dragging_a_tab_reorders_every_pane_and_the_sidebar(cx: &mut TestAppContext) {
    // Dragging a tab creates nothing, destroys nothing and activates nothing,
    // so the sidebar has to hear about it some other way; and the order it
    // lands in has to be the same one whichever worktree the drag happened in.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let open_thread = |panel: &Entity<AgentPanel>, cx: &mut gpui::VisualTestContext| {
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
        sidebar
            .read_with(cx, |sidebar, _cx| {
                sidebar
                    .contents
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(ix, _)| {
                        sidebar.section_of_entry(*ix) == Some(SidebarSection::OpenInZed)
                    })
                    .filter_map(|(_, entry)| match entry {
                        ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .into_iter()
            .map(name)
            .collect::<Vec<_>>()
    };
    // The drag itself: the tab at `from` lands at `to`, and nothing rebuilds
    // the sidebar by hand afterwards.
    let drag_tab = |panel: &Entity<AgentPanel>,
                    from: usize,
                    to: usize,
                    cx: &mut gpui::VisualTestContext| {
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
