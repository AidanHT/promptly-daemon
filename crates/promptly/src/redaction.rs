//! Client-side secret redaction for the auto-submit payload (`20`).
//!
//! The daemon upload carries the player's packaged solution and a git-diff
//! snapshot — *code*, scanned here before it ever leaves the machine. This is the
//! evolved successor to the Phase-1 transcript strip (`lib/telemetry/redact.ts`),
//! which it supersedes: same conservative philosophy (match unambiguous secret
//! SHAPES, never guess at high-entropy prose), plus what `20` adds — a **versioned
//! catalog** ([`REDACTION_CATALOG_VERSION`], stamped onto every result so the
//! server records which ruleset cleaned a payload), **visible categories** (so the
//! player sees *what kind* of secret was stripped), URL-embedded credentials, JWTs,
//! and an **abort** path: a high-confidence secret that can't be cleanly bounded
//! (an unterminated PEM block) fails the whole upload rather than leaking a tail.
//!
//! ## Why no blanket high-entropy sweep
//!
//! `docs/plan/20` lists "high-entropy" among the targets, but a free-floating
//! entropy scan over *code* is the opposite of conservative: it shreds base64
//! fixtures, hashes, UUIDs, and minified assets — corrupting the very artifact
//! being scored. So entropy is caught only in a secret *context*: an opaque value
//! assigned to a secret-named key (`SECRET=…`, `"api_key": "…"`). Free-standing
//! secrets are caught by known shape (provider-key prefixes, JWTs, PEM,
//! `user:pass@` URLs) instead. This matches the explicit stance of the Phase-1
//! redactor ("rather than guessing at high-entropy prose") and the plan's own
//! "allowlist false positives (the word `token` in code)" guidance. Recorded in
//! the plan reconciliation.
//!
//! Pure and deterministic. Bump [`REDACTION_CATALOG_VERSION`] on any rule change.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::{Captures, NoExpand, Regex};

/// Catalog ruleset version, stamped onto every [`Redacted`] result and uploaded so
/// the server records which redaction ruleset cleaned a payload. Bump on any change
/// to the rules below.
pub const REDACTION_CATALOG_VERSION: u32 = 1;

/// The marker every redacted span becomes: `[REDACTED:<category>]`.
const REDACTED_PREFIX: &str = "[REDACTED:";

/// The outcome of redacting a payload span: the cleaned text, the distinct secret
/// categories that fired (sorted, for a stable display), and the catalog version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    /// The input with every secret-shaped span replaced by `[REDACTED:<category>]`.
    pub text: String,
    /// The distinct categories that matched (e.g. `provider_key`, `env_secret`),
    /// sorted and deduped — the visible summary shown before upload.
    pub categories: Vec<String>,
    /// The [`REDACTION_CATALOG_VERSION`] that produced this result.
    pub catalog_version: u32,
}

impl Redacted {
    /// Whether anything was redacted (any category fired).
    pub fn is_clean(&self) -> bool {
        self.categories.is_empty()
    }
}

/// A redaction outcome that must abort the upload: a high-confidence secret that
/// can't be cleanly bounded, so we refuse to send the payload at all.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RedactionError {
    /// An opening secret marker (a PEM `BEGIN … PRIVATE KEY`) with no closing
    /// `END`: the secret's extent is unknown, so redacting it cleanly is
    /// impossible. The category is named so the abort message can be specific.
    #[error("refusing to upload: an unterminated {0} block can't be safely redacted")]
    Uncleanable(String),
}

