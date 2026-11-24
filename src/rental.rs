//! A rental property (E15), for Belgium: an `assets.yaml` item of kind `real_estate_rental`, linked to its bank account (`account`, or
//! the accounts with purpose `rental`) and its loan (`loan`, or the liabilities whose `asset` is it). Month by month: rent received,
//! the costs by kind, the loan instalments, the net and the owner's effort; missing rents against the expected rent (declared, the
//! indexed lease rent, or the observed median); the yearly totals. Belgian specifics: the yearly rent indexation with the health
//! index (base rent x new index / base index), computed ONLY from the indexes the owner typed in (nothing is looked up). No
//! French scheme (Pinel) applies: the scheme block stays empty. Tax: private letting is taxed on the cadastral income, not the rents.
use crate::{analytics::{Dataset, Tx}, loans::{self, Asset, Liability}, util::*};
use chrono::{Datelike, NaiveDate};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const KINDS: [&str; 8] = ["loan", "charges", "fees", "taxes", "insurance", "works", "other", "rent"];
fn kind_of(t: &Tx, loan_rx: &[fancy_regex::Regex]) -> &'static str {
    let c = t.category.as_str();
    if t.amount_c > 0 { return if c.starts_with("transfer.") { "owner_in" } else { "rent" }; }
    if c == "housing.rental_property_loan" || loan_rx.iter().any(|r| r.is_match(&t.mkey).unwrap_or(false) || r.is_match(&t.desc).unwrap_or(false)) { return "loan"; }
    if c.starts_with("transfer.") { return "owner_out"; }
    match c { "housing.property_charges" => "charges", "housing.property_management" => "fees", "housing.property_tax" => "taxes", "housing.property_insurance" => "insurance", "housing.renovation" | "housing.maintenance_diy" => "works",
        _ if c.starts_with("fees.") => "fees", _ if c.starts_with("taxes.") => "taxes", _ if c.starts_with("insurance.") => "insurance", _ => "other" }
}
pub struct Links { pub accounts: Vec<String>, pub loans: Vec<Liability>, pub notes: Vec<String> }
pub fn links(ds: &Dataset, a: &Asset) -> Links {
    let mut notes = vec![]; let accounts: Vec<String> = match &a.account { Some(x) => ds.accounts.values().filter(|ac| &ac.uid == x || ac.label.eq_ignore_ascii_case(x)).map(|ac| ac.uid.clone()).collect(),
        None => { let r: Vec<String> = ds.accounts.values().filter(|ac| ac.purpose.as_deref() == Some("rental")).map(|ac| ac.uid.clone()).collect(); if r.len() > 1 { notes.push("several accounts have purpose rental: set `account` on the property".into()); } if r.len() == 1 { r } else { vec![] } } };
    if accounts.is_empty() { notes.push("no bank account is linked: set `account` on the property (or purpose rental on its account)".into()); }
    let all = loans::liabilities(&ds.memory_dir).0; let ls: Vec<Liability> = all.into_iter().filter(|l| a.loan.as_deref() == Some(l.id.as_str()) || (a.loan.is_none() && l.asset.as_deref() == Some(a.id.as_str()))).collect();
    Links { accounts, loans: ls, notes }
}
/// The indexed rent of a Belgian lease: base rent x current health index / base index (the owner's own figures).
pub fn indexation(a: &Asset) -> Value {
    let Some(l) = &a.lease else { return json!({"status": "unknown", "missing": ["lease.base_rent", "lease.base_index", "lease.current_index"]}) };
    let miss: Vec<&str> = [("lease.base_rent", l.base_rent.is_none()), ("lease.base_index", l.base_index.is_none()), ("lease.current_index", l.current_index.is_none())].into_iter().filter(|x| x.1).map(|x| x.0).collect();
    let rules = ["indexation applies at most once a year, on the anniversary of the lease's entry into force, after a written request by the landlord", "it may be claimed for at most the 3 months before the request",
        "base index: the health index of the month before the lease was signed; new index: that of the month before the anniversary", "a written lease is required; regional rules (energy performance certificate limits in some periods) may restrict it: check your region's rules"];
    if !miss.is_empty() { return json!({"status": "missing", "missing": miss, "rules": rules}); }
    let (b, bi, ci) = (l.base_rent.unwrap(), l.base_index.unwrap(), l.current_index.unwrap()); let ix = to_cents(b * ci / bi);
    let next = l.start_date.as_deref().and_then(d).map(|s| { let t = chrono::Local::now().date_naive(); let mut n = NaiveDate::from_ymd_opt(t.year(), s.month(), s.day().min(28)).unwrap(); if n <= t { n = add_months(n, 12); } n.to_string() });
    json!({"status": "computed", "base_rent": money_str(to_cents(b)), "base_index": bi, "current_index": ci, "current_index_month": l.current_index_month, "indexed_rent": money_str(ix), "change": money_str(ix - to_cents(b)), "next_anniversary": next, "rules": rules,
        "note": "computed only from the indexes you recorded (Statbel publishes the health index); this is general information, not legal advice"})
}
fn expected(a: &Asset, received: &[i64]) -> (Option<i64>, &'static str) {
    if let Some(r) = a.rent_monthly { return (Some(to_cents(r)), "declared"); }
    if let Some(Value::String(s)) = Some(indexation(a)["indexed_rent"].clone()) { return (Some(to_cents(s.parse().unwrap_or(0.0))), "declared"); }
    let mut v: Vec<i64> = received.iter().copied().filter(|x| *x > 0).collect(); if v.len() >= 3 { v.sort(); return (Some(median_c(&v)), "observed_median"); } (None, "unknown")
}
/// Month-by-month cash flow of a property over its linked account(s).
pub fn cashflow(ds: &Dataset, a: &Asset, lk: &Links, months: usize) -> Value {
    let loan_rx: Vec<fancy_regex::Regex> = lk.loans.iter().filter_map(|l| l.payment_match.as_deref()).filter_map(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()).collect();
    let txs: Vec<Tx> = ds.whole().into_iter().filter(|t| lk.accounts.contains(&t.account)).collect();
    let cur = month_key(ds.today); let ms: Vec<String> = (0..months as i32).rev().map(|i| add_months_key(&cur, -i)).collect();
    let mut agg: BTreeMap<String, BTreeMap<&str, i64>> = BTreeMap::new(); let mut ev: BTreeMap<String, Vec<String>> = BTreeMap::new(); let mut n: BTreeMap<String, usize> = BTreeMap::new();
    for t in &txs { let k = kind_of(t, &loan_rx); *agg.entry(t.month()).or_default().entry(k).or_default() += t.amount_c.abs(); *n.entry(t.month()).or_default() += 1; if k == "rent" { ev.entry(t.month()).or_default().push(t.key.clone()); } }
    let rents: Vec<i64> = ms.iter().filter(|m| **m != cur).map(|m| agg.get(m).and_then(|x| x.get("rent")).copied().unwrap_or(0)).collect(); let (exp, src) = expected(a, &rents);
    let first_rent = agg.iter().find(|(_, v)| v.get("rent").is_some_and(|x| *x > 0)).map(|(m, _)| m.clone());
    let vac: Vec<(NaiveDate, Option<NaiveDate>)> = a.vacancies.iter().filter_map(|v| Some((d(&v.start)?, v.end.as_deref().and_then(d)))).collect();
    let covered = |m: &str| lk.accounts.iter().all(|u| ds.coverage.covered(u).contains(m));
    let (mut missing, mut declared, mut unknown) = (vec![], vec![], vec![]);
    let rows: Vec<Value> = ms.iter().map(|m| { let g = |k: &str| agg.get(m).and_then(|x| x.get(k)).copied().unwrap_or(0); let rent = g("rent");
        let costs: i64 = ["loan", "charges", "fees", "taxes", "insurance", "works", "other"].iter().map(|k| g(k)).sum(); let net = rent - costs;
        let in_vac = vac.iter().any(|(s, e)| month_start(m) <= e.unwrap_or(NaiveDate::MAX) && month_end(m) >= *s);
        let status = if first_rent.as_ref().is_none_or(|f| m < f) && rent == 0 { "not_let_yet" } else if in_vac && rent == 0 { "declared_vacancy" } else { match exp { None => if rent > 0 { "received" } else { "unknown" }, Some(e) => if rent >= e * 95 / 100 { "received" } else if rent > 0 { "partial" } else if *m == cur { "unknown" } else { "missing" } } };
        match status { "missing" => missing.push(m.clone()), "declared_vacancy" => declared.push(m.clone()), "unknown" => unknown.push(m.clone()), _ => {} }
        let mut o = json!({"month": m, "rent": money_str(rent), "costs": money_str(costs), "net": money_str(net), "effort": money_str((-net).max(0)), "owner_in": money_str(g("owner_in")), "owner_out": money_str(g("owner_out")), "complete": covered(m) && *m != cur,
            "rent_status": status, "expected_rent": exp.map(money_str), "rent_evidence": ev.get(m).cloned().unwrap_or_default(), "n_tx": n.get(m).copied().unwrap_or(0)});
        for k in KINDS.iter().filter(|k| **k != "rent") { o[*k] = money_str(g(k)).into(); } o }).collect();
    let since = first_rent.as_ref().map_or(0, |f| ms.iter().filter(|m| *m >= f && **m != cur).count());
    let complete: Vec<&Value> = rows.iter().filter(|r| r["complete"] == true).collect(); let avg = |k: &str| money_str(if complete.is_empty() { 0 } else { div_cents(complete.iter().map(|r| to_cents(r[k].as_str().unwrap_or("0").parse().unwrap_or(0.0))).sum(), complete.len() as i64) });
    json!({"months": rows, "rent": {"expected": exp.map(money_str), "source": src, "first_month": first_rent, "usual_day": null}, "n_months": ms.len(), "n_complete": complete.len(),
        "average": if complete.is_empty() { Value::Null } else { json!({"rent": avg("rent"), "costs": avg("costs"), "net": avg("net"), "effort": avg("effort")}) },
        "vacancy": {"missing_months": missing, "declared_months": declared, "late_paid_months": [], "unknown_months": unknown, "n_missing": missing.len(), "n_declared": declared.len(), "months_since_let": since, "occupancy_rate": (since > 0).then(|| round4((since - missing.len() - declared.len()) as f64 / since as f64))}})
}
fn scheme() -> Value { json!({"declared": false, "scheme": null, "start_date": null, "years": null, "end_date": null, "end_source": null, "effective_end_date": null, "state": "unknown", "extension": {"decision": "undecided"},
    "rent_cap": {"cap": null, "cap_basis": null, "rent": null, "rent_basis": null, "status": "unknown"}, "tenant_income": {"limit": null, "tenant_income": null, "status": "unknown"}, "missing": [], "warnings": ["no tax-incentive rental scheme is modelled for Belgium"]}) }
