//! Recurring payment detection (deterministic, no LLM) and price changes.
//! 1. One row per bank transaction (splits merged), zero amounts and `income.refund` skipped; grouped by (direction, key, account),
//!    key = SEPA creditor id (+ mandate), else the merchant entity, else the normalised merchant key.
//! 2. The median gap picks the cadence; accepted when >= 60 % of the gaps fit it and >= 80 % fit or are a 2x / 3x multiple.
//! 3. Amounts: `fixed` when >= 80 % are within the tolerance of the median (or a few price STEPS), `variable` for bill-like
//!    categories; a group that does not fit is split into amount clusters, same-day lanes or day-of-month lanes.
//! 4. >= 3 occurrences (2 for a yearly / quarterly contract-like cadence, low confidence). Ended after 1.5 x the cadence.
//!    > Stable ids `rec_` (first payment), carried over from the stored series when they share most payments.
use super::*;
use crate::memory::{self, Matcher};
use chrono::{Datelike, Duration, Weekday};
use std::collections::{HashMap, HashSet};

/// (name, nominal days, accepted gap, step (months or days), per year)
pub const CADENCES: [(&str, i64, (i64, i64), (char, i64), f64); 7] = [("weekly", 7, (5, 9), ('d', 7), 365.25 / 7.0), ("biweekly", 14, (11, 17), ('d', 14), 365.25 / 14.0), ("monthly", 30, (25, 35), ('m', 1), 12.0),
    ("bimonthly", 61, (53, 68), ('m', 2), 6.0), ("quarterly", 91, (81, 101), ('m', 3), 4.0), ("semiannual", 183, (167, 197), ('m', 6), 2.0), ("yearly", 365, (350, 380), ('m', 12), 1.0)];
pub fn cad(name: &str) -> &'static (&'static str, i64, (i64, i64), (char, i64), f64) { CADENCES.iter().find(|c| c.0 == name).unwrap() }
const CONTRACT_PREFIXES: [&str; 2] = ["subscriptions.", "insurance."];
const CONTRACT_CATEGORIES: [&str; 10] = ["housing.energy", "housing.water", "housing.home_insurance", "transport.car_insurance", "health.health_insurance", "transport.car_loan_lease", "debt.personal_loan", "debt.loan_repayment", "housing.mortgage", "housing.rental_property_loan"];

#[derive(Debug, Clone)]
pub struct Series {
    pub id: String, pub direction: String, pub kind: String, pub key: String, pub entity: String, pub account: String, pub account_label: String, pub category: String, pub cadence: String, pub amount_mode: String,
    pub first_date: NaiveDate, pub last_date: NaiveDate, pub next_expected: Option<NaiveDate>, pub expected_c: i64, pub low_c: i64, pub high_c: i64, pub yearly_c: i64, pub status: String, pub overdue_days: i64,
    pub confidence: f64, pub links: Vec<(String, String, f64)>, pub contract_candidate: bool, pub missing_contract: bool, pub tags: Vec<String>, pub price_steps: usize, pub day_of_month: Option<u32>, pub end_of_month: bool,
    pub aliases: Vec<String>, pub occ: Vec<(String, NaiveDate, i64)>,
}
impl Series {
    pub fn label(&self) -> &'static str { if self.confidence < 0.6 { "low" } else if self.confidence < 0.8 { "medium" } else { "high" } }
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "direction": self.direction, "kind": self.kind, "key": self.key, "entity": self.entity, "account": self.account, "account_label": self.account_label, "category": self.category, "cadence": self.cadence,
            "amount_mode": self.amount_mode, "n_occurrences": self.occ.len(), "first_date": self.first_date.to_string(), "last_date": self.last_date.to_string(), "next_expected": self.next_expected.map(|d| d.to_string()),
            "expected_amount": money_str(self.expected_c), "amount_low": money_str(self.low_c), "amount_high": money_str(self.high_c), "yearly_cost": money_str(self.yearly_c), "status": self.status, "overdue_days": self.overdue_days,
            "confidence": self.confidence, "confidence_label": self.label(), "links": self.links.iter().map(|(k, i, s)| json!({"kind": k, "id": i, "share": s})).collect::<Vec<_>>(), "contract_candidate": self.contract_candidate,
            "missing_contract": self.missing_contract, "tags": self.tags, "price_steps": self.price_steps, "day_of_month": self.day_of_month, "end_of_month": self.end_of_month, "aliases": self.aliases,
            "occurrences": self.occ.iter().map(|(k, d, a)| json!({"tx_key": k, "date": d.to_string(), "amount": money_str(*a)})).collect::<Vec<_>>(), "evidence": self.occ.iter().rev().take(50).rev().map(|o| o.0.clone()).collect::<Vec<_>>()})
    }
}

