//! Plugins' settings: one model for both places they are set, the Studies menu's level editor
//! ("Source settings") and Settings › Plugins. A plugin describes its settings as data
//! (`lines_feed::levels::SettingSpec`); Lines draws every one itself from a closed set of
//! kinds, so nothing here (or in either view) names a plugin. A level source is the first
//! kind of plugin.
//!
//! - **App-wide:** a source's values apply to every chart it serves (one shared feed task per
//!   source and market), so they live in `settings.json` (`Settings::plugins`), never in a
//!   workspace or on a pane.
//! - **Saved:** only values that differ from the default, by source id; a plugin that isn't
//!   installed in this build keeps its values untouched (reinstalling restores them). Secrets
//!   are never set here, so never saved.
//! - **Checked:** each value by its kind (`setting::parse`: never clamped), then all of them
//!   together by the source (`LevelSource::check`, e.g. a remote address while remote access is
//!   off). A refused value applies nothing: it stays in its field with the reason, and is
//!   tried again whenever another of the source's settings changes (turning remote access on
//!   applies the address it refused).
//! - **Applied live:** `levels::runtime::set_settings` (running tasks take it on their next
//!   tick). An outside default (`LevelSource::outside_default`, e.g. an environment variable)
//!   sits under the saved value and is marked where shown.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lines_feed::levels::{
    self, LevelSource, SettingKind, SettingSpec, SettingValues,
    setting::{self, Origin},
};

/// Each plugin's saved values, by plugin id (`Settings::plugins`).
pub type Saved = BTreeMap<String, SettingValues>;

/// A value that was refused: what was typed or chosen, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub text: String,
    pub reason: String,
}

/// Refused values by (plugin, key): shown in both places until they apply or are dropped.
fn refused() -> &'static Mutex<BTreeMap<(String, String), Refused>> {
    static R: Mutex<BTreeMap<(String, String), Refused>> = Mutex::new(BTreeMap::new());
    &R
}

fn refused_of(source: &str, key: &str) -> Option<Refused> {
    refused()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(source.to_owned(), key.to_owned()))
        .cloned()
}

/// Drop a refused value (its field's Esc): the value in force shows again.
pub fn discard(source: &str, key: &str) {
    refused()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(source.to_owned(), key.to_owned()));
}

/// One setting as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub spec: SettingSpec,
    /// The value in force, stored form.
    pub value: String,
    /// As shown ("23 h", "On"); empty: the placeholder.
    pub shown: String,
    pub origin: Origin,
    /// A choice's options now, `(value, label)`.
    pub options: Vec<(String, String)>,
    pub refused: Option<Refused>,
}

impl Row {
    /// The text a field shows: a refused value's own text, else the value as shown.
    pub fn text(&self) -> &str {
        match &self.refused {
            Some(r) => &r.text,
            None => &self.shown,
        }
    }

    /// The label of the choice in force.
    pub fn choice_label(&self) -> String {
        self.options
            .iter()
            .find(|(v, _)| v.eq_ignore_ascii_case(&self.value))
            .map_or_else(|| setting::choice_label(&self.value), |(_, l)| l.clone())
    }

    /// Why it was refused, and what is still in force: "From 1 s to 1 h · still 5 s".
    pub fn refused_line(&self) -> Option<String> {
        let f = self.refused.as_ref()?;
        let now = if self.shown.is_empty() {
            self.spec.placeholder.to_owned()
        } else {
            self.shown.clone()
        };
        Some(if now.is_empty() {
            f.reason.clone()
        } else {
            format!("{} · still {now}", f.reason.trim_end_matches('.'))
        })
    }

    /// − / + from the value in force (`None` at an end, or for a kind without steps).
    pub fn step(&self, up: bool) -> Option<String> {
        setting::step(&self.spec, &self.value, up)
    }

    /// Whether setting `value` asks first (a switch with a question, turning on).
    pub fn asks(&self, value: &str) -> bool {
        !self.spec.confirm.is_empty()
            && self.spec.kind == SettingKind::Bool
            && setting::is_on(value)
            && !setting::is_on(&self.value)
    }
}

