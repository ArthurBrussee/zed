//! Reading a command's own report of how it went.
//!
//! A chip that says "cargo check ran" is a receipt. The thing worth showing is
//! what the run concluded: how many errors, how many tests failed, and where
//! the first problem is so it can be opened. Every toolchain says this in its
//! output already, in a handful of long-stable shapes, so we read those rather
//! than asking the agent to summarize.
//!
//! This is deliberately forgiving: unknown output yields an empty summary and
//! the chip falls back to naming the command.

/// A file position a tool complained about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLocation {
    pub path: String,
    pub line: u32,
    pub column: Option<u32>,
}

/// What a command reported about its own run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSummary {
    pub errors: usize,
    pub warnings: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    /// Where to go to start fixing things.
    pub first_error: Option<OutputLocation>,
}

impl OutputSummary {
    pub fn is_empty(&self) -> bool {
        self.errors == 0 && self.warnings == 0 && self.tests_passed == 0 && self.tests_failed == 0
    }

    /// A short readout for a chip: what the run concluded, in the order a
    /// reader cares about.
    pub fn label(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if self.errors > 0 {
            parts.push(format!(
                "{} error{}",
                self.errors,
                if self.errors == 1 { "" } else { "s" }
            ));
        }
        if self.tests_failed > 0 {
            parts.push(format!("{} failed", self.tests_failed));
        }
        if self.tests_passed > 0 {
            parts.push(format!("{} passed", self.tests_passed));
        }
        // Warnings only earn space when nothing louder happened.
        if self.warnings > 0 && parts.is_empty() {
            parts.push(format!(
                "{} warning{}",
                self.warnings,
                if self.warnings == 1 { "" } else { "s" }
            ));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Reads a command's output for its own verdict.
pub fn summarize_output(output: &str) -> OutputSummary {
    let mut summary = OutputSummary::default();
    let mut counted_errors = 0usize;
    let mut counted_warnings = 0usize;
    // Rust reports a location on the line after the diagnostic, so the first
    // `-->` following the first error is the place to go.
    let mut want_rust_location = false;

    for line in output.lines() {
        let trimmed = line.trim_start();

        // Rust: `error[E0308]: ...`, `error: ...`, `warning: ...`.
        if trimmed.starts_with("error[") || trimmed.starts_with("error:") {
            counted_errors += 1;
            want_rust_location = summary.first_error.is_none();
            continue;
        }
        if trimmed.starts_with("warning:") {
            counted_warnings += 1;
            continue;
        }
        if want_rust_location && trimmed.starts_with("--> ") {
            summary.first_error = parse_location(trimmed.trim_start_matches("--> ").trim());
            want_rust_location = false;
            continue;
        }

        // TypeScript: `path(line,col): error TS1234: ...` or `error TS1234:`.
        if let Some(index) = trimmed.find("error TS") {
            counted_errors += 1;
            if summary.first_error.is_none() {
                summary.first_error = parse_tsc_location(&trimmed[..index]);
            }
            continue;
        }

        // Cargo test: `test result: ok. 42 passed; 1 failed; ...`.
        if let Some(rest) = trimmed.strip_prefix("test result:") {
            for (count, word) in numbered_words(rest) {
                match word {
                    "passed" => summary.tests_passed += count,
                    "failed" => summary.tests_failed += count,
                    _ => {}
                }
            }
            continue;
        }

        // Vitest/jest: `Tests  1 failed | 41 passed (42)`.
        if trimmed.starts_with("Tests ") || trimmed.starts_with("Tests:") {
            for (count, word) in numbered_words(trimmed) {
                match word {
                    "passed" => summary.tests_passed += count,
                    "failed" => summary.tests_failed += count,
                    _ => {}
                }
            }
            continue;
        }

        // Mocha-style: `12 passing`, `2 failing`.
        for (count, word) in numbered_words(trimmed) {
            match word {
                "passing" => summary.tests_passed += count,
                "failing" => summary.tests_failed += count,
                _ => {}
            }
        }

        // ESLint's tally: `✖ 3 problems (2 errors, 1 warning)`.
        if trimmed.contains("problem") && trimmed.contains('(') {
            for (count, word) in numbered_words(trimmed) {
                match word {
                    "error" | "errors" => counted_errors = counted_errors.max(count),
                    "warning" | "warnings" => counted_warnings = counted_warnings.max(count),
                    _ => {}
                }
            }
            continue;
        }

        // ESLint/tsc per-file lines: `path:12:3  error  message`.
        if summary.first_error.is_none()
            && trimmed.contains(" error ")
            && let Some(location) = parse_location(trimmed.split_whitespace().next().unwrap_or(""))
        {
            summary.first_error = Some(location);
        }
    }

    summary.errors = counted_errors;
    summary.warnings = counted_warnings;
    summary
}

/// `(count, word)` pairs, so "1 failed | 41 passed" reads as its numbers.
fn numbered_words(text: &str) -> Vec<(usize, &str)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut pairs = Vec::new();
    for pair in words.windows(2) {
        let count = pair[0].trim_matches(|ch: char| !ch.is_ascii_digit());
        if count.is_empty() {
            continue;
        }
        let Ok(count) = count.parse::<usize>() else {
            continue;
        };
        pairs.push((
            count,
            pair[1].trim_matches(|ch: char| !ch.is_ascii_alphabetic()),
        ));
    }
    pairs
}

/// `path:line:col` or `path:line`.
fn parse_location(text: &str) -> Option<OutputLocation> {
    let text = text.trim_end_matches(':');
    let mut parts = text.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    let rest = parts.next();
    match (rest, middle.parse::<u32>(), last.parse::<u32>()) {
        // path:line:col
        (Some(path), Ok(line), Ok(column)) => Some(OutputLocation {
            path: path.to_string(),
            line,
            column: Some(column),
        }),
        // path:line
        (None, _, Ok(line)) => Some(OutputLocation {
            path: middle.to_string(),
            line,
            column: None,
        }),
        _ => None,
    }
}

/// `path(line,col)`, the shape tsc uses.
fn parse_tsc_location(text: &str) -> Option<OutputLocation> {
    let text = text.trim().trim_end_matches(':').trim();
    let open = text.rfind('(')?;
    let close = text.rfind(')')?;
    let (line, column) = text.get(open + 1..close)?.split_once(',')?;
    Some(OutputLocation {
        path: text[..open].to_string(),
        line: line.trim().parse().ok()?,
        column: column.trim().parse().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rust_diagnostics_and_the_first_location() {
        let output = "\
   Compiling acp_thread v0.1.0
warning: unused variable: `x`
  --> crates/acp_thread/src/lib.rs:10:9
error[E0308]: mismatched types
  --> crates/acp_thread/src/acp_thread.rs:120:17
error: could not compile `acp_thread`";
        let summary = summarize_output(output);
        assert_eq!(summary.errors, 2);
        assert_eq!(summary.warnings, 1);
        assert_eq!(
            summary.first_error,
            Some(OutputLocation {
                path: "crates/acp_thread/src/acp_thread.rs".into(),
                line: 120,
                column: Some(17),
            })
        );
        assert_eq!(summary.label().as_deref(), Some("2 errors"));
    }

    #[test]
    fn reads_cargo_test_results() {
        let output = "test result: ok. 403 passed; 0 failed; 31 ignored; 0 measured";
        let summary = summarize_output(output);
        assert_eq!(summary.tests_passed, 403);
        assert_eq!(summary.tests_failed, 0);
        assert_eq!(summary.label().as_deref(), Some("403 passed"));

        let failing = summarize_output("test result: FAILED. 12 passed; 3 failed; 0 ignored");
        assert_eq!(failing.tests_failed, 3);
        assert_eq!(failing.label().as_deref(), Some("3 failed, 12 passed"));
    }

    #[test]
    fn reads_typescript_errors() {
        let output = "\
portico/src/geom/BSpline3.ts(88,14): error TS2345: Argument of type 'number'
portico/src/tabs/App.ts(12,3): error TS2551: Property does not exist";
        let summary = summarize_output(output);
        assert_eq!(summary.errors, 2);
        assert_eq!(
            summary.first_error,
            Some(OutputLocation {
                path: "portico/src/geom/BSpline3.ts".into(),
                line: 88,
                column: Some(14),
            })
        );
    }

    #[test]
    fn reads_vitest_summaries() {
        let summary = summarize_output("Tests  1 failed | 41 passed (42)");
        assert_eq!(summary.tests_failed, 1);
        assert_eq!(summary.tests_passed, 41);
    }

    #[test]
    fn unknown_output_says_nothing() {
        let summary = summarize_output("hello world\nsome unrelated text");
        assert!(summary.is_empty());
        assert_eq!(summary.label(), None);
    }
}