fn median_f(v: &mut [i64]) -> f64 { v.sort(); let n = v.len(); if n % 2 == 1 { v[n / 2] as f64 } else { (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0 } }
/// (cadence, share of gaps that fit exactly), or None.
pub fn detect_cadence(dates: &[NaiveDate]) -> Option<(&'static str, f64)> {
    let mut ds = dates.to_vec(); ds.sort();
    let mut gaps: Vec<i64> = ds.windows(2).map(|w| (w[1] - w[0]).num_days()).collect(); if gaps.is_empty() { return None; }
    let med = median_f(&mut gaps.clone());
    let c = CADENCES.iter().find(|c| c.2 .0 as f64 <= med && med <= c.2 .1 as f64)?;
    let (lo, hi) = c.2; let (centre, tol) = ((lo + hi) as f64 / 2.0, (hi - lo) as f64 / 2.0);
    let fit1 = gaps.iter().filter(|g| lo <= **g && **g <= hi).count();
    let ok = gaps.iter_mut().filter(|g| (lo <= **g && **g <= hi) || [2.0, 3.0].iter().any(|k| (**g as f64 - k * centre).abs() <= tol * k)).count();
    let n = gaps.len() as f64; if (fit1 as f64) / n < 0.6 || (ok as f64) / n < 0.8 { return None; }
    Some((c.0, fit1 as f64 / n))
}
fn runs(a: &[i64], tol: f64) -> Vec<Vec<i64>> { let mut r: Vec<Vec<i64>> = vec![]; for x in a { match r.last_mut() { Some(l) if ((x - l[0]).abs() as f64) <= tol * l[0] as f64 => l.push(*x), _ => r.push(vec![*x]) } } r }
/// (mode, share within tolerance, price steps) for absolute amounts in date order, or None.
fn amount_mode(a: &[i64], tol: f64, variable_ok: bool) -> Option<(&'static str, f64, usize)> {
    let med = median_c(a); let share = a.iter().filter(|x| ((*x - med).abs() as f64) <= tol * med as f64).count() as f64 / a.len() as f64;
    let r = runs(a, tol);
    if share >= 0.8 { return Some(("fixed", share, r.len() - 1)); }
    if r.len() >= 2 && r.len() <= 2.max(a.len() / 3) && r[..r.len() - 1].iter().all(|x| x.len() >= 2) { return Some(("fixed", r.iter().filter(|x| x.len() >= 2).map(|x| x.len()).sum::<usize>() as f64 / a.len() as f64, r.len() - 1)); }
    if variable_ok && *a.iter().max().unwrap() <= 8 * (*a.iter().min().unwrap()).max(1) { return Some(("variable", share, 0)); }
    None
}
fn clusters(items: &[Tx], tol: f64) -> Vec<Vec<Tx>> {
    let mut s = items.to_vec(); s.sort_by(|a, b| (a.amount_c.abs(), a.date, &a.key).cmp(&(b.amount_c.abs(), b.date, &b.key)));
    let mut out: Vec<Vec<Tx>> = vec![];
    for t in s { match out.iter_mut().find(|c| ((t.amount_c.abs() - c[0].amount_c.abs()).abs() as f64) <= tol * c[0].amount_c.abs() as f64) { Some(c) => c.push(t), None => out.push(vec![t]) } }
    for c in &mut out { c.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key))); } out
}
fn confidence(fit: f64, n: usize, share: f64, cadence: &str, mode: &str) -> f64 {
    let mut c = 0.5 * fit + 0.3 * (n as f64 / 6.0).min(1.0) + 0.2 * if mode == "fixed" { share } else { share.min(0.7) };
    if (cadence == "yearly" || cadence == "quarterly") && n < 3 { c = c.min(0.5); } (c.min(1.0) * 100.0).round() / 100.0
}
/// Banks book a weekend date on the next Monday.
pub fn roll_business_day(d: NaiveDate) -> NaiveDate { d + Duration::days(match d.weekday() { Weekday::Sat => 2, Weekday::Sun => 1, _ => 0 }) }
fn month_date(base: NaiveDate, months: i64, dom: Option<u32>, eom: bool) -> NaiveDate { let k = add_months_key(&month_key(base), months as i32); let last = month_end(&k); NaiveDate::from_ymd_opt(last.year(), last.month(), if eom { last.day() } else { dom.unwrap_or(base.day()).min(last.day()) }).unwrap() }
pub fn expected_date(last: NaiveDate, cadence: &str, dom: Option<u32>, eom: bool, k: i64) -> NaiveDate { let (u, n) = cad(cadence).3; if u == 'd' { last + Duration::days(n * k) } else { roll_business_day(month_date(last, n * k, dom, eom)) } }
/// Dates (with an `overdue` flag) of an active series between start and end; a missed next date is returned once, on `start`.
pub fn occurrences(x: &Series, start: NaiveDate, end: NaiveDate) -> Vec<(NaiveDate, bool)> {
    if x.status != "active" || x.next_expected.is_none() { return vec![]; }
    let mut out = vec![];
    for k in 1..400 { let dd = expected_date(x.last_date, &x.cadence, x.day_of_month, x.end_of_month, k); if dd > end { break; } if dd >= start { out.push((dd, false)); } else if k == 1 { out.push((start, true)); } }
    out
}
/// A grouping key without per-payment references ('AMAZON PRIME*9W8 ...' -> 'AMAZON PRIME'; mixed letter / digit tokens dropped).
pub fn norm_key(text: &str) -> String {
    let base = text.split('*').next().unwrap_or("");
    let toks: Vec<&str> = base.split_whitespace().filter(|t| !(t.len() >= 5 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && t.chars().any(|c| c.is_ascii_digit()))).collect();
    if toks.is_empty() { text.into() } else { toks.join(" ") }
}
fn group_key(ds: &Dataset, t: &Tx) -> String {
    if let Some((Some(c), m)) = ds.parse_meta.get(&t.key) { return format!("sepa:{c}{}", m.as_ref().map_or(String::new(), |m| format!("/{m}"))); }
    norm_key(if !t.entity.is_empty() { &t.entity } else if !t.mkey.is_empty() { &t.mkey } else { &t.key })
}
fn kind(direction: &str, category: &str, tags: &BTreeSet<String>) -> &'static str {
    if SAVED_TAGS.iter().any(|t| tags.contains(*t)) { "saving" } else if is_transfer(category) { "transfer" } else if direction == "in" { if is_income(category) { "income" } else { "inflow" } } else { "expense" }
}
/// One transaction against a contract / loan pattern (and its amount band).
pub fn tx_matches(m: &Matcher, rx: &fancy_regex::Regex, t: &Tx) -> bool {
    if !(is(rx, &t.mkey) || is(rx, &t.desc) || is(rx, &t.entity)) { return false; }
    m.amount.is_none_or(|(a, tol)| ((t.amount_c.abs() as f64 / 100.0) - a).abs() <= a * tol / 100.0 + 0.005)
}
fn links(ms: &[(Matcher, fancy_regex::Regex)], txs: &[Tx]) -> Vec<(String, String, f64)> {
    let mut out: Vec<_> = ms.iter().filter_map(|(m, rx)| { let s = txs.iter().filter(|t| tx_matches(m, rx, t)).count() as f64 / txs.len() as f64; (s >= 0.5).then(|| (m.kind.to_string(), m.id.clone(), (s * 100.0).round() / 100.0)) }).collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1))); out
}
/// Price steps of absolute amounts: [(index, old level, new level, confirmed)]; a blip that returns to the old level is ignored.
pub fn levels(a: &[i64], pct: f64, min_abs: i64) -> Vec<(usize, i64, i64, bool)> {
    let mut out = vec![]; let Some(mut level) = a.first().copied() else { return out };
    for i in 1..a.len() {
        let x = a[i];
        if ((x - level).abs() as f64) >= (level as f64 * pct / 100.0).max(min_abs as f64) {
            match a.get(i + 1) { None => { out.push((i, level, x, false)); level = x; } Some(n) if (n - x).abs() < (n - level).abs() => { out.push((i, level, x, true)); level = x; } _ => {} }
        }
    }
    out
}
fn day_habits(txs: &[Tx]) -> (u32, bool) {
    let l6 = &txs[txs.len().saturating_sub(6)..];
    let eom = l6.iter().filter(|t| t.date == month_end(&month_key(t.date))).count() * 2 > l6.len();
    let mut days: Vec<i64> = l6.iter().map(|t| t.date.day() as i64).collect(); (median_f(&mut days) as u32, eom)
}