/// The installed plugins that have settings Lines shows, in registration order.
pub fn plugins() -> Vec<Arc<dyn LevelSource>> {
    levels::registered()
        .into_iter()
        .filter(|s| s.settings().iter().any(SettingSpec::shown))
        .collect()
}

/// A choice's options now: the source's (what it knows now), else the spec's own list.
pub fn options(source: &dyn LevelSource, spec: &SettingSpec) -> Vec<(String, String)> {
    match spec.kind {
        SettingKind::Choice(own) => source.choices(spec.key).unwrap_or_else(|| {
            own.iter()
                .map(|v| ((*v).to_owned(), setting::choice_label(v)))
                .collect()
        }),
        _ => Vec::new(),
    }
}

/// Every value in force (saved, else outside default, else default): what `check` sees.
fn in_force(source: &dyn LevelSource, saved: &SettingValues) -> SettingValues {
    source
        .settings()
        .iter()
        .filter(|s| s.kind != SettingKind::Secret)
        .map(|s| {
            let outside = source.outside_default(s.key);
            let (v, _) =
                setting::effective(s, saved.get(s.key).map(String::as_str), outside.as_deref());
            (s.key.to_owned(), v)
        })
        .collect()
}

/// The source's settings as shown (not secrets, not those Lines' own controls do).
pub fn rows(source: &dyn LevelSource, saved: Option<&SettingValues>) -> Vec<Row> {
    let empty = SettingValues::new();
    let saved = saved.unwrap_or(&empty);
    source
        .settings()
        .iter()
        .filter(|s| s.shown())
        .map(|s| {
            let outside = source.outside_default(s.key);
            let (value, origin) =
                setting::effective(s, saved.get(s.key).map(String::as_str), outside.as_deref());
            Row {
                spec: *s,
                shown: setting::display(s, &value),
                value,
                origin,
                options: options(source, s),
                refused: refused_of(source.id(), s.key),
            }
        })
        .collect()
}

/// Keep `value` for `spec` in `map` only where it isn't simply the default (a value equal to
/// the default is kept while an outside default would otherwise win).
fn store(map: &mut SettingValues, spec: &SettingSpec, value: String, outside: bool) {
    // The default in the same form as the value (a number's "1" is "1.00").
    let default =
        setting::parse(spec, spec.default, None).unwrap_or_else(|_| spec.default.to_owned());
    if value == default && !outside {
        map.remove(spec.key);
    } else {
        map.insert(spec.key.to_owned(), value);
    }
}

/// `text` for `key`: the source's saved values with it, or why it is refused (it is then kept
/// as refused, and shown with the reason). A success drops its refusal and tries the source's
/// other refused values again on top of it.
pub fn change(
    source: &dyn LevelSource,
    saved: &SettingValues,
    key: &str,
    text: &str,
) -> Result<SettingValues, String> {
    let id = source.id();
    let Some(spec) = source.settings().iter().find(|s| s.key == key && s.shown()) else {
        return Err(format!("{} has no setting {key}", source.label()));
    };
    let refuse = |reason: String| {
        refused().lock().unwrap_or_else(|e| e.into_inner()).insert(
            (id.to_owned(), key.to_owned()),
            Refused {
                text: text.to_owned(),
                reason: reason.clone(),
            },
        );
        Err(reason)
    };
    let opts: Vec<String> = options(source, spec).into_iter().map(|(v, _)| v).collect();
    let value = match setting::parse(
        spec,
        text,
        matches!(spec.kind, SettingKind::Choice(_)).then_some(&opts[..]),
    ) {
        Ok(v) => v,
        Err(why) => return refuse(why),
    };
    let mut next = saved.clone();
    store(
        &mut next,
        spec,
        value,
        source.outside_default(key).is_some(),
    );
    // The source's reasons for these values: this key's, and any new one this change causes (a
    // problem that was already there, e.g. an address from the environment, never blocks an
    // unrelated change).
    let before = source.check(&in_force(source, saved));
    let reasons = source.check(&in_force(source, &next));
    if let Some((k, why)) = reasons
        .iter()
        .find(|(k, _)| *k == key)
        .or_else(|| reasons.iter().find(|r| !before.contains(r)))
    {
        // Another setting's reason, named: this change would leave it refused.
        let why = if *k == key {
            why.clone()
        } else {
            let label = source
                .settings()
                .iter()
                .find(|s| s.key == *k)
                .map_or(*k, |s| s.label);
            format!("{label}: {why}")
        };
        return refuse(why);
    }
    discard(id, key);
    // The source's other refused values, again (in key order): any that pass now apply.
    let others: Vec<(String, Refused)> = refused()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((s, k), _)| s == id && k != key)
        .map(|((_, k), r)| (k.clone(), r.clone()))
        .collect();
    for (k, r) in others {
        let before = refused_of(id, &k);
        match change(source, &next, &k, &r.text) {
            Ok(v) => next = v,
            // Still refused: as it was (its reason may be newer).
            Err(_) if before.is_none() => discard(id, &k),
            Err(_) => {}
        }
    }
    Ok(next)
}

