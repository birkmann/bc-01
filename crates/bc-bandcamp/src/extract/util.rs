//! Blob helpers shared by every extractor: HTML-escaped JSON attributes, Python-ish JSON
//! coercions, selectolax-compatible text reading, and Bandcamp's RFC-1123-ish dates.

use chrono::{NaiveDate, TimeZone, Utc};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use std::sync::OnceLock;

/// First element matching `selector` in the document (selectolax `css_first`).
pub(crate) fn css_first<'a>(doc: &'a Html, selector: &str) -> Option<ElementRef<'a>> {
    let sel = Selector::parse(selector).ok()?;
    doc.select(&sel).next()
}

/// All elements matching `selector`, in document order (selectolax `css`).
pub(crate) fn css_all<'a>(doc: &'a Html, selector: &str) -> Vec<ElementRef<'a>> {
    match Selector::parse(selector) {
        Ok(sel) => doc.select(&sel).collect(),
        Err(_) => Vec::new(),
    }
}

/// First descendant of `node` matching `selector`.
pub(crate) fn node_first<'a>(node: ElementRef<'a>, selector: &str) -> Option<ElementRef<'a>> {
    let sel = Selector::parse(selector).ok()?;
    node.select(&sel).next()
}

/// selectolax `node.text(strip=True)` / `text(deep=False, strip=True)`: each text node is
/// stripped and the pieces are joined with *no* separator, so a child span's text runs
/// straight into the parent's (the "gutgehen" + "nand" lesson in `music_grid`).
pub(crate) fn text_strip(node: ElementRef<'_>, deep: bool) -> String {
    if deep {
        node.text().map(str::trim).collect()
    } else {
        node.children()
            .filter_map(|c| c.value().as_text())
            .map(|t| t.trim())
            .collect()
    }
}

/// Attribute value, `None` when absent.
pub(crate) fn attr<'a>(node: ElementRef<'a>, name: &str) -> Option<&'a str> {
    node.value().attr(name)
}

/// Port of `extract.attr_json`: read an HTML-escaped JSON attribute.
///
/// html5ever already decodes character references in attribute values for both quote
/// styles (the `/artists` roster page uses single quotes), so no second `html.unescape`
/// pass is applied (a second pass would corrupt a literal `&lt;` inside a title).
pub fn attr_json(doc: &Html, selector: &str, attr_name: &str) -> Option<Value> {
    let node = css_first(doc, selector)?;
    let raw = node.value().attr(attr_name)?;
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str(raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("attr_json failed for {selector}[{attr_name}]: {e}");
            None
        }
    }
}

// ---- Python-ish coercions ------------------------------------------------------------

/// Python truthiness of a JSON value (`null`, `false`, `0`, `""`, `[]`, `{}` are falsy).
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str(value)` for a JSON value.
pub(crate) fn pystr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// `str(v or "")`.
pub(crate) fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if truthy(Some(x)) => pystr(x),
        _ => String::new(),
    }
}

/// An integer field (accepts integral floats and numeric strings, rejects bools).
pub(crate) fn as_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `int(v or 0)`.
pub(crate) fn int_or_zero(v: Option<&Value>) -> i64 {
    if truthy(v) { as_int(v).unwrap_or(0) } else { 0 }
}

/// A float field (numbers and numeric strings).
pub(crate) fn as_float(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A string field kept only when it really is a JSON string (non-strings stringified).
pub(crate) fn opt_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(pystr(other)),
    }
}

/// An object-typed field, or an empty map (`x.get(k) or {}` + isinstance guard).
pub(crate) fn obj(v: Option<&Value>) -> serde_json::Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    }
}

// ---- dates ---------------------------------------------------------------------------

fn month_num(m: &str) -> Option<u32> {
    const MONTHS: [&str; 12] =
        ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let l = m.to_ascii_lowercase();
    MONTHS.iter().position(|x| *x == l).map(|i| i as u32 + 1)
}

fn bc_date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(\d{1,2}) ([A-Za-z]{3}) (\d{4})(?: (\d{1,2}):(\d{1,2}):(\d{1,2})(?: (GMT|UTC))?)?$")
            .expect("static regex")
    })
}

/// Port of `extract.parse_bc_date`: `17 Jul 2026 00:00:00 GMT` -> `2026-07-17`.
///
/// Bandcamp uses an RFC-1123-ish format, not ISO, so a naive `[:10]` slice would
/// silently produce garbage.
pub fn parse_bc_date(value: Option<&str>) -> Option<String> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(c) = bc_date_re().captures(text) {
        let day: u32 = c[1].parse().ok()?;
        let year: i32 = c[3].parse().ok()?;
        if let Some(month) = month_num(&c[2]) {
            if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
                return Some(d.format("%Y-%m-%d").to_string());
            }
        }
    }
    let head: String = text.chars().take(10).collect();
    let b = head.as_bytes();
    if b.len() == 10
        && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
    {
        return Some(head);
    }
    if text.bytes().all(|c| c.is_ascii_digit()) {
        let secs: i64 = text.parse().ok()?;
        return Utc
            .timestamp_opt(secs, 0)
            .single()
            .map(|d| d.date_naive().format("%Y-%m-%d").to_string());
    }
    None
}
