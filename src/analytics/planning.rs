//! Budgets (memory/budgets.yaml), goals (memory/goals.yaml), the cash-flow forecast and the upcoming-payments calendar.
//! * Budget suggestion: median of the monthly net spend over the last `budget_suggest_months` covered months, rounded to 5 / 10 / 50 EUR.
//! * Budget status: spent (one-offs, capital, savings apart), carry-over for rollover budgets, projection = spent + recurring payments
//!   still due this month + the pace of the non-recurring spend (plain pace in the first 3 days); over / at_risk / ok.
//! * Goals: one progress source (asset, account balance, or a tag's flows since `start`), pace = average of the last 6 covered
//!   months (or the planned contribution), projected completion, required monthly amount.
//! * Forecast: latest balance + active recurring series replayed at their cadence - the median variable spend per day, with a
//!   z x sigma x sqrt(days / 30.44) band. Loan schedules from memory/liabilities are not projected yet (the loans module comes later).
//! * Calendar: recurring occurrences, contract renewals / commitment ends / notice deadlines, consent expiries, stale assets.
use super::{recurring::{occurrences, Recurring}, *};
use crate::memory::{self, Budget, Goal};
use chrono::{Datelike, Duration};
use std::collections::HashSet;

const DAYS_PER_MONTH: f64 = 30.4375;
fn round_to(c: i64, step: i64) -> i64 { ((c as f64 / step as f64) + 0.5 * (c as f64).signum()).trunc() as i64 * step }
fn cents(x: f64) -> i64 { (x * 100.0).round() as i64 }

// ---------------------------------------------------------------- budgets
fn matches(b: &Budget, cat: &str) -> bool { match &b.category { Some(c) => c == cat, None => b.group.as_deref() == cat.split('.').next() } }
fn budget_scope(b: &Budget) -> Scope { Scope { owners: b.owner.clone().map(|o| [o].into()), accounts: b.account.clone().map(|a| [a].into()), purposes: None } }
fn month_spend<'a>(ds: &'a Dataset, b: &Budget, month: &str, uids: &HashSet<String>) -> (i64, i64, Vec<&'a Tx>) {
    let (mut spent, mut excl, mut txs) = (0, 0, vec![]);
    for t in ds.txs.iter().filter(|t| t.month() == month && uids.contains(&t.account) && is_spending(&t.category) && !t.is_saved() && matches(b, &t.category)) { if t.is_one_off() { excl -= t.amount_c } else { spent -= t.amount_c; txs.push(t) } }
    (spent, excl, txs)
}
pub fn budgets(ds: &Dataset) -> (Vec<Budget>, Vec<String>) { memory::list_file(&ds.memory_dir, "budgets.yaml", "budgets") }

pub fn suggest(ds: &Dataset, scope: &Scope, months: Option<usize>, limit: Option<usize>) -> Value {
    let n = months.unwrap_or(ds.settings.budget_suggest_months); let existing: HashMap<String, f64> = budgets(ds).0.into_iter().filter(|b| b.owner.is_none() && b.account.is_none()).filter_map(|b| b.category.map(|c| (c, b.monthly))).collect();
    let mut by: BTreeMap<String, Vec<&Tx>> = BTreeMap::new(); for t in ds.txs_in(scope).into_iter().filter(|t| is_spending(&t.category) && !t.is_saved() && !t.is_one_off()) { by.entry(t.category.clone()).or_default().push(t); }
    let (mut rows, mut skipped, mut used) = (vec![], vec![], BTreeSet::new());
    for (cat, txs) in by {
        if cat == "other.uncategorized" { skipped.push(json!({"category": cat, "reason": "not a category: classify these transactions first"})); continue; }
        let carriers = ds.carriers_split(&cat, scope).0; let ms = last_n(&ds.coverage.common_months(&carriers), n);
        if ms.is_empty() { skipped.push(json!({"category": cat, "reason": "no month fully covered by all accounts carrying it"})); continue; }
        let mut per: BTreeMap<&String, i64> = ms.iter().map(|m| (m, 0)).collect(); for t in txs { if let Some(v) = per.get_mut(&t.month()) { *v -= t.amount_c; } }
        let vals: Vec<i64> = per.values().copied().collect(); let med = median_c(&vals);
        if med < 1000 { skipped.push(json!({"category": cat, "reason": format!("median {} EUR per month is below 10 EUR", money_str(med))})); continue; }
        let sug = round_to(med, if med < 10000 { 500 } else if med < 100000 { 1000 } else { 5000 });
        rows.push((sug, cat.clone(), json!({"category": cat, "suggested": money_str(sug), "median": money_str(med), "mean": money_str(div_cents(vals.iter().sum(), vals.len() as i64)), "n_months": ms.len(), "months": ms,
            "accounts": carriers.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "existing": existing.get(&cat).map(|e| money_str(cents(*e))), "low_confidence": ms.len() < ds.settings.average_min_months})));
        used.extend(carriers);
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1))); if let Some(l) = limit { rows.truncate(l); }
    json!({"as_of": ds.today.to_string(), "suggestions": rows.into_iter().map(|r| r.2).collect::<Vec<_>>(), "skipped": skipped, "scope": scope.describe(),
        "coverage": ds.coverage.info(&used.into_iter().collect::<Vec<_>>(), &[], "median of the last N months covered by every account carrying the category", vec!["one-offs, capital and savings left out".into()])})
}

