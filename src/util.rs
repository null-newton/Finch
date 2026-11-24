//! Shared helpers: lazily compiled regexes, text folding, money in integer cents, calendar months.
use chrono::{Datelike, NaiveDate};
use fancy_regex::{Captures, Regex};
use rust_decimal::{prelude::*, RoundingStrategy::*};
use std::borrow::Cow;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// A `fancy_regex::Regex` compiled once (Python `re` syntax: lookarounds allowed).
#[macro_export]
macro_rules! re { ($p:expr) => {{ static R: std::sync::LazyLock<fancy_regex::Regex> = std::sync::LazyLock::new(|| fancy_regex::Regex::new($p).expect($p)); &*R }}; }
/// A plain `regex::Regex` compiled once (needed for `split`).
#[macro_export]
macro_rules! rs { ($p:expr) => {{ static R: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new($p).expect($p)); &*R }}; }

pub fn is(r: &Regex, s: &str) -> bool { r.is_match(s).unwrap_or(false) }
pub fn cap<'a>(r: &Regex, s: &'a str) -> Option<Captures<'a>> { r.captures(s).ok().flatten() }
pub fn find(r: &Regex, s: &str) -> Option<(usize, usize)> { r.find(s).ok().flatten().map(|m| (m.start(), m.end())) }
pub fn g<'a>(c: &Captures<'a>, i: usize) -> &'a str { c.get(i).map_or("", |m| m.as_str()) }
pub fn gn<'a>(c: &Captures<'a>, n: &str) -> &'a str { c.name(n).map_or("", |m| m.as_str()) }
pub fn sub<'a>(r: &Regex, s: &'a str, rep: &str) -> Cow<'a, str> { r.replace_all(s, fancy_regex::NoExpand(rep)) }
pub fn sub1<'a>(r: &Regex, s: &'a str, rep: &str) -> Cow<'a, str> { r.replacen(s, 1, fancy_regex::NoExpand(rep)) }
/// Compile a user/config pattern case-insensitively (Python `re.I`); None when invalid.
pub fn rx_i(p: &str) -> Option<Regex> { Regex::new(&format!("(?i){p}")).ok() }
pub fn search_i(p: &str, s: &str) -> bool { rx_i(p).is_some_and(|r| is(&r, s)) }
pub fn escape(s: &str) -> String { regex::escape(s) }

pub fn strip_accents(s: &str) -> String { s.nfkd().filter(|c| !is_combining_mark(*c)).collect() }
pub fn strip_nfd(s: &str) -> String { s.nfd().filter(|c| !is_combining_mark(*c)).collect() }
/// THE normaliser of the privacy layer: lower case, accents removed.
pub fn fold(s: &str) -> String { strip_nfd(&s.to_lowercase()) }
/// fold(s) and, per BYTE of the folded text, the byte span in `s` of the char it came from.
pub fn fold_with_map(s: &str) -> (String, Vec<(usize, usize)>) {
    let (mut out, mut idx) = (String::new(), vec![]);
    for (i, ch) in s.char_indices() { for c in ch.to_lowercase().collect::<String>().nfd().filter(|c| !is_combining_mark(*c)) { out.push(c); idx.extend(std::iter::repeat_n((i, i + ch.len_utf8()), c.len_utf8())); } }
    (out, idx)
}
/// Replace every match of `rx` (written on FOLDED text) in `s`, whatever the case and accents of the original.
pub fn mask_folded(s: &str, rx: &Regex, repl: &str) -> String {
    if s.is_empty() { return s.into(); }
    let (folded, idx) = fold_with_map(s);
    let (mut out, mut last) = (String::new(), 0);
    for m in rx.find_iter(&folded).flatten() {
        if m.start() == m.end() { continue; }
        let (oa, ob) = (idx[m.start()].0, idx[m.end() - 1].1);
        if oa < last { continue; }
        out.push_str(&s[last..oa]); out.push_str(repl); last = ob;
    }
    out.push_str(&s[last..]); out
}
/// `(?<![a-z0-9])(t1|t2...)(?![a-z0-9])` over folded terms of >= 3 chars, longest first.
pub fn term_regex<I: IntoIterator<Item = S>, S: AsRef<str>>(terms: I) -> Option<Regex> {
    let mut ts: Vec<String> = terms.into_iter().map(|t| fold(t.as_ref())).filter(|t| t.chars().count() >= 3).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    ts.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    (!ts.is_empty()).then(|| Regex::new(&format!("(?<![a-z0-9])({})(?![a-z0-9])", ts.iter().map(|t| escape(t)).collect::<Vec<_>>().join("|"))).unwrap())
}
pub fn collapse(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }
pub fn upper(s: &str) -> String { s.to_uppercase() }
pub fn truncate_chars(s: &str, n: usize) -> String { s.chars().take(n).collect() }
pub fn dedup_keep_order(v: impl IntoIterator<Item = String>) -> Vec<String> { let mut seen = std::collections::HashSet::new(); v.into_iter().filter(|x| seen.insert(x.clone())).collect() }

