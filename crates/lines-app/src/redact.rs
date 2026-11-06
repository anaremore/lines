//! Redaction for anything that leaves the app in a diagnostics bundle, and for every log line
//! before it reaches the disk (roadmap U1).
//!
//! Lines never logs credentials, account ids, API keys or level values from paid sources on
//! purpose; this is the second line of defence:
//! - JSON (settings, workspaces): the value of any key that names a secret or an identity
//!   (password, token, API key, account, user, email, …) is replaced, whatever its type.
//! - Text (logs): `key=value` and `key: value` for those keys, e-mail addresses, and long
//!   token-like strings are replaced.
//!
//! Pure and tested; the redaction test in `diagnostics` builds a whole bundle from synthetic
//! settings full of fake secrets and checks none survive.

use serde_json::Value;

pub const REDACTED: &str = "[redacted]";

/// Key fragments that mark a value as secret or identifying (matched case-insensitively,
/// ignoring `_`, `-` and spaces).
const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "credential",
    "authorization",
    "auth",
    "cookie",
    "session",
    "privatekey",
    "account",
    "user",
    "login",
    "email",
    "fcm",
    "clientid",
];

fn normalise(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' ' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a key names something that must not leave the machine.
pub fn is_sensitive_key(key: &str) -> bool {
    let k = normalise(key);
    SENSITIVE.iter().any(|s| k.contains(s))
}

/// A JSON document with every sensitive key's value replaced and every e-mail-looking or
/// token-looking string replaced.
pub fn json(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let v = if is_sensitive_key(k) {
                        Value::String(REDACTED.into())
                    } else {
                        json(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(json).collect()),
        Value::String(s) => Value::String(text(s)),
        other => other.clone(),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// A token-looking word: long, mixing letters and digits (API keys, session ids, JWT parts).
fn looks_like_token(w: &str) -> bool {
    w.len() >= 24
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.'))
        && w.chars().any(|c| c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_alphabetic())
}

fn looks_like_email(w: &str) -> bool {
    let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    match w.split_once('@') {
        Some((user, host)) => !user.is_empty() && host.contains('.') && !host.starts_with('.'),
        None => false,
    }
}

/// One line (or any text) with secrets replaced: values after a sensitive key (`=` or `:`,
/// quoted or not), e-mail addresses and token-like words.
pub fn text(s: &str) -> String {
    // 1. Whole whitespace-separated chunks that are e-mail addresses or tokens.
    let mut chunks = String::with_capacity(s.len());
    for piece in s.split_inclusive(char::is_whitespace) {
        let (word, ws) = piece.split_at(piece.trim_end_matches(char::is_whitespace).len());
        let core = word.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
        });
        if looks_like_email(core) {
            chunks.push_str(&word.replace(core, "[redacted-email]"));
        } else if looks_like_token(core) {
            chunks.push_str(&word.replace(core, "[redacted-token]"));
        } else {
            chunks.push_str(word);
        }
        chunks.push_str(ws);
    }
    // 2. Values after sensitive keys.
    let chars: Vec<char> = chunks.chars().collect();
    let mut out = String::with_capacity(chunks.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !(is_word_char(c) && (i == 0 || !is_word_char(chars[i - 1]))) {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let mut j = i;
        // Optional closing quote of a JSON key, then spaces.
        if j < chars.len() && matches!(chars[j], '"' | '\'') {
            j += 1;
        }
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        if !(is_sensitive_key(&word) && j < chars.len() && matches!(chars[j], '=' | ':')) {
            out.push_str(&word);
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        // The value: quoted, or up to whitespace or a delimiter.
        let mut k = j;
        if k < chars.len() && matches!(chars[k], '"' | '\'') {
            let q = chars[k];
            k += 1;
            while k < chars.len() && chars[k] != q {
                k += 1;
            }
            k = (k + 1).min(chars.len());
        } else {
            while k < chars.len()
                && !chars[k].is_whitespace()
                && !matches!(chars[k], ',' | ';' | '}' | ')' | ']')
            {
                k += 1;
            }
        }
        out.extend(&chars[start..j]);
        if k > j {
            out.push_str(REDACTED);
        }
        i = k;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys() {
        for k in [
            "password",
            "LINES_CQG_PASSWORD",
            "apiKey",
            "api_key",
            "API-Key",
            "account_id",
            "accounts",
            "user_name",
            "Email",
            "session_token",
            "client_id",
        ] {
            assert!(is_sensitive_key(k), "{k}");
        }
        for k in [
            "recents",
            "favourites",
            "bar_px",
            "grouping",
            "name",
            "symbol",
            "product",
        ] {
            assert!(!is_sensitive_key(k), "{k}");
        }
    }

    #[test]
    fn json_values_under_sensitive_keys_are_replaced_whatever_their_type() {
        let v: Value = serde_json::from_str(
            r#"{"name":"desk","recents":[2,1],"rithmic":{"user":"trader7","password":"hunter2","accounts":["APEX-1234",{"id":"APEX-5678"}]},"api_key":12345,"note":"mail me at a.b@example.com"}"#,
        )
        .unwrap();
        let r = json(&v);
        let s = r.to_string();
        for secret in [
            "trader7",
            "hunter2",
            "APEX-1234",
            "APEX-5678",
            "12345",
            "a.b@example.com",
        ] {
            assert!(!s.contains(secret), "{secret} survived: {s}");
        }
        assert_eq!(r["name"], "desk");
        assert_eq!(r["recents"], serde_json::json!([2, 1]));
    }

    #[test]
    fn text_secrets_are_replaced_and_the_rest_kept() {
        let cases = [
            (
                "logon user=trader7 password=hunter2 ok",
                &["trader7", "hunter2"][..],
            ),
            (r#"{"password": "hunter2", "symbol": "ES"}"#, &["hunter2"]),
            ("Authorization: Bearer abc", &["Bearer"]),
            ("api_key='k-123' then", &["k-123"]),
            (
                "sent to someone@example.org today",
                &["someone@example.org"],
            ),
            (
                "key 9f8e7d6c5b4a39281706f5e4d3c2b1a0 used",
                &["9f8e7d6c5b4a39281706f5e4d3c2b1a0"],
            ),
            ("account_id: APEX-1234; done", &["APEX-1234"]),
        ];
        for (line, secrets) in cases {
            let r = text(line);
            for s in secrets {
                assert!(!r.contains(s), "{s} survived in {r:?}");
            }
            assert!(r.contains(REDACTED) || r.contains("[redacted-"), "{r:?}");
        }
        // Ordinary log lines are untouched.
        let plain =
            "feed ESZ26 live: 5000 updates/s, gaps 0, reconnects 2 (wss://example.invalid/ws)";
        assert_eq!(text(plain), plain);
    }
}