pub fn status(ds: &Dataset, rec: &Recurring, as_of: Option<NaiveDate>) -> Value {
    let as_of = as_of.unwrap_or(ds.today); let month = month_key(as_of); let elapsed = as_of.day() as i64; let dim = month_end(&month).day() as i64;
    let (list, problems) = budgets(ds); let (mut out, mut all) = (vec![], BTreeSet::new());
    for b in &list {
        let start_m = b.start.as_deref().and_then(d).map(month_key); if start_m.as_ref().is_some_and(|s| *s > month) { continue; }
        let uids: HashSet<String> = ds.uids_in(&budget_scope(b)).into_iter().collect(); all.extend(uids.iter().cloned());
        let mut flags: Vec<String> = vec![]; if uids.is_empty() { flags.push("no_matching_account".into()); }
        let monthly = cents(b.monthly); let (spent, excl, txs) = month_spend(ds, b, &month, &uids);
        let mut carry = 0;
        if let (true, Some(sm)) = (b.rollover, &start_m) {
            let first = sm.clone().max(add_months_key(&month, -12)); let last = add_months_key(&month, -1);
            let carriers: Vec<String> = ds.txs.iter().filter(|t| uids.contains(&t.account) && matches(b, &t.category)).map(|t| t.account.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            let usable: HashSet<String> = ds.coverage.common_months(&carriers).into_iter().collect();
            if first <= last { for m in months_between(&first, &last).into_iter().filter(|m| usable.contains(m)) { carry += (monthly - month_spend(ds, b, &m, &uids).0).max(0); } }
        }
        let available = monthly + carry;
        let (mut proj, mut method) = (if elapsed > 0 { div_cents(spent * dim, elapsed) } else { spent }, "pace");
        if elapsed >= 3 {
            let (mut later, mut paid) = (0, 0);
            for x in rec.series.iter().filter(|x| x.kind == "expense" && uids.contains(&x.account) && matches(b, &x.category)) {
                paid += x.occ.iter().filter(|o| month_key(o.1) == month).map(|o| -o.2).sum::<i64>();
                later += occurrences(x, as_of + Duration::days(1), month_end(&month)).len() as i64 * -x.expected_c;
            }
            proj = spent + later + div_cents((spent - paid).max(0) * (dim - elapsed), elapsed); method = "pace+recurring";
        }
        let st = if spent > available { "over" } else if proj > available { "at_risk" } else { "ok" };
        if ds.txs.iter().filter(|t| uids.contains(&t.account)).map(|t| t.date).max().is_some_and(|l| (as_of - l).num_days() > 10) { flags.push("stale_data".into()); }
        if b.rollover && carry != 0 { flags.push("includes_carry_over".into()); }
        let mut ev: Vec<&&Tx> = txs.iter().collect(); ev.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key)));
        out.push(json!({"id": b.id, "target": b.category.clone().unwrap_or_else(|| format!("group:{}", b.group.clone().unwrap_or_default())), "month": month, "monthly": money_str(monthly), "carry": money_str(carry), "available": money_str(available),
            "spent": money_str(spent), "excluded": money_str(excl), "remaining": money_str(available - spent), "percent_used": pct(spent, available), "projected": money_str(proj), "projected_percent": pct(proj, available), "method": method, "status": st,
            "rollover": b.rollover, "owner": b.owner, "account": b.account, "flags": flags, "evidence": ev.iter().take(10).map(|t| t.key.clone()).collect::<Vec<_>>()}));
    }
    let ord = |s: &Value| ["over", "at_risk", "ok"].iter().position(|x| s["status"] == *x).unwrap_or(3);
    out.sort_by(|a, b| ord(a).cmp(&ord(b)).then(b["percent_used"].as_f64().unwrap_or(0.0).total_cmp(&a["percent_used"].as_f64().unwrap_or(0.0))).then(a["id"].as_str().cmp(&b["id"].as_str())));
    let mut spend: BTreeMap<&str, i64> = BTreeMap::new(); for t in ds.txs.iter().filter(|t| t.month() == month && is_spending(&t.category) && !t.is_saved() && !t.is_one_off()) { *spend.entry(&t.category).or_default() -= t.amount_c; }
    let mut unb: Vec<(&str, i64)> = spend.into_iter().filter(|(c, v)| *v > 0 && !list.iter().any(|b| matches(b, c))).collect(); unb.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let counts: serde_json::Map<String, Value> = ["over", "at_risk", "ok"].iter().map(|k| (k.to_string(), out.iter().filter(|p| p["status"] == *k).count().into())).collect();
    let ev: BTreeSet<String> = out.iter().flat_map(|p| p["evidence"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from))).collect();
    json!({"as_of": as_of.to_string(), "month": month, "budgets": out, "counts": counts, "unbudgeted": unb.into_iter().take(10).map(|(c, v)| json!({"category": c, "spent": money_str(v)})).collect::<Vec<_>>(),
        "unallocated_refunds": money_str(ds.txs.iter().filter(|t| t.month() == month && t.category == "income.refund" && t.amount_c > 0).map(|t| t.amount_c).sum()), "warnings": problems.iter().map(|p| format!("budget {p} -- left out")).collect::<Vec<_>>(),
        "coverage": ds.coverage.info(&all.into_iter().collect::<Vec<_>>(), &[], &format!("month to date: {} to {as_of}", month_start(&month)), if list.is_empty() { vec!["no budgets set: see `finch budgets suggest`".into()] } else { vec![] }), "evidence": ev})
}