struct Ctx<'a> { ds: &'a Dataset, ms: Vec<(Matcher, fancy_regex::Regex)> }
impl Ctx<'_> {
    #[allow(clippy::too_many_arguments)]
    fn build(&self, direction: &str, key: &str, account: &str, txs: &[Tx], cadence: &str, mode: &str, fit: f64, share: f64, steps: usize, aliases: Vec<String>) -> Series {
        let (ds, s) = (self.ds, &self.ds.settings);
        let id = format!("rec_{}", &sha1_hex(&format!("{direction}|{account}|{}", txs[0].key))[..10]);
        let amts: Vec<i64> = txs.iter().map(|t| t.amount_c).collect();
        let expected = if mode == "fixed" { let st = levels(&amts.iter().map(|a| a.abs()).collect::<Vec<_>>(), s.price_change_threshold_pct, (s.price_change_min_abs * 100.0) as i64); let since = st.last().map_or(0, |x| x.0); let tail = &amts[since..]; median_c(&tail[tail.len().saturating_sub(3)..]) } else { median_c(&amts[amts.len().saturating_sub(6)..]) };
        let mut recent = amts[amts.len().saturating_sub(6)..].to_vec(); recent.sort_by_key(|a| a.abs());
        let (first, last) = (txs[0].date, txs.last().unwrap().date); let nominal = cad(cadence).1;
        let ended = (ds.today - last).num_days() as f64 > s.recurring_ended_factor * nominal as f64;
        let (dom, eom) = if ["monthly", "bimonthly", "quarterly"].contains(&cadence) { let (d, e) = day_habits(txs); (Some(d), e) } else { (None, false) };
        let next = (!ended).then(|| expected_date(last, cadence, dom, eom, 1));
        let overdue = next.map_or(0, |n| (ds.today - n).num_days().max(0));
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new(); for t in txs { *counts.entry(&t.category).or_default() += 1; }
        let top = *counts.values().max().unwrap(); let category = txs.iter().rev().find(|t| counts[t.category.as_str()] == top).unwrap().category.clone();
        let tags: BTreeSet<String> = txs.iter().flat_map(|t| t.tags.iter().cloned()).collect();
        let k = kind(direction, &category, &tags); let lk = links(&self.ms, txs);
        let candidate = k == "expense" && (CONTRACT_PREFIXES.iter().any(|p| category.starts_with(p)) || CONTRACT_CATEGORIES.contains(&category.as_str()));
        Series { id, direction: direction.into(), kind: k.into(), key: key.into(), entity: txs.last().unwrap().entity.clone(), account: account.into(), account_label: ds.label(account), category, cadence: cadence.into(), amount_mode: mode.into(),
            first_date: first, last_date: last, next_expected: next, expected_c: expected, low_c: recent[0], high_c: *recent.last().unwrap(), yearly_c: mul_cents(expected.abs(), cad(cadence).4), status: if ended { "ended" } else { "active" }.into(), overdue_days: overdue,
            confidence: confidence(fit, txs.len(), share, cadence, mode), missing_contract: candidate && lk.is_empty() && !ended, links: lk, contract_candidate: candidate, tags: tags.into_iter().collect(), price_steps: steps, day_of_month: dom, end_of_month: eom, aliases,
            occ: txs.iter().map(|t| (t.key.clone(), t.date, t.amount_c)).collect() }
    }
    fn detect_group(&self, direction: &str, key: &str, account: &str, txs: &[Tx]) -> Vec<Series> {
        let s = &self.ds.settings; let tol = s.recurring_amount_tolerance;
        if txs.len() < 2 { return vec![]; }
        let mut cc: BTreeMap<&str, usize> = BTreeMap::new(); for t in txs { *cc.entry(&t.category).or_default() += 1; }
        let major = cc.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0))).unwrap().0.to_string(); let group = major.split('.').next().unwrap_or("").to_string();
        let discretionary = in_list(&major, &s.recurring_discretionary_groups) || s.recurring_discretionary_groups.contains(&group);
        let pair_ok = s.recurring_yearly_pair_groups.contains(&group);
        let minocc = if discretionary { s.recurring_min_occurrences.max(s.recurring_discretionary_min_occurrences) } else { s.recurring_min_occurrences };
        let accept = |n: usize, c: &str| n >= minocc || ((c == "yearly" || c == "quarterly") && n >= 2 && pair_ok && !discretionary);
        let try_one = |c: &[Tx], allow_var: bool| -> Option<Series> {
            let (cadence, fit) = detect_cadence(&c.iter().map(|t| t.date).collect::<Vec<_>>())?;
            if !accept(c.len(), cadence) { return None; }
            let var_ok = allow_var && !discretionary && (in_list(&major, &s.recurring_variable_categories) || (fit >= 0.85 && c.len() >= 5 && in_list(&major, &s.recurring_loose_amount_categories)));
            let am = amount_mode(&c.iter().map(|t| t.amount_c.abs()).collect::<Vec<_>>(), tol, var_ok).filter(|a| !(discretionary && a.0 != "fixed"))?;
            Some(self.build(direction, key, account, c, cadence, am.0, fit, am.1, am.2, vec![]))
        };
        if let Some(x) = try_one(txs, true) { return vec![x]; }
        for t_ in [tol, 0.03] {
            let mut out = vec![];
            for c in clusters(txs, t_).into_iter().filter(|c| c.len() >= 2) {
                if let Some(x) = try_one(&c, false) { out.push(x); continue; }
                let l: Vec<Series> = lanes(&c).iter().filter_map(|l| try_one(l, false)).collect();
                if l.is_empty() { out.extend(dom_lanes(&c).iter().filter_map(|l| try_one(l, false))); } else { out.extend(l); }
            }
            if !out.is_empty() { return out; }
        }
        lanes(txs).iter().filter_map(|l| try_one(l, true)).collect()
    }
}
/// Several payments on one day are separate series: the k-th largest payment of each day belongs to lane k.
fn lanes(txs: &[Tx]) -> Vec<Vec<Tx>> {
    let mut by: BTreeMap<NaiveDate, Vec<&Tx>> = BTreeMap::new(); for t in txs { by.entry(t.date).or_default().push(t); }
    if by.values().filter(|v| v.len() > 1).count() < 2 { return vec![]; }
    let mut l: Vec<Vec<Tx>> = vec![];
    for v in by.values_mut() { v.sort_by(|a, b| (b.amount_c.abs(), &b.key).cmp(&(a.amount_c.abs(), &a.key))); for (k, t) in v.iter().enumerate() { if l.len() <= k { l.push(vec![]); } l[k].push((*t).clone()); } }
    l.into_iter().filter(|x| x.len() >= 2).collect()
}
/// Two lines of similar size billed on different days of the month: split by day of month.
fn dom_lanes(txs: &[Tx]) -> Vec<Vec<Tx>> {
    let mut o = txs.to_vec(); o.sort_by(|a, b| (a.date.day(), a.date, &a.key).cmp(&(b.date.day(), b.date, &b.key)));
    let mut l: Vec<Vec<Tx>> = vec![];
    for t in o { match l.last_mut() { Some(x) if t.date.day() - x.last().unwrap().date.day() <= 3 => x.push(t), _ => l.push(vec![t]) } }
    if l.len() < 2 { return vec![]; }
    l.into_iter().filter(|x| x.len() >= 2).map(|mut x| { x.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key))); x }).collect()
}

