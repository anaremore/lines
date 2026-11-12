//! A level source's setting values (decision 0021: settings are data): where a value comes
//! from, how typed text is checked, how − / + step it, and how it reads. Pure; shared by the
//! app's two settings surfaces (the Studies menu's level editor and Settings › Plugins) and by
//! sources that want the same rules.
//!
//! Values are text ([`super::SettingValues`]), stored as their canonical form:
//! - [`SettingKind::Bool`]: `"true"` / `"false"`;
//! - [`SettingKind::Seconds`]: whole seconds (`"82800"`), shown as "23 h";
//! - [`SettingKind::Number`]: the decimal with exactly its places (`"0.50"`), or `""` when
//!   optional and empty;
//! - [`SettingKind::Url`]: an `http://` or `https://` address, trimmed;
//! - [`SettingKind::Choice`] and [`SettingKind::Text`]: the text, trimmed.

use super::{SettingKind, SettingSpec};

/// Where a value comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Set in Lines (saved in its settings).
    Saved,
    /// The source's default from outside Lines (its environment):
    /// [`super::LevelSource::outside_default`].
    Outside,
    /// The spec's default.
    Default,
}

/// The value in force: a saved one, else the source's outside default, else the spec's.
pub fn effective(
    spec: &SettingSpec,
    saved: Option<&str>,
    outside: Option<&str>,
) -> (String, Origin) {
    match (saved, outside) {
        (Some(v), _) => (v.to_owned(), Origin::Saved),
        (None, Some(v)) => (v.to_owned(), Origin::Outside),
        (None, None) => (spec.default.to_owned(), Origin::Default),
    }
}

/// `value` as shown: seconds in their unit ("23 h"), a number with its places and unit
/// ("0.50", "12.25 pt"), a switch as "On"/"Off", a choice with an initial capital; an empty
/// optional number as `""` (the caller shows the placeholder).
pub fn display(spec: &SettingSpec, value: &str) -> String {
    match spec.kind {
        SettingKind::Bool => if is_on(value) { "On" } else { "Off" }.to_owned(),
        SettingKind::Seconds { .. } => value
            .trim()
            .parse::<u32>()
            .map_or_else(|_| value.to_owned(), seconds_text),
        SettingKind::Number { decimals, unit, .. } => match parse_decimal(value, decimals) {
            Some(Ok(n)) => with_unit(&decimal_text(n, decimals), unit),
            Some(Err(_)) => value.to_owned(),
            None => String::new(),
        },
        SettingKind::Choice(_) => choice_label(value),
        SettingKind::Secret => String::new(),
        SettingKind::Text | SettingKind::Url => value.to_owned(),
    }
}

/// A choice's label: its text with an initial capital ("auto" → "Auto"; "ES" stays).
pub fn choice_label(value: &str) -> String {
    let mut c = value.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Whether a switch's value is on.
pub fn is_on(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
}

/// Typed `text` for `spec` as its stored form, or why it is refused (a sentence, for the
/// field's inline reason). `choices`: a choice's options now (`None`: the spec's own list).
/// Never clamps: a value out of range is refused.
pub fn parse(spec: &SettingSpec, text: &str, choices: Option<&[String]>) -> Result<String, String> {
    let t = text.trim();
    match spec.kind {
        SettingKind::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Ok("true".into()),
            "false" | "off" | "no" => Ok("false".into()),
            _ => Err("On or off".into()),
        },
        SettingKind::Seconds { min, max } => {
            let s = parse_seconds(t)
                .ok_or_else(|| "A time: seconds, or with a unit (90 s, 5 min, 2 h)".to_owned())?;
            if s < min || s > max {
                return Err(format!(
                    "From {} to {}",
                    seconds_text(min),
                    seconds_text(max)
                ));
            }
            Ok(s.to_string())
        }
        SettingKind::Number {
            min,
            max,
            decimals,
            unit,
            optional,
            ..
        } => match parse_decimal(t, decimals) {
            None if optional => Ok(String::new()),
            None => Err("Needs a value".into()),
            Some(Err(why)) => Err(why),
            Some(Ok(n)) if n < min || n > max => Err(format!(
                "From {} to {}",
                with_unit(&decimal_text(min, decimals), unit),
                with_unit(&decimal_text(max, decimals), unit)
            )),
            Some(Ok(n)) => Ok(decimal_text(n, decimals)),
        },
        SettingKind::Url => {
            if t.is_empty() {
                return Err("Needs an address".into());
            }
            let rest = t
                .strip_prefix("http://")
                .or_else(|| t.strip_prefix("https://"))
                .ok_or_else(|| "An http:// or https:// address".to_owned())?;
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            if host.is_empty() || host.contains(char::is_whitespace) {
                return Err("An http:// or https:// address with a host".into());
            }
            Ok(t.to_owned())
        }
        SettingKind::Choice(own) => {
            let ok = match choices {
                Some(c) => c.iter().any(|c| c.eq_ignore_ascii_case(t)),
                None => own.iter().any(|c| c.eq_ignore_ascii_case(t)),
            };
            if ok {
                Ok(t.to_owned())
            } else {
                Err(format!("Not one of the choices: {t}"))
            }
        }
        SettingKind::Text => Ok(t.to_owned()),
        SettingKind::Secret => Err("Secrets are never set here".into()),
    }
}

