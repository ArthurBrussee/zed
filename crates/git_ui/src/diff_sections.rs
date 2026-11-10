//! quiet-ui: the project and branch diffs in three sections, code, tests and
//! generated files.
//!
//! Which section a file is in is a question about its name, not its content,
//! so this is a table of name rules. The section is the outer part of the sort
//! prefix upstream already orders the diff by, so the files sort themselves into
//! sections; the first file of each section carries the section's name in its
//! header, through upstream's header-controls hook, and a click on it folds or
//! unfolds the section with the editor's own buffer folding.

use collections::HashSet;
use editor::{Addon, SplittableEditor};
use gpui::{Context, Entity, WeakEntity};
use language::BufferId;
use multi_buffer::ExcerptBoundaryInfo;
use ui::{Tooltip, prelude::*};
use util::rel_path::RelPath;

use crate::{diff_multibuffer::DiffMultibuffer, git_panel::GitPanelAddon};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum DiffSection {
    Code,
    Tests,
    Generated,
}

impl DiffSection {
    /// Name rules only, so nothing has to be read to place a file.
    pub fn of_path(path: &RelPath) -> Self {
        let components: Vec<&str> = path.as_unix_str().split('/').collect();
        let Some(name) = components.last().copied() else {
            return Self::Code;
        };
        let directories = &components[..components.len().saturating_sub(1)];
        let lower = name.to_ascii_lowercase();

        // Generated wins: a generated test is noise either way.
        if is_generated_name(&lower) || directories.iter().any(|dir| is_generated_dir(dir)) {
            return Self::Generated;
        }
        if is_test_name(&lower) || directories.iter().any(|dir| is_test_dir(dir)) {
            return Self::Tests;
        }
        Self::Code
    }

    /// The outer key of the diff's sort prefix, around upstream's status
    /// prefix (1 conflict, 2 tracked, 3 new), which stays the order inside
    /// each section. Code keeps upstream's prefixes unchanged.
    pub fn sort_prefix(self, status_prefix: u64) -> u64 {
        let section = match self {
            Self::Code => 0,
            Self::Tests => 1,
            Self::Generated => 2,
        };
        section * 10 + status_prefix
    }

    /// The section a diff's sort prefix was made for, by `sort_prefix`.
    pub fn of_sort_prefix(sort_prefix: u64) -> Self {
        match sort_prefix / 10 {
            0 => Self::Code,
            1 => Self::Tests,
            _ => Self::Generated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Tests => "Tests",
            Self::Generated => "Generated",
        }
    }

    /// Generated files are rarely what a diff is opened for.
    pub fn starts_folded(self) -> bool {
        self == Self::Generated
    }
}

fn is_test_name(lower_name: &str) -> bool {
    // Rust: `mod tests;`'s own file, and the Zed repository's convention.
    matches!(lower_name, "tests.rs" | "test.rs")
        || lower_name.starts_with("test_") && lower_name.ends_with(".rs")
        || lower_name.ends_with("_tests.rs")
        || lower_name.ends_with("_test.rs")
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
        || lower_name.starts_with("test_") && lower_name.ends_with(".py")
        || lower_name.ends_with("_test.py")
        || lower_name.ends_with("_test.go")
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
        || lower_name.contains(".generated.")
}

fn is_generated_dir(dir: &str) -> bool {
    matches!(dir, "generated" | "gen")
}

/// Where each section starts in a diff, and which sections are folded.
#[derive(Default)]
pub(crate) struct DiffSections {
    /// Each present section, in order, with the file it starts at and how
    /// many files it holds.
    heads: Vec<SectionHead>,
    folded: HashSet<DiffSection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SectionHead {
    pub section: DiffSection,
    pub first_buffer: BufferId,
    pub files: usize,
}

impl DiffSections {
    pub fn new() -> Self {
        Self {
            heads: Vec::new(),
            folded: [
                DiffSection::Code,
                DiffSection::Tests,
                DiffSection::Generated,
            ]
            .into_iter()
            .filter(|section| section.starts_folded())
            .collect(),
        }
    }

    #[cfg(test)]
    pub fn heads(&self) -> &[SectionHead] {
        &self.heads
    }

    pub fn is_folded(&self, section: DiffSection) -> bool {
        self.folded.contains(&section)
    }

    /// The section headed at this file, when the diff has more than one: a
    /// diff of code alone needs no section names.
    pub fn headed_at(&self, buffer_id: BufferId) -> Option<SectionHead> {
        if self.heads.len() < 2 {
            return None;
        }
        self.heads
            .iter()
            .find(|head| head.first_buffer == buffer_id)
            .copied()
    }

    /// Recomputes the heads from the files in diff order. Returns whether they
    /// changed.
    pub fn update(&mut self, files: impl IntoIterator<Item = (DiffSection, BufferId)>) -> bool {
        let mut heads: Vec<SectionHead> = Vec::new();
        for (section, buffer_id) in files {
            match heads.last_mut() {
                Some(head) if head.section == section => head.files += 1,
                _ => heads.push(SectionHead {
                    section,
                    first_buffer: buffer_id,
                    files: 1,
                }),
            }
        }
        if heads == self.heads {
            return false;
        }
        self.heads = heads;
        true
    }