pub struct Recurring { pub series: Vec<Series>, pub json: Value }
pub fn detect(ds: &Dataset, scope: &Scope, include_ended: bool) -> Recurring {
    let ctx = Ctx { ds, ms: memory::matchers(&ds.memory_dir).into_iter().filter_map(|m| rx_i(&m.pattern).map(|r| (m, r))).collect() };
    let allowed: HashSet<String> = ds.uids_in(scope).into_iter().collect(); let whole = ds.whole();
    let mut groups: BTreeMap<(String, String, String), Vec<Tx>> = BTreeMap::new();
    for t in whole.iter().filter(|t| t.amount_c != 0 && t.category != "income.refund" && allowed.contains(&t.account)) { groups.entry((if t.amount_c < 0 { "out" } else { "in" }.into(), group_key(ds, t), t.account.clone())).or_default().push(t.clone()); }
    let mut series: Vec<Series> = vec![];
    let mut by_brand: BTreeMap<(String, String, String), Vec<(String, Vec<Tx>, Vec<String>)>> = BTreeMap::new();
    for ((dir, key, acc), mut txs) in groups {
        if txs.len() < 2 { continue; }
        txs.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key)));
        let got = ctx.detect_group(&dir, &key, &acc, &txs);
        let brand = key.split_whitespace().next().filter(|_| !key.starts_with("sepa:")).map(str::to_lowercase).filter(|b| b.chars().count() >= 4);
        if let Some(b) = brand { by_brand.entry((dir.clone(), acc.clone(), b)).or_default().push((key.clone(), txs.clone(), got.iter().map(|x| x.id.clone()).collect())); }
        series.extend(got);
    }
    for ((dir, acc, _), parts) in by_brand.into_iter().filter(|(_, p)| p.len() >= 2) {
        let mut pool: Vec<Tx> = parts.iter().flat_map(|p| p.1.clone()).collect(); pool.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key)));
        let key = parts.iter().max_by_key(|p| p.1.len()).unwrap().0.clone();
        let pooled = ctx.detect_group(&dir, &key, &acc, &pool);
        let old: HashSet<&String> = parts.iter().flat_map(|p| &p.2).collect();
        let old_n: usize = series.iter().filter(|x| old.contains(&x.id)).map(|x| x.occ.len()).sum();
        if !pooled.is_empty() && pooled.iter().map(|x| x.occ.len()).sum::<usize>() > old_n { series.retain(|x| !old.contains(&x.id)); series.extend(pooled); }
    }
    let by_key: HashMap<&str, &Tx> = whole.iter().map(|t| (t.key.as_str(), t)).collect();
    let txs_of = |x: &Series| x.occ.iter().filter_map(|o| by_key.get(o.0.as_str()).map(|t| (*t).clone())).collect::<Vec<Tx>>();
    // merge successors: the same payment under a varying descriptor
    let tol = ds.settings.recurring_amount_tolerance;
    let mut pool = series; pool.sort_by(|a, b| (&a.direction, &a.account, &a.cadence, &a.category, a.first_date, &a.id).cmp(&(&b.direction, &b.account, &b.cadence, &b.category, b.first_date, &b.id)));
    let mut merged_out = vec![];
    for i in 0..pool.len() {
        let a = pool[i].clone(); let mut merged = false;
        #[allow(clippy::needless_range_loop)]
        for j in i + 1..pool.len() {
            let b = &pool[j];
            if (&b.direction, &b.account, &b.cadence, &b.category) != (&a.direction, &a.account, &a.cadence, &a.category) { break; }
            let nominal = cad(&a.cadence).1 as f64;
            if ((a.expected_c.abs() - b.expected_c.abs()).abs() as f64) <= tol * a.expected_c.abs() as f64 && a.entity != b.entity && a.amount_mode == b.amount_mode && a.first_date <= b.last_date && b.first_date <= a.last_date + Duration::days((nominal * 1.5) as i64) {
                let mut txs = [txs_of(&a), txs_of(b)].concat(); txs.sort_by(|x, y| (x.date, &x.key).cmp(&(y.date, &y.key)));
                let c = detect_cadence(&txs.iter().map(|t| t.date).collect::<Vec<_>>());
                let am = amount_mode(&txs.iter().map(|t| t.amount_c.abs()).collect::<Vec<_>>(), tol, a.amount_mode == "variable");
                if let (Some((cc, fit)), Some(am)) = (c, am) { if cc == a.cadence && fit >= 0.8 {
                    let mut al: Vec<String> = a.aliases.iter().chain(&b.aliases).cloned().chain([a.entity.clone()]).collect(); al.sort(); al.dedup();
                    pool[j] = ctx.build(&a.direction, &b.key, &a.account, &txs, cc, am.0, fit, am.1, am.2, al); merged = true; break;
                } }
            }
        }
        if !merged { merged_out.push(a); }
    }
    // absorb renamed payers: a payment of another payer in the cadence window after an overdue series is its missed occurrence
    let mut used: HashSet<String> = merged_out.iter().flat_map(|x| x.occ.iter().map(|o| o.0.clone())).collect();
    let mut series = vec![];
    for x in merged_out {
        if x.status != "active" || x.next_expected.is_none_or(|n| n >= ds.today) { series.push(x); continue; }
        let (lo, hi) = cad(&x.cadence).2; let r = x.expected_c.abs();
        let cand = whole.iter().filter(|t| !used.contains(&t.key) && t.account == x.account && (t.amount_c < 0) == (x.expected_c < 0) && x.last_date < t.date && t.date <= ds.today && (lo..=hi + 5).contains(&(t.date - x.last_date).num_days())
            && ((t.amount_c.abs() - r).abs() as f64) <= tol * r as f64 && t.category.split('.').next() == x.category.split('.').next()).min_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key)));
        let Some(t) = cand else { series.push(x); continue };
        let mut txs = txs_of(&x); txs.push(t.clone());
        let c = detect_cadence(&txs.iter().map(|t| t.date).collect::<Vec<_>>()); let am = amount_mode(&txs.iter().map(|t| t.amount_c.abs()).collect::<Vec<_>>(), tol, x.amount_mode == "variable");
        match (c, am) { (Some((cc, fit)), Some(am)) if cc == x.cadence => { used.insert(t.key.clone()); let mut al: Vec<String> = x.aliases.iter().cloned().chain([t.entity.clone()]).filter(|e| *e != x.entity).collect(); al.sort(); al.dedup(); series.push(ctx.build(&x.direction, &x.key, &x.account, &txs, cc, am.0, fit, am.1, am.2, al)); } _ => series.push(x) }
    }
    carry_ids(ds, &mut series);
    if !include_ended { series.retain(|x| x.status == "active"); }
    series.sort_by(|a, b| (a.status != "active", -a.yearly_c, &a.id).cmp(&(b.status != "active", -b.yearly_c, &b.id)));
    let active: Vec<&Series> = series.iter().filter(|x| x.status == "active").collect();
    let by_cadence: serde_json::Map<String, Value> = CADENCES.iter().filter(|c| active.iter().any(|x| x.cadence == c.0)).map(|c| (c.0.to_string(), active.iter().filter(|x| x.cadence == c.0).count().into())).collect();
    let uids: Vec<String> = series.iter().map(|x| x.account.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let cov = ds.coverage.info(&uids, &[], &format!("history of each series; a series needs >= {} occurrences (2 for yearly, low confidence)", ds.settings.recurring_min_occurrences), ds.base_notes());
    let json = json!({"as_of": ds.today.to_string(), "series": series.iter().map(Series::to_json).collect::<Vec<_>>(), "by_cadence": by_cadence,
        "counts": {"active": active.len(), "ended": series.len() - active.len(), "missing_contract": active.iter().filter(|x| x.missing_contract).count()}, "scope": scope.describe(), "coverage": cov, "evidence": series.iter().map(|x| x.id.clone()).collect::<Vec<_>>()});
    Recurring { series, json }
}
/// Id continuity: a series keeps the id of the stored series it shares most payments with (at least half of the smaller one).
fn carry_ids(ds: &Dataset, series: &mut [Series]) {
    let stored = &ds.stored_members; if stored.is_empty() { return; }
    let mut taken: HashSet<String> = series.iter().filter(|x| stored.contains_key(&x.id)).map(|x| x.id.clone()).collect();
    let mut order: Vec<usize> = (0..series.len()).collect(); order.sort_by(|a, b| series[*a].id.cmp(&series[*b].id));
    for i in order {
        if stored.contains_key(&series[i].id) { continue; }
        let mine: BTreeSet<String> = series[i].occ.iter().map(|o| o.0.clone()).collect();
        let best = stored.iter().filter(|(k, _)| !taken.contains(*k)).max_by(|a, b| (a.1.intersection(&mine).count(), a.0).cmp(&(b.1.intersection(&mine).count(), b.0)));
        if let Some((id, m)) = best { if m.intersection(&mine).count() * 2 >= mine.len().min(m.len()) && m.intersection(&mine).count() > 0 { series[i].id = id.clone(); taken.insert(id.clone()); } }
    }
}

