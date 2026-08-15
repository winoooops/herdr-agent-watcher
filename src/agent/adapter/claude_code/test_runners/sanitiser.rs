//! Conservative redaction for content shown in the UI. Catches the common
//! shapes; not a comprehensive secret scanner.
//!
//! TWO patterns are exposed:
//!   - sanitize_for_command — narrower; tuned for `command_preview` where
//!     the matcher pipeline already stripped leading env-var prefixes. Only
//!     redacts known-sensitive prefixes, plus bearer/authorization/api-key/
//!     jwt shapes. Leaves benign env vars like NODE_ENV=test or
//!     VITEST_POOL_ID=1 readable in the panel header.
//!   - sanitize_for_output — broader; tuned for `output_excerpt` where the
//!     content is arbitrary captured stderr/stdout that may contain any
//!     env-var dump. Adds the catch-all KEY=VALUE rule on top of the
//!     command patterns.

use once_cell::sync::Lazy;
use regex::Regex;

const REDACTED: &str = "[REDACTED]";

// Patterns shared by both sanitisers — catch token-shaped secrets that
// can appear anywhere in either surface.
static SHARED_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        // Bearer tokens (case-insensitive). Charset includes base64 chars
        // (`+`, `/`, `=`) — earlier `[A-Za-z0-9._\-]+` only redacted the
        // PREFIX of a base64-encoded credential and left the suffix
        // visible. Now matches the full token through the next whitespace.
        Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._+/=\-]+").unwrap(),
        // Authorization headers (case-insensitive). Match through end of
        // line — the previous `\S+` only consumed the FIRST non-whitespace
        // token, so multi-token credentials like `Authorization: Basic
        // abc xyz` left `abc xyz` visible.
        Regex::new(r"(?i)\bAuthorization:\s*[^\r\n]+").unwrap(),
        // Stripe/etc-style API keys: (sk|pk|rk)_(live|test)_<alnum16+>
        Regex::new(r"\b(sk|pk|rk)_(live|test)_[A-Za-z0-9]{16,}").unwrap(),
        // JWT-like: eyJ followed by base64-ish chunk
        Regex::new(r"\beyJ[A-Za-z0-9._\-]{10,}").unwrap(),
    ]
});

// Command-only: KEY=VALUE assignments where KEY matches a KNOWN-SECRET
// prefix list. Catches the common credential shapes without redacting
// benign diagnostic vars (NODE_ENV, CI, DEBUG, VITEST_POOL_ID, etc.).
// The matcher's `strip_env_prefix` already removes leading env-var
// assignments before the preview is built, so this rule mostly catches
// secrets passed as flag values or positional args.
static COMMAND_ONLY_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        // SECRET_*, API_*, AUTH_*, TOKEN_*, KEY_*, PASSWORD_*, PASS_*,
        // PWD_*, PRIVATE_*, ACCESS_*, plus exact GITHUB_TOKEN /
        // STRIPE_KEY / ANTHROPIC_API_KEY / OPENAI_API_KEY / AWS_*.
        Regex::new(
            r"\b(SECRET|API|AUTH|TOKEN|KEY|PASSWORD|PASS|PWD|PRIVATE|ACCESS|GITHUB|STRIPE|ANTHROPIC|OPENAI|AWS|GCP|AZURE)_[A-Z0-9_]*=\S+",
        )
        .unwrap(),
    ]
});

// Output-only: broad KEY=VALUE rule for arbitrary captured terminal
// output where the over-redaction trade-off is acceptable (a 240-char
// error excerpt loses less by over-redacting than by leaking a secret).
static OUTPUT_ONLY_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        // Any uppercase identifier with =value
        Regex::new(r"\b[A-Z][A-Z0-9_]{2,}=\S+").unwrap(),
    ]
});

/// Sanitiser tuned for `command_preview`. Use this when displaying the
/// reconstructed test command in the panel header.
pub fn sanitize_for_command(input: &str) -> String {
    apply_patterns(
        input,
        SHARED_PATTERNS.iter().chain(COMMAND_ONLY_PATTERNS.iter()),
    )
}

/// Sanitiser tuned for `output_excerpt`. Use this for arbitrary captured
/// stderr/stdout that may contain env-var dumps or unstructured output.
pub fn sanitize_for_output(input: &str) -> String {
    apply_patterns(
        input,
        SHARED_PATTERNS.iter().chain(OUTPUT_ONLY_PATTERNS.iter()),
    )
}