// ---------------------------------------------------------------- goals
fn assets(ds: &Dataset) -> Vec<serde_yaml::Value> { std::fs::read_to_string(ds.memory_dir.join("assets.yaml")).ok().and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok()).and_then(|v| v.get("assets").and_then(|a| a.as_sequence()).cloned()).unwrap_or_default() }
pub fn goals(ds: &Dataset) -> Value {
    let (list, problems): (Vec<Goal>, Vec<String>) = memory::list_file(&ds.memory_dir, "goals.yaml", "goals"); let today = ds.today; let whole = ds.whole(); let assets = assets(ds);
    let src = |g: &Goal| g.asset.clone().or(g.account.clone()).or(g.tag.clone()).unwrap_or_default();
    let (mut out, mut used) = (vec![], BTreeSet::new());
    for g in &list {
        let mut flags: Vec<String> = vec![]; let mut ev: Vec<String> = vec![];
        let shared: Vec<&str> = list.iter().filter(|o| o.id != g.id && src(o) == src(g)).map(|o| o.id.as_str()).collect(); if !shared.is_empty() { flags.push(format!("shares_source_with:{}", shared.join(","))); }
        let (mut current, mut as_of, mut pace, mut basis) = (None::<i64>, None::<NaiveDate>, None::<i64>, "none"); let mut planned = g.monthly_contribution.map(cents);
        let (source, sref);
        if let Some(a) = &g.asset {
            (source, sref) = ("asset", a.clone());
            match assets.iter().find(|x| x.get("id").and_then(|i| i.as_str()) == Some(a)) {
                None => flags.push("unknown_asset".into()),
                Some(x) => { let amt = x.get("value").or(x.get("balance")).and_then(|v| v.as_f64()); current = amt.map(cents); as_of = x.get("as_of").and_then(|v| v.as_str()).and_then(d);
                    if amt.is_none() { flags.push("asset_has_no_value".into()); } else if as_of.is_none_or(|a| a < add_months(today, -(ds.settings.asset_stale_months as i32))) { flags.push("stale_value".into()); }
                    if planned.is_none() { planned = x.get("contribution_monthly").and_then(|v| v.as_f64()).map(cents); } }
            }
        } else if let Some(acc) = &g.account {
            (source, sref) = ("account", acc.clone());
            match ds.accounts.values().find(|a| a.uid == *acc || a.label.eq_ignore_ascii_case(acc)) {
                None => flags.push("unknown_account".into()),
                Some(a) => { used.insert(a.uid.clone()); match ds.balances.get(&a.uid) { Some(b) => { current = Some(b.amount_c); as_of = Some(b.as_of); } None => flags.push("account_has_no_balance".into()) }
                    let ms = last_n(&ds.coverage.covered(&a.uid).into_iter().collect::<Vec<_>>(), 6);
                    if ms.len() >= 2 { let tot: i64 = whole.iter().filter(|t| t.account == a.uid && ms.contains(&t.month()) && !t.is_one_off()).map(|t| t.amount_c).sum(); pace = Some(div_cents(tot, ms.len() as i64)); basis = "actual"; } }
            }
        } else {
            let tag = g.tag.clone().unwrap_or_default(); (source, sref) = ("tag", tag.clone()); let start = g.start.as_deref().and_then(d);
            let tagged: Vec<&Tx> = whole.iter().filter(|t| t.tags.contains(&tag) && start.is_none_or(|s| t.date >= s)).collect();
            if start.is_none() { flags.push("counts_all_history".into()); }
            let carriers: Vec<String> = whole.iter().filter(|t| t.tags.contains(&tag)).map(|t| t.account.clone()).collect::<BTreeSet<_>>().into_iter().collect(); used.extend(carriers.iter().cloned());
            current = Some(g.baseline.map(cents).unwrap_or(0) - tagged.iter().map(|t| t.amount_c).sum::<i64>()); as_of = tagged.iter().map(|t| t.date).max().or(start);
            ev = tagged.iter().rev().take(20).rev().map(|t| t.key.clone()).collect(); if tagged.is_empty() { flags.push("no_tagged_flows".into()); }
            let ms = last_n(&ds.coverage.common_months(&carriers), 6);
            if ms.len() >= 2 { let tot: i64 = -whole.iter().filter(|t| t.tags.contains(&tag) && ms.contains(&t.month())).map(|t| t.amount_c).sum::<i64>(); pace = Some(div_cents(tot, ms.len() as i64)); basis = "actual"; }
        }
        if pace.is_none() && planned.is_some_and(|p| p != 0) { pace = planned; basis = "planned"; }
        let target = cents(g.target_amount); let remaining = current.map(|c| target - c); let tdate = g.target_date.as_deref().and_then(d);
        let months_left = tdate.map(|t| if t > today { (((t - today).num_days() as f64) / DAYS_PER_MONTH).ceil().max(1.0) as i64 } else { 0 });
        let (mut projected, mut required) = (None, None);
        let st = match (current, remaining) {
            (None, _) => "no_data", (_, Some(r)) if r <= 0 => "achieved",
            (_, Some(r)) => {
                if let Some(ml) = months_left.filter(|m| *m > 0) { required = Some(-(-r).div_euclid(ml)); }
                let mut s = match pace.filter(|p| *p > 0) { Some(p) => { let pr = add_months(today, (r as f64 / p as f64).ceil() as i32); projected = Some(pr); if tdate.is_some_and(|t| pr > t) { "behind" } else { "on_track" } } None => "no_pace" };
                if tdate.is_some_and(|t| t < today) { s = "overdue"; } s
            }
            _ => unreachable!(),
        };
        out.push(json!({"id": g.id, "title": g.title, "source": source, "source_ref": sref, "target": money_str(target), "target_date": tdate.map(|t| t.to_string()), "current": money_opt(current), "current_as_of": as_of.map(|a| a.to_string()),
            "percent": current.and_then(|c| pct(c, target)), "remaining": money_opt(remaining), "pace": money_opt(pace), "pace_basis": basis, "planned": money_opt(planned), "required_monthly": money_opt(required), "months_left": months_left,
            "projected_date": projected.map(|p| p.to_string()), "status": st, "flags": flags, "evidence": ev}));
    }
    let order = ["overdue", "behind", "no_pace", "no_data", "on_track", "achieved"]; let ix = |g: &Value| order.iter().position(|o| g["status"] == *o).unwrap_or(9);
    out.sort_by(|a, b| ix(a).cmp(&ix(b)).then(a["id"].as_str().cmp(&b["id"].as_str())));
    let counts: serde_json::Map<String, Value> = order.iter().filter(|o| out.iter().any(|g| g["status"] == **o)).map(|o| (o.to_string(), out.iter().filter(|g| g["status"] == *o).count().into())).collect();
    let ev: BTreeSet<String> = out.iter().flat_map(|g| g["evidence"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from))).collect();
    json!({"as_of": today.to_string(), "goals": out, "counts": counts, "coverage": ds.coverage.info(&used.into_iter().collect::<Vec<_>>(), &[], "pace: last 6 months covered by the accounts involved (tag / account goals)", if list.is_empty() { vec!["no goals set".into()] } else { vec![] }),
        "evidence": ev, "warnings": problems.iter().map(|p| format!("goal {p} -- left out")).collect::<Vec<_>>()})
}

