//! The year in review: the year's cash flow (complete months apart), events, top merchants, and the categories that moved most against
//! the year before (coverage-aware averages of each year; a category without enough covered months in either year is listed apart).
//! The monthly review and a spike explained compare a month with its USUAL: the average spending (one-offs apart) of the months before
//! it that every major account carrying the category covers; no such month = no baseline (listed apart, never a made-up figure).
use super::{anomalies, averages::{is_lumpy, spending_txs}, cashflow::cashflow, is_income, is_spending, last_n, planning, recurring::{self, Recurring}, Dataset, Scope, Tx};
use crate::util::*;
use chrono::Datelike;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
    let mut inc: Vec<&(i64, Value)> = ch.iter().filter(|c| c.0 > 0).collect(); inc.sort_by(|a, b| b.0.cmp(&a.0)); let mut dec: Vec<&(i64, Value)> = ch.iter().filter(|c| c.0 < 0).collect(); dec.sort_by_key(|a| a.0);
    let mut notes = vec![]; if partial { notes.push(format!("{year} is partial: {} complete month(s) of {} listed{}", complete.len(), rows.len(), if end_m >= format!("{year:04}-12") { String::new() } else { format!(", year not over (data to {end_m})") })); }
    if !rows.is_empty() && complete.is_empty() { notes.push("no month of the year is complete: income and net are NOT meaningful".into()); }
    json!({"year": year, "previous_year": year - 1, "partial": partial, "months_listed": rows.len(), "months_complete": complete.len(), "flow": {"months": rows, "totals_complete": sum_rows(&complete), "totals_all": sum_rows(&rows)},
        "events": events.into_iter().map(|e| e.1).collect::<Vec<_>>(), "top_merchants": tops.into_iter().take(top).map(|t| t.1).collect::<Vec<_>>(), "biggest_increases": inc.into_iter().take(top).map(|c| c.1.clone()).collect::<Vec<_>>(),
        "biggest_decreases": dec.into_iter().take(top).map(|c| c.1.clone()).collect::<Vec<_>>(), "not_comparable": notcomp, "notes": notes,
        "evidence": txs.iter().filter(|t| t.is_one_off()).map(|t| t.key.clone()).collect::<Vec<_>>()})
}

