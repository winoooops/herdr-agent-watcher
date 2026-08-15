//! Shared extraction + schema validation for the delegated-review block
//! (VIM-304). Adapter-agnostic sibling of `reply.rs`: each adapter passes the
//! agent's completed reply text; the sentinel scan, JSON parse, and schema live
//! here once.

use serde::Deserialize;

use crate::agent::types::{
    AgentReviewEvent, AgentReviewFinding, ReviewFindingCategory, ReviewFindingScope,
    ReviewFindingSide,
};

const OPEN: &str = "<<<VIMEFLOW_REVIEW";
const CLOSE: &str = "VIMEFLOW_REVIEW>>>";

#[derive(Debug, PartialEq)]
pub(crate) enum AgentReviewOutcome {
    // nonce and reviewer are best-effort on malformed — recovered leniently from
    // a parseable-but-invalid object so the degrade note can be gated (nonce) and
    // named (reviewer); either may be None when unrecoverable.
    Malformed {
        nonce: Option<String>,
        reviewer: Option<String>,
    },
    Structured {
        nonce: String,
        reviewer: String,
        findings: Vec<AgentReviewFinding>, // may be empty (a clean review)
        omitted_finding_count: u32,
    },
}

#[derive(Deserialize)]
struct BlockDto {
    v: Option<i64>,
    nonce: Option<String>,
    reviewer: Option<String>,
    findings: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")] // the wire sends startLine / endLine
struct FindingDto {
    scope: Option<String>,
    path: Option<String>,
    side: Option<String>,
    line: Option<i64>,
    start_line: Option<i64>,
    end_line: Option<i64>,
    category: Option<String>,
    text: Option<String>,
}

/// None                     → no open sentinel (not a review — emit nothing).
/// Some(Malformed { .. })   → sentinel present but truncated or schema-invalid.
/// Some(Structured { .. })  → schema-valid (findings may be empty).
pub(crate) fn extract_agent_review(reply_text: &str) -> Option<AgentReviewOutcome> {
    let block = crate::agent::reply::select_protocol_block(reply_text, OPEN, CLOSE)?;
    let json = crate::agent::reply::normalize_reply_json(block.body.trim());
    if !block.complete {
        // open sentinel, no close → truncated
        return Some(malformed(&json));
    }

    match validate(&json) {
        Some((nonce, reviewer, findings, omitted_finding_count)) => {
            Some(AgentReviewOutcome::Structured {
                nonce,
                reviewer,
                findings,
                omitted_finding_count,
            })
        }
        None => Some(malformed(&json)),
    }
}

/// Map an outcome to the wire event for a session (shared by both adapters).
pub(crate) fn map_review_outcome(
    session_id: &str,
    outcome: AgentReviewOutcome,
) -> AgentReviewEvent {
    let (nonce, reviewer, findings, omitted_finding_count) = match outcome {
        AgentReviewOutcome::Structured {
            nonce,
            reviewer,
            findings,
            omitted_finding_count,
        } => (
            Some(nonce),
            Some(reviewer),
            Some(findings),
            omitted_finding_count,
        ),
        AgentReviewOutcome::Malformed { nonce, reviewer } => (nonce, reviewer, None, 0),
    };

    AgentReviewEvent {
        session_id: session_id.to_string(),
        nonce,
        reviewer,
        findings,
        omitted_finding_count,
    }
}

/// A non-empty string field recovered leniently from an otherwise-invalid block.
fn best_effort_field(json: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let s = value.get(key)?.as_str()?;

    (!s.is_empty()).then(|| s.to_string())
}

fn malformed(json: &str) -> AgentReviewOutcome {
    AgentReviewOutcome::Malformed {
        nonce: best_effort_field(json, "nonce"),
        reviewer: best_effort_field(json, "reviewer"),
    }
}

fn validate(json: &str) -> Option<(String, String, Vec<AgentReviewFinding>, u32)> {
    let dto: BlockDto = serde_json::from_str(json).ok()?;
    if dto.v != Some(1) {
        return None;
    }
    let nonce = dto.nonce.filter(|s| !s.is_empty())?;
    let reviewer = dto.reviewer.filter(|s| !s.is_empty())?;
    let raw_findings = dto.findings?; // may be empty

    let finding_count = raw_findings.len();
    let findings: Vec<_> = raw_findings
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let ordinal = u32::try_from(index).ok()?.checked_add(1)?;
            let finding = serde_json::from_value(value).ok()?;
            validate_finding(finding, ordinal)
        })
        .collect();
    let omitted_finding_count = u32::try_from(finding_count - findings.len()).unwrap_or(u32::MAX);

    Some((nonce, reviewer, findings, omitted_finding_count))
}

