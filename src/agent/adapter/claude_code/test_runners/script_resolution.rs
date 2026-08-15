//! Resolve `npm/yarn/pnpm test` and `npm/yarn/pnpm/bun run <name>` against
//! package.json scripts, with a depth-3 recursion bound to defend against
//! pathological alias loops.

use std::fs;
use std::path::Path;

use serde_json::Value;

const MAX_RECURSION_DEPTH: usize = 3;

/// Returns Some(resolved_command_string) if the first two tokens are a known
/// npm-family script invocation AND package.json contains the script.
/// Returns None for everything else (caller continues with original tokens).
///
/// `bun test` is intentionally NOT treated as an alias — bun has a built-in
/// test runner. Adding a Bun runner is future work.
pub fn resolve_alias(tokens: &[String], cwd: Option<&Path>) -> Option<String> {
    let cwd = cwd?;
    let (script_name, consumed) = parse_alias_invocation(tokens)?;
    let resolved = resolve_recursive(&script_name, cwd, 0)?;

    // Preserve any CLI args after the script name. npm forwards
    // `npm test -- src/foo.test.ts` as `<script-body> src/foo.test.ts`
    // (the `--` separator is dropped before forwarding). Without this,
    // `npm test -- --reporter verbose` would resolve to just `vitest`,
    // losing the user-supplied args.
    let leftover = &tokens[consumed..];
    let leftover = if leftover.first().map(String::as_str) == Some("--") {
        &leftover[1..]
    } else {
        leftover
    };
    if leftover.is_empty() {
        Some(resolved)
    } else {
        // shell-words::join re-escapes any tokens with spaces or quotes
        // so the resulting string round-trips through tokenize cleanly.
        Some(format!(
            "{} {}",
            resolved,
            shell_words::join(leftover.iter())
        ))
    }
}

fn parse_alias_invocation(tokens: &[String]) -> Option<(String, usize)> {
    if tokens.len() < 2 {
        return None;
    }
    let pm = tokens[0].as_str();
    let arg = tokens[1].as_str();
    match (pm, arg) {
        // bare `test` — treat as `run test`
        ("npm" | "yarn" | "pnpm", "test") => Some(("test".to_string(), 2)),
        // `run <name>`
        ("npm" | "yarn" | "pnpm" | "bun", "run") => {
            let name = tokens.get(2)?.clone();
            Some((name, 3))
        }
        _ => None,
    }
}

fn resolve_recursive(script_name: &str, cwd: &Path, depth: usize) -> Option<String> {
    if depth >= MAX_RECURSION_DEPTH {
        return None;
    }
    let pkg_path = cwd.join("package.json");
    let content = fs::read_to_string(&pkg_path).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;
    let resolved = package_script(&json, script_name)?;

    // If the resolved string itself is an npm-family alias, recurse.
    if let Some(tokens) = super::matcher::tokenize(resolved) {
        if let Some((next_script, _)) = parse_alias_invocation(&tokens) {
            if let Some(deeper) = resolve_recursive(&next_script, cwd, depth + 1) {
                return Some(deeper);
            }
        }
    }
    Some(resolved.to_string())
}

fn package_script<'a>(json: &'a Value, script_name: &str) -> Option<&'a str> {
    json.get("scripts")
        .and_then(|scripts| scripts.get(script_name))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_pkg(dir: &Path, scripts: &str) {
        fs::write(
            dir.join("package.json"),
            format!(r#"{{"scripts": {{ {scripts} }} }}"#),
        )
        .unwrap();
    }

    fn vec_of(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn npm_test_resolves_to_script() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""test": "vitest --passWithNoTests""#);
        assert_eq!(
            resolve_alias(&vec_of(&["npm", "test"]), Some(dir.path())),
            Some("vitest --passWithNoTests".to_string())
        );
    }

    #[test]
    fn npm_run_named_script_resolves() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""test:int": "vitest run integration""#);
        assert_eq!(
            resolve_alias(&vec_of(&["npm", "run", "test:int"]), Some(dir.path())),
            Some("vitest run integration".to_string())
        );
    }

    #[test]
    fn missing_script_returns_none() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""build": "tsc""#);
        assert_eq!(
            resolve_alias(&vec_of(&["npm", "test"]), Some(dir.path())),
            None
        );
    }

    #[test]
    fn missing_cwd_returns_none() {
        assert_eq!(resolve_alias(&vec_of(&["npm", "test"]), None), None);
    }

    #[test]
    fn alias_loop_does_not_hang_at_depth_limit() {
        // Renamed from `alias_loop_bounded_to_depth_3` to reflect what's
        // actually being asserted: this test only verifies termination,
        // NOT that resolve_alias detects the cycle. Cross-call cycle
        // safety lives in `match_command_inner`'s visited HashSet (see
        // matcher.rs); resolve_alias here returns the LITERAL resolved
        // string when depth bottoms out, which would then re-enter
        // match_command_inner and be caught by the visited set.
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""test": "npm run test""#);
        // Some(literal string) is the EXPECTED return — termination is
        // the only property under test here. End-to-end cycle safety is
        // covered by match_command_breaks_on_self_referential_npm_script
        // in matcher.rs.
        let result = resolve_alias(&vec_of(&["npm", "test"]), Some(dir.path()));
        assert!(result.is_some());
    }

    #[test]
    fn bun_test_not_treated_as_alias() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""test": "this should not be returned""#);
        assert_eq!(
            resolve_alias(&vec_of(&["bun", "test"]), Some(dir.path())),
            None
        );
    }

    #[test]
    fn bun_run_resolves_normally() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#""test": "vitest""#);
        assert_eq!(
            resolve_alias(&vec_of(&["bun", "run", "test"]), Some(dir.path())),
            Some("vitest".to_string())
        );
    }
}