    /// Flips a section's fold state, returning whether it is now folded.
    pub fn toggle(&mut self, section: DiffSection) -> bool {
        if self.folded.remove(&section) {
            false
        } else {
            self.folded.insert(section);
            true
        }
    }
}

/// Draws a section's name in its first file's header. The editor draws one
/// addon's header controls, so this one also draws the git panel's, which it
/// takes over from `GitPanelAddon` where that was registered.
pub(crate) struct DiffSectionAddon {
    diff: WeakEntity<DiffMultibuffer>,
    git_panel: Option<GitPanelAddon>,
}

impl DiffSectionAddon {
    pub(crate) fn install(
        editor: &mut SplittableEditor,
        diff: WeakEntity<DiffMultibuffer>,
        cx: &mut Context<SplittableEditor>,
    ) {
        editor.rhs_editor().update(cx, |editor, _| {
            let git_panel = editor.addon::<GitPanelAddon>().map(|addon| GitPanelAddon {
                workspace: addon.workspace.clone(),
            });
            editor.unregister_addon::<GitPanelAddon>();
            editor.register_addon(DiffSectionAddon { diff, git_panel });
        });
    }
}

impl Addon for DiffSectionAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_buffer_header_controls(
        &self,
        excerpt: &ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let git_panel_controls = self
            .git_panel
            .as_ref()
            .and_then(|addon| addon.render_buffer_header_controls(excerpt, buffer, window, cx));
        let diff = self.diff.upgrade();
        let head = diff
            .as_ref()
            .and_then(|diff| diff.read(cx).sections().headed_at(excerpt.buffer_id()));
        let (Some(diff), Some(head)) = (diff, head) else {
            return git_panel_controls;
        };
        Some(
            h_flex()
                .gap_1()
                .child(render_section_name(diff, head, cx))
                .children(git_panel_controls)
                .into_any_element(),
        )
    }
}

