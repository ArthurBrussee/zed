//! The project diff in three groups: code, tests and generated.
//!
//! Which group a file is in is a question about its name, not its content, so
//! this is a table of name rules and nothing else. The group is folded into
//! the sort prefix upstream already orders the diff by, so the files group
//! themselves; a header row on top of each group's first file header says
//! what is in it and folds it, through the editor's own buffer folding.

use editor::{Addon, BUFFER_HEADER_PADDING};
use gpui::{Entity, WeakEntity};
use multi_buffer::ExcerptBoundaryInfo;
use ui::{DiffStat, Tooltip, prelude::*};
use util::rel_path::RelPath;

use crate::diff_multibuffer::DiffMultibuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum DiffGroup {
    Code,
    Tests,
    Generated,
}

impl DiffGroup {
    pub const ALL: [DiffGroup; 3] = [DiffGroup::Code, DiffGroup::Tests, DiffGroup::Generated];

    /// Name rules only: no content sniffing, so nothing has to be read to know
    /// which group a file belongs to.
    pub fn of_path(path: &RelPath) -> Self {
        let components: Vec<&str> = path.as_unix_str().split('/').collect();
        let Some(name) = components.last().copied() else {
            return Self::Code;
        };
        let directories = &components[..components.len().saturating_sub(1)];
        let lower = name.to_ascii_lowercase();

        if is_generated_name(&lower) || directories.iter().any(|dir| is_generated_dir(dir)) {
            return Self::Generated;
        }
        if is_test_name(&lower) || directories.iter().any(|dir| is_test_dir(dir)) {
            return Self::Tests;
        }
        Self::Code
    }

    /// Folded into the status ordering upstream already has, as its outer key.
    pub fn sort_prefix(self) -> u64 {
        match self {
            Self::Code => 0,
            Self::Tests => 1,
            Self::Generated => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Tests => "Tests",
            Self::Generated => "Generated",
        }
    }

    /// Tests and generated files are rarely what a diff is opened for.
    pub fn starts_collapsed(self) -> bool {
        !matches!(self, Self::Code)
    }
}

fn is_test_name(lower_name: &str) -> bool {
    // Rust: `mod tests;`'s own file, and the Zed repository's convention.
    matches!(lower_name, "tests.rs" | "test.rs")
        || lower_name.starts_with("test_") && lower_name.ends_with(".rs")
        || lower_name.ends_with("_tests.rs")
        || lower_name.ends_with("_test.rs")
        // TypeScript and JavaScript.
        || [
            ".test.ts",
            ".test.tsx",
            ".test.js",
            ".test.jsx",
            ".spec.ts",
            ".spec.tsx",
            ".spec.js",
            ".spec.jsx",
        ]
        .iter()
        .any(|suffix| lower_name.ends_with(suffix))
        // Python and Go.
        || lower_name.starts_with("test_") && lower_name.ends_with(".py")
        || lower_name.ends_with("_test.py")
        || lower_name.ends_with("_test.go")
        // Java, Kotlin, Ruby and Swift, cased as their conventions write them.
        || lower_name.ends_with("test.java")
        || lower_name.ends_with("test.kt")
        || lower_name.ends_with("_spec.rb")
        || lower_name.ends_with("tests.swift")
}

fn is_test_dir(dir: &str) -> bool {
    matches!(
        dir,
        "tests"
            | "__tests__"
            | "test"
            | "spec"
            | "testdata"
            | "fixtures"
            | "test_support"
            | "test-support"
    )
}

fn is_generated_name(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "cargo.lock" | "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "bun.lockb"
    ) || lower_name.ends_with(".snap")
        || lower_name.ends_with(".pb.go")
        || lower_name.ends_with("_pb2.py")
        || lower_name.ends_with("_pb2.pyi")
        || generated_infix(lower_name)
}

/// `*.generated.*`, which is a middle component rather than a suffix.
fn generated_infix(lower_name: &str) -> bool {
    lower_name.contains(".generated.")
}

