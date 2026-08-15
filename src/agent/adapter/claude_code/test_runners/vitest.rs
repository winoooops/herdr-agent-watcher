//! Vitest result parser. Targets vitest 1.x summary format.
//!
//! Canonical summary lines (after ANSI strip):
//!   Test Files  3 passed (3)
//!        Tests  47 passed | 2 failed | 1 skipped (50)
//! Per-file lines (within run output):
//!   ✓ src/foo.test.ts (12)
//!   ✗ src/bar.test.ts (8 | 3 failed)

use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use super::path_resolution::resolve_group_path_with_cwd_canonical;
use super::types::{
    CapturedOutput, TestGroup, TestGroupKind, TestGroupStatus, TestRunSummary, TestRunner,
};
use super::ANSI_RE;

pub static VITEST: TestRunner = TestRunner {
    name: "vitest",
    matches: vitest_matches,
    parse_result: vitest_parse_result,
};

const MAX_GROUPS: usize = 500;

fn vitest_matches(tokens: &[&str]) -> bool {
    matches!(tokens.first(), Some(&"vitest"))
}

// Capture passed/failed/skipped counts from the "Tests" summary line.
// Examples:
//   Tests  47 passed (47)
//   Tests  47 passed | 2 failed (49)
//   Tests  47 passed | 2 failed | 1 skipped (50)
//   Tests  no tests
static TESTS_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*Tests\s+(?:(\d+)\s+passed)?(?:.*?(\d+)\s+failed)?(?:.*?(\d+)\s+skipped)?")
        .unwrap()
});

// File rows: "✓ src/foo.test.ts (12)" or "✗ src/bar.test.ts (8 | 3 failed)"
static FILE_ROW_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*([✓✗⊘])\s+(\S+\.[A-Za-z]+)\s*\((\d+)(?:\s*\|\s*(\d+)\s+failed)?\)")
        .unwrap()
});

fn vitest_parse_result(out: &CapturedOutput, cwd: &Path) -> Option<TestRunSummary> {
    let stripped = ANSI_RE.replace_all(&out.content, "").to_string();

    // Pull summary counts from the Tests line.
    let caps = TESTS_LINE_RE.captures(&stripped)?;
    let passed = caps
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let failed = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let skipped = caps
        .get(3)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let total = passed + failed + skipped;

    // Canonicalize CWD ONCE up front. resolve_group_path's per-call
    // version would re-canonicalize for every group — wasted OS round-trip
    // when MAX_GROUPS=500. None means cwd doesn't exist; per-group path
    // resolution falls back to None for every group (rows render
    // non-clickable, summary counts unaffected).
    let cwd_canonical = cwd.canonicalize().ok();

    // Pull per-file groups.
    let mut groups: Vec<TestGroup> = Vec::new();
    for cap in FILE_ROW_RE.captures_iter(&stripped) {
        if groups.len() >= MAX_GROUPS {
            break;
        }
        let icon = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let label = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let file_total: u32 = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let file_failed: u32 = cap
            .get(4)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let status = match icon {
            "✓" => TestGroupStatus::Pass,
            "✗" => TestGroupStatus::Fail,
            "⊘" => TestGroupStatus::Skip,
            _ => TestGroupStatus::Pass,
        };
        // Vitest 1.x file rows don't include `| N skipped` in the
        // parenthesised suffix — only `| N failed`. Without explicit
        // counts, infer from the icon: a `⊘` row means the file_total
        // count is the SKIPPED count, not the passed count. Otherwise
        // the file_total minus file_failed is the passed count.
        let (file_passed, file_skipped) = if matches!(status, TestGroupStatus::Skip) {
            (0, file_total)
        } else {
            (file_total.saturating_sub(file_failed), 0)
        };
        let path = cwd_canonical
            .as_ref()
            .and_then(|c| resolve_group_path_with_cwd_canonical(c, &label));
        groups.push(TestGroup {
            label,
            path,
            kind: TestGroupKind::File,
            passed: file_passed,
            failed: file_failed,
            skipped: file_skipped,
            total: file_total,
            status,
        });
    }

    Some(TestRunSummary {
        passed,
        failed,
        skipped,
        total,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn captured(content: &str) -> CapturedOutput {
        CapturedOutput {
            content: content.to_string(),
            is_error: false,
        }
    }

    #[test]
    fn vitest_matches_first_token() {
        assert!((VITEST.matches)(&["vitest"]));
        assert!((VITEST.matches)(&["vitest", "run", "src/foo.test.ts"]));
        assert!(!(VITEST.matches)(&["jest"]));
        assert!(!(VITEST.matches)(&[]));
    }

    #[test]
    fn parses_simple_pass_summary() {
        let out = captured("Test Files  1 passed (1)\n     Tests  3 passed (3)\n");
        let s = vitest_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.passed, 3);
        assert_eq!(s.failed, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.total, 3);
    }

    #[test]
    fn parses_mixed_pass_fail_skip() {
        let out = captured(
            "Test Files  2 passed | 1 failed (3)\n     Tests  47 passed | 2 failed | 1 skipped (50)\n",
        );
        let s = vitest_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.passed, 47);
        assert_eq!(s.failed, 2);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.total, 50);
    }

    #[test]
    fn parses_per_file_rows() {
        let dir = tempdir().unwrap();
        let foo = dir.path().join("foo.test.ts");
        fs::write(&foo, "").unwrap();
        let out_str = "✓ foo.test.ts (12)\n✗ missing.test.ts (8 | 3 failed)\n     Tests  17 passed | 3 failed (20)\n";
        let out = captured(out_str);
        let s = vitest_parse_result(&out, dir.path()).unwrap();
        assert_eq!(s.groups.len(), 2);
        assert_eq!(s.groups[0].label, "foo.test.ts");
        assert!(s.groups[0].path.is_some());
        assert_eq!(s.groups[0].passed, 12);
        assert_eq!(s.groups[0].failed, 0);
        assert_eq!(s.groups[0].skipped, 0);
        assert_eq!(s.groups[1].label, "missing.test.ts");
        assert!(s.groups[1].path.is_none()); // file doesn't exist → no path
        assert_eq!(s.groups[1].passed, 5);
        assert_eq!(s.groups[1].failed, 3);
        assert_eq!(s.groups[1].skipped, 0);
    }

    #[test]
    fn parses_skipped_file_row() {
        // Regression: a `⊘` row (entire file skipped) used to be recorded as
        // `passed: file_total, skipped: 0`, so the count badge read "3/3"
        // alongside the skip icon — contradictory. Now the file_total is
        // attributed to skipped instead.
        let out_str = "⊘ skipped.test.ts (3)\n     Tests  0 passed | 0 failed | 3 skipped (3)\n";
        let out = captured(out_str);
        let s = vitest_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].label, "skipped.test.ts");
        assert_eq!(s.groups[0].status, TestGroupStatus::Skip);
        assert_eq!(s.groups[0].passed, 0, "skipped row must not report passes");
        assert_eq!(s.groups[0].failed, 0);
        assert_eq!(s.groups[0].skipped, 3);
        assert_eq!(s.groups[0].total, 3);
    }

    #[test]
    fn strips_ansi_before_parsing() {
        let out = captured("\x1b[32m✓\x1b[0m foo.test.ts (12)\n     Tests  12 passed (12)\n");
        let s = vitest_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.total, 12);
        assert_eq!(s.groups.len(), 1);
    }

    #[test]
    fn returns_none_when_no_tests_line() {
        let out = captured("error: command not found");
        assert!(vitest_parse_result(&out, &PathBuf::from("/tmp")).is_none());
    }
}
