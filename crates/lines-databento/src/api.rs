//! Databento's historical HTTP API: the free cost check, and the download it guards.
//!
//! `POST https://hist.databento.com/v0/<method>`, form-encoded, HTTP Basic auth with the API
//! key as the user name and no password (as the official client does). Blocking `ureq` with
//! rustls + `ring` (no aws-lc, no OpenSSL). The key goes only into the `Authorization` header:
//! never into a URL, a log line or an error message.
//!
//! - `metadata.get_cost` → US dollars (free to call);
//! - `metadata.get_billable_size` → bytes (uncompressed DBN, what Databento bills);
//! - `timeseries.get_range` → the data itself, DBN + zstd, streamed to a file.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::key::ApiKey;

pub const BASE: &str = "https://hist.databento.com/v0/";
pub const DATASET: &str = "GLBX.MDP3";

/// What to price or fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub dataset: String,
    pub schema: String,
    pub symbols: Vec<String>,
    /// `raw_symbol` (ESZ6), `parent` (ES.FUT), `continuous` (ES.v.0), `instrument_id`.
    pub stype_in: String,
    /// Inclusive start, ISO 8601 (`2026-11-02` or `2026-11-02T13:30`), UTC.
    pub start: String,
    /// Exclusive end, same format.
    pub end: String,
}

pub const SCHEMAS: &[&str] = &["definition", "trades", "mbp-10"];

impl Query {
    pub fn validate(&self) -> Result<()> {
        if !SCHEMAS.contains(&self.schema.as_str()) {
            bail!(
                "schema {:?}: the importer reads {}",
                self.schema,
                SCHEMAS.join(", ")
            );
        }
        if self.symbols.is_empty() || self.symbols.iter().any(|s| s.trim().is_empty()) {
            bail!("no symbols");
        }
        if self.symbols.len() > 2000 {
            bail!("at most 2000 symbols per request");
        }
        for (name, v) in [("start", &self.start), ("end", &self.end)] {
            let date = v.get(..10).unwrap_or("");
            let ok = date.len() == 10
                && date.as_bytes()[4] == b'-'
                && date.as_bytes()[7] == b'-'
                && date
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
            if !ok {
                bail!("{name} {v:?}: use YYYY-MM-DD or an ISO 8601 time (UTC)");
            }
        }
        if self.end <= self.start {
            bail!("end must be after start (end is exclusive)");
        }
        Ok(())
    }

    /// The form fields shared by every method.
    pub fn form(&self) -> Vec<(&'static str, String)> {
        vec![
            ("dataset", self.dataset.clone()),
            ("schema", self.schema.clone()),
            ("symbols", self.symbols.join(",")),
            ("stype_in", self.stype_in.clone()),
            ("start", self.start.clone()),
            ("end", self.end.clone()),
        ]
    }
}

/// `Authorization: Basic base64(key + ":")`.
pub fn basic_auth(key: &ApiKey) -> String {
    format!("Basic {}", base64(format!("{}:", key.secret()).as_bytes()))
}

/// Standard base64 with padding (RFC 4648), enough for one header.
pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("lines-databento/", env!("CARGO_PKG_VERSION")))
        // Read Databento's error bodies ourselves (they say what was wrong).
        .http_status_as_error(false)
        .build()
        .into()
}