/// Store the detection: upsert by stable id, delete series that are gone. Idempotent.
pub fn refresh(con: &rusqlite::Connection, ds: &Dataset) -> Result<Value> {
    let rec = detect(ds, &Scope::default(), true); let stamp = now_iso();
    let old: HashMap<String, (String, String)> = con.prepare("SELECT id, payload, first_detected_at FROM recurring_series")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?;
    let (mut created, mut updated, mut unchanged) = (0, 0, 0);
    con.execute_batch("BEGIN")?;
    let res = (|| -> Result<usize> {
        for x in &rec.series {
            let payload = serde_json::to_string(&sorted(&x.to_json()))?;
            if old.get(&x.id).is_some_and(|o| o.0 == payload) { unchanged += 1; continue; }
            let first = old.get(&x.id).map_or(stamp.clone(), |o| o.1.clone());
            con.execute("INSERT OR REPLACE INTO recurring_series(id, direction, kind, group_key, account_uid, entity, category, cadence, amount_mode, n_occurrences, first_date, last_date, next_expected, expected_amount_c, amount_low_c, amount_high_c, yearly_cost_c, status, confidence, payload, first_detected_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![x.id, x.direction, x.kind, x.key, x.account, x.entity, x.category, x.cadence, x.amount_mode, x.occ.len() as i64, x.first_date.to_string(), x.last_date.to_string(), x.next_expected.map(|d| d.to_string()), x.expected_c, x.low_c, x.high_c, x.yearly_c, x.status, x.confidence, payload, first, stamp])?;
            con.execute("DELETE FROM recurring_members WHERE series_id=?", [&x.id])?;
            for (k, dd, a) in &x.occ { con.execute("INSERT OR IGNORE INTO recurring_members(series_id, tx_key, date, amount_c) VALUES (?,?,?,?)", rusqlite::params![x.id, k, dd.to_string(), a])?; }
            if old.contains_key(&x.id) { updated += 1 } else { created += 1 }
        }
        let keep: HashSet<&String> = rec.series.iter().map(|x| &x.id).collect(); let gone: Vec<&String> = old.keys().filter(|k| !keep.contains(k)).collect();
        for g in &gone { con.execute("DELETE FROM recurring_series WHERE id=?", [g])?; con.execute("DELETE FROM recurring_members WHERE series_id=?", [g])?; }
        Ok(gone.len())
    })();
    match res { Ok(removed) => { con.execute_batch("COMMIT")?; Ok(json!({"series": rec.series.len(), "created": created, "updated": updated, "removed": removed, "unchanged": unchanged})) } Err(e) => { let _ = con.execute_batch("ROLLBACK"); Err(e) } }
}
/// JSON with object keys sorted (a stable payload for change detection).
pub fn sorted(v: &Value) -> Value { match v { Value::Object(o) => { let mut k: Vec<_> = o.keys().collect(); k.sort(); Value::Object(k.into_iter().map(|k| (k.clone(), sorted(&o[k]))).collect()) } Value::Array(a) => Value::Array(a.iter().map(sorted).collect()), x => x.clone() } }

// ---------------------------------------------------------------- price changes
/// Price changes of active expense / income series: a confirmed step, an unconfirmed latest one, or (variable series) the last 3
/// against the 3 before with the wide threshold. `delta` > 0 = the payment got bigger.
pub fn price_changes(ds: &Dataset, rec: &Recurring, since: Option<NaiveDate>, only_confirmed: bool, dismissed: &HashSet<String>, include_dismissed: bool) -> Value {
    let s = &ds.settings; let min_abs = (s.price_change_min_abs * 100.0).round() as i64; let mut found = vec![];
    for x in rec.series.iter().filter(|x| (x.kind == "expense" || x.kind == "income") && x.status == "active") {
        let a: Vec<i64> = x.occ.iter().map(|o| o.2.abs()).collect();
        let steps: Vec<(usize, i64, i64, bool, bool)> = if x.amount_mode == "variable" {
            if a.len() >= 6 { let (p, l) = (median_c(&a[a.len() - 6..a.len() - 3]), median_c(&a[a.len() - 3..])); if p != 0 && ((l - p).abs() as f64 / p as f64) >= s.price_change_variable_pct / 100.0 && (l - p).abs() >= min_abs { vec![(a.len() - 3, p, l, false, true)] } else { vec![] } } else { vec![] }
        } else { levels(&a, if s.recurring_variable_categories.contains(&x.category) { s.price_change_variable_pct } else { s.price_change_threshold_pct }, min_abs).into_iter().map(|(i, o, n, c)| (i, o, n, c, false)).collect() };
        for (i, old, new, confirmed, variable) in steps {
            let (k, dd, _) = &x.occ[i]; let delta = new - old; let pct = if old != 0 { round4(delta as f64 / old as f64) } else { 0.0 };
            if x.kind == "income" && !(confirmed && pct.abs() * 100.0 >= s.price_change_income_pct) { continue; }
            let id = format!("chg_{}", &sha1_hex(&format!("{}|{k}", x.id))[..10]); let dis = dismissed.contains(&id);
            if since.is_some_and(|s| *dd < s) || (only_confirmed && !confirmed) || (dis && !include_dismissed) { continue; }
            let up = delta > 0;
            found.push((*dd, x.id.clone(), json!({"id": id, "series_id": x.id, "entity": x.entity, "account_label": x.account_label, "category": x.category, "cadence": x.cadence, "date": dd.to_string(), "tx_key": k, "old": money_str(old), "new": money_str(new), "delta": money_str(delta), "pct": pct,
                "direction": if up { "increase" } else { "decrease" }, "effect": if x.direction == "out" { if up { "costs_more" } else { "costs_less" } } else if up { "pays_more" } else { "pays_less" }, "yearly_impact": money_str(mul_cents(delta, cad(&x.cadence).4)), "confirmed": confirmed, "variable": variable, "dismissed": dis})));
        }
    }
    found.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    let ch: Vec<Value> = found.into_iter().map(|f| f.2).collect();
    let n = |k: &str, v: &str| ch.iter().filter(|c| c[k] == v).count();
    json!({"as_of": ds.today.to_string(), "counts": {"increase": n("direction", "increase"), "decrease": n("direction", "decrease"), "unconfirmed": ch.iter().filter(|c| c["confirmed"] == false).count()},
        "evidence": ch.iter().filter_map(|c| c["series_id"].as_str().map(String::from)).collect::<BTreeSet<_>>(), "changes": ch, "coverage": rec.json["coverage"].clone()})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn day(s: &str) -> NaiveDate { d(s).unwrap() }
    #[test] fn cadences() {
        let m: Vec<NaiveDate> = ["2025-01-05", "2025-02-05", "2025-03-05", "2025-04-07", "2025-05-05"].map(day).to_vec(); assert_eq!(detect_cadence(&m).unwrap().0, "monthly");
        let skip: Vec<NaiveDate> = ["2025-01-05", "2025-02-05", "2025-04-05", "2025-05-05", "2025-06-05"].map(day).to_vec(); assert_eq!(detect_cadence(&skip).unwrap().0, "monthly");
        assert!(detect_cadence(&["2025-01-01", "2025-01-03", "2025-03-01"].map(day)).is_none());
        assert_eq!(expected_date(day("2025-01-31"), "monthly", Some(31), true, 1).to_string(), "2025-02-28");
        assert_eq!(roll_business_day(day("2025-03-01")).to_string(), "2025-03-03");
    }
    #[test] fn steps() { assert_eq!(levels(&[1000, 1000, 1100, 1100], 3.0, 50), vec![(2, 1000, 1100, true)]); assert!(levels(&[1000, 1500, 1000, 1000], 3.0, 50).is_empty(), "blip"); assert_eq!(levels(&[1000, 1000, 1200], 3.0, 50), vec![(2, 1000, 1200, false)]); }
    #[test] fn keys() { assert_eq!(norm_key("AMAZON PRIME*9W8"), "AMAZON PRIME"); assert_eq!(norm_key("SHOP P30FCBC3 GENT"), "SHOP GENT"); }
}
