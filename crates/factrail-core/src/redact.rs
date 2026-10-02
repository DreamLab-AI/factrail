//! Credential redaction for everything that leaves the transcript: the judge's
//! request body and the saved copies of full outputs.
//!
//! Deliberately narrow: every rule is a recognisable credential shape or a named
//! credential slot. Entropy guesses ("any 32+ char mixed string") are left out on
//! purpose — they hit tool-use ids, content hashes and base64 payloads the judge
//! needs to correlate its questions with the state. A match whose body is one
//! repeated character is a placeholder (`sk-xxxxxxxx…`) and is left alone. The
//! session's own transcript is never touched.

use std::sync::LazyLock;

use regex::{Captures, Regex, RegexBuilder};
use serde_json::Value;

fn re(pattern: &str, case_insensitive: bool) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .expect("static regex")
}

/// A credential family: a name, its pattern, and whether the pattern's first
/// group is a label to keep (`Bearer `, `aws_secret_access_key=`).
struct Family {
    name: &'static str,
    pattern: Regex,
    keeps_label: bool,
    /// The match must not follow a letter, digit or `-` (a lookbehind the regex engine lacks).
    standalone: bool,
}

static FAMILIES: LazyLock<Vec<Family>> = LazyLock::new(|| {
    let f = |name, pattern: &str, ci, keeps_label, standalone| Family {
        name,
        pattern: re(pattern, ci),
        keeps_label,
        standalone,
    };
    vec![
        // PEM private keys, also truncated ones (a capped tool input can cut the END line).
        f(
            "private-key",
            r"-----BEGIN [A-Z0-9 ]{0,40}PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END [A-Z0-9 ]{0,40}PRIVATE KEY(?: BLOCK)?-----|$)",
            false,
            false,
            false,
        ),
        f(
            "jwt",
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            false,
            false,
            false,
        ),
        f("sk-ant", r"sk-ant-[A-Za-z0-9_-]{8,}", false, false, false),
        f(
            "sk-proj",
            r"sk-proj-[A-Za-z0-9_-]{20,}",
            false,
            false,
            false,
        ),
        f(
            "sk-or-v1",
            r"sk-or-v1-[A-Za-z0-9]{30,}",
            false,
            false,
            false,
        ),
        f(
            "sk",
            r"sk-(?:svcacct-|admin-)?[A-Za-z0-9_.-]{16,}",
            false,
            false,
            true,
        ),
        f(
            "stripe",
            r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
            false,
            false,
            false,
        ),
        f(
            "github-pat",
            r"github_pat_[A-Za-z0-9_]{16,}",
            false,
            false,
            false,
        ),
        f(
            "gh-token",
            r"\bgh[pousr]_[A-Za-z0-9]{16,}",
            false,
            false,
            false,
        ),
        f("glpat", r"glpat-[A-Za-z0-9_-]{20,}", false, false, false),
        f("hf", r"\bhf_[A-Za-z0-9]{20,}", false, false, false),
        f("npm", r"\bnpm_[A-Za-z0-9]{20,}", false, false, false),
        f("nia", r"\bnk_[A-Za-z0-9]{20,}", false, false, false),
        f("google-api", r"AIza[0-9A-Za-z_-]{30,}", false, false, false),
        f(
            "slack",
            r"\bxox[abeprs]-[0-9A-Za-z-]{10,}",
            false,
            false,
            false,
        ),
        f(
            "aws-akia",
            r"\b(?:AKIA|ASIA|ABIA|ACCA|AGPA|AIDA|AROA|ANPA|ANVA)[0-9A-Z]{16}\b",
            false,
            false,
            false,
        ),
        f(
            "aws-secret",
            r#"(aws_secret_access_key["'\s=:]+)[A-Za-z0-9/+=]{30,}"#,
            true,
            true,
            false,
        ),
        f(
            "nostr-nsec",
            r"\bnsec1[02-9ac-hj-np-z]{20,}",
            false,
            false,
            false,
        ),
        f("pplx", r"pplx-[A-Za-z0-9]{20,}", false, false, false),
        f("xai", r"xai-[A-Za-z0-9]{20,}", false, false, false),
        f("groq", r"gsk_[A-Za-z0-9]{20,}", false, false, false),
        f("sakana", r"fish_[A-Za-z0-9_]{20,}", false, false, false),
        f("keenable", r"keen_[A-Za-z0-9_]{20,}", false, false, false),
        f(
            "sendgrid",
            r"SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}",
            false,
            false,
            false,
        ),
        f(
            "telegram",
            r"\b\d{8,12}:AA[A-Za-z0-9_-]{30,}",
            false,
            false,
            false,
        ),
    ]
});

