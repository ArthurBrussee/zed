//! The project diff in three groups: code, tests and generated.
//!
//! Which group a file is in is a question about its name, not its content, so
//! this is a table of name rules and nothing else. The group is folded into
//! the sort prefix upstream already orders the diff by, so the files group
//! themselves; the strip above the diff says what is in each group and
//! collapses it.

use util::rel_path::RelPath;

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