// ---------------------------------------------------------------- money (integer cents, half-even)
fn dec(x: f64) -> Decimal { Decimal::from_str(&format!("{x}")).unwrap_or_default() }
pub fn to_cents(x: f64) -> i64 { (dec(x) * Decimal::from(100)).round_dp_with_strategy(0, MidpointNearestEven).to_i64().unwrap_or(0) }
pub fn money_str(c: i64) -> String { format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.unsigned_abs() / 100, c.unsigned_abs() % 100) }
pub fn money_opt(c: Option<i64>) -> serde_json::Value { c.map_or(serde_json::Value::Null, |c| money_str(c).into()) }
pub fn div_cents(total: i64, n: i64) -> i64 { (Decimal::from(total) / Decimal::from(n)).round_dp_with_strategy(0, MidpointNearestEven).to_i64().unwrap_or(0) }
pub fn mul_cents(c: i64, f: f64) -> i64 { (Decimal::from(c) * dec(f)).round_dp_with_strategy(0, MidpointNearestEven).to_i64().unwrap_or(0) }
pub fn round4(x: f64) -> f64 { (dec(x) * Decimal::from(10000)).round_dp_with_strategy(0, MidpointNearestEven).to_f64().unwrap_or(x) / 10000.0 }
pub fn pct(num: i64, den: i64) -> Option<f64> { (den != 0).then(|| round4(num as f64 / den as f64)) }
pub fn median_c(v: &[i64]) -> i64 { let mut v = v.to_vec(); v.sort(); let n = v.len(); if n % 2 == 1 { v[n / 2] } else { div_cents(v[n / 2 - 1] + v[n / 2], 2) } }
/// Python `round(x, 2)` then `f"{x:.2f}"` for content fingerprints.
pub fn amount2(x: f64) -> String { format!("{:.2}", dec(x).round_dp_with_strategy(2, MidpointNearestEven)) }

// ---------------------------------------------------------------- dates / months
pub fn d(s: &str) -> Option<NaiveDate> { NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok() }
pub fn month_key(x: NaiveDate) -> String { format!("{:04}-{:02}", x.year(), x.month()) }
pub fn month_start(k: &str) -> NaiveDate { NaiveDate::from_ymd_opt(k[..4].parse().unwrap_or(1970), k[5..7].parse().unwrap_or(1), 1).unwrap() }
pub fn month_end(k: &str) -> NaiveDate { let s = month_start(&add_months_key(k, 1)); s.pred_opt().unwrap() }
pub fn add_months_key(k: &str, n: i32) -> String { let s = month_start(k); let i = s.year() * 12 + s.month0() as i32 + n; format!("{:04}-{:02}", i.div_euclid(12), i.rem_euclid(12) + 1) }
pub fn add_months(x: NaiveDate, n: i32) -> NaiveDate { let k = add_months_key(&month_key(x), n); let e = month_end(&k); NaiveDate::from_ymd_opt(e.year(), e.month(), x.day().min(e.day())).unwrap() }
pub fn months_between(first: &str, last: &str) -> Vec<String> { let (mut out, mut cur) = (vec![], first.to_string()); while cur.as_str() <= last { out.push(cur.clone()); cur = add_months_key(&cur, 1); } out }
pub fn last_closed_month(today: NaiveDate) -> String { add_months_key(&month_key(today), -1) }
pub fn now_iso() -> String { chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string() }
pub fn sha1_hex(s: &str) -> String { use sha1::Digest; hex::encode(sha1::Sha1::digest(s.as_bytes())) }
pub fn sha256_hex(b: &[u8]) -> String { use sha2::Digest; hex::encode(sha2::Sha256::digest(b)) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn cents() { assert_eq!(to_cents(0.1 + 0.2), 30); assert_eq!(to_cents(-12.345), -1234); assert_eq!(to_cents(2.675), 268); assert_eq!(money_str(-5), "-0.05"); assert_eq!(div_cents(5, 2), 2); assert_eq!(div_cents(7, 2), 4); }
    #[test] fn months() { assert_eq!(add_months_key("2025-01", -1), "2024-12"); assert_eq!(add_months(NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(), 1).to_string(), "2024-02-29"); assert_eq!(month_end("2025-02").to_string(), "2025-02-28"); }
    #[test] fn folding() { assert_eq!(fold("Élodie"), "elodie"); let (f, m) = fold_with_map("aÉb"); assert_eq!(f, "aeb"); assert_eq!(m[1], (1, 3)); assert_eq!(mask_folded("Chez ÉLODIE x", &term_regex(["elodie"]).unwrap(), "[p]"), "Chez [p] x"); assert_eq!(strip_accents("Crédit"), "Credit"); }
}
