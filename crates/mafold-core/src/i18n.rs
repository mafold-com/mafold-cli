//! The portable i18n engine — string lookup + CLDR plural selection + `{name}`
//! interpolation + the active→base→key fallback chain. PURE (no I/O, no async),
//! so it's identical native + wasm and fully unit-testable here.
//!
//! A language pack is, Telegram-style, a delta over the BASE (English): a key
//! missing from the active language falls back to the base, then to the key name
//! itself (so the UI never shows blank). `set_language` / delta sync + caching
//! live in `store.rs`; this module is just the resolver the clients call per-string.

use serde_json::{Map, Value};

/// The in-memory active pack the synchronous `t` / `plural` read. Held behind a
/// `RwLock` in `Store`. `base` is English (always loaded); `active` is the chosen
/// language's strings (== base when English is selected).
#[derive(Default)]
pub struct LangPack {
    base_lang: String,
    base: Map<String, Value>,
    active_lang: String,
    active_version: u32,
    active: Map<String, Value>,
    rtl: bool,
}

impl LangPack {
    pub fn set_base(&mut self, lang: &str, strings: Map<String, Value>) {
        self.base_lang = lang.to_string();
        self.base = strings;
    }

    pub fn set_active(&mut self, lang: &str, version: u32, strings: Map<String, Value>, rtl: bool) {
        self.active_lang = lang.to_string();
        self.active_version = version;
        self.active = strings;
        self.rtl = rtl;
    }

    /// Merge a server delta onto the active strings (changed wins, deleted drop).
    pub fn apply_active_diff(&mut self, version: u32, changed: Map<String, Value>, deleted: &[String]) {
        for (k, v) in changed {
            self.active.insert(k, v);
        }
        for k in deleted {
            self.active.remove(k);
        }
        self.active_version = version;
    }

    pub fn active_lang(&self) -> &str { &self.active_lang }
    pub fn active_version(&self) -> u32 { self.active_version }
    pub fn base_lang(&self) -> &str { &self.base_lang }
    pub fn rtl(&self) -> bool { self.rtl }

    /// Whether this pack can actually resolve strings — i.e. some tier of the
    /// cloud pack has been delivered (from cache or network). Hosts GATE THEIR
    /// FIRST PAINT on this: there is no bundled baseline by design, so a frame
    /// painted before it is true is a frame of raw `a.b.c` keys.
    ///
    /// EITHER tier counts. A zh session that got the en base but lost the zh
    /// fetch paints English — English is the pack's real base tier, not an
    /// invented fallback — and flips to Chinese when the retry lands. Waiting
    /// for the active tier instead would hold the whole UI hostage to the
    /// weaker of two requests and show nothing at all.
    pub fn is_loaded(&self) -> bool {
        !self.base.is_empty() || !self.active.is_empty()
    }

    /// Raw value for `key`: active language first, then the base (English).
    fn raw(&self, key: &str) -> Option<&Value> {
        self.active.get(key).or_else(|| self.base.get(key))
    }

    /// `t(key, args)` — a plain string with `{name}` interpolation; falls back
    /// active → base → the key itself. A plural object referenced without a count
    /// resolves to its `other` form.
    pub fn t(&self, key: &str, args: &Map<String, Value>) -> String {
        match self.raw(key) {
            Some(Value::String(s)) => interpolate(s, args),
            Some(Value::Object(o)) => {
                interpolate(o.get("other").and_then(|v| v.as_str()).unwrap_or(key), args)
            }
            _ => key.to_string(),
        }
    }

    /// `plural(key, n, args)` — pick the CLDR category for the ACTIVE language,
    /// fall back to `other`, then interpolate (with `{n}` made available).
    pub fn plural(&self, key: &str, n: f64, args: &Map<String, Value>) -> String {
        let mut a = args.clone();
        a.entry("n".to_string()).or_insert_with(|| number_value(n));
        let cat = plural_category(&self.active_lang, n);
        match self.raw(key) {
            Some(Value::Object(o)) => {
                let s = o
                    .get(cat)
                    .and_then(|v| v.as_str())
                    .or_else(|| o.get("other").and_then(|v| v.as_str()))
                    .unwrap_or(key);
                interpolate(s, &a)
            }
            Some(Value::String(s)) => interpolate(s, &a),
            _ => key.to_string(),
        }
    }

    /// The effective string map (base overlaid by the active language) — handed
    /// to the RN runtime once per language change so guest JS does fast local
    /// lookups instead of a per-string FFI hop.
    pub fn effective(&self) -> Map<String, Value> {
        let mut m = self.base.clone();
        for (k, v) in &self.active {
            m.insert(k.clone(), v.clone());
        }
        m
    }
}

/// CLDR plural category for `lang` at count `n`. v0 covers the languages we ship
/// (en, zh) precisely; everything else uses the safe English-like one/other rule
/// until its real CLDR rule is added.
fn plural_category(lang: &str, n: f64) -> &'static str {
    let base = lang.split('-').next().unwrap_or(lang);
    match base {
        // Languages with a single form (no plural distinction).
        "zh" | "ja" | "ko" | "vi" | "th" | "id" | "ms" | "lo" | "my" | "km" => "other",
        // English-style: exactly one → one, else other.
        "en" => {
            if n == 1.0 {
                "one"
            } else {
                "other"
            }
        }
        _ => {
            if n == 1.0 {
                "one"
            } else {
                "other"
            }
        }
    }
}