/// `Authorization: Bearer x`: keep the scheme, drop the credential.
static AUTH_SCHEME: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b(Bearer|Basic|Token)([ \t]{1,4})([A-Za-z0-9._~+/=-]{8,})",
        false,
    )
});
/// `scheme://user:password@host`: keep the user, drop the password.
static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(\b[a-z][a-z0-9+.-]{1,15}://[^\s/@:]{1,64}:)([^\s@/]{1,128})(@)",
        true,
    )
});
/// `AWS_SECRET_ACCESS_KEY=…`, `"apiKey": "…"`, `password: …`: keep the name, drop the value.
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(\b[A-Za-z0-9_-]{0,40}(?:api[_-]?key|secret|token|passw(?:or)?d|private[_-]?key|access[_-]?key)[A-Za-z0-9_-]{0,20}["']?[ \t]{0,3}[:=][ \t]{0,3}["']?)([^\s"'`<>{}()\[\],;\\]{8,})"#,
        true,
    )
});
/// `--token value`, `--api-key=value` on a command line.
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(--[A-Za-z0-9-]{0,30}(?:api-?key|secret|token|passw(?:or)?d)[A-Za-z0-9-]{0,20}(?:=|[ \t]{1,3})["']?)([^\s"'`<>{}()\[\],;\\]{8,})"#,
        true,
    )
});
static PROVIDER_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z]+[_-](?:[a-z0-9]+-)?", false));
static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z_.]*$", false));
static WORDY: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:true|false|null|undefined|none|required|optional|string|number)$",
        true,
    )
});

/// The marker a redacted value becomes.
pub fn marker(family: &str) -> String {
    format!("[REDACTED:{family}]")
}

/// A value after a credential name that is a reference or code, not a secret.
fn not_a_secret(value: &str) -> bool {
    value.starts_with("[REDACTED")
        || value.starts_with(['$', '%', '/', '~', '.'])
        || WORDY.is_match(value)
        || IDENTIFIER.is_match(value)
}

/// One repeated character after the provider prefix: a placeholder, not a key.
fn placeholder(body: &str) -> bool {
    let body = PROVIDER_PREFIX.replace(body, "");
    let mut chars = body.chars();
    match chars.next() {
        Some(first) => chars.all(|c| c == first),
        None => true,
    }
}

/// Redacts every credential-shaped substring of `text`, and every occurrence of
/// each of `known` (exact values, eight chars or longer: the caller's own keys).
/// Returns the text and how many values were replaced.
///
/// ```
/// use factrail_core::redact::redact;
/// let (out, n) = redact("curl -H 'Authorization: Bearer abcdefgh12345678' https://u:hunter22@h/x", &[]);
/// assert_eq!(n, 2);
/// assert!(!out.contains("abcdefgh12345678") && !out.contains("hunter22"));
/// let (same, none) = redact("sk-xxxxxxxxxxxxxxxxxxxxxxxx is a placeholder", &[]);
/// assert_eq!(none, 0);
/// assert!(same.starts_with("sk-xxx"));
/// ```
pub fn redact(text: &str, known: &[&str]) -> (String, usize) {
    let mut count = 0;
    let mut out = text.to_owned();
    for secret in known {
        if secret.len() >= 8 && out.contains(secret) {
            count += out.matches(secret).count();
            out = out.replace(secret, &marker("known"));
        }
    }
    for family in FAMILIES.iter() {
        let source = out.clone();
        out = family
            .pattern
            .replace_all(&source, |caps: &Captures<'_>| {
                let whole = caps.get(0).expect("match");
                if family.standalone {
                    let before = source[..whole.start()].chars().next_back();
                    if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-') {
                        return whole.as_str().to_owned();
                    }
                }
                let label = if family.keeps_label {
                    caps.get(1).map_or("", |m| m.as_str())
                } else {
                    ""
                };
                if placeholder(&whole.as_str()[label.len()..]) {
                    return whole.as_str().to_owned();
                }
                count += 1;
                format!("{label}{}", marker(family.name))
            })
            .into_owned();
    }
    out = AUTH_SCHEME
        .replace_all(&out, |c: &Captures<'_>| {
            let value = &c[3];
            if value.chars().all(|ch| ch.is_ascii_alphabetic()) || value.starts_with("[REDACTED") {
                return c[0].to_owned();
            }
            count += 1;
            format!("{}{}{}", &c[1], &c[2], marker("bearer"))
        })
        .into_owned();
    out = URL_CREDENTIALS
        .replace_all(&out, |c: &Captures<'_>| {
            if c[2].contains("[REDACTED") {
                return c[0].to_owned();
            }
            count += 1;
            format!("{}{}{}", &c[1], marker("url-password"), &c[3])
        })
        .into_owned();
    for pattern in [&*NAMED, &*FLAG] {
        out = pattern
            .replace_all(&out, |c: &Captures<'_>| {
                if not_a_secret(&c[2]) {
                    return c[0].to_owned();
                }
                count += 1;
                format!("{}{}", &c[1], marker("named"))
            })
            .into_owned();
    }
    (out, count)
}