/// The versioned catalog of whole-span secret patterns: each match is replaced
/// entirely by `[REDACTED:<category>]`. Ordered most- to least-specific so a
/// precise prefix (Anthropic `sk-ant-`) is consumed before the generic `sk-`. The
/// patterns mirror the Phase-1 shapes (`lib/telemetry/redact.ts`) and add a JWT
/// rule. Multiple rules may share a category (all provider keys collapse to one).
const WHOLE_SPAN_RULES: &[(&str, &str)] = &[
    // PEM private-key blocks (any key type) — the whole BEGIN…END span.
    (
        "private_key",
        r"-----BEGIN(?: [A-Z0-9]+)* PRIVATE KEY-----[\s\S]*?-----END(?: [A-Z0-9]+)* PRIVATE KEY-----",
    ),
    // Anthropic keys, before the generic provider rule below.
    ("provider_key", r"\bsk-ant-[A-Za-z0-9_-]{16,}"),
    // OpenAI / generic provider keys (sk-…, sk-proj-…).
    ("provider_key", r"\bsk-[A-Za-z0-9_-]{20,}"),
    // AWS access key id.
    ("provider_key", r"\bAKIA[0-9A-Z]{16}\b"),
    // Google API key.
    ("provider_key", r"\bAIza[0-9A-Za-z_-]{20,}"),
    // GitHub fine-grained PAT, then classic PATs / OAuth / app tokens.
    ("provider_key", r"\bgithub_pat_[A-Za-z0-9_]{22,}"),
    ("provider_key", r"\bgh[oprsu]_[A-Za-z0-9]{36,}"),
    // Slack tokens.
    ("provider_key", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    // Authorization bearer tokens.
    ("bearer_token", r"\bBearer\s+[A-Za-z0-9._~+/-]{16,}=*"),
    // JSON Web Tokens (`header.payload.signature`, each base64url, header `eyJ`).
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    ),
];

/// The secret-named key fragment shared by the assignment rules: a bounded prefix
/// (so matching stays linear — no ReDoS) ending in a credential keyword. Bounded
/// to `{0,40}` of the prefix exactly like the Phase-1 redactor.
const SECRET_KEY_FRAGMENT: &str = r"[A-Za-z0-9_.\-]{0,40}(?:api[_-]?key|secret|token|password|passwd|pwd|access[_-]?key|client[_-]?secret|database_url)";

fn compiled_whole_span_rules() -> &'static [(&'static str, Regex)] {
    static RULES: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    RULES.get_or_init(|| {
        WHOLE_SPAN_RULES
            .iter()
            .map(|(category, pattern)| {
                (
                    *category,
                    Regex::new(pattern).expect("catalog whole-span pattern compiles"),
                )
            })
            .collect()
    })
}

/// An opening PEM private-key marker; used to detect an unterminated block (a
/// `BEGIN` that survived whole-span redaction because it had no matching `END`).
fn pem_begin_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"-----BEGIN(?: [A-Z0-9]+)* PRIVATE KEY-----")
            .expect("pem-begin pattern compiles")
    })
}

/// Credentials embedded in a URL's userinfo (`scheme://user:password@host`): only
/// the password segment is redacted, leaving the (non-secret) scheme/user/host.
fn url_credentials_rule() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b([a-z][a-z0-9+.\-]*://[^\s:@/]*):([^\s:@/]+)@")
            .expect("url-credentials pattern compiles")
    })
}

/// A `.env`-style line: `KEY=value` (optionally `export`-prefixed, indentation
/// preserved) for a secret-named key, with the value running to end of line. The
/// line anchor is the discriminator from code: a `.env` value ends the line,
/// whereas `let token = compute();` has the value followed by `(` / `;`. The value
/// must be 12+ token-alphabet chars, so short flags and code references don't match.
fn env_line_rule() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?im)^([ \t]*(?:export[ \t]+)?{SECRET_KEY_FRAGMENT}[ \t]*=[ \t]*)([A-Za-z0-9_+/=~\-]{{12,}})[ \t]*$"
        ))
        .expect("env-line pattern compiles")
    })
}

/// A quoted secret assignment anywhere (JSON/YAML/code): `"api_key": "…"`,
/// `password = '…'`. The key (optionally quoted) and separator are preserved and
/// only the quoted value (8+ chars) is redacted. The quoted-literal requirement is
/// what keeps `token = computeToken()` (an unquoted call) from matching.
fn quoted_assignment_rule() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r#"(?i)(["']?)({SECRET_KEY_FRAGMENT})(["']?)([ \t]*[:=][ \t]*)(?:"([^"\r\n]{{8,}})"|'([^'\r\n]{{8,}})')"#
        ))
        .expect("quoted-assignment pattern compiles")
    })
}

