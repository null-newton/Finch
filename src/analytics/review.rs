//! The year in review: the year's cash flow (complete months apart), events, top merchants, and the categories that moved most against
//! the year before (coverage-aware averages of each year; a category without enough covered months in either year is listed apart).
use super::{cashflow::cashflow, is_income, is_spending, Dataset, Scope, Tx};
use crate::util::*;
use chrono::Datelike;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn sum_rows(rows: &[&Value]) -> Value {
    let keys = ["income", "spending", "saved", "net", "one_off_spending", "debt_service", "refunds"];
    let mut o: serde_json::Map<String, Value> = keys.iter().map(|k| (k.to_string(), json!(money_str(rows.iter().map(|r| r[*k].as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents)).sum())))).collect();
    let (inc, net) = (rows.iter().map(|r| r["income"].as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents)).sum::<i64>(), rows.iter().map(|r| r["net"].as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents)).sum::<i64>());
    o.insert("savings_rate".into(), json!(pct(net, inc))); o.insert("months".into(), json!(rows.len())); Value::Object(o)
}
/// Average monthly spending of a category over the covered months of a year: (cents, months).
fn year_avg(ds: &Dataset, sp: &[&Tx], cat: &str, year: i32, sc: &Scope) -> Option<(i64, usize)> {
    let carriers = ds.carriers_split(cat, sc).0; let ms: Vec<String> = ds.coverage.common_months(&carriers).into_iter().filter(|m| m.starts_with(&year.to_string())).collect(); if ms.is_empty() { return None; }
    let tot = -sp.iter().filter(|t| t.category == cat && ms.contains(&t.month()) && !t.is_one_off()).map(|t| t.amount_c).sum::<i64>(); Some((div_cents(tot, ms.len() as i64), ms.len()))
}
pub fn year_review(ds: &Dataset, year: Option<i32>, sc: &Scope, top: usize) -> Value {
    let lc = last_closed_month(ds.today); let year = year.unwrap_or_else(|| lc[..4].parse().unwrap()); let end_m = format!("{year:04}-12").min(lc.clone());
    let n = if end_m.starts_with(&format!("{year:04}")) { end_m[5..].parse::<usize>().unwrap_or(0) } else { 0 };
    let cf = (n > 0).then(|| cashflow(ds, sc, n, Some(&end_m), false, false)); let rows: Vec<&Value> = cf.as_ref().and_then(|c| c["household"]["months"].as_array()).map(|a| a.iter().filter(|r| r["month"].as_str().is_some_and(|m| m.starts_with(&format!("{year:04}-")))).collect()).unwrap_or_default();
    let complete: Vec<&Value> = rows.iter().copied().filter(|r| r["complete"] == true).collect(); let partial = end_m < format!("{year:04}-12") || complete.len() < 12;
    let txs: Vec<&Tx> = ds.txs_in(sc).into_iter().filter(|t| t.date.year() == year).collect();
    let mut ev: BTreeMap<String, Vec<&Tx>> = BTreeMap::new(); for t in &txs { if let Some(e) = &t.event { if (is_spending(&t.category) || is_income(&t.category)) && !t.is_saved() { ev.entry(e.clone()).or_default().push(t); } } }
    let mut events: Vec<(i64, Value)> = ev.iter().map(|(n, ts)| { let sp = -ts.iter().filter(|t| is_spending(&t.category)).map(|t| t.amount_c).sum::<i64>();
        (sp, json!({"event": n, "spending": money_str(sp), "income": money_str(ts.iter().filter(|t| is_income(&t.category)).map(|t| t.amount_c).sum()), "n_tx": ts.len(), "first": ts.iter().map(|t| t.date).min().unwrap().to_string(), "last": ts.iter().map(|t| t.date).max().unwrap().to_string()})) }).collect();
    events.sort_by(|a, b| b.0.cmp(&a.0));
    let mut ents: BTreeMap<String, Vec<&Tx>> = BTreeMap::new(); for t in &txs { if is_spending(&t.category) && !t.is_saved() { ents.entry(if t.entity.is_empty() { t.mkey.clone() } else { t.entity.clone() }).or_default().push(t); } }
    let mut tops: Vec<(i64, Value)> = ents.iter().filter_map(|(n, ts)| { let tot = -ts.iter().map(|t| t.amount_c).sum::<i64>(); (tot > 0).then(|| { let mut c: BTreeMap<&str, i64> = BTreeMap::new(); for t in ts { *c.entry(&t.category).or_default() -= t.amount_c; }
        (tot, json!({"entity": n, "spending": money_str(tot), "one_off": money_str(-ts.iter().filter(|t| t.is_one_off()).map(|t| t.amount_c).sum::<i64>()), "n_tx": ts.len(), "category": c.iter().max_by_key(|x| x.1).map(|x| *x.0)})) }) }).collect();
    tops.sort_by(|a, b| b.0.cmp(&a.0));
    let sp: Vec<&Tx> = ds.txs_in(sc).into_iter().filter(|t| is_spending(&t.category) && !t.is_saved()).collect(); let cats: std::collections::BTreeSet<&str> = sp.iter().filter(|t| [year, year - 1].contains(&t.date.year())).map(|t| t.category.as_str()).collect();
    let (mut ch, mut notcomp) = (vec![], vec![]); let mn = ds.settings.average_min_months;
    for c in cats { match (year_avg(ds, &sp, c, year, sc), year_avg(ds, &sp, c, year - 1, sc)) { (Some(a), Some(b)) if a.1 >= mn && b.1 >= mn => ch.push((a.0 - b.0, json!({"category": c, "monthly_avg": money_str(a.0), "monthly_avg_prev": money_str(b.0), "delta": money_str(a.0 - b.0), "pct": if b.0 != 0 { json!(pct(a.0 - b.0, b.0)) } else { Value::Null }, "months": a.1, "months_prev": b.1, "partial": a.1 < 12 || b.1 < 12}))),
        (a, b) => notcomp.push(json!({"category": c, "months_year": a.map_or(0, |x| x.1), "months_prev": b.map_or(0, |x| x.1), "reason": format!("fewer than {mn} fully covered months in one of the two years")})) } }
    let mut inc: Vec<&(i64, Value)> = ch.iter().filter(|c| c.0 > 0).collect(); inc.sort_by(|a, b| b.0.cmp(&a.0)); let mut dec: Vec<&(i64, Value)> = ch.iter().filter(|c| c.0 < 0).collect(); dec.sort_by(|a, b| a.0.cmp(&b.0));
    let mut notes = vec![]; if partial { notes.push(format!("{year} is partial: {} complete month(s) of {} listed{}", complete.len(), rows.len(), if end_m >= format!("{year:04}-12") { String::new() } else { format!(", year not over (data to {end_m})") })); }
    if !rows.is_empty() && complete.is_empty() { notes.push("no month of the year is complete: income and net are NOT meaningful".into()); }
    json!({"year": year, "previous_year": year - 1, "partial": partial, "months_listed": rows.len(), "months_complete": complete.len(), "flow": {"months": rows, "totals_complete": sum_rows(&complete), "totals_all": sum_rows(&rows)},
        "events": events.into_iter().map(|e| e.1).collect::<Vec<_>>(), "top_merchants": tops.into_iter().take(top).map(|t| t.1).collect::<Vec<_>>(), "biggest_increases": inc.into_iter().take(top).map(|c| c.1.clone()).collect::<Vec<_>>(),
        "biggest_decreases": dec.into_iter().take(top).map(|c| c.1.clone()).collect::<Vec<_>>(), "not_comparable": notcomp, "notes": notes,
        "evidence": txs.iter().filter(|t| t.is_one_off()).map(|t| t.key.clone()).collect::<Vec<_>>()})
}