/// [`redact`] over every string inside a JSON value (object keys untouched).
pub fn redact_value(value: &Value, known: &[&str]) -> (Value, usize) {
    let mut count = 0;
    fn walk(v: &Value, known: &[&str], count: &mut usize) -> Value {
        match v {
            Value::String(s) => {
                let (out, n) = redact(s, known);
                *count += n;
                Value::String(out)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|x| walk(x, known, count)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, x)| (k.clone(), walk(x, known, count)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let out = walk(value, known, &mut count);
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s, &[]).0
    }

    #[test]
    fn families() {
        let samples = [
            ("sk-ant-api03-abcdefghijklmnop", "sk-ant"),
            ("sk-proj-abcdefghijklmnopqrstuvwxyz12", "sk-proj"),
            ("ghp_abcdefghijklmnopqrstuvwxyz0123", "gh-token"),
            ("github_pat_11ABCDEFG0123456789abcdef", "github-pat"),
            ("AKIAABCDEFGHIJKLMNOP", "aws-akia"),
            ("xoxb-1234567890-abcdefghij", "slack"),
            ("AIzaSyA1234567890abcdefghijklmnopqrstu", "google-api"),
            (
                "nsec1qqqsyqcyq5rqwzqfpg9scrgwpugpzysnzs23v9ccrydpk8qarc0jqxyz",
                "nostr-nsec",
            ),
            (
                "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4fw",
                "jwt",
            ),
            ("hf_abcdefghijklmnopqrstuvwxyz", "hf"),
            ("sk_live_abcdefghijklmnop1234", "stripe"),
        ];
        for (secret, family) in samples {
            let out = r(&format!("value {secret} end"));
            assert_eq!(out, format!("value [REDACTED:{family}] end"), "{secret}");
        }
    }

    #[test]
    fn sk_needs_a_boundary() {
        assert_eq!(
            r("task-abcdefghijklmnopqrstuv"),
            "task-abcdefghijklmnopqrstuv"
        );
        assert_eq!(r("key=sk-abcdefghijklmnopqrstuv"), "key=[REDACTED:sk]");
    }

    #[test]
    fn named_slots_flags_and_references() {
        assert_eq!(r("API_KEY=s3cr3tvalue99"), "API_KEY=[REDACTED:named]");
        assert_eq!(
            r(r#""password": "hunter2hunter2""#),
            r#""password": "[REDACTED:named]""#
        );
        assert_eq!(r("--token abcdef123456"), "--token [REDACTED:named]");
        assert_eq!(r("API_KEY=$OPENAI_API_KEY"), "API_KEY=$OPENAI_API_KEY");
        assert_eq!(r("token: required"), "token: required");
        assert_eq!(
            r("secret = config.secret_value"),
            "secret = config.secret_value"
        );
        assert_eq!(r("Bearer authentication"), "Bearer authentication");
    }

    #[test]
    fn known_values_and_truncated_pem() {
        let (out, n) = redact("the key is abc12345XYZ here", &["abc12345XYZ"]);
        assert_eq!(out, "the key is [REDACTED:known] here");
        assert_eq!(n, 1);
        assert_eq!(
            r("-----BEGIN RSA PRIVATE KEY-----\nMIIEow"),
            "[REDACTED:private-key]"
        );
    }

    #[test]
    fn deep_json() {
        let v = serde_json::json!({"a": ["ghp_abcdefghijklmnopqrstuvwxyz0123"], "ghp_key": 1});
        let (out, n) = redact_value(&v, &[]);
        assert_eq!(n, 1);
        assert_eq!(
            out,
            serde_json::json!({"a": ["[REDACTED:gh-token]"], "ghp_key": 1})
        );
    }
}