const MOVER_MIN_C: i64 = 2000;
fn check_month(ds: &Dataset, month: Option<&str>) -> Result<String, String> { let m = month.map(String::from).unwrap_or_else(|| last_closed_month(ds.today)); if m > month_key(ds.today) { return Err("that month has not started yet".into()); } Ok(m) }
fn window(ds: &Dataset, cat: &str) -> usize { let s = &ds.settings; if is_lumpy(cat, &s.lumpy_categories) { s.average_window_months.max(s.lumpy_min_months) } else { s.average_window_months } }
fn spent(ts: &[&Tx], f: impl Fn(&Tx) -> bool) -> i64 { -ts.iter().filter(|t| f(t)).map(|t| t.amount_c).sum::<i64>() }
fn c(v: &Value) -> i64 { v.as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents) }
/// One month against usual: its cash flow, the biggest movers with their evidence, the one-offs apart, budgets at month end, the forecast flags.
pub fn monthly_review(ds: &Dataset, month: Option<&str>, rec: &Recurring, top: usize, dismissed_anm: &HashSet<String>, dismissed_chg: &HashSet<String>) -> Result<Value, String> {
    let sc = Scope::default(); let month = check_month(ds, month)?; let cur = month_key(ds.today); let sp = spending_txs(ds, &sc); let mut notes: Vec<String> = vec![];
    let mut by_cat: BTreeMap<&str, Vec<&Tx>> = BTreeMap::new(); for t in &sp { by_cat.entry(&t.category).or_default().push(t); }
    let cf = cashflow(ds, &sc, 1, (month != cur).then_some(month.as_str()), month == cur, false); let mf = cf["household"]["months"].as_array().and_then(|m| m.iter().find(|r| r["month"] == month.as_str())).cloned();
    let flow = mf.as_ref().map(|m| { let mut f = m.clone(); if let Some(o) = f.as_object_mut() { o.remove("n_tx"); }
        let sav: BTreeSet<&String> = ds.accounts.values().filter(|a| a.purpose.as_deref() == Some("savings")).map(|a| &a.uid).collect(); let inf: Vec<&Tx> = ds.txs.iter().filter(|t| t.month() == month && sav.contains(&t.account) && t.category == "transfer.internal").collect();
        f["deposited_to_savings_accounts"] = money_str(inf.iter().filter(|t| t.amount_c > 0).map(|t| t.amount_c).sum()).into(); f["drawn_from_savings_accounts"] = money_str(-inf.iter().filter(|t| t.amount_c < 0).map(|t| t.amount_c).sum::<i64>()).into();
        if m["complete"] == false { notes.push(format!("the month is not fully covered by every account ({} lack data): figures are partial", m["missing_accounts"].as_array().map_or(0, |a| a.len()))); } f });
    if flow.is_none() { notes.push("no data for this month".into()); } if month == cur { notes.push("the month is still running: figures are month to date and 'usual' is a full month".into()); }
    struct Row { cat: String, run: i64, one: i64, n: usize, complete: bool, usual: Option<i64>, months: usize }
    let mut rows = vec![];
    for (cat, txs) in &by_cat {
        let carriers = ds.carriers_split(cat, &sc).0; let covered = ds.coverage.common_months(&carriers); let bm = last_n(&covered.iter().filter(|m| **m < month).cloned().collect::<Vec<_>>(), window(ds, cat));
        let in_m: Vec<&Tx> = txs.iter().copied().filter(|t| t.month() == month).collect(); let usual = (!bm.is_empty()).then(|| div_cents(spent(txs, |t| bm.contains(&t.month()) && !t.is_one_off()), bm.len() as i64));
        if in_m.is_empty() && usual.is_none_or(|u| u < MOVER_MIN_C) { continue; }
        rows.push(Row { cat: cat.to_string(), run: spent(&in_m, |t| !t.is_one_off()), one: spent(&in_m, |t| t.is_one_off()), n: in_m.len(), complete: covered.contains(&month), usual, months: bm.len() });
    }
    let anm = anomalies::detect(ds, &sc, rec, dismissed_anm, false); let chg = recurring::price_changes(ds, rec, None, false, dismissed_chg, false); let inm = |v: &Value, k: &str| v[k].as_str().is_some_and(|p| p.starts_with(&month));
    let rj = |r: &Row| { let mut v = json!({"category": r.cat, "spent": money_str(r.run), "one_off": money_str(r.one), "n_tx": r.n, "month_complete": r.complete});
        match r.usual { Some(u) => { v["usual"] = money_str(u).into(); v["delta"] = money_str(r.run - u).into(); v["pct"] = json!(pct(r.run - u, u)); v["usual_months"] = r.months.into(); v["low_confidence"] = (r.months < ds.settings.average_min_months).into(); } None => v["baseline"] = Value::Null } v };
    let mut movers: Vec<&Row> = rows.iter().filter(|r| r.usual.is_some_and(|u| (r.run - u).abs() >= MOVER_MIN_C)).collect(); movers.sort_by(|a, b| (-(a.run - a.usual.unwrap()).abs(), &a.cat).cmp(&(-(b.run - b.usual.unwrap()).abs(), &b.cat)));
    let movers: Vec<Value> = movers.into_iter().take(top).map(|r| { let mut v = rj(r); v["direction"] = if r.run > r.usual.unwrap() { "up" } else { "down" }.into();
        let mut ev: Vec<&&Tx> = by_cat[r.cat.as_str()].iter().filter(|t| t.month() == month && !t.is_one_off()).collect(); ev.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
        // one evidence list: transactions (hashed on the way out), then rec_ series, anm_ anomalies and chg_ price changes (kept as they are)
        let mut e: Vec<String> = ev.iter().take(3).map(|t| t.key.clone()).collect(); e.extend(rec.series.iter().filter(|x| x.category == r.cat && x.status == "active" && x.occ.iter().any(|o| month_key(o.1) == month)).take(5).map(|x| x.id.clone()));
        e.extend(anm["anomalies"].as_array().into_iter().flatten().filter(|a| a["subject"] == r.cat.as_str() && inm(a, "period")).take(3).filter_map(|a| a["id"].as_str().map(String::from)));
        e.extend(chg["changes"].as_array().into_iter().flatten().filter(|x| x["category"] == r.cat.as_str() && inm(x, "date")).take(3).filter_map(|x| x["id"].as_str().map(String::from))); v["evidence"] = e.into(); v }).collect();
    let based: Vec<&Row> = rows.iter().filter(|r| r.usual.is_some()).collect(); let (comp, usual_t) = (based.iter().map(|r| r.run).sum::<i64>(), based.iter().map(|r| r.usual.unwrap()).sum::<i64>());
    let mut nobase: Vec<&Row> = rows.iter().filter(|r| r.usual.is_none()).collect(); nobase.sort_by(|a, b| a.cat.cmp(&b.cat));
    let mut oneoffs: Vec<&&Tx> = sp.iter().filter(|t| t.month() == month && t.is_one_off()).collect(); oneoffs.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
    let as_of = if month < cur { month_end(&month) } else { ds.today }; let bs = planning::status(ds, rec, Some(as_of));
    let budgets: Vec<Value> = bs["budgets"].as_array().into_iter().flatten().map(|b| json!({"id": b["id"], "target": b["target"], "monthly": b["monthly"], "available": b["available"], "spent": b["spent"], "remaining": b["remaining"], "percent_used": b["percent_used"], "flags": b["flags"],
        "status": if month < cur { if c(&b["spent"]) > c(&b["available"]) { json!("over") } else { json!("ok") } } else { b["status"].clone() }})).collect();
    let cnt = |k: &str| budgets.iter().filter(|b| b["status"] == k).count();
    let fc = planning::forecast(ds, rec, &sc, 60, false)["household"].clone(); if month != cur && month != last_closed_month(ds.today) { notes.push("the forecast looks ahead from today, not from the reviewed month".into()); }
    let complete = mf.as_ref().is_some_and(|m| m["complete"] == true);
    Ok(json!({"month": month, "partial_month": month == cur, "cash_flow": flow,
        "against_usual": {"month_spending_ex_one_offs": money_str(rows.iter().map(|r| r.run).sum()), "comparable_month_spending": money_str(comp), "usual_for_those_categories": money_str(usual_t), "delta": money_str(comp - usual_t), "pct": pct(comp - usual_t, usual_t), "categories_without_baseline": nobase.len()},
        "movers": movers, "categories_without_baseline": nobase.iter().take(5).map(|r| rj(r)).collect::<Vec<_>>(),
        "one_offs": {"total": money_str(-oneoffs.iter().map(|t| t.amount_c).sum::<i64>()), "n": oneoffs.len(), "items": oneoffs.iter().take(8).map(|t| json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "category": t.category, "tags": t.tags, "entity": t.entity})).collect::<Vec<_>>()},
        "anomalies": anm["anomalies"].as_array().into_iter().flatten().filter(|a| inm(a, "period")).take(6).map(|a| json!({"id": a["id"], "type": a["type"], "severity": a["severity"], "subject": a["subject"], "amount": a["amount"], "evidence": a["evidence"].as_array().map(|e| e.iter().take(3).cloned().collect::<Vec<_>>())})).collect::<Vec<_>>(),
        "budgets": {"as_of": as_of.to_string(), "counts": {"over": cnt("over"), "at_risk": cnt("at_risk"), "ok": cnt("ok")}, "items": budgets, "unbudgeted_top": bs["unbudgeted"].as_array().map(|u| u.iter().take(5).cloned().collect::<Vec<_>>())},
        "forecast": {"as_of": ds.today.to_string(), "start_balance": fc["start_balance"], "min_balance": fc["min_balance"], "min_date": fc["min_date"], "first_negative": fc["first_negative"], "first_at_risk": fc["first_at_risk"], "flags": fc["flags"], "milestones": fc["milestones"]},
        "coverage": ds.coverage.info(&ds.accounts.keys().cloned().collect::<Vec<_>>(), &if complete { vec![month.clone()] } else { vec![] }, "category baselines: the months before the reviewed month that every major account of the category covers", notes)}))
}
/// Why a category (or group: `leaves`) or an account (`account` uid) was high in a month: the month split into one-off, recurring, new-merchant and
/// habitual spending, each merchant's change against its own usual (`delta = month - usual`), the top transactions and the same month a year before.
pub fn explain_spike(ds: &Dataset, leaves: Option<&[String]>, account: Option<&str>, target: &str, month: Option<&str>, rec: &Recurring, top: usize) -> Result<Value, String> {
    if leaves.is_some() == account.is_some() { return Err("give exactly one of category (a category or a group) or account".into()); }
    let month = check_month(ds, month)?; let sp = spending_txs(ds, &Scope::default());
    let (txs, carriers): (Vec<&Tx>, Vec<String>) = match (leaves, account) { (Some(l), _) => { let txs: Vec<&Tx> = sp.iter().copied().filter(|t| l.contains(&t.category)).collect(); let mut cr: BTreeSet<String> = l.iter().flat_map(|c| ds.carriers_split(c, &Scope::default()).0).collect();
            if cr.is_empty() { cr = txs.iter().map(|t| t.account.clone()).collect(); } (txs, cr.into_iter().collect()) } (_, Some(u)) => (sp.iter().copied().filter(|t| t.account == u).collect(), vec![u.to_string()]), _ => unreachable!() };
    let covered: BTreeSet<String> = ds.coverage.common_months(&carriers).into_iter().collect(); let bm = last_n(&covered.iter().filter(|m| **m < month).cloned().collect::<Vec<_>>(), ds.settings.average_window_months);
    let (bset, n): (BTreeSet<&String>, i64) = (bm.iter().collect(), bm.len() as i64); let in_m: Vec<&Tx> = txs.iter().copied().filter(|t| t.month() == month).collect();
    let members: HashMap<&str, &str> = rec.series.iter().flat_map(|x| x.occ.iter().map(move |o| (o.0.as_str(), x.id.as_str()))).collect();
    let mut first: HashMap<&str, chrono::NaiveDate> = HashMap::new(); for t in sp.iter().filter(|t| t.amount_c < 0) { let e = first.entry(&t.entity).or_insert(t.date); if t.date < *e { *e = t.date; } }
    let (m0, m1) = (month_start(&month), month_end(&month)); let new = |t: &Tx| first.get(t.entity.as_str()).is_some_and(|f| *f >= m0 && *f <= m1);
    let klass = |t: &Tx| if t.is_one_off() { "one_off" } else if members.contains_key(t.key.as_str()) { "recurring" } else if new(t) { "new_merchant" } else { "habitual" };
    let mut classes: BTreeMap<&str, i64> = ["one_off", "recurring", "new_merchant", "habitual"].iter().map(|k| (*k, 0)).collect(); for t in &in_m { *classes.get_mut(klass(t)).unwrap() -= t.amount_c; }
    let run = classes["recurring"] + classes["new_merchant"] + classes["habitual"]; let usual = (n > 0).then(|| div_cents(spent(&txs, |t| bset.contains(&t.month()) && !t.is_one_off()), n));
    let mut ent: BTreeMap<&str, (i64, usize, bool, bool)> = BTreeMap::new(); for t in in_m.iter().filter(|t| !t.is_one_off()) { let e = ent.entry(&t.entity).or_default(); e.0 -= t.amount_c; e.1 += 1; e.2 |= members.contains_key(t.key.as_str()); e.3 |= new(t); }
    let mut base: BTreeMap<&str, i64> = BTreeMap::new(); for t in txs.iter().filter(|t| bset.contains(&t.month()) && !t.is_one_off()) { *base.entry(&t.entity).or_default() -= t.amount_c; }
    let mut merchants: Vec<(i64, Value)> = vec![]; let mut by_class: BTreeMap<&str, i64> = BTreeMap::new();
    if n > 0 { for name in ent.keys().chain(base.keys()).collect::<BTreeSet<_>>() { let e = ent.get(name).copied().unwrap_or_default(); let b = base.get(name).copied().unwrap_or(0); let num = n * e.0 - b;
        let cls = if e.2 { "recurring" } else if e.3 { "new_merchant" } else if e.0 == 0 { "stopped" } else { "habitual" }; *by_class.entry(cls).or_default() += num;
        merchants.push((num, json!({"entity": name, "class": cls, "spent": money_str(e.0), "n_tx": e.1, "usual": money_str(div_cents(b, n)), "delta": money_str(div_cents(num, n))}))); }
        merchants.sort_by(|a, b| (-a.0, a.1["entity"].as_str()).cmp(&(-b.0, b.1["entity"].as_str()))); }
    let mut cats: BTreeMap<&str, Vec<&Tx>> = BTreeMap::new(); for t in txs.iter().filter(|t| t.month() == month || bset.contains(&t.month())) { cats.entry(&t.category).or_default().push(t); }
    let mut cat_rows: Vec<(i64, Value)> = if n > 0 { cats.iter().map(|(c, ts)| { let r = spent(ts, |t| t.month() == month && !t.is_one_off()); let u = div_cents(spent(ts, |t| bset.contains(&t.month()) && !t.is_one_off()), n); (r - u, json!({"category": c, "spent": money_str(r), "usual": money_str(u), "delta": money_str(r - u)})) }).collect() } else { vec![] };
    cat_rows.sort_by(|a, b| (-a.0, a.1["category"].as_str()).cmp(&(-b.0, b.1["category"].as_str())));
    let mut toptx = in_m.clone(); toptx.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
    let ly = add_months_key(&month, -12); let ly_out = if covered.contains(&ly) { let (all, r) = (spent(&txs, |t| t.month() == ly), spent(&txs, |t| t.month() == ly && !t.is_one_off())); json!({"month": ly, "available": true, "spent": money_str(all), "spent_ex_one_offs": money_str(r), "delta_ex_one_offs": money_str(run - r), "pct": pct(run - r, r)}) }
        else { let none = !txs.iter().any(|t| t.month() == ly) && !carriers.iter().any(|a| ds.coverage.first(a).is_some_and(|f| f <= month_start(&ly))); json!({"month": ly, "available": false, "reason": if none { "no data" } else { "that month is not fully covered by every account carrying this spending" }}) };
    let mut notes = vec![]; if !covered.contains(&month) { notes.push("the reviewed month is not fully covered by every account carrying this spending: figures are partial".to_string()); }
    if n == 0 { notes.push("no earlier month is fully covered: there is no baseline, the breakdown is shown without a comparison".into()); } else if (n as usize) < ds.settings.average_min_months { notes.push(format!("the baseline rests on {n} month(s) only: low confidence")); }
    Ok(json!({"scope": {"kind": if leaves.is_some() { "category" } else { "account" }, "target": target}, "month": month, "month_complete": covered.contains(&month),
        "totals": {"spent": money_str(run + classes["one_off"]), "spent_ex_one_offs": money_str(run), "usual_ex_one_offs": usual.map(money_str), "excess_ex_one_offs": usual.map(|u| money_str(run - u)), "pct": usual.and_then(|u| pct(run - u, u)), "baseline_months": n},
        "by_class": classes.iter().map(|(k, v)| (k.to_string(), json!(money_str(*v)))).collect::<serde_json::Map<_, _>>(), "excess_by_class": (n > 0).then(|| by_class.iter().map(|(k, v)| (k.to_string(), json!(money_str(div_cents(*v, n))))).collect::<serde_json::Map<_, _>>()),
        "merchants": merchants.iter().take(top).map(|m| m.1.clone()).collect::<Vec<_>>(), "largest_decreases": merchants.iter().rev().filter(|m| m.0 < 0).take(3).map(|m| m.1.clone()).collect::<Vec<_>>(), "categories": cat_rows.into_iter().take(top).map(|c| c.1).collect::<Vec<_>>(),
        "top_transactions": toptx.iter().take(top).map(|t| json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "category": t.category, "entity": t.entity, "class": klass(t), "tags": t.tags})).collect::<Vec<_>>(),
        "same_month_last_year": ly_out, "coverage": ds.coverage.info(&carriers, &bm, "baseline: months before the reviewed month that every account carrying the scope covers", notes)}))
}