/// POST a method; `Ok(body)` on 2xx, else an error with Databento's message (never the key).
fn post(
    key: &ApiKey,
    method: &str,
    form: &[(&'static str, String)],
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>> {
    let resp = agent(timeout)
        .post(format!("{BASE}{method}"))
        .header("Authorization", basic_auth(key))
        .send_form(form.iter().map(|(k, v)| (*k, v.as_str())))
        .with_context(|| format!("POST {method}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let mut resp = resp;
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    bail!("{method}: HTTP {status}: {}", error_message(&body))
}

/// Databento's error body is JSON with a `detail` (string or object with `message`).
pub fn error_message(body: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.chars().take(300).collect(),
    };
    match &v["detail"] {
        serde_json::Value::String(s) => s.clone(),
        d if d["message"].is_string() => d["message"].as_str().unwrap_or("").to_string(),
        _ => body.chars().take(300).collect(),
    }
}

/// Parse `metadata.get_cost`'s body: a JSON number of US dollars.
pub fn parse_cost(body: &str) -> Result<f64> {
    let v: f64 = body
        .trim()
        .parse()
        .context("cost response is not a number")?;
    if !v.is_finite() || v < 0.0 {
        bail!("cost response {v} is not a price");
    }
    Ok(v)
}

/// Parse `metadata.get_billable_size`'s body: a JSON integer of bytes.
pub fn parse_size(body: &str) -> Result<u64> {
    body.trim()
        .parse()
        .context("size response is not an integer")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub usd: f64,
    pub bytes: u64,
}

/// The free cost check (nothing is downloaded or billed).
pub fn estimate(key: &ApiKey, q: &Query) -> Result<Estimate> {
    q.validate()?;
    let form = q.form();
    let t = Duration::from_secs(60);
    let usd = parse_cost(
        &post(key, "metadata.get_cost", &form, t)?
            .body_mut()
            .read_to_string()?,
    )?;
    let bytes = parse_size(
        &post(key, "metadata.get_billable_size", &form, t)?
            .body_mut()
            .read_to_string()?,
    )?;
    Ok(Estimate { usd, bytes })
}

/// Download `q` to `out` (DBN + zstd), only if the cost check is within `max_usd`.
/// Written to `<out>.part` and renamed when complete.
pub fn download(key: &ApiKey, q: &Query, max_usd: f64, out: &Path) -> Result<(Estimate, u64)> {
    let est = estimate(key, q)?;
    if est.usd > max_usd {
        bail!(
            "{} {}: costs ${:.4} ({}), more than --max-usd {max_usd}: nothing downloaded",
            q.schema,
            q.symbols.join(","),
            est.usd,
            human_bytes(est.bytes)
        );
    }
    let mut form = q.form();
    form.push(("encoding", "dbn".into()));
    form.push(("compression", "zstd".into()));
    let resp = post(
        key,
        "timeseries.get_range",
        &form,
        Duration::from_secs(3600),
    )?;
    let part = out.with_extension("part");
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?,
    );
    let mut body = resp.into_body().into_reader();
    let n = std::io::copy(&mut body, &mut file).context("downloading")?;
    file.flush()?;
    drop(file);
    std::fs::rename(&part, out)?;
    Ok((est, n))
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Query {
        Query {
            dataset: DATASET.into(),
            schema: "mbp-10".into(),
            symbols: vec!["ESZ6".into(), "NQZ6".into()],
            stype_in: "raw_symbol".into(),
            start: "2026-11-02".into(),
            end: "2026-11-05".into(),
        }
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn basic_auth_is_the_key_and_an_empty_password() {
        let k = ApiKey::new("db-key".into()).unwrap();
        assert_eq!(basic_auth(&k), format!("Basic {}", base64(b"db-key:")));
    }

    #[test]
    fn queries_are_validated_and_formed() {
        assert!(q().validate().is_ok());
        assert_eq!(
            q().form(),
            vec![
                ("dataset", "GLBX.MDP3".to_string()),
                ("schema", "mbp-10".into()),
                ("symbols", "ESZ6,NQZ6".into()),
                ("stype_in", "raw_symbol".into()),
                ("start", "2026-11-02".into()),
                ("end", "2026-11-05".into()),
            ]
        );
        let mut bad = q();
        bad.schema = "mbo".into();
        assert!(bad.validate().is_err());
        let mut bad = q();
        bad.end = "2026-11-02".into();
        assert!(bad.validate().is_err(), "end is exclusive");
        let mut bad = q();
        bad.start = "11/02/2026".into();
        assert!(bad.validate().is_err());
        let mut bad = q();
        bad.symbols.clear();
        assert!(bad.validate().is_err());
        let mut ok = q();
        ok.start = "2026-11-02T13:30".into();
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn responses_parse() {
        assert_eq!(parse_cost("2.587353944778").unwrap(), 2.587353944778);
        assert!(parse_cost("-1").is_err());
        assert!(parse_cost("{\"detail\":\"x\"}").is_err());
        assert_eq!(parse_size("1234567\n").unwrap(), 1_234_567);
        assert_eq!(
            error_message(r#"{"detail":"Authentication failed."}"#),
            "Authentication failed."
        );
        assert_eq!(
            error_message(r#"{"detail":{"case":"x","message":"Invalid symbol"}}"#),
            "Invalid symbol"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(12), "12 B");
    }
}