/// Apply `key` = `text` for plugin `id`: saved to `settings.json` and applied to every chart it
/// serves; `Err` with the reason when refused (nothing applied).
pub fn set(cx: &mut gpui::App, id: &str, key: &str, text: &str) -> Result<(), String> {
    let Some(source) = levels::registered().into_iter().find(|s| s.id() == id) else {
        return Err(format!("{id} isn't installed"));
    };
    let saved = crate::picker_view::settings(cx)
        .plugins
        .get(id)
        .cloned()
        .unwrap_or_default();
    let next = change(&*source, &saved, key, text)?;
    apply(cx, id, next);
    Ok(())
}

/// Save plugin `id`'s values and apply them live.
fn apply(cx: &mut gpui::App, id: &str, mut values: SettingValues) {
    // Never a secret, even one written into the file by hand.
    if let Some(s) = levels::registered().into_iter().find(|s| s.id() == id) {
        values.retain(|k, _| {
            s.settings()
                .iter()
                .find(|x| x.key == k.as_str())
                .is_none_or(|x| x.kind != SettingKind::Secret)
        });
    }
    levels::runtime::set_settings(id, values.clone());
    crate::picker_view::update_settings(cx, |s| {
        if values.is_empty() {
            s.plugins.remove(id);
        } else {
            s.plugins.insert(id.to_owned(), values);
        }
    });
}