/// `value` one step down or up (− / +), within range, as its stored form; `None` at an end
/// (or for a kind without steps). Seconds step in the unit shown (1 s under a minute, 1 min
/// under an hour, 1 h from there); a number by its step, onto the step's grid. An empty
/// optional number steps up to its least value (or one step, if that is above 0).
pub fn step(spec: &SettingSpec, value: &str, up: bool) -> Option<String> {
    match spec.kind {
        SettingKind::Seconds { min, max } => {
            let s = value.trim().parse::<u32>().ok()?.clamp(min, max);
            let next = if up {
                let u = if s < 60 {
                    1
                } else if s < 3600 {
                    60
                } else {
                    3600
                };
                (s / u + 1) * u
            } else {
                let u = if s <= 60 {
                    1
                } else if s <= 3600 {
                    60
                } else {
                    3600
                };
                if s % u == 0 { s - u } else { s - s % u }
            };
            let next = next.clamp(min, max);
            (next != s).then(|| next.to_string())
        }
        SettingKind::Number {
            min,
            max,
            step,
            decimals,
            ..
        } => {
            let step = step.max(1);
            let next = match parse_decimal(value, decimals) {
                None if up => min.max(step.min(max)).max(min),
                None => return None,
                Some(Err(_)) => return None,
                Some(Ok(n)) => {
                    let below = n.div_euclid(step) * step;
                    if up {
                        below + step
                    } else if below == n {
                        n - step
                    } else {
                        below
                    }
                }
            };
            let now = parse_decimal(value, decimals).and_then(Result::ok);
            if next < min || next > max {
                return None;
            }
            (Some(next) != now).then(|| decimal_text(next, decimals))
        }
        _ => None,
    }
}

/// Whole seconds as they read best: "45 s", "5 min", "1 h 30 min", "23 h", "2 d".
pub fn seconds_text(s: u32) -> String {
    match s {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 && s % 60 == 0 => format!("{} min", s / 60),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s if s % 86_400 == 0 => format!("{} d", s / 86_400),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// Typed seconds: a whole number with an optional unit (s, min, m, h, d).
fn parse_seconds(t: &str) -> Option<u32> {
    let t = t.trim().to_ascii_lowercase();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult: u64 = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };
    u32::try_from(n.checked_mul(mult)?).ok()
}

/// A decimal in units of `10^-decimals`: `None` when empty, `Some(Err)` when not a number or
/// with more places than it has.
fn parse_decimal(t: &str, decimals: u8) -> Option<Result<i64, String>> {
    let t = t.trim().replace('\u{2212}', "-");
    if t.is_empty() {
        return None;
    }
    let bad = || Err::<i64, String>("A number".to_owned());
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(&t)),
    };
    let (whole, frac) = body.split_once('.').unwrap_or((body, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Some(bad());
    }
    let frac = frac.trim_end_matches('0');
    if frac.len() > usize::from(decimals) {
        return Some(Err(match decimals {
            0 => "A whole number".to_owned(),
            1 => "One decimal place at most".to_owned(),
            d => format!("{d} decimal places at most"),
        }));
    }
    let scale = 10i64.checked_pow(u32::from(decimals))?;
    let w: i64 = if whole.is_empty() {
        0
    } else {
        match whole.parse() {
            Ok(w) => w,
            Err(_) => return Some(bad()),
        }
    };
    let f: i64 = if frac.is_empty() {
        0
    } else {
        frac.parse::<i64>().ok()? * 10i64.pow(u32::from(decimals) - frac.len() as u32)
    };
    let n = w.checked_mul(scale).and_then(|v| v.checked_add(f));
    Some(match n {
        Some(n) => Ok(if neg { -n } else { n }),
        None => bad(),
    })
}