// ---------------------------------------------------------------- forecast
#[derive(Clone)] struct Ev { date: NaiveDate, account: String, label: String, amount: i64, low: i64, high: i64, rf: String, overdue: bool }
fn stdev(v: &[i64]) -> f64 { let n = v.len() as f64; if n < 2.0 { return 0.0; } let m = v.iter().sum::<i64>() as f64 / n; (v.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt() }
/// (monthly median, sigma, has history) of one account's non-recurring net spend.
fn variable_stats(ds: &Dataset, whole: &[Tx], uid: &str, members: &HashSet<String>, skip: &HashSet<String>) -> (i64, i64, bool) {
    if ds.accounts[uid].purpose.as_deref() == Some("savings") { return (0, 0, true); }
    let months = last_n(&ds.coverage.covered(uid).into_iter().collect::<Vec<_>>(), ds.settings.forecast_variable_months);
    if months.len() < 2 { return (0, 0, false); }
    if !ds.txs.iter().any(|t| t.account == uid && months.contains(&t.month())) { return (0, 0, true); }
    let mut per: BTreeMap<&String, i64> = months.iter().map(|m| (m, 0)).collect();
    for t in whole.iter().filter(|t| t.account == uid && (is_spending(&t.category) || is_transfer(&t.category)) && !t.is_saved() && !t.is_one_off() && !members.contains(&t.key) && !skip.contains(&t.key)) { if let Some(v) = per.get_mut(&t.month()) { *v -= t.amount_c; } }
    let vals: Vec<i64> = per.values().copied().collect(); (median_c(&vals), stdev(&vals).round() as i64, true)
}
pub fn forecast(ds: &Dataset, rec: &Recurring, scope: &Scope, days: i64, points: bool) -> Value {
    let s = &ds.settings; let today = ds.today; let end = today + Duration::days(days); let whole = ds.whole();
    let allowed = ds.uids_in(scope);
    let members: HashSet<String> = rec.series.iter().flat_map(|x| x.occ.iter().map(|o| o.0.clone())).collect();
    // payments flagged large / new-merchant, and pre-series instalments, are not habits
    let an = super::anomalies::detect(ds, &Scope::default(), rec, &HashSet::new(), true);
    let mut skip: HashSet<String> = an["anomalies"].as_array().unwrap().iter().filter(|a| a["type"] == "large_transaction" || a["type"] == "new_merchant").flat_map(|a| a["evidence"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from))).collect();
    for x in rec.series.iter().filter(|x| x.status == "active") { let r = x.expected_c.abs(); skip.extend(whole.iter().filter(|t| t.account == x.account && t.date < x.first_date && (t.amount_c < 0) == (x.expected_c < 0) && ((t.amount_c.abs() - r).abs() as f64) <= 0.01 * r as f64).map(|t| t.key.clone())); }
    let (mut accs, mut used, mut hh_events, mut with_bal) = (vec![], BTreeSet::new(), vec![], vec![]);
    let (mut hh_start, mut hh_mean, mut hh_var) = (0i64, 0i64, 0f64);
    for u in &allowed {
        let bal = ds.balances.get(u); let start = bal.map_or(today, |b| b.as_of.min(today)) + Duration::days(1);
        let mut evs: Vec<Ev> = vec![];
        for x in rec.series.iter().filter(|x| &x.account == u) { for (dd, ov) in occurrences(x, start, end) {
            let (lo, hi) = if x.amount_mode == "fixed" { (x.expected_c, x.expected_c) } else if x.expected_c < 0 { (x.high_c, x.low_c) } else { (x.low_c, x.high_c) };
            evs.push(Ev { date: dd, account: u.clone(), label: x.entity.clone(), amount: x.expected_c, low: lo, high: hi, rf: x.id.clone(), overdue: ov }); used.insert(x.id.clone()); } }
        evs.sort_by(|a, b| (a.date, &a.rf).cmp(&(b.date, &b.rf)));
        let (mean, sigma, has) = variable_stats(ds, &whole, u, &members, &skip);
        let mut flags: Vec<String> = vec![]; if !has { flags.push("no_variable_history".into()); }
        match bal { None => flags.push("no_balance".into()), Some(b) if (today - b.as_of).num_days() > s.forecast_balance_stale_days as i64 => flags.push("balance_stale".into()), _ => {} }
        let pts = bal.map(|b| project(today, end, start, b.amount_c, &evs, mean, sigma, s.forecast_band_z)).unwrap_or_default();
        if let Some(b) = bal { hh_start += b.amount_c; hh_mean += mean; hh_var += (sigma as f64).powi(2); hh_events.extend(evs.clone()); with_bal.push(pts.clone()); }
        accs.push(block(Some(u), &ds.label(u), ds.accounts.get(u), bal.map(|b| b.amount_c), bal.map(|b| b.as_of), bal.map(|b| b.ty.clone()), mean, sigma, &pts, &evs, flags, days, points));
    }
    hh_events.sort_by(|a, b| (a.date, &a.account, &a.rf).cmp(&(b.date, &b.account, &b.rf)));
    let mut hh_flags: Vec<String> = vec![]; if with_bal.len() != accs.len() { hh_flags.push(format!("accounts_without_balance:{}", accs.len() - with_bal.len())); }
    if accs.iter().any(|a| a["flags"].as_array().unwrap().iter().any(|f| f == "balance_stale")) { hh_flags.push("balance_stale".into()); }
    let mut hp = vec![];
    if !with_bal.is_empty() && with_bal.iter().all(|p| !p.is_empty()) {
        for i in 0..with_bal.iter().map(|p| p.len()).min().unwrap() {
            let e: i64 = with_bal.iter().map(|p| p[i].1).sum();
            let lo = with_bal.iter().map(|p| ((p[i].1 - p[i].2) as f64).powi(2)).sum::<f64>().sqrt(); let hi = with_bal.iter().map(|p| ((p[i].3 - p[i].1) as f64).powi(2)).sum::<f64>().sqrt();
            hp.push((with_bal[0][i].0, e, (e as f64 - lo).round() as i64, (e as f64 + hi).round() as i64));
        }
    }
    let household = block(None, "household", None, (!with_bal.is_empty()).then_some(hh_start), (!with_bal.is_empty()).then_some(today), None, hh_mean, hh_var.sqrt().round() as i64, &hp, &hh_events, hh_flags, days, points);
    json!({"as_of": today.to_string(), "horizon_days": days, "household": household, "accounts": accs, "scope": scope.describe(), "evidence": used,
        "assumptions": ["start = latest balance snapshot per account (bank sync or a CODA new-balance record); recurring series replayed from their next expected date",
            "variable spend = median monthly non-recurring spend over the account's own covered months, spread evenly", format!("band = +-{} sigma of the monthly variable spend (sqrt-time) plus the amount range of variable series; non-recurring income is not projected", s.forecast_band_z),
            "loan instalments of memory/liabilities without a recurring series are not projected yet"],
        "coverage": ds.coverage.info(&allowed, &[], "variable spend: each account's own last covered months", ds.base_notes())})
}
/// Daily (date, expected, low, high) after today.
fn project(today: NaiveDate, end: NaiveDate, start: NaiveDate, start_c: i64, evs: &[Ev], mean: i64, sigma: i64, z: f64) -> Vec<(NaiveDate, i64, i64, i64)> {
    let per_day = mean as f64 / DAYS_PER_MONTH; let (mut exp, mut cum, mut lo_ev, mut hi_ev, mut t, mut day, mut out) = (start_c as f64, 0.0, 0i64, 0i64, 0, start, vec![]);
    while day <= end {
        for e in evs.iter().filter(|e| e.date == day) { exp += e.amount as f64; lo_ev += (e.low - e.amount).min(0); hi_ev += (e.high - e.amount).max(0); }
        cum += per_day; t += 1;
        if day > today { let band = z * sigma as f64 * (t as f64 / DAYS_PER_MONTH).sqrt(); let e = (exp - cum).round() as i64; out.push((day, e, ((exp - cum - band) as i64 + lo_ev).min(e), ((exp - cum + band) as i64 + hi_ev).max(e))); }
        day += Duration::days(1);
    }
    out
}
#[allow(clippy::too_many_arguments)]
fn block(uid: Option<&String>, label: &str, acc: Option<&AccountInfo>, start: Option<i64>, start_date: Option<NaiveDate>, ty: Option<String>, mean: i64, sigma: i64, pts: &[(NaiveDate, i64, i64, i64)], evs: &[Ev], mut flags: Vec<String>, days: i64, points: bool) -> Value {
    let min = pts.iter().min_by_key(|p| p.1); let neg = pts.iter().find(|p| p.1 < 0).map(|p| p.0); let risk = pts.iter().find(|p| p.2 < 0).map(|p| p.0);
    if neg.is_some() { flags.push("projected_negative".into()); } else if risk.is_some() { flags.push("at_risk".into()); }
    let pj = |p: &(NaiveDate, i64, i64, i64)| json!({"date": p.0.to_string(), "balance": money_str(p.1), "low": money_str(p.2), "high": money_str(p.3)});
    json!({"account": uid, "label": label, "purpose": acc.and_then(|a| a.purpose.clone()), "owner": acc.and_then(|a| a.owner.clone()), "start_balance": money_opt(start), "start_date": start_date.map(|d| d.to_string()), "balance_type": ty,
        "variable_monthly": money_str(mean), "variable_sigma": money_str(sigma), "milestones": ([30i64, 60, 90]).iter().filter(|n| **n <= days && pts.len() >= **n as usize).map(|n| { let mut v = pj(&pts[*n as usize - 1]); v["days"] = (*n).into(); v }).collect::<Vec<_>>(),
        "min_balance": money_opt(min.map(|p| p.1)), "min_date": min.map(|p| p.0.to_string()), "first_negative": neg.map(|d| d.to_string()), "first_at_risk": risk.map(|d| d.to_string()), "flags": flags,
        "points": if points { pts.iter().map(pj).collect::<Vec<_>>() } else { vec![] },
        "events": evs.iter().map(|e| json!({"date": e.date.to_string(), "account": e.account, "label": e.label, "amount": money_str(e.amount), "low": money_str(e.low), "high": money_str(e.high), "source": "recurring", "ref": e.rf, "certainty": "observed", "overdue": e.overdue})).collect::<Vec<_>>()})
}

