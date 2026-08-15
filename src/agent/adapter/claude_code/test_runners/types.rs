use std::path::Path;

use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TestRunSnapshot {
    pub session_id: String,
    pub runner: String,
    pub command_preview: String,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub status: TestRunStatus,
    pub summary: TestRunSummary,
    pub output_excerpt: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TestRunStatus {
    Pass,
    Fail,
    NoTests,
    Error,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TestRunSummary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub total: u32,
    pub groups: Vec<TestGroup>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TestGroup {
    pub label: String,
    pub path: Option<String>,
    pub kind: TestGroupKind,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub total: u32,
    pub status: TestGroupStatus,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TestGroupKind {
    File,
    Suite,
    Module,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TestGroupStatus {
    Pass,
    Fail,
    Skip,
}

/// Bash tool_result content captured for a matched test-run tool_use.
pub struct CapturedOutput {
    pub content: String,
    pub is_error: bool,
}

pub struct TestRunner {
    pub name: &'static str,
    pub matches: fn(tokens: &[&str]) -> bool,
    pub parse_result: fn(out: &CapturedOutput, cwd: &Path) -> Option<TestRunSummary>,
}