fn apply_patterns<'a>(input: &str, patterns: impl Iterator<Item = &'a Regex>) -> String {
    let mut out = input.to_string();
    for re in patterns {
        out = re.replace_all(&out, REDACTED).to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- command preview ---

    #[test]
    fn command_redacts_known_secret_env_prefixes() {
        let s = sanitize_for_command("STRIPE_KEY=sk_live_abc1234567890abcd vitest");
        assert!(!s.contains("sk_live_abc1234567890abcd"));
        assert!(s.contains("[REDACTED]"));
    }

    #[test]
    fn command_does_not_redact_benign_env_vars() {
        // Regression: previously `\b[A-Z][A-Z0-9_]{2,}=\S+` redacted these
        // unconditionally, leaving "[REDACTED] vitest run" in the header.
        let s = sanitize_for_command("NODE_ENV=test VITEST_POOL_ID=1 CI=true vitest run");
        assert!(
            s.contains("NODE_ENV=test"),
            "NODE_ENV should be visible: {}",
            s
        );
        assert!(
            s.contains("VITEST_POOL_ID=1"),
            "VITEST_POOL_ID should be visible: {}",
            s
        );
        assert!(s.contains("CI=true"), "CI should be visible: {}", s);
    }

    #[test]
    fn command_redacts_bearer_and_jwt() {
        let s = sanitize_for_command(
            "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.foo' https://api",
        );
        assert!(!s.contains("eyJhbGciOiJIUzI1NiJ9.foo"));
    }

    #[test]
    fn command_redacts_bare_api_key_prefixes() {
        let s = sanitize_for_command("--key sk_live_1234567890abcdef1234");
        assert!(!s.contains("sk_live_1234567890abcdef1234"));
    }

    #[test]
    fn command_clean_strings_unchanged() {
        let s = sanitize_for_command("vitest run src/foo.test.ts");
        assert_eq!(s, "vitest run src/foo.test.ts");
    }

    #[test]
    fn command_does_not_redact_short_uppercase_words() {
        let s = sanitize_for_command("FAIL src/foo.test.ts");
        assert_eq!(s, "FAIL src/foo.test.ts");
    }

    // --- output excerpt (broader rule kicks in) ---

    #[test]
    fn output_redacts_arbitrary_uppercase_assignments() {
        // Output excerpts may contain env-var dumps from `env` or panic
        // messages; the conservative pattern is appropriate here.
        let s = sanitize_for_output("Crashed with NODE_ENV=test SECRET_KEY=abc123");
        assert!(!s.contains("NODE_ENV=test"));
        assert!(!s.contains("SECRET_KEY=abc123"));
    }

    #[test]
    fn output_still_redacts_token_shapes() {
        let s = sanitize_for_output("got token: eyJabcdefghijklmnop.body.sig");
        assert!(!s.contains("eyJabcdefghijklmnop"));
    }

    // --- regression: multi-token Authorization values fully redacted ---

    #[test]
    fn redacts_full_basic_authorization_header() {
        // Regression: previously `Authorization:\s*\S+` only matched
        // "Basic" and left "abc xyz" visible.
        let s = sanitize_for_command("Authorization: Basic abc xyz");
        assert!(!s.contains("abc xyz"), "secret leaked: {}", s);
        let t = sanitize_for_output("Authorization: Basic dXNlcjpwYXNz");
        assert!(!t.contains("dXNlcjpwYXNz"), "secret leaked: {}", t);
    }

    // --- regression: bearer tokens with base64 chars fully redacted ---

    #[test]
    fn redacts_bearer_tokens_with_base64_chars() {
        // Regression: previously `Bearer\s+[A-Za-z0-9._\-]+` excluded
        // `+`, `/`, `=`, leaving the suffix of base64-encoded tokens
        // visible (e.g. "Bearer YWJj+/==" only redacted "YWJj").
        let s = sanitize_for_command("Bearer YWJjZGVm+/==");
        assert!(!s.contains("YWJjZGVm+/=="), "secret leaked: {}", s);
        let t = sanitize_for_output("auth: Bearer abc/def+ghi=");
        assert!(!t.contains("abc/def+ghi="), "secret leaked: {}", t);
    }
}