// ---------------------------------------------------------------- calendar
pub fn calendar(ds: &Dataset, con: &rusqlite::Connection, rec: &Recurring, days: Option<usize>, include_transfers: bool) -> Value {
    let today = ds.today; let days = days.unwrap_or(ds.settings.calendar_days) as i64; let end = today + Duration::days(days); let mut items: Vec<Value> = vec![];
    let mut add = |date: NaiveDate, source: &str, kind: &str, title: &str, amount: Option<i64>, rf: &str, cert: &str, acc: Option<&str>, note: Option<String>| items.push(json!({"date": date.to_string(), "days_until": (date - today).num_days(), "source": source, "kind": kind, "title": title, "amount": money_opt(amount), "ref": rf, "certainty": cert, "account_label": acc, "note": note}));
    for x in rec.series.iter().filter(|x| x.status == "active") {
        if x.kind == "transfer" && !include_transfers { continue; }
        for (dd, ov) in occurrences(x, today, end) {
            let note = Some(format!("{}{}{}", x.cadence, if x.amount_mode == "variable" { " (variable amount)" } else { "" }, if ov { "; overdue: the last expected date has passed" } else { "" }));
            if x.kind == "transfer" { add(dd, "recurring", "transfer", "transfer between own accounts", Some(x.expected_c), &x.id, "observed", None, note); continue; }
            add(dd, "recurring", if x.direction == "in" { "income" } else if x.kind == "saving" { "saving" } else { "payment" }, &x.entity, Some(x.expected_c), &x.id, "observed", Some(&x.account_label), note);
        }
    }
    let mut files: Vec<_> = std::fs::read_dir(ds.memory_dir.join("contracts")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "yaml") && !p.file_name().unwrap().to_string_lossy().starts_with('_')).collect(); files.sort();
    for f in files {
        let Some(c) = std::fs::read_to_string(&f).ok().and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok()) else { continue };
        let Some(id) = c.get("id").and_then(|x| x.as_str()) else { continue };
        let label = c.get("provider").and_then(|x| x.as_str()).unwrap_or(id); let amount = c.get("billing").and_then(|b| b.get("amount")).and_then(|a| a.as_f64()).map(|a| -cents(a));
        let notice = c.get("notice_period_days").and_then(|x| x.as_i64());
        for (k, kind, word) in [("renewal", "renewal", "renews"), ("commitment_end", "commitment_end", "commitment ends")] {
            let Some(dd) = c.get(k).and_then(|x| x.as_str()).and_then(d) else { continue };
            if today <= dd && dd <= end { add(dd, "contract", kind, &format!("{label} {word}"), amount, id, "scheduled", None, None); }
            if let Some(n) = notice { let dl = dd - Duration::days(n); if today <= dl && dl <= end { add(dl, "contract", "notice_deadline", &format!("{label}: last day to give notice ({word} {dd})"), None, id, "deadline", None, Some(format!("notice period {n} days (from the contract file)"))); } }
        }
    }
    if let Ok(mut st) = con.prepare("SELECT session_id, aspsp_name, valid_until, status FROM sessions WHERE status='active' AND valid_until IS NOT NULL") {
        let rows: Vec<(String, Option<String>, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|i| i.flatten().collect()).unwrap_or_default();
        for (sid, bank, vu) in rows { if let Some(dd) = d(&vu) { if dd <= end { let when = dd.max(today); add(when, "consent", if dd < today { "consent_expired" } else { "consent_expiry" }, &format!("bank consent {} {}", bank.unwrap_or_default(), if dd < today { "expired" } else { "expires" }), None, &format!("consent:{}", truncate_chars(&sid, 8)), "scheduled", None, Some("reconnect the bank to keep syncing".into())); } } }
    }
    for a in assets(ds) {
        let Some(id) = a.get("id").and_then(|x| x.as_str()) else { continue };
        let due = a.get("as_of").and_then(|x| x.as_str()).and_then(d).map(|x| add_months(x, ds.settings.asset_stale_months as i32)).unwrap_or(today).max(today);
        if due <= end { add(due, "asset", "value_refresh", &format!("refresh the value of {id}"), None, id, "scheduled", None, None); }
    }
    items.sort_by(|a, b| (a["date"].as_str(), a["source"].as_str(), a["ref"].as_str()).cmp(&(b["date"].as_str(), b["source"].as_str(), b["ref"].as_str())));
    let mut counts: BTreeMap<String, usize> = BTreeMap::new(); for i in &items { *counts.entry(i["source"].as_str().unwrap().into()).or_default() += 1; }
    json!({"as_of": today.to_string(), "days": days, "counts": counts, "evidence": items.iter().filter(|i| i["source"] == "recurring").filter_map(|i| i["ref"].as_str().map(String::from)).collect::<BTreeSet<_>>(), "items": items,
        "coverage": ds.coverage.info(&ds.uids_in(&Scope::default()), &[], "recurring series, contract dates, consents and asset values known today", vec!["loan instalments and Belgian notice rules are not computed yet: only the dates in the contract files".into()])})
}

#[cfg(test)]
mod tests { use super::*; #[test] fn rounding() { assert_eq!(round_to(4750, 500), 5000); assert_eq!(round_to(4740, 500), 4500); assert_eq!(round_to(23456, 1000), 23000); } }