/// At startup (and after loading settings): every installed plugin's saved values, applied.
/// Values of plugins not installed stay in the file untouched.
pub fn load(saved: &Saved) {
    for s in levels::registered() {
        let values: SettingValues = saved
            .get(s.id())
            .map(|v| {
                v.iter()
                    // Never a secret, even one written into the file by hand.
                    .filter(|(k, _)| {
                        s.settings()
                            .iter()
                            .find(|x| x.key == k.as_str())
                            .is_none_or(|x| x.kind != SettingKind::Secret)
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        levels::runtime::set_settings(s.id(), values);
    }
}

/// Screenshots (`--plugin-ask ID.KEY`, `--plugin-open ID.KEY`): a switch's question asked, a
/// choice's list open, in whichever of the two places the shot shows.
#[derive(Clone, Debug, Default)]
pub struct Shot {
    pub ask: Option<(String, &'static str)>,
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub open: Option<(String, &'static str)>,
}

fn shot_state() -> &'static Mutex<Shot> {
    static S: Mutex<Shot> = Mutex::new(Shot {
        ask: None,
        open: None,
    });
    &S
}

/// The screenshot's question and open list (`set_shot`).
pub fn shot() -> Shot {
    shot_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// `ID.KEY` as a plugin and its setting's key (`&'static`: the spec's own).
fn spec_key(id_key: &str) -> Result<(String, &'static str), String> {
    let (id, key) = id_key
        .split_once('.')
        .ok_or_else(|| format!("{id_key}: expected ID.KEY"))?;
    let s = levels::registered()
        .into_iter()
        .find(|s| s.id() == id)
        .ok_or_else(|| format!("{id}: no such plugin"))?;
    let k = s
        .settings()
        .iter()
        .find(|x| x.key == key && x.shown())
        .map(|x| x.key)
        .ok_or_else(|| format!("{id} has no setting {key}"))?;
    Ok((id.to_owned(), k))
}

/// Screenshots: ask `ask`'s question, open `open`'s list.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub fn set_shot(ask: Option<&str>, open: Option<&str>) -> Result<(), String> {
    let s = Shot {
        ask: ask.map(spec_key).transpose()?,
        open: open.map(spec_key).transpose()?,
    };
    *shot_state().lock().unwrap_or_else(|e| e.into_inner()) = s;
    Ok(())
}

/// Screenshots (`--plugin-set ID.KEY=VALUE`): set a value as typed, without saving: refused
/// ones show refused, as in the app.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub fn set_for_screenshot(cx: &mut gpui::App, spec: &str) -> Result<(), String> {
    let (id_key, value) = spec
        .split_once('=')
        .ok_or_else(|| format!("{spec}: expected ID.KEY=VALUE"))?;
    let (id, key) = id_key
        .split_once('.')
        .ok_or_else(|| format!("{id_key}: expected ID.KEY"))?;
    let Some(source) = levels::registered().into_iter().find(|s| s.id() == id) else {
        return Err(format!("{id}: no such plugin"));
    };
    let saved = crate::picker_view::settings(cx)
        .plugins
        .get(id)
        .cloned()
        .unwrap_or_default();
    // A refusal is the evidence: kept and shown, not an error.
    if let Ok(next) = change(&*source, &saved, key, value) {
        apply(cx, id, next);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::levels::demo::{DemoLevels, key};

    fn saved(pairs: &[(&str, &str)]) -> SettingValues {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // Tests use the demo source, whose refusals are kept by its id in one process-wide map:
    // they run one at a time (`serial`), and every test cleans up.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static S: Mutex<()> = Mutex::new(());
        S.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clean() {
        for k in [
            key::FEED_URL,
            key::ALLOW_REMOTE,
            key::BAND_WIDTH,
            key::REFRESH,
        ] {
            discard("demo", k);
        }
    }

    #[test]
    fn rows_show_every_kind_but_secrets_and_lines_own() {
        let _serial = serial();
        let d = DemoLevels;
        let rows = rows(&d, Some(&saved(&[(key::REFRESH, "82800")])));
        let keys: Vec<&str> = rows.iter().map(|r| r.spec.key).collect();
        assert!(!keys.contains(&key::LABELS), "Lines' own switch does it");
        assert_eq!(keys.len(), 8);
        let refresh = rows.iter().find(|r| r.spec.key == key::REFRESH).unwrap();
        assert_eq!(
            (refresh.shown.as_str(), refresh.origin),
            ("23 h", Origin::Saved)
        );
        let sym = rows.iter().find(|r| r.spec.key == key::SYMBOL).unwrap();
        assert_eq!(sym.choice_label(), "Auto");
        assert_eq!(sym.options.len(), 3, "the source's own list");
        let anchor = rows
            .iter()
            .find(|r| r.spec.key == key::ANCHOR_OFFSET)
            .unwrap();
        assert_eq!(anchor.shown, "", "empty: the placeholder shows");
        assert_eq!(anchor.spec.placeholder, "from the map");
    }

    #[test]
    fn only_values_that_differ_are_saved() {
        let _serial = serial();
        let d = DemoLevels;
        let s = change(&d, &SettingValues::new(), key::BAND_WIDTH, "1.5").unwrap();
        assert_eq!(s, saved(&[(key::BAND_WIDTH, "1.50")]));
        let s = change(&d, &s, key::BAND_WIDTH, "1").unwrap();
        assert!(s.is_empty(), "back to the default: nothing saved");
        // Other plugins' and unknown keys are kept untouched.
        let s = change(&d, &saved(&[("someday", "x")]), key::STATS, "off").unwrap();
        assert_eq!(s, saved(&[("someday", "x"), (key::STATS, "false")]));
        clean();
    }

    #[test]
    fn a_refused_value_applies_nothing_and_says_why() {
        let _serial = serial();
        let d = DemoLevels;
        let base = SettingValues::new();
        // Out of range: refused, never clamped.
        let e = change(&d, &base, key::REFRESH, "2h").unwrap_err();
        assert_eq!(e, "From 1 s to 1 h");
        assert_eq!(
            refused_of("demo", key::REFRESH),
            Some(Refused {
                text: "2h".into(),
                reason: e
            })
        );
        discard("demo", key::REFRESH);
        // A remote address while remote access is off: the source's own reason.
        let e = change(&d, &base, key::FEED_URL, "http://100.71.4.20:7878").unwrap_err();
        assert!(e.starts_with("Not this machine"), "{e}");
        let r = rows(&d, Some(&base));
        let url = r.iter().find(|r| r.spec.key == key::FEED_URL).unwrap();
        assert_eq!(
            url.text(),
            "http://100.71.4.20:7878",
            "its text stays in the field"
        );
        assert_eq!(
            url.value, "http://127.0.0.1:7878",
            "the value in force is unchanged"
        );
        // Turning remote access on applies the address it refused.
        let s = change(&d, &base, key::ALLOW_REMOTE, "on").unwrap();
        assert_eq!(
            s.get(key::FEED_URL).map(String::as_str),
            Some("http://100.71.4.20:7878")
        );
        assert_eq!(refused_of("demo", key::FEED_URL), None);
        // Turning it off again would leave the address refused: refused itself, named.
        let e = change(&d, &s, key::ALLOW_REMOTE, "off").unwrap_err();
        assert!(e.starts_with("Feed URL: Not this machine"), "{e}");
        discard("demo", key::ALLOW_REMOTE);
        // A problem already there (a remote address saved by hand) doesn't block another change.
        let bad = saved(&[(key::FEED_URL, "http://100.71.4.20:7878")]);
        assert!(change(&d, &bad, key::BANDS, "off").is_ok());
        clean();
    }

    #[test]
    fn a_default_typed_in_another_form_isnt_saved() {
        let _serial = serial();
        let d = DemoLevels;
        let s = change(&d, &SettingValues::new(), key::BAND_WIDTH, "1").unwrap();
        assert!(s.is_empty(), "1 is the default 1.00");
        clean();
    }

    #[test]
    fn a_switch_with_a_question_asks_only_to_turn_on() {
        let _serial = serial();
        let d = DemoLevels;
        let r = rows(&d, None);
        let remote = r.iter().find(|r| r.spec.key == key::ALLOW_REMOTE).unwrap();
        assert!(remote.asks("true"));
        assert!(!remote.asks("false"));
        let on = rows(&d, Some(&saved(&[(key::ALLOW_REMOTE, "true")])));
        let remote = on.iter().find(|r| r.spec.key == key::ALLOW_REMOTE).unwrap();
        assert!(!remote.asks("false"), "turning off never asks");
        let bands = r.iter().find(|r| r.spec.key == key::BANDS).unwrap();
        assert!(!bands.asks("true"), "no question");
    }

    #[test]
    fn secrets_are_never_set_or_loaded() {
        let _serial = serial();
        struct WithSecret;
        static S: [SettingSpec; 1] = [SettingSpec::new(
            "token",
            "Token",
            SettingKind::Secret,
            "",
            "",
        )];
        impl LevelSource for WithSecret {
            fn id(&self) -> &'static str {
                "with-secret"
            }
            fn label(&self) -> &'static str {
                "With secret"
            }
            fn serves(&self, _: &lines_core::Product) -> bool {
                false
            }
            fn refresh(&self) -> levels::RefreshMode {
                levels::RefreshMode::OneShot
            }
            fn stale_after(&self) -> std::time::Duration {
                std::time::Duration::from_secs(1)
            }
            fn settings(&self) -> &'static [SettingSpec] {
                &S
            }
            fn connect(
                &self,
                _: &levels::LevelContext,
            ) -> levels::BoxFuture<'static, anyhow::Result<Box<dyn levels::LevelTransport>>>
            {
                Box::pin(async { anyhow::bail!("never") })
            }
            fn decode(
                &self,
                _: &levels::LevelContext,
                _: &dyn levels::ChartPrices,
                _: Option<&levels::LevelMap>,
                _: &[levels::LevelFrame],
            ) -> Result<Option<levels::LevelMap>, String> {
                Ok(None)
            }
        }
        assert!(rows(&WithSecret, None).is_empty(), "never shown");
        assert!(change(&WithSecret, &SettingValues::new(), "token", "x").is_err());
    }
}