fn positive_u32(n: Option<i64>) -> Option<u32> {
    u32::try_from(n?).ok().filter(|&x| x > 0)
}

fn validate_finding(f: FindingDto, ordinal: u32) -> Option<AgentReviewFinding> {
    let scope = match f.scope.as_deref()? {
        "line" => ReviewFindingScope::Line,
        "range" if f.start_line.is_none() && f.end_line.is_none() && f.line.is_some() => {
            ReviewFindingScope::Line
        }
        "range" => ReviewFindingScope::Range,
        "file" => ReviewFindingScope::File,
        _ => return None,
    };
    let category = match f.category.as_deref()? {
        "bug" => ReviewFindingCategory::Bug,
        "suggestion" => ReviewFindingCategory::Suggestion,
        "change" => ReviewFindingCategory::Change,
        "question" => ReviewFindingCategory::Question,
        _ => return None,
    };
    let text = f.text.filter(|s| !s.is_empty())?;
    let path = f.path.filter(|s| !s.is_empty())?;
    let side = match f.side.as_deref() {
        Some("additions") => Some(ReviewFindingSide::Additions),
        Some("deletions") => Some(ReviewFindingSide::Deletions),
        Some(_) => return None,
        None => None,
    };

    let (line, start_line, end_line) = match scope {
        ReviewFindingScope::Line => {
            side.as_ref()?; // side required
            (Some(positive_u32(f.line)?), None, None)
        }
        ReviewFindingScope::Range => {
            side.as_ref()?; // side required
            let s = positive_u32(f.start_line)?;
            let e = positive_u32(f.end_line)?;
            if s > e {
                return None;
            }
            (None, Some(s), Some(e))
        }
        ReviewFindingScope::File => (None, None, None),
    };

    Some(AgentReviewFinding {
        ordinal,
        scope,
        path,
        side,
        line,
        start_line,
        end_line,
        category,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> String {
        format!("prose\n{OPEN}\n{json}\n{CLOSE}\nafter")
    }

    #[test]
    fn no_sentinel_is_none() {
        assert_eq!(extract_agent_review("nothing here"), None);
    }

    #[test]
    fn last_complete_standalone_block_wins() {
        let example = block(r#"{"v":1,"nonce":"old","reviewer":"example","findings":[]}"#);
        let real = block(r#"{"v":1,"nonce":"new","reviewer":"Kimi","findings":[]}"#);
        let text = format!("The inline marker {OPEN} is prose.\n{example}\n{real}");

        let outcome = extract_agent_review(&text).expect("block found");
        assert!(matches!(
            outcome,
            AgentReviewOutcome::Structured { ref nonce, .. } if nonce == "new"
        ));
    }

    #[test]
    fn valid_line_finding_is_structured() {
        let t = block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[{"scope":"line","path":"a.ts","side":"additions","line":42,"category":"bug","text":"x"}]}"#,
        );
        match extract_agent_review(&t) {
            Some(AgentReviewOutcome::Structured {
                nonce,
                reviewer,
                findings,
                ..
            }) => {
                assert_eq!(nonce, "n");
                assert_eq!(reviewer, "codex");
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].scope, ReviewFindingScope::Line);
                assert_eq!(findings[0].line, Some(42));
                assert_eq!(findings[0].side, Some(ReviewFindingSide::Additions));
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn valid_range_finding_is_structured() {
        let t = block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[{"scope":"range","path":"a.ts","side":"additions","startLine":88,"endLine":94,"category":"suggestion","text":"x"}]}"#,
        );
        match extract_agent_review(&t) {
            Some(AgentReviewOutcome::Structured { findings, .. }) => {
                assert_eq!(findings[0].scope, ReviewFindingScope::Range);
                assert_eq!(findings[0].start_line, Some(88));
                assert_eq!(findings[0].end_line, Some(94));
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn range_with_only_line_degrades_to_line_finding() {
        let t = block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[{"scope":"range","path":"a.ts","side":"additions","line":42,"category":"bug","text":"x"}]}"#,
        );
        match extract_agent_review(&t) {
            Some(AgentReviewOutcome::Structured { findings, .. }) => {
                assert_eq!(findings[0].scope, ReviewFindingScope::Line);
                assert_eq!(findings[0].line, Some(42));
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn valid_file_finding_is_structured() {
        let t = block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[{"scope":"file","path":"a.ts","category":"bug","text":"x"}]}"#,
        );
        match extract_agent_review(&t) {
            Some(AgentReviewOutcome::Structured { findings, .. }) => {
                assert_eq!(findings[0].scope, ReviewFindingScope::File);
                assert_eq!(findings[0].side, None);
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn invalid_findings_are_omitted_without_discarding_valid_findings() {
        let t = block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[{"scope":"range","path":"a.ts","side":"additions","startLine":11,"category":"change","text":"missing range fields"},{"scope":"line","path":"a.ts","side":"additions","line":42,"category":"bug","text":"valid"},{"scope":"line","path":"b.ts","side":"deletions","line":"8","category":"bug","text":"wrong line type"}]}"#,
        );

        match extract_agent_review(&t) {
            Some(AgentReviewOutcome::Structured {
                findings,
                omitted_finding_count,
                ..
            }) => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].path, "a.ts");
                assert_eq!(findings[0].ordinal, 2);
                assert_eq!(omitted_finding_count, 2);
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn empty_findings_is_clean_structured() {
        let t = block(r#"{"v":1,"nonce":"n","reviewer":"codex","findings":[]}"#);
        assert!(matches!(
            extract_agent_review(&t),
            Some(AgentReviewOutcome::Structured { ref findings, .. }) if findings.is_empty()
        ));
    }

    #[test]
    fn mapped_event_reports_omitted_finding_count() {
        let outcome = extract_agent_review(&block(
            r#"{"v":1,"nonce":"n","reviewer":"codex","findings":["invalid"]}"#,
        ))
        .expect("review outcome");

        let event = map_review_outcome("pty-1", outcome);

        assert_eq!(event.omitted_finding_count, 1);
        assert_eq!(event.findings, Some(Vec::new()));
    }

    #[test]
    fn block_schema_violations_are_malformed() {
        let cases = [
            r#"{"v":2,"nonce":"n","reviewer":"r","findings":[]}"#, // bad version
            r#"{"v":1,"nonce":"","reviewer":"r","findings":[]}"#,  // empty nonce
            r#"{"v":1,"nonce":"n","reviewer":"","findings":[]}"#,  // empty reviewer
        ];
        for c in cases {
            assert!(
                matches!(
                    extract_agent_review(&block(c)),
                    Some(AgentReviewOutcome::Malformed { .. })
                ),
                "expected Malformed for {c}"
            );
        }
    }

    #[test]
    fn invalid_finding_schemas_are_omitted() {
        let cases = [
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"line","path":"a","category":"bug","text":"x"}]}"#, // line missing side/line
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"range","path":"a","side":"additions","startLine":9,"endLine":2,"category":"bug","text":"x"}]}"#, // start > end
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"file","path":"a","category":"nope","text":"x"}]}"#, // bad category
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"file","category":"bug","text":"x"}]}"#, // file missing path
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"line","path":"a","side":"sideways","line":1,"category":"bug","text":"x"}]}"#, // bad side
            r#"{"v":1,"nonce":"n","reviewer":"r","findings":[{"scope":"line","path":"a","side":"additions","line":0,"category":"bug","text":"x"}]}"#, // zero line
        ];
        for c in cases {
            assert!(
                matches!(
                    extract_agent_review(&block(c)),
                    Some(AgentReviewOutcome::Structured {
                        ref findings,
                        omitted_finding_count: 1,
                        ..
                    }) if findings.is_empty()
                ),
                "expected one omitted finding for {c}"
            );
        }
    }

    #[test]
    fn malformed_recovers_best_effort_nonce_and_reviewer() {
        let t = block(r#"{"v":2,"nonce":"keep","reviewer":"codex","findings":[]}"#);
        assert!(matches!(
            extract_agent_review(&t),
            Some(AgentReviewOutcome::Malformed { nonce: Some(n), reviewer: Some(r), .. })
                if n == "keep" && r == "codex"
        ));
    }

    #[test]
    fn unparseable_block_has_no_nonce_or_reviewer() {
        assert!(matches!(
            extract_agent_review(&block("{not json")),
            Some(AgentReviewOutcome::Malformed {
                nonce: None,
                reviewer: None,
                ..
            })
        ));
    }

    #[test]
    fn open_without_close_is_malformed() {
        assert!(matches!(
            extract_agent_review(&format!("{OPEN}\n{{")),
            Some(AgentReviewOutcome::Malformed { .. })
        ));
    }

    #[test]
    fn markdown_fenced_and_quote_prefixed_still_extracts() {
        let t = format!(
            "reply\n{OPEN}\n> {{\"v\":1,\"nonce\":\"n\",\"reviewer\":\"codex\",\"findings\":[]}}\n{CLOSE}"
        );
        assert!(matches!(
            extract_agent_review(&t),
            Some(AgentReviewOutcome::Structured { .. })
        ));
    }
}