/// Redact every secret-shaped span from `input`, returning the cleaned text, the
/// categories that fired, and the catalog version.
///
/// Returns [`RedactionError::Uncleanable`] when a high-confidence secret can't be
/// cleanly bounded (an unterminated PEM block) — the caller must abort the upload.
pub fn redact(input: &str) -> Result<Redacted, RedactionError> {
    let mut categories: BTreeSet<&'static str> = BTreeSet::new();
    let mut text = input.to_string();

    // 1) Whole-span known shapes (most- to least-specific). Each fully replaces its
    //    match, so a more precise prefix is consumed before a generic one.
    for (category, regex) in compiled_whole_span_rules() {
        if regex.is_match(&text) {
            let placeholder = format!("{REDACTED_PREFIX}{category}]");
            text = regex
                .replace_all(&text, NoExpand(&placeholder))
                .into_owned();
            categories.insert(category);
        }
    }

    // 2) Abort if a PEM `BEGIN` survived: it had no matching `END`, so the secret's
    //    extent is unknown and we refuse to upload rather than leak its tail.
    if pem_begin_marker().is_match(&text) {
        return Err(RedactionError::Uncleanable("private_key".to_string()));
    }

    // 3) URL-embedded credentials: redact only the password in `user:pass@`.
    {
        let mut fired = false;
        let replaced = url_credentials_rule().replace_all(&text, |caps: &Captures| {
            fired = true;
            format!("{}:{REDACTED_PREFIX}url_credentials]@", &caps[1])
        });
        if fired {
            categories.insert("url_credentials");
        }
        text = replaced.into_owned();
    }

    // 4) `.env`-style secret assignments (value to end of line).
    {
        let mut fired = false;
        let replaced = env_line_rule().replace_all(&text, |caps: &Captures| {
            fired = true;
            format!("{}{REDACTED_PREFIX}env_secret]", &caps[1])
        });
        if fired {
            categories.insert("env_secret");
        }
        text = replaced.into_owned();
    }

    // 5) Quoted secret assignments anywhere — skipping a value a whole-span rule
    //    already replaced, so the more specific category (and a single redaction)
    //    is preserved.
    {
        let mut fired = false;
        let replaced = quoted_assignment_rule().replace_all(&text, |caps: &Captures| {
            let double = caps.get(5);
            let value = double
                .or_else(|| caps.get(6))
                .map(|m| m.as_str())
                .unwrap_or("");
            if value.contains(REDACTED_PREFIX) {
                // Already redacted by a whole-span rule; leave it untouched.
                return caps[0].to_string();
            }
            fired = true;
            let quote = if double.is_some() { '"' } else { '\'' };
            format!(
                "{}{}{}{}{quote}{REDACTED_PREFIX}env_secret]{quote}",
                &caps[1], &caps[2], &caps[3], &caps[4]
            )
        });
        if fired {
            categories.insert("env_secret");
        }
        text = replaced.into_owned();
    }

    Ok(Redacted {
        text,
        categories: categories.into_iter().map(str::to_string).collect(),
        catalog_version: REDACTION_CATALOG_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(input: &str) -> Redacted {
        redact(input).expect("no uncleanable secret")
    }

    #[test]
    fn provider_keys_of_each_shape_are_redacted() {
        for (input, label) in [
            ("const k = \"sk-ant-api03-abcdefghijklmnop\";", "anthropic"),
            ("OPENAI=sk-abcdefghijklmnopqrstuvwxyz012", "openai"),
            ("aws = AKIAIOSFODNN7EXAMPLE", "aws"),
            ("g = AIzaSyA0000000000000000000000000000000", "google"),
            ("p = ghp_0123456789012345678901234567890123456789", "github"),
            ("s = xoxb-0123456789-abcdefABCDEF", "slack"),
        ] {
            let out = clean(input);
            assert!(
                !out.text.contains("sk-ant-"),
                "{label}: anthropic prefix gone"
            );
            assert!(
                out.text.contains("[REDACTED:provider_key]"),
                "{label} redacted"
            );
            assert_eq!(
                out.categories,
                vec!["provider_key".to_string()],
                "{label} category"
            );
        }
    }

    #[test]
    fn anthropic_prefix_wins_over_the_generic_sk_rule() {
        // The sk-ant rule runs first, so the whole key collapses once (not a
        // generic sk- match leaving `-ant-…`).
        let out = clean("key=sk-ant-api03-abcdefghijklmnop");
        assert_eq!(out.text, "key=[REDACTED:provider_key]");
    }

    #[test]
    fn bearer_tokens_and_jwts_are_redacted() {
        let bearer = clean("Authorization: Bearer abcdef0123456789ABCDEF");
        assert!(bearer.text.contains("[REDACTED:bearer_token]"));
        assert_eq!(bearer.categories, vec!["bearer_token".to_string()]);

        let jwt =
            clean("t = eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N");
        assert!(jwt.text.contains("[REDACTED:jwt]"));
        assert_eq!(jwt.categories, vec!["jwt".to_string()]);
    }

    #[test]
    fn a_pem_private_key_block_is_redacted_whole() {
        let input = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabcd\n-----END RSA PRIVATE KEY-----\nafter";
        let out = clean(input);
        assert_eq!(out.text, "before\n[REDACTED:private_key]\nafter");
        assert_eq!(out.categories, vec!["private_key".to_string()]);
    }

    #[test]
    fn an_unterminated_pem_block_aborts_the_upload() {
        let input = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n(no end marker)";
        let err = redact(input).expect_err("unterminated key can't be cleanly redacted");
        assert_eq!(err, RedactionError::Uncleanable("private_key".to_string()));
    }

    #[test]
    fn a_certificate_block_is_left_intact() {
        // Certificates are public; only PRIVATE KEY blocks are secrets.
        let input = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        let out = clean(input);
        assert_eq!(out.text, input);
        assert!(out.is_clean());
    }

    #[test]
    fn url_embedded_credentials_redact_only_the_password() {
        let out = clean("DATABASE_URL=postgres://app_user:s3cr3tP4ss@db.internal:5432/prod");
        assert_eq!(
            out.text,
            "DATABASE_URL=postgres://app_user:[REDACTED:url_credentials]@db.internal:5432/prod"
        );
        assert_eq!(out.categories, vec!["url_credentials".to_string()]);
    }

    #[test]
    fn a_plain_url_without_credentials_is_untouched() {
        let out = clean("fetch(\"https://api.example.com:8080/v1/items?limit=20\")");
        assert!(out.is_clean());
    }

    #[test]
    fn dotenv_style_secret_assignments_are_redacted() {
        let out = clean("export OPENAI_API_KEY=abcdef0123456789ghij\nPORT=3000\n");
        assert!(out.text.contains("OPENAI_API_KEY=[REDACTED:env_secret]"));
        assert!(
            out.text.contains("PORT=3000"),
            "non-secret keys are left alone"
        );
        assert_eq!(out.categories, vec!["env_secret".to_string()]);
    }

    #[test]
    fn quoted_secret_assignments_preserve_the_key_and_redact_the_value() {
        let out = clean("{ \"api_key\": \"super-secret-value-here\" }");
        assert_eq!(out.text, "{ \"api_key\": \"[REDACTED:env_secret]\" }");
        assert_eq!(out.categories, vec!["env_secret".to_string()]);
    }

    #[test]
    fn the_word_token_in_ordinary_code_is_not_redacted() {
        // The plan's named false-positive: `token` as an identifier in code.
        for code in [
            "let token = computeToken();",
            "const apiToken = await fetchToken(session);",
            "if (token.length > 0) { return token; }",
            "self.secret = other_secret;",
            "password = get_password()",
        ] {
            let out = clean(code);
            assert_eq!(out.text, code, "unchanged: {code}");
            assert!(out.is_clean(), "no category fired: {code}");
        }
    }

    #[test]
    fn an_already_redacted_value_is_not_double_redacted() {
        // The token rule catches the provider key inside the quotes; the quoted
        // assignment rule must leave that placeholder (and its category) alone.
        let out = clean("api_key = \"sk-ant-api03-abcdefghijklmnop\"");
        assert_eq!(out.text, "api_key = \"[REDACTED:provider_key]\"");
        assert_eq!(out.categories, vec!["provider_key".to_string()]);
    }

    #[test]
    fn benign_code_is_returned_unchanged_with_no_categories() {
        let code = "fn add(a: i32, b: i32) -> i32 { a + b }\nlet xs = vec![1, 2, 3];";
        let out = clean(code);
        assert_eq!(out.text, code);
        assert!(out.is_clean());
        assert_eq!(out.catalog_version, REDACTION_CATALOG_VERSION);
    }

    #[test]
    fn multiple_distinct_secrets_report_sorted_deduped_categories() {
        let input = "key=sk-abcdefghijklmnopqrstuvwxyz012\nDB_PASSWORD=hunter2hunter2hunter2\n";
        let out = clean(input);
        // provider_key (the sk- key) + env_secret (the .env password), sorted.
        assert_eq!(
            out.categories,
            vec!["env_secret".to_string(), "provider_key".to_string()]
        );
    }
}