pub fn properties(ds: &Dataset) -> Vec<Asset> { loans::assets(&ds.memory_dir).into_iter().filter(|a| a.kind == "real_estate_rental").collect() }
pub fn pnl(cf: &Value, year: i32, today: NaiveDate) -> Value {
    let rows: Vec<&Value> = cf["months"].as_array().into_iter().flatten().filter(|m| m["month"].as_str().is_some_and(|x| x.starts_with(&year.to_string()))).collect();
    let keys = ["rent", "loan", "charges", "fees", "taxes", "insurance", "works", "other", "costs", "net", "effort", "owner_in", "owner_out"];
    let totals: serde_json::Map<String, Value> = keys.iter().map(|k| (k.to_string(), json!(money_str(rows.iter().map(|r| to_cents(r[*k].as_str().unwrap_or("0").parse().unwrap_or(0.0))).sum())))).collect();
    let n = rows.len() as i64; let eff = totals["effort"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
    json!({"year": year, "totals": totals, "n_months": n, "months_expected": if year == today.year() { today.month() as i64 } else { 12 }, "months_incomplete": rows.iter().filter(|r| r["complete"] == false).map(|r| r["month"].clone()).collect::<Vec<_>>(), "months_missing_data": [],
        "year_in_progress": year == today.year(), "complete": rows.iter().all(|r| r["complete"] == true) && n == 12, "monthly_average_effort": (n > 0).then(|| money_str(div_cents(to_cents(eff), n))), "vacancy": cf["vacancy"], "loan_split": null})
}
fn asset_json(a: &Asset) -> Value { json!({"id": a.id, "kind": a.kind, "account": a.account, "loan": a.loan, "scheme": a.scheme, "value": a.amount(), "as_of": a.as_of, "rent_monthly": a.rent_monthly, "purchase_price": a.purchase_price, "purchase_date": a.purchase_date, "commitment": null,
    "market_rate": a.market_rate.as_ref().map(crate::memstore::jsonify), "vacancies": a.vacancies.iter().map(|v| json!({"start": v.start, "end": v.end, "note": v.note})).collect::<Vec<_>>()}) }
pub fn list(ds: &Dataset) -> Value {
    let ps = properties(ds); let linked: Vec<String> = ps.iter().flat_map(|a| links(ds, a).accounts).collect();
    let rows: Vec<Value> = ps.iter().map(|a| { let lk = links(ds, a); let cf = cashflow(ds, a, &lk, 13); let last = cf["months"].as_array().and_then(|m| m.iter().rev().nth(1)).cloned();
        let owed: Option<i64> = lk.loans.iter().map(|l| loans::compute(l, ds.today).remaining.or(l.outstanding.map(loans::cents))).sum();
        let mut miss = lk.notes.clone(); if a.amount().is_none() { miss.push("value".into()); }
        json!({"id": a.id, "account_link": if lk.accounts.is_empty() { "none" } else { "linked" }, "loan_link": if lk.loans.is_empty() { "none" } else { "linked" }, "n_loans": lk.loans.len(), "expected_rent": cf["rent"]["expected"], "rent_source": cf["rent"]["source"],
            "last_month": last.map(|m| json!({"month": m["month"], "rent": m["rent"], "net": m["net"], "effort": m["effort"], "complete": m["complete"], "rent_status": m["rent_status"]})), "vacancy": cf["vacancy"],
            "scheme": {"declared": false, "scheme": null, "state": "unknown", "end_date": null, "effective_end_date": null, "months_left": null, "decision_needed": null},
            "net_equity": a.amount().and_then(|v| owed.map(|o| money_str(loans::cents(v) - o))), "missing": miss}) }).collect();
    json!({"as_of": ds.today.to_string(), "properties": rows, "unlinked_rental_accounts": ds.accounts.values().filter(|ac| ac.purpose.as_deref() == Some("rental") && !linked.contains(&ac.uid)).map(|ac| json!({"uid": ac.uid, "label": ac.label})).collect::<Vec<_>>(), "property_categories": KINDS})
}
pub fn detail(ds: &Dataset, id: &str, year: Option<i32>) -> Option<Value> {
    let a = properties(ds).into_iter().find(|a| a.id == id)?; let lk = links(ds, &a); let cf = cashflow(ds, &a, &lk, 24); let cur = cf["months"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
    let years: Vec<i32> = cf["months"].as_array().into_iter().flatten().filter(|m| m["n_tx"].as_u64().unwrap_or(0) > 0).filter_map(|m| m["month"].as_str()?.get(..4)?.parse().ok()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let y = year.unwrap_or(ds.today.year());
    Some(json!({"id": a.id, "kind": a.kind, "links": {"account": if lk.accounts.is_empty() { "none" } else { "linked" }, "accounts": lk.accounts.len(), "loan": if lk.loans.is_empty() { "none" } else { "linked" }, "loans": lk.loans.len(), "notes": lk.notes},
        "cashflow": cf, "current_month": {"month": cur["month"], "rent": cur["rent"], "expected": cur["expected_rent"], "status": cur["rent_status"]}, "pnl": pnl(&cf, y, ds.today), "scheme": scheme(), "years": years, "flows_to_label": 0, "asset": asset_json(&a), "tag": format!("rental:{}", a.id),
        "indexation": indexation(&a)}))
}
/// Belgium: private residential letting is taxed on the indexed cadastral income, not on the rents received.
pub fn tax(year: i32) -> Value {
    json!({"year": year, "country": "BE", "status": "not_applicable", "note": "In Belgium, letting a home privately to a private tenant is taxed on the property's indexed cadastral income (KI / RC), not on the rents: no rent-based figure is computed. Professional letting or furnished letting differ: ask an adviser.",
        "disclaimer": crate::disclaimers::TAX, "disclaimer_key": "tax"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn index() { let a: Asset = serde_yaml::from_str("id: flat\nkind: real_estate_rental\nlease: {start_date: 2022-09-01, base_rent: 800, base_index: 118.5, current_index: 130.2}\n").unwrap(); let v = indexation(&a);
        assert_eq!(v["indexed_rent"], "878.99", "800 x 130.2 / 118.5 = 878.987"); assert_eq!(v["change"], "78.99"); }
}