fn render_section_name(diff: Entity<DiffMultibuffer>, head: SectionHead, cx: &App) -> AnyElement {
    let section = head.section;
    let folded = diff.read(cx).sections().is_folded(section);
    let id = format!("diff-section-{}", section.label());
    h_flex()
        .id(SharedString::from(id.clone()))
        .debug_selector(move || id)
        .flex_none()
        .px_1()
        .gap_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(cx.theme().colors().element_background)
        .hover(|style| style.bg(cx.theme().colors().element_hover))
        .child(
            Icon::new(if folded {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(IconSize::XSmall)
            .color(Color::Muted),
        )
        .child(
            Label::new(section.label())
                .size(LabelSize::Small)
                .weight(gpui::FontWeight::SEMIBOLD),
        )
        .child(
            Label::new(format!(
                "{} file{}",
                head.files,
                if head.files == 1 { "" } else { "s" }
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .tooltip(Tooltip::text(if folded {
            format!("Unfold {}", section.label())
        } else {
            format!("Fold {}", section.label())
        }))
        .on_click(move |_, _, cx| {
            diff.update(cx, |diff, cx| diff.toggle_section(section, cx));
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(path: &str) -> DiffSection {
        DiffSection::of_path(RelPath::from_unix_str(path).unwrap())
    }

    #[test]
    fn test_a_file_is_placed_by_its_name_alone() {
        assert_eq!(section("crates/sidebar/src/sidebar.rs"), DiffSection::Code);
        assert_eq!(section("portico/src/app/page.tsx"), DiffSection::Code);
        assert_eq!(section("README.md"), DiffSection::Code);

        // Rust's `mod tests;` file, which the request names.
        assert_eq!(section("crates/sidebar/src/tests.rs"), DiffSection::Tests);
        assert_eq!(section("crates/sidebar/src/test.rs"), DiffSection::Tests);
        assert_eq!(
            section("crates/sidebar/src/sidebar_tests.rs"),
            DiffSection::Tests
        );
        assert_eq!(
            section("crates/sidebar/src/sidebar_tests/fork_tests.rs"),
            DiffSection::Tests
        );
        assert_eq!(section("crates/zed/tests/smoke.rs"), DiffSection::Tests);
        assert_eq!(section("portico/src/api.test.ts"), DiffSection::Tests);
        assert_eq!(section("portico/src/__tests__/api.ts"), DiffSection::Tests);
        assert_eq!(section("api/test_things.py"), DiffSection::Tests);
        assert_eq!(section("api/things_test.go"), DiffSection::Tests);
        assert_eq!(section("pkg/test-support/data.json"), DiffSection::Tests);

        assert_eq!(section("Cargo.lock"), DiffSection::Generated);
        assert_eq!(section("portico/pnpm-lock.yaml"), DiffSection::Generated);
        assert_eq!(section("src/snapshots/rows.snap"), DiffSection::Generated);
        assert_eq!(section("src/types.generated.ts"), DiffSection::Generated);
        assert_eq!(
            section("src/generated/api_tests.rs"),
            DiffSection::Generated
        );

        assert_eq!(section("crates/testing/src/lib.rs"), DiffSection::Code);
        assert_eq!(section("src/generation/thing.rs"), DiffSection::Code);
    }

    /// The section is the outer key: a new test file sorts below changed
    /// code, and inside a section upstream's status order stands.
    #[test]
    fn test_the_section_outranks_the_status() {
        let key = |path: &str, status: u64| section(path).sort_prefix(status);
        assert_eq!(key("src/lib.rs", 2), 2, "code keeps upstream's prefixes");
        assert!(key("src/lib.rs", 3) < key("src/tests.rs", 1));
        assert!(key("src/tests.rs", 3) < key("Cargo.lock", 1));
        assert!(key("src/a.rs", 1) < key("src/b.rs", 2));
        for status in 1..=3 {
            for path in ["src/lib.rs", "src/tests.rs", "Cargo.lock"] {
                assert_eq!(
                    DiffSection::of_sort_prefix(key(path, status)),
                    section(path)
                );
            }
        }
    }

    /// The project diff sorts its files into sections, names each section in
    /// its first file's header, starts generated files folded, and a click on
    /// a section's name folds exactly that section.
    #[gpui::test]
    async fn test_the_project_diff_is_in_sections(cx: &mut gpui::TestAppContext) {
        use crate::project_diff::{self, ProjectDiff};
        use gpui::Action as _;
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
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });

        // Named so that plain path order would interleave the sections.
        let files = [
            "Cargo.lock",
            "src/a_tests.rs",
            "src/lib.rs",
            "src/tests.rs",
            "src/z.rs",
        ];
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "Cargo.lock": "unchanged\nnew-0\n",
                "src": {
                    "a_tests.rs": "unchanged\nnew-1\n",
                    "lib.rs": "unchanged\nnew-2\n",
                    "tests.rs": "unchanged\nnew-3\n",
                    "z.rs": "unchanged\nnew-4\n",
                },
            }),
        )
        .await;
        let head: Vec<(&str, String)> = files
            .iter()
            .enumerate()
            .map(|(index, file)| (*file, format!("unchanged\nold-{index}\n")))
            .collect();
        fs.set_head_and_index_for_repo(Path::new(path!("/project/.git")), &head);

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
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
        let editor = cx
            .update(|_, cx| handle.act_as::<editor::Editor>(cx))
            .expect("the project diff has an editor");
        let diff = editor
            .read_with(cx, |editor, _| {
                let addon = editor.addon::<DiffSectionAddon>()?;
                assert!(
                    addon.git_panel.is_some() && editor.addon::<GitPanelAddon>().is_none(),
                    "the section addon draws the git panel's controls in its place"
                );
                addon.diff.upgrade()
            })
            .expect("the project diff's editor carries the section addon");

        let order = editor.read_with(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let mut order: Vec<String> = Vec::new();
            for excerpt in snapshot.excerpts() {
                let buffer = snapshot
                    .buffer_for_id(excerpt.context.start.buffer_id)
                    .unwrap();
                let path = buffer.file().unwrap().path().as_unix_str().to_string();
                if order.last() != Some(&path) {
                    order.push(path);
                }
            }
            order
        });
        assert_eq!(
            order,
            [
                "src/lib.rs",
                "src/z.rs",
                "src/a_tests.rs",
                "src/tests.rs",
                "Cargo.lock"
            ],
            "code, then tests, then generated files"
        );

        let buffer_for = |path: &str, cx: &mut gpui::VisualTestContext| {
            editor.read_with(cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .buffers_with_paths()
                    .find(|(_, path_key)| path_key.path.as_unix_str() == path)
                    .map(|(buffer, _)| buffer.remote_id())
                    .unwrap()
            })
        };
        let lib = buffer_for("src/lib.rs", cx);
        let a_tests = buffer_for("src/a_tests.rs", cx);
        let tests = buffer_for("src/tests.rs", cx);
        let lock = buffer_for("Cargo.lock", cx);
        let heads = diff.read_with(cx, |diff, _| diff.sections().heads().to_vec());
        assert_eq!(
            heads
                .iter()
                .map(|head| (head.section, head.first_buffer, head.files))
                .collect::<Vec<_>>(),
            [
                (DiffSection::Code, lib, 2),
                (DiffSection::Tests, a_tests, 2),
                (DiffSection::Generated, lock, 1),
            ]
        );

        let folded = |buffer: BufferId, cx: &mut gpui::VisualTestContext| {
            editor.read_with(cx, |editor, cx| editor.is_buffer_folded(buffer, cx))
        };
        assert!(folded(lock, cx), "generated files start folded");
        assert!(!folded(lib, cx) && !folded(tests, cx));

        cx.run_until_parked();
        let tests_name = cx
            .debug_bounds("diff-section-Tests")
            .expect("the tests section is named in its first file's header");
        cx.simulate_click(tests_name.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            folded(a_tests, cx) && folded(tests, cx),
            "the click folds tests"
        );
        assert!(!folded(lib, cx), "and nothing else");
    }
}
