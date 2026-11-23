//! Income vs spending and savings rate per month. income = `income.*` except `income.refund`; refunds = positive amounts in a
//! spending category + `income.refund` (negative spending, never income); spending = gross outflows minus refunds; saved = net
//! outflow of `savings` / `investment`-tagged items; net = income - spending; savings_rate = net / income. Internal transfers and
//! transfers to / from people are never spending nor income: their totals are listed under `transfers`. A month is `complete`
//! when every account of the scope covers it. (Loan principal splits need the loans module: debt_service is 0 / null here.)
use super::*;
use std::collections::HashSet;

#[derive(Default, Clone)]
struct Flow { income: i64, refunds: i64, gross: i64, one_off: i64, saved: i64, unc: i64, ii: i64, io: i64, pi: i64, po: i64, drawn: i64, sent: i64, n: usize }

/// Keys of internal-transfer legs with no counterpart (same amount, other account, within the window) in the connected accounts.
pub fn unconnected_internal(ds: &Dataset, window: i64) -> HashSet<String> {
    let w = ds.whole();
    let legs: Vec<&Tx> = w.iter().filter(|t| t.category == "transfer.internal" && !t.is_saved() && !t.is_one_off()).collect();
    let pool: Vec<&Tx> = w.iter().filter(|t| t.category.starts_with("transfer.") && !t.is_saved()).collect();
    let (mut used, mut matched) = (HashSet::new(), HashSet::new());
    for o in &legs {
        let c = pool.iter().filter(|c| !used.contains(&c.key) && c.key != o.key && c.account != o.account && c.amount_c == -o.amount_c && (c.date - o.date).num_days().abs() <= window)
            .min_by_key(|c| ((c.date - o.date).num_days().abs(), c.date, c.key.clone()));
        if let Some(c) = c { used.insert(c.key.clone()); used.insert(o.key.clone()); matched.insert(o.key.clone()); matched.insert(c.key.clone()); }
    }
    legs.iter().map(|t| t.key.clone()).filter(|k| !matched.contains(k)).collect()
}

fn month_flow(txs: &[&Tx], unconnected: &HashSet<String>) -> Flow {
    let mut f = Flow { n: txs.len(), ..Default::default() };
    for t in txs {
        let (a, c) = (t.amount_c, t.category.as_str());
        if t.is_saved() { f.saved -= a; }
        else if is_transfer(c) { match (a >= 0, c == "transfer.internal") { (true, true) => f.ii += a, (true, false) => f.pi += a, (false, true) => f.io -= a, (false, false) => f.po -= a } }
        else if c == "income.refund" { if a >= 0 { f.refunds += a } else { f.gross -= a } }
        else if is_income(c) { f.income += a }
        else { if a < 0 { f.gross -= a } else { f.refunds += a } if t.is_one_off() { f.one_off -= a; } if c == "other.uncategorized" { f.unc -= a; } }
        if unconnected.contains(&t.key) { if a > 0 { f.drawn += a } else { f.sent -= a } }
    }
    f
}
fn flow_json(m: Option<&str>, f: &Flow, n_months: Option<usize>, complete: Option<(bool, Vec<String>)>) -> Value {
    let sp = f.gross - f.refunds; let net = f.income - sp;
    let mut v = serde_json::Map::new();
    if let Some(m) = m { v.insert("month".into(), m.into()); }
    if let Some(n) = n_months { v.insert("n_months".into(), n.into()); }
    for (k, x) in [("income", f.income), ("refunds", f.refunds)] { v.insert(k.into(), money_str(x).into()); }
    if m.is_some() { v.insert("spending_gross".into(), money_str(f.gross).into()); }
    for (k, x) in [("spending", sp), ("one_off_spending", f.one_off), ("spending_ex_one_offs", sp - f.one_off), ("saved", f.saved), ("net", net)] { v.insert(k.into(), money_str(x).into()); }
    v.insert("savings_rate".into(), json!(pct(net, f.income))); v.insert("saved_rate".into(), json!(pct(f.saved, f.income)));
    if m.is_some() { v.insert("uncategorized".into(), money_str(f.unc).into()); v.insert("transfers".into(), json!({"internal_in": money_str(f.ii), "internal_out": money_str(f.io), "people_in": money_str(f.pi), "people_out": money_str(f.po)})); v.insert("n_tx".into(), f.n.into()); }
    if let Some((c, miss)) = complete { v.insert("complete".into(), c.into()); v.insert("missing_accounts".into(), json!(miss)); }
    v.insert("debt_service".into(), "0.00".into()); v.insert("loan_principal".into(), Value::Null); v.insert("savings_rate_incl_principal".into(), Value::Null);
    v.insert("drawn_unconnected".into(), money_str(f.drawn).into()); v.insert("sent_unconnected".into(), money_str(f.sent).into());
    Value::Object(v)
}
fn total(rows: &[&(String, Flow, bool, Vec<String>)]) -> Value {
    if rows.is_empty() { return Value::Null; }
    let mut t = Flow::default();
    for (_, f, _, _) in rows { t.income += f.income; t.refunds += f.refunds; t.gross += f.gross; t.one_off += f.one_off; t.saved += f.saved; t.drawn += f.drawn; t.sent += f.sent; }
    flow_json(None, &t, Some(rows.len()), None)
}
fn block(ds: &Dataset, uids: &[String], months: &[String], unconnected: &HashSet<String>) -> (Value, Vec<(String, bool, Vec<String>)>) {
    let ok: HashSet<&String> = uids.iter().collect(); let flow = ds.flow_accounts();
    let needed: Vec<&String> = uids.iter().filter(|u| flow.contains(*u)).collect();
    let mut by: BTreeMap<String, Vec<&Tx>> = BTreeMap::new(); for t in ds.txs.iter().filter(|t| ok.contains(&t.account)) { by.entry(t.month()).or_default().push(t); }
    let rows: Vec<(String, Flow, bool, Vec<String>)> = months.iter().map(|m| { let miss: Vec<String> = needed.iter().filter(|u| !ds.coverage.covered(u).contains(m)).map(|u| ds.label(u)).collect(); (m.clone(), month_flow(by.get(m).map_or(&[][..], |v| v), unconnected), miss.is_empty(), miss) }).collect();
    let complete: Vec<_> = rows.iter().filter(|r| r.2).collect(); let all: Vec<_> = rows.iter().collect();
    (json!({"accounts": uids.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "months": rows.iter().map(|(m, f, c, miss)| flow_json(Some(m), f, None, Some((*c, miss.clone())))).collect::<Vec<_>>(), "totals_complete": total(&complete), "totals_all": total(&all)}),
     rows.into_iter().map(|(m, _, c, miss)| (m, c, miss)).collect())
}

