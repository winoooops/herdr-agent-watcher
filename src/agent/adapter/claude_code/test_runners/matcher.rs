//! Command matching: tokenize → strip env → first segment → strip wrappers
//! → resolve script alias → match against RUNNERS.

use std::path::Path;

use super::script_resolution;
use super::types::TestRunner;
use super::RUNNERS;

/// Tokenize a command string into shell-words, returning None if the input
/// can't be parsed (e.g. unmatched quotes). None is treated as "no match"
/// downstream — never as a hard error — so we don't false-positive on weird
/// shell syntax we haven't seen before.
pub fn tokenize(cmd: &str) -> Option<Vec<String>> {
    shell_words::split(cmd).ok()
}

/// Drop leading KEY=VALUE assignments (env-style) until the first non-assignment.
pub fn strip_env_prefix(tokens: &[String]) -> &[String] {
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        // KEY=value: must contain '=', and the part before '=' must look like
        // an identifier (letters/digits/underscore, not starting with digit).
        if let Some(eq) = t.find('=') {
            let key = &t[..eq];
            if !key.is_empty()
                && key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                i += 1;
                continue;
            }
        }
        break;
    }
    &tokens[i..]
}

/// Take only the first command segment — split on &&, ;, ||, |.
pub fn first_segment(tokens: &[String]) -> &[String] {
    let separators = ["&&", "||", ";", "|"];
    for (i, t) in tokens.iter().enumerate() {
        if separators.contains(&t.as_str()) {
            return &tokens[..i];
        }
    }
    tokens
}

/// Strip leading wrapper prefixes, looping until no further wrapper is
/// recognised. Composed wrappers like `dotenv -- npx vitest` or
/// `pnpm exec dotenv -- vitest` would otherwise leave an inner wrapper
/// in place, the runner registry wouldn't match, and the test-run event
/// would silently never fire.
///
/// The loop is bounded at 8 iterations to defend against pathological
/// inputs (a long chain of wrappers, repeated tokenization round-trips).
/// 8 comfortably exceeds any realistic composition.
pub fn strip_wrappers(tokens: &[String]) -> &[String] {
    const MAX_ITERATIONS: usize = 8;
    let mut current = tokens;
    for _ in 0..MAX_ITERATIONS {
        let next = strip_one_wrapper(current);
        if std::ptr::eq(next, current) {
            return current;
        }
        current = next;
    }
    current
}

fn strip_one_wrapper(tokens: &[String]) -> &[String] {
    if tokens.is_empty() {
        return tokens;
    }
    match tokens[0].as_str() {
        "npx" => &tokens[1..],
        "pnpm" if tokens.get(1).map(String::as_str) == Some("exec") => &tokens[2..],
        "yarn" if tokens.get(1).map(String::as_str) == Some("exec") => &tokens[2..],
        "bun" if tokens.get(1).map(String::as_str) == Some("x") => &tokens[2..],
        "dotenv" if tokens.get(1).map(String::as_str) == Some("--") => &tokens[2..],
        _ => tokens,
    }
}

/// Result of a successful match.
pub struct MatchedCommand {
    pub runner: &'static TestRunner,
    /// Tokens after env-strip + segment-first + wrapper-strip + script
    /// resolution. Used by build_command_preview for display.
    pub stripped_tokens: Vec<String>,
}

/// The full matching pipeline. Returns None when the command does not match
/// any known runner.
pub fn match_command(cmd: &str, cwd: Option<&Path>) -> Option<MatchedCommand> {
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    match_command_inner(cmd, cwd, &mut visited)
}

