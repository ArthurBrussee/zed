//! The project diff in three groups: code, tests and generated.
//!
//! Which group a file is in is a question about its name, not its content, so
//! this is a table of name rules and nothing else. The group is folded into
//! the sort prefix upstream already orders the diff by, so the files group
//! themselves; a strip in the diff's toolbar says what is in each group and
//! folds it, through the editor's own buffer folding.

use gpui::{Context, EventEmitter, Render, Subscription, WeakEntity, Window};
use ui::{DiffStat, Tooltip, prelude::*};
use util::rel_path::RelPath;
use workspace::{ItemHandle, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView};

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
    // Rust, and the Zed repository's own convention.
    lower_name.ends_with("_tests.rs")
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
}

fn is_test_dir(dir: &str) -> bool {
    matches!(dir, "tests" | "__tests__")
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

impl DiffGroupSummary {
    pub fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// The group strip, in the diff's toolbar: one chip per group saying what is in
/// it, and a click folds or unfolds every file in it.
pub struct DiffGroupsToolbar {
    diff: Option<WeakEntity<DiffMultibuffer>>,
    _observe_diff: Option<Subscription>,
}

impl DiffGroupsToolbar {
    pub fn new() -> Self {
        Self {
            diff: None,
            _observe_diff: None,
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for DiffGroupsToolbar {}

impl ToolbarItemView for DiffGroupsToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let diff = active_pane_item.and_then(|item| item.act_as::<DiffMultibuffer>(cx));
        self._observe_diff = diff
            .as_ref()
            .map(|diff| cx.observe(diff, |_, _, cx| cx.notify()));
        self.diff = diff.as_ref().map(|diff| diff.downgrade());
        if diff.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for DiffGroupsToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(diff) = self.diff.as_ref().and_then(|diff| diff.upgrade()) else {
            return div();
        };
        let (summaries, collapsed) = {
            let diff = diff.read(cx);
            (diff.group_summaries(cx), diff.collapsed_groups().clone())
        };
        let summaries: Vec<_> = summaries
            .into_iter()
            .filter(|(_, summary)| !summary.is_empty())
            .collect();
        // One group is no grouping.
        if summaries.len() < 2 {
            return div();
        }
        h_flex()
            .gap_3()
            .children(summaries.into_iter().map(|(group, summary)| {
                let is_collapsed = collapsed.contains(&group);
                let diff = diff.downgrade();
                h_flex()
                    .id(SharedString::from(format!("diff-group-{}", group.label())))
                    .gap_1()
                    .cursor_pointer()
                    .child(
                        Icon::new(if is_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(Label::new(group.label()).size(LabelSize::Small))
                    .child(
                        Label::new(format!(
                            "{} file{}",
                            summary.files,
                            if summary.files == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::Small)
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
                        diff.update(cx, |diff, cx| diff.toggle_group(group, cx)).ok();
                    })
            }))
    }
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
    /// the one before it, inside an editor that has real room; and a group's
    /// strip chip folds exactly that group's files.
    #[gpui::test]
    async fn test_grouped_diff_lays_out_and_folds_by_group(cx: &mut gpui::TestAppContext) {
        use crate::project_diff::{self, ProjectDiff};
        use gpui::{Action as _, VisualTestContext};
        use multi_buffer::MultiBufferOffset;
        use project::{FakeFs, Project};
        use serde_json::json;
        use settings::{DiffViewStyle, SettingsStore};
        use std::path::Path;
        use util::path;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
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
        fs.insert_tree(path!("/project"), json!({ ".git": {} })).await;
        for (index, (file, _)) in files.iter().enumerate() {
            let file = Path::new(path!("/project")).join(file);
            fs::Fs::create_dir(fs.as_ref(), file.parent().unwrap())
                .await
                .unwrap();
            fs.insert_file(file, format!("unchanged-{index}\nnew-{index}\n").into_bytes())
                .await;
        }
        let head: Vec<(&str, String)> = files
            .iter()
            .enumerate()
            .map(|(index, (file, _))| (*file, format!("unchanged-{index}\nold-{index}\n")))
            .collect();
        fs.set_head_and_index_for_repo(Path::new(path!("/project/.git")), &head);

        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        cx.focus(&workspace);
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
            cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
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
        let assert_stacked = |tops: &[f32]| {
            for pair in tops.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "each file is drawn below the one before it: {tops:?}"
                );
            }
        };

        assert!(!folded(DiffGroup::Code, cx));
        assert!(folded(DiffGroup::Tests, cx), "tests start folded");
        assert!(folded(DiffGroup::Generated, cx), "generated starts folded");
        assert_stacked(&line_tops(&[0, 1], cx));

        diff.update(cx, |diff, cx| diff.toggle_group(DiffGroup::Tests, cx));
        cx.run_until_parked();
        assert!(!folded(DiffGroup::Tests, cx));
        assert!(!folded(DiffGroup::Code, cx), "unfolding tests leaves code alone");
        assert!(folded(DiffGroup::Generated, cx), "and generated folded");
        assert_stacked(&line_tops(&[0, 1, 2, 3], cx));

        diff.update(cx, |diff, cx| diff.toggle_group(DiffGroup::Code, cx));
        cx.run_until_parked();
        assert!(folded(DiffGroup::Code, cx));
        assert!(!folded(DiffGroup::Tests, cx), "folding code folds only code");
        assert!(folded(DiffGroup::Generated, cx));
        assert_stacked(&line_tops(&[2, 3], cx));
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
