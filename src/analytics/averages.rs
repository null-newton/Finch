//! Coverage-aware monthly averages per category: a category's total over the months fully covered by EVERY (major) account
//! carrying it, divided by the number of those months (at most the last `average_window_months`). Never over months with no data.
//! The run-rate figure leaves out `one_off` / `exclude_from_averages` / `capital` items; `saved`-tagged items are never spending.
use super::*;
use std::collections::HashSet;

pub fn is_lumpy(cat: &str, names: &[String]) -> bool { names.iter().any(|n| cat == n || (n.ends_with('.') && cat.starts_with(n.as_str()))) }
/// Spending categories plus `income.refund` (a refund that cannot be tied to a category is negative spending).
pub fn spending_txs<'a>(ds: &'a Dataset, s: &Scope) -> Vec<&'a Tx> { ds.txs_in(s).into_iter().filter(|t| (is_spending(&t.category) || t.category == "income.refund") && !t.is_saved()).collect() }
fn item(t: &Tx) -> Value { json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "category": t.category, "event": t.event, "tags": t.tags, "account": t.account}) }

pub fn category_averages(ds: &Dataset, scope: &Scope, window: Option<usize>) -> Value {
    let s = &ds.settings; let window = window.unwrap_or(s.average_window_months);
    let sp = spending_txs(ds, scope);
    let mut by_cat: BTreeMap<&str, Vec<&Tx>> = BTreeMap::new(); for t in &sp { by_cat.entry(&t.category).or_default().push(t); }
    let (mut rows, mut unavailable, mut used) = (vec![], vec![], BTreeSet::new());
    for (cat, txs) in &by_cat {
        let (carriers, minor) = ds.carriers_split(cat, scope); let lumpy = is_lumpy(cat, &s.lumpy_categories);
        let months = last_n(&ds.coverage.common_months(&carriers), if lumpy { window.max(s.lumpy_min_months) } else { window });
        if months.is_empty() { unavailable.push(json!({"category": cat, "reason": "no month fully covered by all accounts carrying it", "accounts": carriers.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "n_tx": txs.len()})); continue; }
        let ms: HashSet<&String> = months.iter().collect(); let inm: Vec<_> = txs.iter().filter(|t| ms.contains(&t.month())).collect();
        let run: i64 = -inm.iter().filter(|t| !t.is_one_off()).map(|t| t.amount_c).sum::<i64>(); let all: i64 = -inm.iter().map(|t| t.amount_c).sum::<i64>(); let n = months.len() as i64;
        rows.push((div_cents(run, n), cat.to_string(), json!({"category": cat, "monthly_avg": money_str(div_cents(run, n)), "monthly_avg_with_one_offs": money_str(div_cents(all, n)), "total": money_str(run), "excluded_total": money_str(all - run), "n_months": n,
            "months": months, "accounts": carriers.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "ignored_accounts": minor.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "lumpy": lumpy, "low_confidence": (n as usize) < if lumpy { s.lumpy_min_months } else { s.average_min_months }})));
        used.extend(carriers);
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let carriers: Vec<String> = sp.iter().map(|t| t.account.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let hh_months = last_n(&ds.coverage.common_months(&carriers), window);
    let (mut hh_run, mut hh_all) = (None, None);
    if !hh_months.is_empty() { let ms: HashSet<&String> = hh_months.iter().collect(); let inm: Vec<_> = sp.iter().filter(|t| ms.contains(&t.month())).collect(); let n = hh_months.len() as i64; hh_run = Some(div_cents(-inm.iter().filter(|t| !t.is_one_off()).map(|t| t.amount_c).sum::<i64>(), n)); hh_all = Some(div_cents(-inm.iter().map(|t| t.amount_c).sum::<i64>(), n)); }
    let (mut by_acc, mut alt) = (vec![], 0i64);
    for u in &carriers {
        let msu = last_n(&ds.coverage.covered(u).into_iter().collect::<Vec<_>>(), window); if msu.is_empty() { continue; }
        let mset: HashSet<&String> = msu.iter().collect(); let run = -sp.iter().filter(|t| &t.account == u && mset.contains(&t.month()) && !t.is_one_off()).map(|t| t.amount_c).sum::<i64>();
        let avg = div_cents(run, msu.len() as i64); alt += avg; by_acc.push(json!({"account": u, "label": ds.label(u), "monthly_avg": money_str(avg), "n_months": msu.len()}));
    }
    let mismatch = hh_run.is_some_and(|h| !by_acc.is_empty() && (alt - h).abs() as f64 > s.household_mismatch_pct * (h.abs().max(1)) as f64);
    let excluded: Vec<Value> = sp.iter().filter(|t| NO_AVERAGE_TAGS.iter().any(|x| t.tags.contains(*x))).map(|t| item(t)).collect();
    let capital: Vec<Value> = sp.iter().filter(|t| t.tags.contains(CAPITAL_TAG) && !NO_AVERAGE_TAGS.iter().any(|x| t.tags.contains(*x))).map(|t| item(t)).collect();
    let mut notes = ds.base_notes();
    if mismatch { notes.push(format!("the household estimate ({}) and the sum of per-account averages ({}) differ by more than {:.0}%: the accounts' histories do not overlap enough", money_str(hh_run.unwrap()), money_str(alt), s.household_mismatch_pct * 100.0)); }
    let mut cu: Vec<String> = used.into_iter().collect(); cu.extend(carriers.iter().cloned());
    let ev: BTreeSet<String> = excluded.iter().chain(&capital).filter_map(|e| e["tx_key"].as_str().map(String::from)).collect();
    json!({"as_of": ds.today.to_string(), "window_months": window, "household_monthly_avg": money_opt(hh_run), "household_monthly_avg_with_one_offs": money_opt(hh_all), "household_months": hh_months, "household_by_account": by_acc,
        "household_sum": if by_acc.is_empty() { Value::Null } else { money_str(alt).into() }, "household_mismatch": mismatch, "categories": rows.into_iter().map(|r| r.2).collect::<Vec<_>>(), "unavailable": unavailable, "excluded": excluded, "capital": capital,
        "scope": scope.describe(), "coverage": ds.coverage.info(&cu, &hh_months, "per category: months covered by every account that carries the category; household: months covered by every account that carries any spending", notes), "evidence": ev})
}