/// Inner matching pipeline carrying the cycle-breaking visited set across
/// the cross-function `match_command -> resolve_alias -> match_command`
/// recursion. Without this, a self-referential script like
/// `{"test": "npm run test"}` overflows the stack: `resolve_alias` returns
/// the literal `"npm run test"`, the next `match_command` resolves the same
/// alias, and so on. The depth bound inside `resolve_recursive` only guards
/// the package.json chain — not the cross-function loop.
fn match_command_inner(
    cmd: &str,
    cwd: Option<&Path>,
    visited: &mut std::collections::HashSet<String>,
) -> Option<MatchedCommand> {
    if !visited.insert(cmd.to_string()) {
        // Already saw this exact command string in the recursion chain; cycle
        // detected — refuse to match rather than overflow the stack.
        return None;
    }

    let initial = tokenize(cmd)?;
    let after_env = strip_env_prefix(&initial).to_vec();
    let after_segment = first_segment(&after_env).to_vec();
    let after_wrapper = strip_wrappers(&after_segment).to_vec();

    // Try script alias resolution. If it resolves, recurse on the resolved
    // string (which goes through the SAME pipeline so its env/wrapper/etc.
    // are also normalised). Pass the visited set down so multi-step cycles
    // are caught the second time the same command string reappears.
    if let Some(resolved) = script_resolution::resolve_alias(&after_wrapper, cwd) {
        return match_command_inner(&resolved, cwd, visited);
    }

    // Walk RUNNERS. First match wins.
    let token_refs: Vec<&str> = after_wrapper.iter().map(String::as_str).collect();
    for runner in RUNNERS {
        if (runner.matches)(&token_refs) {
            return Some(MatchedCommand {
                runner,
                stripped_tokens: after_wrapper,
            });
        }
    }

    // Fallback: direct invocations like `pnpm vitest` or `yarn cargo test`
    // skip the wrapper-strip path (`pnpm exec` / `yarn exec`) and skip the
    // script-alias path (the second token isn't a known script name in our
    // registry). Try once more with the leading `pnpm` / `yarn` dropped.
    // Only fires AFTER the script-alias path returned None, so a real
    // `pnpm test` / `yarn test` still resolves through the alias resolver
    // and never reaches this fallback.
    if matches!(
        after_wrapper.first().map(String::as_str),
        Some("pnpm") | Some("yarn")
    ) && after_wrapper.len() > 1
    {
        let after_pm: Vec<String> = after_wrapper[1..].to_vec();
        let pm_token_refs: Vec<&str> = after_pm.iter().map(String::as_str).collect();
        for runner in RUNNERS {
            if (runner.matches)(&pm_token_refs) {
                return Some(MatchedCommand {
                    runner,
                    stripped_tokens: after_pm,
                });
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tokenize_simple_command() {
        assert_eq!(tokenize("vitest run"), Some(vec_of(&["vitest", "run"])));
    }

    #[test]
    fn tokenize_handles_quotes() {
        assert_eq!(
            tokenize(r#"vitest run "src/foo bar.test.ts""#),
            Some(vec_of(&["vitest", "run", "src/foo bar.test.ts"]))
        );
    }

    #[test]
    fn tokenize_unmatched_quote_returns_none() {
        assert_eq!(tokenize(r#"vitest "run"#), None);
    }

    #[test]
    fn strip_env_drops_assignments_only() {
        let tokens = vec_of(&["CI=1", "NODE_ENV=test", "vitest"]);
        assert_eq!(strip_env_prefix(&tokens), &tokens[2..]);
    }

    #[test]
    fn strip_env_does_not_drop_value_with_equals() {
        // "vitest" doesn't contain '=', so nothing stripped
        let tokens = vec_of(&["vitest", "--reporter=verbose"]);
        assert_eq!(strip_env_prefix(&tokens), &tokens[..]);
    }

    #[test]
    fn first_segment_at_first_separator() {
        let tokens = vec_of(&["cargo", "build", "&&", "cargo", "test"]);
        assert_eq!(first_segment(&tokens), &tokens[..2]);
    }

    #[test]
    fn first_segment_no_separator_returns_all() {
        let tokens = vec_of(&["vitest", "run", "src/foo.test.ts"]);
        assert_eq!(first_segment(&tokens), &tokens[..]);
    }

    #[test]
    fn strip_wrappers_handles_npx() {
        let tokens = vec_of(&["npx", "vitest"]);
        assert_eq!(strip_wrappers(&tokens), &tokens[1..]);
    }

    #[test]
    fn strip_wrappers_handles_pnpm_exec() {
        let tokens = vec_of(&["pnpm", "exec", "vitest"]);
        assert_eq!(strip_wrappers(&tokens), &tokens[2..]);
    }

    #[test]
    fn strip_wrappers_no_change_when_no_wrapper() {
        let tokens = vec_of(&["vitest"]);
        assert_eq!(strip_wrappers(&tokens), &tokens[..]);
    }

    #[test]
    fn strip_wrappers_handles_composed_dotenv_npx() {
        // dotenv -- npx vitest → vitest (both wrappers must be peeled)
        let tokens = vec_of(&["dotenv", "--", "npx", "vitest"]);
        assert_eq!(strip_wrappers(&tokens), &tokens[3..]);
    }

    #[test]
    fn strip_wrappers_handles_composed_pnpm_exec_dotenv() {
        // pnpm exec dotenv -- vitest → vitest
        let tokens = vec_of(&["pnpm", "exec", "dotenv", "--", "vitest"]);
        assert_eq!(strip_wrappers(&tokens), &tokens[4..]);
    }

    #[test]
    fn match_command_finds_vitest_through_composed_wrappers() {
        let m = match_command("dotenv -- npx vitest run", None).expect("should match");
        assert_eq!(m.runner.name, "vitest");
        assert_eq!(m.stripped_tokens, vec_of(&["vitest", "run"]));
    }

    #[test]
    fn match_command_finds_vitest() {
        let m = match_command("vitest run src/foo.test.ts", None).expect("should match");
        assert_eq!(m.runner.name, "vitest");
        assert_eq!(
            m.stripped_tokens,
            vec_of(&["vitest", "run", "src/foo.test.ts"])
        );
    }

    #[test]
    fn match_command_finds_cargo_test() {
        let m = match_command("cargo test --release", None).expect("should match");
        assert_eq!(m.runner.name, "cargo");
    }

    #[test]
    fn match_command_strips_env_and_wrapper() {
        let m = match_command("CI=1 npx vitest", None).expect("should match");
        assert_eq!(m.runner.name, "vitest");
        assert_eq!(m.stripped_tokens, vec_of(&["vitest"]));
    }

    #[test]
    fn match_command_first_segment_only() {
        // cargo build && cargo test → only the first segment (cargo build) considered → no match
        assert!(match_command("cargo build && cargo test", None).is_none());
    }

    #[test]
    fn match_command_unknown_returns_none() {
        assert!(match_command("git diff test.txt", None).is_none());
        assert!(match_command("eslint test/", None).is_none());
        assert!(match_command("make test", None).is_none());
    }

    #[test]
    fn match_command_resolves_npm_test() {
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "vitest --passWithNoTests"}}"#,
        )
        .unwrap();
        let m = match_command("npm test", Some(dir.path())).expect("should match");
        assert_eq!(m.runner.name, "vitest");
    }

    #[test]
    fn match_command_bun_test_does_not_match_in_v1() {
        // bun has a built-in test runner; v1 doesn't ship a BUN runner, so this
        // should NOT match (and definitely not try to resolve a script alias).
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "vitest"}}"#,
        )
        .unwrap();
        assert!(match_command("bun test", Some(dir.path())).is_none());
    }

    #[test]
    fn match_command_breaks_on_self_referential_npm_script() {
        // Regression for stack overflow on cyclic aliases. Without the
        // cross-call visited HashSet in match_command_inner, this would
        // infinite-loop: resolve_alias returns "npm run test" each time
        // and match_command keeps re-entering with that same string.
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "npm run test"}}"#,
        )
        .unwrap();
        assert!(match_command("npm test", Some(dir.path())).is_none());
        assert!(match_command("npm run test", Some(dir.path())).is_none());
    }

    #[test]
    fn match_command_breaks_on_two_step_alias_cycle() {
        // Regression for the multi-step cycle the simpler "if resolved == cmd"
        // guard would miss. a → b → a must terminate.
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"a": "npm run b", "b": "npm run a"}}"#,
        )
        .unwrap();
        assert!(match_command("npm run a", Some(dir.path())).is_none());
        assert!(match_command("npm run b", Some(dir.path())).is_none());
    }

    #[test]
    fn match_command_finds_runner_through_pnpm_direct_invocation() {
        // Regression: `pnpm vitest` (no `exec`) is documented as a valid
        // way to run a binary via pnpm. The wrapper-strip path only
        // handled `pnpm exec vitest`. Now a fallback in match_command_inner
        // strips a leading `pnpm`/`yarn` and re-walks the runner registry.
        let m = match_command("pnpm vitest run src/foo.test.ts", None).expect("should match");
        assert_eq!(m.runner.name, "vitest");
        assert_eq!(
            m.stripped_tokens,
            vec_of(&["vitest", "run", "src/foo.test.ts"])
        );
    }

    #[test]
    fn match_command_finds_runner_through_yarn_direct_invocation() {
        let m = match_command("yarn cargo test --release", None).expect("should match");
        assert_eq!(m.runner.name, "cargo");
    }

    #[test]
    fn match_command_pnpm_test_still_resolves_via_alias_not_fallback() {
        // The fallback only fires AFTER alias resolution returns None.
        // `pnpm test` MUST still go through script-alias resolution
        // (so the resolved script body, not the literal `test`, is matched).
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "vitest run"}}"#,
        )
        .unwrap();
        let m = match_command("pnpm test", Some(dir.path())).expect("should match");
        assert_eq!(m.runner.name, "vitest");
    }

    #[test]
    fn match_command_preserves_cli_args_after_npm_test_separator() {
        // Regression: `npm test -- src/foo.test.ts` used to drop the args
        // because resolve_alias returned only the script body. Now CLI args
        // after the script name (with `--` separator stripped) are appended
        // to the resolved command, then re-tokenized through the pipeline.
        use std::fs;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "vitest"}}"#,
        )
        .unwrap();
        let m =
            match_command("npm test -- src/foo.test.ts", Some(dir.path())).expect("should match");
        assert_eq!(m.runner.name, "vitest");
        assert_eq!(
            m.stripped_tokens,
            vec_of(&["vitest", "src/foo.test.ts"]),
            "CLI args after npm `--` separator should be preserved"
        );
    }
}