fn is_generated_dir(dir: &str) -> bool {
    matches!(dir, "generated" | "gen")
}

/// What one group's chip says: its name, how many files, and their stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiffGroupSummary {
    pub files: usize,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Draws each group's header row on top of its first file's header, where
/// `DiffMultibuffer` makes room for it.
pub(crate) struct GroupHeaderAddon {
    pub(crate) diff: WeakEntity<DiffMultibuffer>,
}

impl Addon for GroupHeaderAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_above_buffer_header(
        &self,
        excerpt: &ExcerptBoundaryInfo,
        _: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let diff = self.diff.upgrade()?;
        let group = diff.read(cx).group_headed_by(excerpt.buffer_id())?;
        Some(render_group_header(diff, group, cx))
    }
}

/// A group's header row in the diff: its name, how many files and their stats,
/// and a click folds or unfolds every file in it.
fn render_group_header(diff: Entity<DiffMultibuffer>, group: DiffGroup, cx: &App) -> AnyElement {
    let (summary, is_collapsed) = {
        let diff = diff.read(cx);
        let summary = diff
            .group_summaries(cx)
            .into_iter()
            .find(|(known, _)| *known == group)
            .map(|(_, summary)| summary)
            .unwrap_or_default();
        (summary, diff.collapsed_groups().contains(&group))
    };
    let id = format!("diff-group-header-{}", group.label());
    // The file header's own padding, so the two line up.
    div()
        .size_full()
        .p(BUFFER_HEADER_PADDING)
        .child(
            h_flex()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .size_full()
                .pl_1()
                .pr_2()
                .gap_1p5()
                .items_center()
                .cursor_pointer()
                .rounded_sm()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    Icon::new(if is_collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .color(Color::Muted),
                )
                .child(Label::new(group.label()).weight(gpui::FontWeight::SEMIBOLD))
                .child(
                    Label::new(format!(
                        "{} file{}",
                        summary.files,
                        if summary.files == 1 { "" } else { "s" }
                    ))
                    .color(Color::Muted),
                )
                .child(DiffStat::new(
                    SharedString::from(format!("diff-group-stat-{}", group.label())),
                    summary.lines_added as usize,
                    summary.lines_removed as usize,
                ))
                .tooltip(Tooltip::text(if is_collapsed {
                    format!("Expand {}", group.label())
                } else {
                    format!("Collapse {}", group.label())
                }))
                .on_click(move |_, _, cx| {
                    diff.update(cx, |diff, cx| diff.toggle_group(group, cx));
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(path: &str) -> DiffGroup {
        DiffGroup::of_path(RelPath::from_unix_str(path).unwrap())
    }

    #[test]
    fn test_a_file_is_grouped_by_its_name_alone() {
        assert_eq!(group("crates/sidebar/src/sidebar.rs"), DiffGroup::Code);
        assert_eq!(group("portico/src/app/page.tsx"), DiffGroup::Code);
        assert_eq!(group("README.md"), DiffGroup::Code);

        // The Zed repository's own convention, and the other Rust one.
        assert_eq!(
            group("crates/sidebar/src/sidebar_tests.rs"),
            DiffGroup::Tests
        );
        assert_eq!(group("crates/editor/src/editor_test.rs"), DiffGroup::Tests);
        assert_eq!(group("crates/zed/tests/smoke.rs"), DiffGroup::Tests);
        // TypeScript, as mech writes them.
        assert_eq!(group("portico/src/api.test.ts"), DiffGroup::Tests);
        assert_eq!(group("portico/src/api.spec.tsx"), DiffGroup::Tests);
        assert_eq!(group("portico/src/__tests__/api.ts"), DiffGroup::Tests);
        assert_eq!(group("api/test_things.py"), DiffGroup::Tests);
        assert_eq!(group("api/things_test.go"), DiffGroup::Tests);
        // Rust's `mod tests;` file was put under Code.
        assert_eq!(group("crates/sidebar/src/tests.rs"), DiffGroup::Tests);
        assert_eq!(group("crates/sidebar/src/test.rs"), DiffGroup::Tests);
        assert_eq!(group("crates/sidebar/src/test_rows.rs"), DiffGroup::Tests);
        for dir in [
            "test",
            "spec",
            "testdata",
            "fixtures",
            "test_support",
            "test-support",
        ] {
            assert_eq!(
                group(&format!("pkg/{dir}/data.json")),
                DiffGroup::Tests,
                "{dir}"
            );
        }
        assert_eq!(group("app/src/FooTest.java"), DiffGroup::Tests);
        assert_eq!(group("app/src/FooTest.kt"), DiffGroup::Tests);
        assert_eq!(group("lib/foo_spec.rb"), DiffGroup::Tests);
        assert_eq!(group("Sources/FooTests.swift"), DiffGroup::Tests);

        assert_eq!(group("Cargo.lock"), DiffGroup::Generated);
        assert_eq!(group("portico/pnpm-lock.yaml"), DiffGroup::Generated);
        assert_eq!(
            group("crates/sidebar/src/snapshots/rows.snap"),
            DiffGroup::Generated
        );
        assert_eq!(group("api/things.pb.go"), DiffGroup::Generated);
        assert_eq!(group("api/things_pb2.py"), DiffGroup::Generated);
        assert_eq!(group("src/types.generated.ts"), DiffGroup::Generated);
        assert_eq!(group("src/generated/types.ts"), DiffGroup::Generated);
        assert_eq!(group("src/gen/types.ts"), DiffGroup::Generated);

        // Generated wins: a generated test is noise either way, and the
        // ordering has to be a function of the name with no ambiguity.
        assert_eq!(group("src/generated/api_tests.rs"), DiffGroup::Generated);
        // A directory that merely starts with "test" is not a test directory.
        assert_eq!(group("crates/testing/src/lib.rs"), DiffGroup::Code);
        assert_eq!(group("src/generation/thing.rs"), DiffGroup::Code);
    }

    /// Upstream orders the diff by a status prefix; the group has to be the
    /// outer key, or a new test file would sort above changed code.
    #[test]
    fn test_the_group_outranks_the_status() {
        // Upstream's status prefixes are 1 (conflict), 2 (tracked), 3 (new).
        let key = |path: &str, status_prefix: u64| group(path).sort_prefix() * 10 + status_prefix;
        assert!(key("src/lib.rs", 3) < key("src/lib_tests.rs", 1));
        assert!(key("src/lib_tests.rs", 3) < key("Cargo.lock", 1));
        // And inside a group the status ordering is upstream's, untouched.
        assert!(key("src/a.rs", 1) < key("src/b.rs", 2));
        assert!(key("src/b.rs", 2) < key("src/c.rs", 3));
    }

    /// The whole view, laid out: every unfolded file has rows of its own, below
    /// the one before it, inside an editor that has real room; each group's
    /// header sits above its first file and below the group before it, folded
    /// or not; and clicking a header folds exactly that group's files.
    #[gpui::test]
    async fn test_grouped_diff_lays_out_and_folds_by_group(cx: &mut gpui::TestAppContext) {
        grouped_diff_lays_out_and_folds_by_group(settings::DiffViewStyle::Unified, cx).await;
    }

    /// The same in the split view, where each header is a block on the
    /// right-hand editor that the left-hand one balances with a spacer. A file
    /// in a folded group changing used to add another copy of the header.
    #[gpui::test]
    async fn test_split_grouped_diff_lays_out_and_folds_by_group(cx: &mut gpui::TestAppContext) {
        grouped_diff_lays_out_and_folds_by_group(settings::DiffViewStyle::Split, cx).await;
    }

    async fn grouped_diff_lays_out_and_folds_by_group(
        style: settings::DiffViewStyle,
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::project_diff::{self, ProjectDiff};
        use editor::display_map::{Block, DisplayRow};
        use gpui::{Action as _, VisualTestContext};
        use multi_buffer::MultiBufferOffset;
        use project::{FakeFs, Project};
        use serde_json::json;
        use settings::{DiffViewStyle, SettingsStore};
        use std::path::Path;
        use util::path;
        use workspace::{ItemHandle, MultiWorkspace};

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(style);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });

        // Two files per group, each with a line nothing else has.
        let files = [
            ("src/lib.rs", DiffGroup::Code),
            ("src/main.rs", DiffGroup::Code),
            ("src/lib_tests.rs", DiffGroup::Tests),
            ("tests/smoke.rs", DiffGroup::Tests),
            ("Cargo.lock", DiffGroup::Generated),
            ("src/generated/types.rs", DiffGroup::Generated),
        ];
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {} }))
            .await;
        for (index, (file, _)) in files.iter().enumerate() {
            let file = Path::new(path!("/project")).join(file);
            fs::Fs::create_dir(fs.as_ref(), file.parent().unwrap())
                .await
                .unwrap();
            fs.insert_file(
                file,
                format!("unchanged-{index}\nnew-{index}\n").into_bytes(),
            )
            .await;
        }
        let head: Vec<(&str, String)> = files
            .iter()
            .enumerate()
            .map(|(index, (file, _))| (*file, format!("unchanged-{index}\nold-{index}\n")))
            .collect();
        fs.set_head_and_index_for_repo(Path::new(path!("/project/.git")), &head);

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
        // Room for the split view, which a narrow window falls back from.
        cx.simulate_resize(gpui::size(gpui::px(2400.), gpui::px(2400.)));
        cx.update(|window, cx| window.dispatch_action(project_diff::Diff.boxed_clone(), cx));
        cx.run_until_parked();

        let item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        let handle: Box<dyn ItemHandle> = Box::new(item);
        let diff = cx
            .update(|_, cx| handle.act_as::<DiffMultibuffer>(cx))
            .expect("the project diff is a grouped diff");
        let editor = diff.read_with(cx, |diff, cx| diff.editor().read(cx).rhs_editor().clone());

        let folded = |group: DiffGroup, cx: &mut VisualTestContext| {
            let buffers = diff.read_with(cx, |diff, cx| diff.buffers_in_group(group, cx));
            assert_eq!(buffers.len(), 2, "{group:?} holds its two files");
            let folded: Vec<bool> = editor.read_with(cx, |editor, cx| {
                buffers
                    .iter()
                    .map(|buffer| editor.is_buffer_folded(*buffer, cx))
                    .collect()
            });
            assert!(
                folded.iter().all(|f| *f) || folded.iter().all(|f| !*f),
                "{group:?} is folded or unfolded as a whole: {folded:?}"
            );
            folded[0]
        };
        // Where each listed file's own line is drawn, top of the editor down.
        let line_tops = |indices: &[usize], cx: &mut VisualTestContext| -> Vec<f32> {
            cx.simulate_resize(gpui::size(gpui::px(2400.), gpui::px(2400.)));
            cx.run_until_parked();
            let bounds = editor
                .read_with(cx, |editor, _| editor.last_bounds().copied())
                .expect("the editor was laid out");
            assert!(
                bounds.size.width > gpui::px(600.) && bounds.size.height > gpui::px(1200.),
                "the diff's editor fills the pane rather than sharing a row with anything: \
                 {bounds:?}"
            );
            editor.update_in(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                let text = editor.buffer().read(cx).snapshot(cx).text();
                indices
                    .iter()
                    .map(|index| {
                        let offset = text.find(&format!("new-{index}")).unwrap();
                        let anchor = editor
                            .buffer()
                            .read(cx)
                            .snapshot(cx)
                            .anchor_before(MultiBufferOffset(offset));
                        let top = editor
                            .to_pixel_point(anchor, &snapshot, window, cx)
                            .expect("on screen")
                            .y;
                        assert!(
                            top >= gpui::px(0.) && top < bounds.size.height,
                            "file {index}'s line is inside the editor: {top:?}"
                        );
                        f32::from(top)
                    })
                    .collect()
            })
        };
        fn header_selector(group: DiffGroup) -> &'static str {
            match group {
                DiffGroup::Code => "diff-group-header-Code",
                DiffGroup::Tests => "diff-group-header-Tests",
                DiffGroup::Generated => "diff-group-header-Generated",
            }
        }
        let header_top = |group: DiffGroup, cx: &mut VisualTestContext| -> f32 {
            let bounds = cx
                .debug_bounds(header_selector(group))
                .unwrap_or_else(|| panic!("{group:?} has a header in the diff"));
            f32::from(bounds.top())
        };
        let click_header = |group: DiffGroup, cx: &mut VisualTestContext| {
            let bounds = cx.debug_bounds(header_selector(group)).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            cx.run_until_parked();
        };
        let assert_stacked = |tops: &[f32]| {
            for pair in tops.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "each file is drawn below the one before it: {tops:?}"
                );
            }
        };

        // Each group's header row sits on one file's header, in order: that
        // header's block is taller by the row, on the right-hand editor and, when
        // split, on the left-hand one, which keeps the two level. No custom blocks
        // are involved. The row spans the whole view, both panes when split.
        let one_header_per_group = |split: bool, cx: &mut VisualTestContext| {
            cx.run_until_parked();
            let (headers, rhs, lhs) = diff.read_with(cx, |diff, cx| {
                let editor = diff.editor().read(cx);
                (
                    diff.group_headers().to_vec(),
                    editor.rhs_editor().clone(),
                    editor.lhs_editor().cloned(),
                )
            });
            assert_eq!(
                headers.iter().map(|(group, _)| *group).collect::<Vec<_>>(),
                [DiffGroup::Code, DiffGroup::Tests, DiffGroup::Generated]
            );
            // The buffers whose header blocks are taller than a file header, and
            // whether any custom block is in the view.
            let tall_headers = |editor: &gpui::Entity<editor::Editor>,
                                cx: &mut VisualTestContext| {
                editor.update_in(cx, |editor, window, cx| {
                    let snapshot = editor.snapshot(window, cx);
                    let header_height = snapshot.buffer_header_height();
                    let max_row = snapshot.max_point().row();
                    let blocks: Vec<_> = snapshot
                        .blocks_in_range(DisplayRow(0)..DisplayRow(max_row.0 + 1))
                        .map(|(_, block)| block.clone())
                        .collect();
                    let tall = blocks
                        .iter()
                        .filter_map(|block| match block {
                            Block::BufferHeader { excerpt, height }
                            | Block::FoldedBuffer {
                                first_excerpt: excerpt,
                                height,
                            } => (*height > header_height).then(|| excerpt.buffer_id()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    let custom = blocks.iter().any(|block| matches!(block, Block::Custom(_)));
                    (tall, custom)
                })
            };
            let (tall, custom) = tall_headers(&rhs, cx);
            assert_eq!(
                tall,
                headers
                    .iter()
                    .map(|(_, buffer)| *buffer)
                    .collect::<Vec<_>>(),
                "each group's header row is on its first file's header, once"
            );
            assert!(!custom);
            assert_eq!(lhs.is_some(), split, "the view is split: {split}");
            let mut view_width = rhs
                .read_with(cx, |editor, _| editor.last_bounds().copied())
                .expect("laid out")
                .size
                .width;
            if let Some(lhs) = &lhs {
                let (tall, custom) = tall_headers(lhs, cx);
                assert_eq!(tall.len(), headers.len(), "{tall:?}");
                assert!(!custom);
                view_width += lhs
                    .read_with(cx, |editor, _| editor.last_bounds().copied())
                    .expect("laid out")
                    .size
                    .width;
            }
            for (group, _) in &headers {
                let bounds = cx
                    .debug_bounds(header_selector(*group))
                    .unwrap_or_else(|| panic!("{group:?} has a header in the diff"));
                assert!(
                    bounds.size.width > view_width * 0.9,
                    "{group:?}'s header spans the view: {bounds:?} of {view_width:?}"
                );
            }
        };
        let split = style == DiffViewStyle::Split;
        let write = |file: &str, round: usize| {
            let fs = fs.clone();
            let file = Path::new(path!("/project")).join(file);
            async move {
                let text: String = (0..round * 3)
                    .map(|line| format!("written-{line}\n"))
                    .collect();
                fs.insert_file(file, text.into_bytes()).await;
            }
        };

        one_header_per_group(split, cx);
        assert!(!folded(DiffGroup::Code, cx));
        assert!(folded(DiffGroup::Tests, cx), "tests start folded");
        assert!(folded(DiffGroup::Generated, cx), "generated starts folded");
        let lines = line_tops(&[0, 1], cx);
        assert_stacked(&lines);
        let headers = [
            header_top(DiffGroup::Code, cx),
            header_top(DiffGroup::Tests, cx),
            header_top(DiffGroup::Generated, cx),
        ];
        assert_stacked(&[headers[0], lines[0], lines[1], headers[1], headers[2]]);

        click_header(DiffGroup::Tests, cx);
        assert!(!folded(DiffGroup::Tests, cx));
        assert!(
            !folded(DiffGroup::Code, cx),
            "unfolding tests leaves code alone"
        );
        assert!(folded(DiffGroup::Generated, cx), "and generated folded");
        let lines = line_tops(&[0, 1, 2, 3], cx);
        assert_stacked(&lines);
        assert_stacked(&[
            header_top(DiffGroup::Code, cx),
            lines[0],
            lines[1],
            header_top(DiffGroup::Tests, cx),
            lines[2],
            lines[3],
            header_top(DiffGroup::Generated, cx),
        ]);

        click_header(DiffGroup::Code, cx);
        assert!(folded(DiffGroup::Code, cx));
        assert!(
            !folded(DiffGroup::Tests, cx),
            "folding code folds only code"
        );
        assert!(folded(DiffGroup::Generated, cx));
        let lines = line_tops(&[2, 3], cx);
        assert_stacked(&[
            header_top(DiffGroup::Code, cx),
            header_top(DiffGroup::Tests, cx),
            lines[0],
            lines[1],
            header_top(DiffGroup::Generated, cx),
        ]);
        one_header_per_group(split, cx);

        // Every group folded, and files in them changing, as an agent's writes
        // do: the second file of each folded group, then the first.
        click_header(DiffGroup::Tests, cx);
        assert!(folded(DiffGroup::Tests, cx));
        for round in 1..=3 {
            for file in [
                "tests/smoke.rs",
                "src/generated/types.rs",
                "src/lib_tests.rs",
            ] {
                write(file, round).await;
                one_header_per_group(split, cx);
            }
        }

        // A file joining a group.
        write("src/extra.rs", 1).await;
        one_header_per_group(split, cx);
        assert_eq!(
            diff.read_with(cx, |diff, cx| diff
                .buffers_in_group(DiffGroup::Code, cx)
                .len()),
            3
        );

        // And the view toggled between split and unified, both ways.
        let toggle_view = |cx: &mut VisualTestContext| {
            let editor = diff.read_with(cx, |diff, _| diff.editor().clone());
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.toggle_split(&editor::ToggleSplitDiff, window, cx)
                })
            });
        };
        toggle_view(cx);
        one_header_per_group(!split, cx);
        write("tests/smoke.rs", 4).await;
        one_header_per_group(!split, cx);
        toggle_view(cx);
        one_header_per_group(split, cx);
        write("tests/smoke.rs", 5).await;
        one_header_per_group(split, cx);
    }

    #[test]
    fn test_the_groups_order_code_first() {
        let mut groups = DiffGroup::ALL;
        groups.sort_by_key(|group| group.sort_prefix());
        assert_eq!(
            groups,
            [DiffGroup::Code, DiffGroup::Tests, DiffGroup::Generated]
        );
    }
}