/// `n` units of `10^-decimals` as text with exactly that many places (a true minus sign is the
/// display's business: stored values use `-`).
fn decimal_text(n: i64, decimals: u8) -> String {
    if decimals == 0 {
        return n.to_string();
    }
    let scale = 10u64.pow(u32::from(decimals));
    let a = n.unsigned_abs();
    format!(
        "{}{}.{:0w$}",
        if n < 0 { "-" } else { "" },
        a / scale,
        a % scale,
        w = usize::from(decimals)
    )
}

fn with_unit(n: &str, unit: &str) -> String {
    if unit.is_empty() {
        n.to_owned()
    } else {
        format!("{n} {unit}")
    }
}

/// Whether `url`'s host is this machine (`localhost`, `127.x.x.x`, `[::1]`): what a source
/// that refuses remote services checks ([`super::LevelSource::check`]).
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .trim()
        .strip_prefix("http://")
        .or_else(|| url.trim().strip_prefix("https://"))
        .unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Without credentials and port: `user@host:port`, `[::1]:80`.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: SettingKind) -> SettingSpec {
        SettingSpec::new("k", "K", kind, "", "")
    }

    const MULT: SettingKind = SettingKind::Number {
        min: 5,
        max: 1000,
        step: 5,
        decimals: 2,
        unit: "",
        optional: false,
    };
    const RI: SettingKind = SettingKind::Number {
        min: 25,
        max: 100_000,
        step: 25,
        decimals: 2,
        unit: "pt",
        optional: true,
    };

    #[test]
    fn numbers_are_exact_and_never_clamped() {
        let m = spec(MULT);
        assert_eq!(parse(&m, "0.5", None), Ok("0.50".into()));
        assert_eq!(parse(&m, " 1 ", None), Ok("1.00".into()));
        assert_eq!(parse(&m, "2.000", None), Ok("2.00".into()));
        assert!(parse(&m, "0.505", None).is_err(), "more places than it has");
        assert_eq!(parse(&m, "0.01", None), Err("From 0.05 to 10.00".into()));
        assert_eq!(parse(&m, "", None), Err("Needs a value".into()));
        assert!(parse(&m, "abc", None).is_err());
        let r = spec(RI);
        assert_eq!(parse(&r, "", None), Ok(String::new()), "optional: empty");
        assert_eq!(parse(&r, "12.25", None), Ok("12.25".into()));
        assert_eq!(
            parse(&r, "\u{2212}4", None),
            Err("From 0.25 pt to 1000.00 pt".into())
        );
        assert_eq!(display(&r, "12.25"), "12.25 pt");
        assert_eq!(display(&r, ""), "");
        assert_eq!(display(&m, "1"), "1.00");
    }

    #[test]
    fn numbers_step_on_their_grid() {
        let m = spec(MULT);
        assert_eq!(step(&m, "0.50", true), Some("0.55".into()));
        assert_eq!(step(&m, "0.50", false), Some("0.45".into()));
        assert_eq!(step(&m, "0.52", true), Some("0.55".into()), "onto the grid");
        assert_eq!(step(&m, "0.52", false), Some("0.50".into()));
        assert_eq!(step(&m, "0.05", false), None, "its least");
        assert_eq!(step(&m, "10.00", true), None, "its most");
        let r = spec(RI);
        assert_eq!(
            step(&r, "", true),
            Some("0.25".into()),
            "empty: up to its least"
        );
        assert_eq!(step(&r, "", false), None);
        assert_eq!(step(&r, "12.25", true), Some("12.50".into()));
    }

    #[test]
    fn seconds_read_and_step_in_their_unit() {
        let s = spec(SettingKind::Seconds {
            min: 1,
            max: 604_800,
        });
        assert_eq!(display(&s, "82800"), "23 h");
        assert_eq!(display(&s, "1"), "1 s");
        assert_eq!(display(&s, "300"), "5 min");
        assert_eq!(display(&s, "5400"), "1 h 30 min");
        assert_eq!(display(&s, "604800"), "7 d");
        assert_eq!(step(&s, "59", true), Some("60".into()));
        assert_eq!(step(&s, "60", true), Some("120".into()));
        assert_eq!(step(&s, "60", false), Some("59".into()));
        assert_eq!(step(&s, "3600", true), Some("7200".into()));
        assert_eq!(step(&s, "3600", false), Some("3540".into()));
        assert_eq!(step(&s, "82800", false), Some("79200".into()));
        assert_eq!(step(&s, "1", false), None);
        assert_eq!(parse(&s, "5m", None), Ok("300".into()));
        assert_eq!(parse(&s, "23 h", None), Ok("82800".into()));
        assert_eq!(parse(&s, "90", None), Ok("90".into()));
        assert_eq!(parse(&s, "8d", None), Err("From 1 s to 7 d".into()));
        assert!(parse(&s, "soon", None).is_err());
    }

    #[test]
    fn urls_switches_and_choices() {
        let u = spec(SettingKind::Url);
        assert_eq!(
            parse(&u, " http://127.0.0.1:7878 ", None),
            Ok("http://127.0.0.1:7878".into())
        );
        assert!(parse(&u, "127.0.0.1:7878", None).is_err());
        assert!(parse(&u, "http://", None).is_err());
        let b = spec(SettingKind::Bool);
        assert_eq!(parse(&b, "On", None), Ok("true".into()));
        assert_eq!(display(&b, "true"), "On");
        assert_eq!(display(&b, "false"), "Off");
        let c = spec(SettingKind::Choice(&["auto"]));
        assert_eq!(parse(&c, "auto", None), Ok("auto".into()));
        assert!(parse(&c, "ES", None).is_err());
        let listed = vec!["auto".to_owned(), "ES".to_owned()];
        assert_eq!(parse(&c, "ES", Some(&listed)), Ok("ES".into()));
        assert_eq!(display(&c, "auto"), "Auto");
    }

    #[test]
    fn where_a_value_comes_from() {
        let s = SettingSpec::new("k", "K", SettingKind::Text, "d", "");
        assert_eq!(
            effective(&s, Some("a"), Some("b")),
            ("a".into(), Origin::Saved)
        );
        assert_eq!(
            effective(&s, None, Some("b")),
            ("b".into(), Origin::Outside)
        );
        assert_eq!(effective(&s, None, None), ("d".into(), Origin::Default));
    }

    #[test]
    fn loopback_addresses() {
        for u in [
            "http://127.0.0.1:7878",
            "http://localhost/x",
            "https://[::1]:8443",
            "http://127.4.5.6",
        ] {
            assert!(is_loopback_url(u), "{u}");
        }
        for u in [
            "http://100.71.4.20:7878",
            "http://example.com",
            "http://127.0.0.1.evil.com",
        ] {
            assert!(!is_loopback_url(u), "{u}");
        }
    }

    #[test]
    fn a_spec_built_with_setters() {
        const S: SettingSpec = SettingSpec::new("k", "K", SettingKind::Bool, "false", "h")
            .group("G")
            .confirm("Sure?")
            .connection();
        assert_eq!((S.group, S.confirm, S.connection), ("G", "Sure?", true));
        assert!(S.shown());
        assert!(
            !SettingSpec::new("k", "K", SettingKind::Bool, "", "")
                .owned_by_lines()
                .shown()
        );
        assert!(!SettingSpec::new("k", "K", SettingKind::Secret, "", "").shown());
    }
}
