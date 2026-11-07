//! Redaction for anything that leaves the app in a diagnostics bundle, and for every log line
//! before it reaches the disk (roadmap U1).
//!
//! Lines never logs credentials, account ids, API keys or level values from paid sources on
//! purpose; this is the second line of defence:
//! - JSON (settings, workspaces): the value of any key that names a secret or an identity
//!   (password, token, API key, account, user, email, …) is replaced, whatever its type;
//!   maps keyed by account (`guardrails`, `bracket_presets`, `prop_rules`) get their keys
//!   replaced too, and every other key is redacted as text.
//! - Text (logs): `key=value` and `key: value` for those keys (an `Authorization` header to
//!   the end of its line), e-mail addresses, long token-like strings and the user's name in
//!   home-folder paths are replaced.
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

/// Keys that are sensitive only as a whole word (as fragments they'd match ordinary keys,
/// "hotkey", "design"): URL signing parameters and API keys.
const SENSITIVE_EXACT: &[&str] = &["key", "sig", "signature", "nonce"];

/// Endings that mark a signing or API-key name (`access_key`, `X-Amz-Signature`,
/// `API-Sign`), and the ordinary words that share them.
const SENSITIVE_ENDINGS: &[&str] = &["key", "sig", "sign", "signature"];
const NOT_SECRET: &[&str] = &["hotkey", "design", "monkey", "turnkey", "assign"];

/// Authorization schemes: the word after one is a credential wherever it appears.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "digest"];