pub fn cashflow(ds: &Dataset, scope: &Scope, months: usize, end: Option<&str>, include_current: bool, breakdown: bool) -> Value {
    let uids = ds.uids_in(scope);
    let firsts: Vec<NaiveDate> = uids.iter().filter_map(|u| ds.coverage.first(u)).collect();
    let last_m = if include_current { month_key(ds.today) } else { end.map(String::from).unwrap_or_else(|| last_closed_month(ds.today)) };
    let mut first_m = add_months_key(&last_m, -(months.max(1) as i32 - 1));
    if let Some(f) = firsts.iter().min() { first_m = first_m.max(month_key(*f)); }
    let ml = if !firsts.is_empty() && first_m <= last_m { months_between(&first_m, &last_m) } else { vec![] };
    let unc = unconnected_internal(ds, 3);
    let (hh, status) = block(ds, &uids, &ml, &unc);
    let (mut by_purpose, mut by_owner) = (serde_json::Map::new(), serde_json::Map::new());
    if breakdown {
        let acc = ds.accounts_in(scope);
        for p in acc.iter().map(|a| a.purpose.clone().unwrap_or_else(|| "unset".into())).collect::<BTreeSet<_>>() { by_purpose.insert(p.clone(), block(ds, &acc.iter().filter(|a| a.purpose.clone().unwrap_or_else(|| "unset".into()) == p).map(|a| a.uid.clone()).collect::<Vec<_>>(), &ml, &unc).0); }
        for o in acc.iter().map(|a| a.owner.clone().unwrap_or_else(|| "unset".into())).collect::<BTreeSet<_>>() { by_owner.insert(o.clone(), block(ds, &acc.iter().filter(|a| a.owner.clone().unwrap_or_else(|| "unset".into()) == o).map(|a| a.uid.clone()).collect::<Vec<_>>(), &ml, &unc).0); }
    }
    let ms: HashSet<&String> = ml.iter().collect();
    let ev: BTreeSet<String> = ds.txs_in(scope).into_iter().filter(|t| ms.contains(&t.month()) && (is_income(&t.category) || t.is_saved() || (is_spending(&t.category) && t.is_one_off()))).map(|t| t.key.clone()).collect();
    let complete: Vec<String> = status.iter().filter(|s| s.1).map(|s| s.0.clone()).collect();
    let incomplete: Vec<_> = status.iter().filter(|s| !s.1).collect();
    let mut notes = ds.base_notes();
    if !incomplete.is_empty() { notes.push(format!("incomplete months: {}", incomplete.iter().map(|(m, _, miss)| format!("{m} (no full data for {})", miss.join(", "))).collect::<Vec<_>>().join(", "))); }
    let mut cov = ds.coverage.info(&uids, &complete, "a month is complete when every account of the scope covers all its days; totals_complete sums complete months only, totals_all every listed month", notes);
    cov["skipped_months"] = json!(incomplete.iter().map(|s| s.0.clone()).collect::<Vec<_>>());
    json!({"as_of": ds.today.to_string(), "scope": scope.describe(), "household": hh, "by_purpose": by_purpose, "by_owner": by_owner, "coverage": cov, "evidence": ev})
}