fn number_value(n: f64) -> Value {
    // Render integral counts as integers ("5"), not "5.0".
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

fn value_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Replace `{name}` tokens with `args[name]` (stringified). An unmatched `{` or an
/// unknown `{name}` is left verbatim, so a stray brace never eats the rest.
fn interpolate(s: &str, args: &Map<String, Value>) -> String {
    if !s.contains('{') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('}') {
            let name = &after[..close];
            if !name.is_empty() && !name.contains('{') {
                match args.get(name) {
                    Some(v) => out.push_str(&value_to_str(v)),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
                continue;
            }
        }
        out.push('{');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Parse an optional JSON-object args string into a map (empty on None/invalid).
pub fn parse_args(json: Option<&str>) -> Map<String, Value> {
    json.and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(o) => o,
            _ => Map::new(),
        }
    }

    fn pack() -> LangPack {
        let mut p = LangPack::default();
        p.set_base(
            "en",
            obj(json!({
                "settings.title": "Settings",
                "presence.online": "online",
                "card.quote.buy": "Buy {symbol}",
                "messages.count": { "one": "{n} message", "other": "{n} messages" }
            })),
        );
        p.set_active(
            "zh-Hans",
            7,
            obj(json!({
                "settings.title": "设置",
                "card.quote.buy": "买入 {symbol}",
                "messages.count": { "other": "{n} 条消息" }
            })),
            false,
        );
        p
    }

    #[test]
    fn lookup_uses_active_then_base_then_key() {
        let p = pack();
        assert_eq!(p.t("settings.title", &Map::new()), "设置"); // active
        assert_eq!(p.t("presence.online", &Map::new()), "online"); // active missing → base (en)
        assert_eq!(p.t("does.not.exist", &Map::new()), "does.not.exist"); // → key
    }

    #[test]
    fn interpolation_named_args() {
        let p = pack();
        assert_eq!(p.t("card.quote.buy", &obj(json!({ "symbol": "AAPL" }))), "买入 AAPL");
        // Unknown token left verbatim; a stray brace doesn't eat the string.
        assert_eq!(interpolate("a {x} {y", &obj(json!({ "x": 1 }))), "a 1 {y");
        assert_eq!(interpolate("{n} of {n}", &obj(json!({ "n": 3 }))), "3 of 3");
    }

    #[test]
    fn plural_en_one_vs_other() {
        let mut p = LangPack::default();
        p.set_active(
            "en",
            1,
            obj(json!({ "messages.count": { "one": "{n} message", "other": "{n} messages" } })),
            false,
        );
        assert_eq!(p.plural("messages.count", 1.0, &Map::new()), "1 message");
        assert_eq!(p.plural("messages.count", 5.0, &Map::new()), "5 messages");
        assert_eq!(p.plural("messages.count", 0.0, &Map::new()), "0 messages");
    }

    #[test]
    fn plural_zh_single_form() {
        let p = pack(); // active = zh-Hans
        assert_eq!(p.plural("messages.count", 1.0, &Map::new()), "1 条消息");
        assert_eq!(p.plural("messages.count", 9.0, &Map::new()), "9 条消息");
    }

    #[test]
    fn plural_falls_back_to_base_other_when_active_missing() {
        let mut p = LangPack::default();
        p.set_base(
            "en",
            obj(json!({ "x": { "one": "{n} item", "other": "{n} items" } })),
        );
        p.set_active("fr", 1, obj(json!({})), false); // active empty → base
        // fr has no rule yet → english-like; 2 → other.
        assert_eq!(p.plural("x", 2.0, &Map::new()), "2 items");
    }

    #[test]
    fn apply_diff_merges_and_deletes() {
        let mut p = pack();
        p.apply_active_diff(
            8,
            obj(json!({ "settings.title": "偏好设置", "new.key": "新" })),
            &["card.quote.buy".to_string()],
        );
        assert_eq!(p.active_version(), 8);
        assert_eq!(p.t("settings.title", &Map::new()), "偏好设置"); // changed
        assert_eq!(p.t("new.key", &Map::new()), "新"); // added
        assert_eq!(p.t("card.quote.buy", &obj(json!({ "symbol": "X" }))), "Buy X"); // deleted from active → base (en)
    }

    #[test]
    fn effective_overlays_active_on_base() {
        let eff = pack().effective();
        assert_eq!(eff.get("settings.title").unwrap(), "设置"); // active wins
        assert_eq!(eff.get("presence.online").unwrap(), "online"); // base-only kept
        assert!(eff.contains_key("messages.count"));
    }

    /// The FFI boundary hands args as an optional JSON string — every
    /// non-object input must degrade to "no args", never panic.
    #[test]
    fn parse_args_none_invalid_nonobject_empty() {
        assert!(parse_args(None).is_empty());
        assert!(parse_args(Some("not json")).is_empty());
        assert!(parse_args(Some("[1,2]")).is_empty());
        let m = parse_args(Some(r#"{"a":1,"name":"Ops"}"#));
        assert_eq!(m.get("name").and_then(|v| v.as_str()), Some("Ops"));
        assert_eq!(m.len(), 2);
    }
}