/// Settings maps keyed by account name or id: their keys are identities.
const ACCOUNT_KEYED: &[&str] = &["guardrails", "bracket_presets", "prop_rules"];

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
        || SENSITIVE_EXACT.contains(&k.as_str())
        || (SENSITIVE_ENDINGS.iter().any(|e| k.ends_with(e))
            && !NOT_SECRET.iter().any(|w| k.ends_with(w)))
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
                    } else if ACCOUNT_KEYED.contains(&k.as_str())
                        && let Value::Object(by_account) = v
                    {
                        // One entry per account, numbered so they stay apart.
                        Value::Object(
                            by_account
                                .values()
                                .enumerate()
                                .map(|(i, v)| (format!("[redacted-account-{}]", i + 1), json(v)))
                                .collect(),
                        )
                    } else {
                        json(v)
                    };
                    (text(k), v)
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
    let s = &home_paths(s);
    // 1. Whole whitespace-separated chunks that are e-mail addresses or tokens.
    let mut chunks = String::with_capacity(s.len());
    // The previous word was an authorization scheme ("Bearer"): this one is its credential.
    let mut after_scheme = false;
    for piece in s.split_inclusive(char::is_whitespace) {
        let (word, ws) = piece.split_at(piece.trim_end_matches(char::is_whitespace).len());
        let core = word.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
        });
        let credential = after_scheme && !core.is_empty();
        after_scheme = AUTH_SCHEMES.contains(&core.to_ascii_lowercase().as_str());
        if credential {
            chunks.push_str(&word.replace(core, REDACTED));
        } else if looks_like_email(core) {
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
        // Optional closing quote of a JSON key (escaped, in a logged JSON string), then
        // spaces.
        if j + 1 < chars.len() && chars[j] == '\\' && matches!(chars[j + 1], '"' | '\'') {
            j += 2;
        } else if j < chars.len() && matches!(chars[j], '"' | '\'') {
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
        // The value: quoted, or up to whitespace or a delimiter (an Authorization header's
        // scheme and credentials: to the end of the line).
        let mut k = j;
        // An escaped opening quote (a logged JSON string) counts as the quote.
        if k + 1 < chars.len() && chars[k] == '\\' && matches!(chars[k + 1], '"' | '\'') {
            k += 1;
        }
        let quoted = k < chars.len() && matches!(chars[k], '"' | '\'');
        if normalise(&word).contains("authorization") && !quoted {
            while k < chars.len() && !matches!(chars[k], '\n' | '\r') {
                k += 1;
            }
        } else if quoted {
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

/// The user's name in home-folder paths (`C:\Users\<name>\`, `/Users/<name>/`,
/// `/home/<name>/`, any case, any slashes, escaped or not) replaced by `[redacted-user]`. A
/// name may hold spaces ("John Smith"): it runs to the next slash on the line, else to the
/// next space.
fn home_paths(s: &str) -> String {
    const SEP: [char; 2] = ['/', '\\'];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'scan: while !rest.is_empty() {
        if out.ends_with(SEP) {
            for root in ["users", "home"] {
                let Some(head) = rest.get(..root.len()) else {
                    continue;
                };
                if !head.eq_ignore_ascii_case(root) {
                    continue;
                }
                let after = &rest[root.len()..];
                let seps = after.len() - after.trim_start_matches(SEP).len();
                let name = &after[seps..];
                // The line's end, or a quote closing the path.
                let end = name.find(['"', '\'', '\n', '\r']).unwrap_or(name.len());
                let name_len = match name[..end].find(SEP) {
                    Some(n) => n,
                    None => name[..end].find(char::is_whitespace).unwrap_or(end),
                };
                if seps > 0 && name_len > 0 {
                    out.push_str(head);
                    out.push_str(&after[..seps]);
                    out.push_str("[redacted-user]");
                    rest = &name[name_len..];
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().expect("not empty");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys() {
        for k in [
            "access_key",
            "X-Amz-Signature",
            "API-Sign",
            "nonce",
            "sig",
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
            "hotkey",
            "global_hotkey",
            "design",
            "redesign",
            "assign",
            "monkey",
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
            ("Authorization: Bearer abc", &["Bearer", "abc"]),
            (
                "GET /v1/feed?key=k9secret&sig=s1gned&n=2 then Authorization: Basic dXNlcjpwYXNz",
                &["k9secret", "s1gned", "dXNlcjpwYXNz"],
            ),
            (r"log dir C:\Users\jsmith\AppData\Local\lines", &["jsmith"]),
            (r#"path "C:\\Users\\jsmith\\AppData""#, &["jsmith"]),
            (
                "saved /Users/jsmith/Library/x and /home/jsmith/.config/y",
                &["jsmith"],
            ),
            (
                r"log_dir C:\Users\John Smith\AppData\Local\lines and C:\USERS\bob\x",
                &["John", "Smith", "bob"],
            ),
            (
                "Proxy-Authorization: Bearer prx-cred then X-Auth: Basic dXNlcjpwYXNz",
                &["prx-cred", "dXNlcjpwYXNz"],
            ),
            ("Proxy-Authorization:Basic cHJveHk6cHc=", &["cHJveHk6cHc="]),
            (
                r#"body="{\"authorization\":\"Bearer esc-cred\",\"password\":\"hunter2\"}""#,
                &["esc-cred", "hunter2"],
            ),
            (
                "GET /o?X-Amz-Signature=amzsig&access_key=ak1 API-Sign: krsig",
                &["amzsig", "ak1", "krsig"],
            ),
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
        // Other folders and ordinary words are kept.
        let kept = "store /data/lines/users.idx, monkey hotkey design global_hotkey=ctrl-k redesign: x assign=1";
        assert_eq!(text(kept), kept);
    }

    /// Maps keyed by account lose their keys, numbered; nothing else is renamed.
    #[test]
    fn account_keyed_maps_lose_their_keys() {
        let v: Value = serde_json::from_str(
            r#"{"guardrails":{"PA-123456":{"max_trades_per_day":3},"Rithmic 50K":{}},"bracket_presets":{"PA-123456":{}},"prop_rules":{"PA-9":{"preset":"50K"}},"drawing_styles":{"trend":{}}}"#,
        )
        .unwrap();
        let s = json(&v).to_string();
        for id in ["PA-123456", "Rithmic 50K", "PA-9"] {
            assert!(!s.contains(id), "{id} survived: {s}");
        }
        assert!(s.contains("[redacted-account-2]") && s.contains("max_trades_per_day"));
        assert!(s.contains("\"trend\""), "{s}");
    }
}
