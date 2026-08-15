//! Cargo test result parser. Targets cargo 1.7x output format.
//!
//! Cargo writes its summary to stderr (CapturedOutput.content carries combined
//! stdout+stderr because the Bash tool returns combined output).
//!
//! Canonical summary line:
//!   test result: ok. 47 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out;
//!
//! Per-test lines:
//!   test some_module::test_foo ... ok
//!   test some_module::test_bar ... FAILED
//!   test some_module::test_baz ... ignored

use std::collections::HashMap;
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use super::types::{
    CapturedOutput, TestGroup, TestGroupKind, TestGroupStatus, TestRunSummary, TestRunner,
};
use super::ANSI_RE;

pub static CARGO_TEST: TestRunner = TestRunner {
    name: "cargo",
    matches: cargo_matches,
    parse_result: cargo_parse_result,
};

const MAX_GROUPS: usize = 500;

fn cargo_matches(tokens: &[&str]) -> bool {
    matches!(tokens, ["cargo", "test", ..])
}

// Summary line. Cargo may produce multiple summary lines (one per binary);
// we sum across all of them.
//   test result: ok. 47 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out
static SUMMARY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)test result:\s+\w+\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored")
        .unwrap()
});

// Individual test outcome lines:
//   test foo::bar::test_x ... ok
//   test foo::bar::test_y ... FAILED
//   test foo::bar::test_z ... ignored
static TEST_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^test\s+([\w:]+)\s+\.\.\.\s+(ok|FAILED|ignored)\s*$").unwrap());

fn cargo_parse_result(out: &CapturedOutput, _cwd: &Path) -> Option<TestRunSummary> {
    let stripped = ANSI_RE.replace_all(&out.content, "").to_string();

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;
    let mut found_summary = false;
    for cap in SUMMARY_RE.captures_iter(&stripped) {
        found_summary = true;
        // Use saturating_add — bare `+=` would panic in debug and wrap
        // silently in release if a workspace had implausibly many test
        // binaries. Saturating clamps at u32::MAX with no extra cost.
        let p: u32 = cap
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let f: u32 = cap
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let s: u32 = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        passed = passed.saturating_add(p);
        failed = failed.saturating_add(f);
        skipped = skipped.saturating_add(s);
    }
    if !found_summary {
        return None;
    }
    let total = passed.saturating_add(failed).saturating_add(skipped);

    // Build per-module groups from individual test lines.
    let mut module_counts: HashMap<String, (u32, u32, u32)> = HashMap::new(); // (pass, fail, skip)
    for cap in TEST_LINE_RE.captures_iter(&stripped) {
        let full_path = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let outcome = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        // Module = everything before the last "::"
        let module = match full_path.rfind("::") {
            Some(i) => &full_path[..i],
            None => full_path,
        };
        let entry = module_counts.entry(module.to_string()).or_default();
        match outcome {
            "ok" => entry.0 += 1,
            "FAILED" => entry.1 += 1,
            "ignored" => entry.2 += 1,
            _ => {}
        }
    }

    // Sort BEFORE truncating — `take(MAX_GROUPS)` against a HashMap's
    // randomized iteration order would otherwise pick a non-deterministic
    // subset on each run for workspaces with > MAX_GROUPS modules. Sort
    // by label first so the same subset (the alphabetically-first
    // MAX_GROUPS modules) shows up reproducibly across runs.
    let mut groups: Vec<TestGroup> = module_counts
        .into_iter()
        .map(|(label, (p, f, s))| {
            let total = p + f + s;
            let status = if f > 0 {
                TestGroupStatus::Fail
            } else if total == 0 || (s > 0 && p == 0) {
                TestGroupStatus::Skip
            } else {
                TestGroupStatus::Pass
            };
            TestGroup {
                label,
                path: None, // cargo modules don't map to files reliably in v1
                kind: TestGroupKind::Module,
                passed: p,
                failed: f,
                skipped: s,
                total,
                status,
            }
        })
        .collect();
    groups.sort_by(|a, b| a.label.cmp(&b.label));
    groups.truncate(MAX_GROUPS);

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
    use std::path::PathBuf;

    fn captured(content: &str) -> CapturedOutput {
        CapturedOutput {
            content: content.to_string(),
            is_error: false,
        }
    }

    #[test]
    fn cargo_matches_cargo_test() {
        assert!((CARGO_TEST.matches)(&["cargo", "test"]));
        assert!((CARGO_TEST.matches)(&["cargo", "test", "--release"]));
        assert!(!(CARGO_TEST.matches)(&["cargo", "build"]));
        assert!(!(CARGO_TEST.matches)(&["cargo"]));
        assert!(!(CARGO_TEST.matches)(&[]));
    }

    #[test]
    fn parses_simple_pass_summary() {
        let out = captured(
            "running 3 tests
test mycrate::tests::test_a ... ok
test mycrate::tests::test_b ... ok
test mycrate::tests::test_c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        let s = cargo_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.passed, 3);
        assert_eq!(s.failed, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].label, "mycrate::tests");
        assert_eq!(s.groups[0].passed, 3);
        assert_eq!(s.groups[0].kind, TestGroupKind::Module);
        assert!(s.groups[0].path.is_none());
    }

    #[test]
    fn parses_mixed_outcomes() {
        let out = captured(
            "running 3 tests
test mycrate::a::test_x ... ok
test mycrate::a::test_y ... FAILED
test mycrate::b::test_z ... ignored

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out\n",
        );
        let s = cargo_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.passed, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.groups.len(), 2);
        let a = s.groups.iter().find(|g| g.label == "mycrate::a").unwrap();
        assert_eq!(a.passed, 1);
        assert_eq!(a.failed, 1);
        assert_eq!(a.status, TestGroupStatus::Fail);
        let b = s.groups.iter().find(|g| g.label == "mycrate::b").unwrap();
        assert_eq!(b.skipped, 1);
        assert_eq!(b.status, TestGroupStatus::Skip);
    }

    #[test]
    fn sums_multiple_summary_lines() {
        let out = captured(
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        let s = cargo_parse_result(&out, &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.passed, 8);
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn returns_none_when_no_summary() {
        let out = captured("error: failed to compile crate");
        assert!(cargo_parse_result(&out, &PathBuf::from("/tmp")).is_none());
    }
}
