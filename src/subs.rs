//! Subscriptions and contracts (E8), for Belgium: the inventory of recurring costs (detected series + `contracts/*.yaml`), their
//! contracts, usage as the user states it, and whether / when a contract can be ended under Belgian consumer rules. The rules are
//! general information with their legal basis, never legal advice; nothing is ever cancelled or sent.
use crate::{analytics::Dataset, util::*};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Contract { pub id: String, pub provider: Option<String>, pub kind: Option<String>, pub holder: Option<String>, pub amount_match: Option<AmountMatch>, pub contract_number: Option<String>, pub merchant_match: Option<String>,
    pub start_date: Option<String>, pub renewal: Option<String>, pub billing: Option<Billing>, pub commitment_end: Option<String>, pub notice_period_days: Option<i64>, pub usage: Option<serde_yaml::Value>, pub keep: Option<serde_yaml::Value>, pub notes: Option<String>, #[serde(skip)] pub file: String }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct AmountMatch { pub amount: f64, pub tolerance_pct: Option<f64> }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Billing { pub amount: Option<f64>, pub period: Option<String> }
pub fn contracts(mem: &Path) -> Vec<Contract> {
    let mut v: Vec<Contract> = std::fs::read_dir(mem.join("contracts")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "yaml") && !p.file_name().unwrap().to_string_lossy().starts_with('_'))
        .filter_map(|p| { let mut c: Contract = serde_yaml::from_str(&std::fs::read_to_string(&p).ok()?).ok()?; c.file = format!("contracts/{}", p.file_name()?.to_string_lossy()); (!c.id.is_empty()).then_some(c) }).collect(); v.sort_by(|a, b| a.id.cmp(&b.id)); v
}
fn dd(s: &Option<String>) -> Option<NaiveDate> { s.as_deref().and_then(d) }
const GROUPS: [(&str, &str); 6] = [("telecom", "Telecom"), ("utilities", "Energy and water"), ("insurance", "Insurance"), ("subscriptions", "Subscriptions"), ("health", "Health"), ("other", "Other")];
fn group_of(kind: Option<&str>, cat: &str) -> &'static str {
    match kind { Some("telecom") => "telecom", Some("energy" | "water") => "utilities", Some(k) if k.starts_with("insurance") => "insurance", Some("streaming" | "software" | "membership") => "subscriptions", Some("health") => "health",
        _ => if cat.contains("telecom") { "telecom" } else if cat.contains("energy") || cat.contains("water") { "utilities" } else if cat.starts_with("insurance") { "insurance" } else if cat.starts_with("subscriptions") { "subscriptions" } else if cat.starts_with("health") { "health" } else { "other" } }
}
fn family(c: &Contract, cat: &str) -> &'static str {
    match c.kind.as_deref() { Some("telecom") => "telecom", Some("energy") => "energy", Some("insurance_health") | Some("health") => "insurance_health", Some(k) if k.starts_with("insurance") => "insurance", Some("streaming" | "software" | "membership") => "tacit_renewal", Some("water") => "utility",
        _ => if cat.contains("telecom") { "telecom" } else if cat.contains("energy") { "energy" } else if cat.starts_with("insurance") { "insurance" } else { "tacit_renewal" } }
}
fn rule(id: &str, name: &str, law: &str, source: &str) -> Value { json!({"id": id, "name": name, "law": law, "source": source, "last_reviewed": "2026-11-23"}) }
const DISC: &str = "General summary of Belgian consumer rules, not legal advice: the contract's own terms and your situation decide.";
const VERIFY: &str = "Verify with your contract and the official source (FPS Economy, BIPT for telecom, CREG or the regional regulator for energy, the Insurance Ombudsman) before acting.";
/// Can this contract be ended, when, at what notice and cost, under the Belgian rule of its family.
#[allow(unused_assignments)]
pub fn cancellability(c: &Contract, cat: &str, monthly: Option<i64>, today: NaiveDate) -> Value {
    let fam = family(c, cat); let start = dd(&c.start_date); let renewal = dd(&c.renewal); let commit = dd(&c.commitment_end);
    let (mut can, mut earliest, mut notice, mut conds, mut unknown, mut cost, mut anniv, mut send_by): (Option<bool>, Option<NaiveDate>, Option<i64>, Vec<String>, Vec<String>, Value, Value, Option<NaiveDate>) = (None, None, None, vec![], vec![], Value::Null, Value::Null, None);
    let (method, basis) = match fam {
        "telecom" => { can = Some(true); earliest = Some(today); notice = Some(0);
            conds.push("you may end the contract at any time; a new operator can handle the switch (Easy Switch)".into());
            match start { Some(s) if add_months(s, 6) > today => { let free = add_months(s, 6); let left = ((free - today).num_days() + 29) / 30;
                    conds.push(format!("within the first 6 months the operator may charge at most the subscription fees until {free}")); cost = json!({"basis": "the subscription fees until the end of month 6", "share_pct": null, "remaining_months": left, "free_exit_date": free.to_string(), "amount": monthly.map(|m| (m * left) as f64 / 100.0), "needs": if monthly.is_none() { json!("the monthly price") } else { Value::Null }}); }
                Some(_) => conds.push("the first 6 months are over: no termination fee for the subscription".into()), None => unknown.push("start_date (the first 6 months may carry a fee)".into()) }
            conds.push("a phone bought with the contract at a reduced price may have a residual value to pay".into());
            ("a written or online notice to the operator, or the new operator's switching service", vec![rule("be-telecom", "End of an electronic communications contract", "Law of 13 June 2005 on electronic communications, art. 111/3", "https://www.bipt.be")]) }
        "energy" => { can = Some(true); notice = Some(30); earliest = Some(add_months(today, 1));
            conds.push("a household may end an electricity or gas supply contract at any time, free of charge, with one month's notice".into()); conds.push("switching supplier: the new supplier handles the change".into());
            ("a notice to the supplier, or a new supply contract", vec![rule("be-energy", "Free termination of an energy supply contract", "Laws on the electricity (29 April 1999) and gas (12 April 1965) markets, consumer protection provisions", "https://www.creg.be")]) }
        "insurance" | "insurance_health" => { notice = Some(c.notice_period_days.unwrap_or(90));
            conds.push("a contract of at most one year renews tacitly; it ends at the annual due date if notice is given at least 3 months before".into());
            if fam == "insurance_health" { conds.push("a hospitalisation (health) insurance is lifelong for the insurer, but you may end it each year with the same notice".into()); }
            match renewal { Some(r) => { let mut due = r; while add_months(due, -3) < today { due = add_months(due, 12); } let by = add_months(due, -3); send_by = Some(by); earliest = Some(due); can = Some(true);
                    anniv = json!({"effective": due.to_string(), "send_notice_by": by.to_string(), "months_notice": 3, "notice_still_possible": true}); }
                None => unknown.push("renewal (the annual due date)".into()) }
            ("a registered letter, a bailiff's writ, or a letter handed over against a receipt", vec![rule("be-insurance", "Termination of an insurance contract at its annual due date", "Insurance Act of 4 April 2014, art. 85", "https://www.ombudsman-insurance.be")]) }
        _ => { notice = Some(c.notice_period_days.unwrap_or(60).min(60));
            match (commit, renewal) { (Some(e), _) if e > today => { earliest = Some(e); can = Some(false); conds.push(format!("fixed term until {e}: ending it earlier follows the contract")); }
                (_, Some(_)) | (Some(_), None) => { can = Some(true); earliest = Some(today + chrono::Duration::days(notice.unwrap())); conds.push("after a tacit renewal of a fixed-term services contract, you may end it at any time with at most 2 months' notice, without compensation".into()); }
                (None, None) => { unknown.push("commitment_end or renewal".into()); conds.push("an open-ended subscription usually ends with the notice its terms set; a tacitly renewed fixed-term one at any time with at most 2 months' notice".into()); } }
            ("the cancellation route of the terms (account settings, e-mail or letter)", vec![rule("be-tacit-renewal", "Tacitly renewed services contracts", "Code of Economic Law, art. VI.91/1 to VI.91/3", "https://economie.fgov.be")]) }
    };
    let deadline = match (renewal, c.notice_period_days) { (Some(r), Some(n)) if r > today => { let by = r - chrono::Duration::days(n); json!({"renewal": r.to_string(), "send_notice_by": by.to_string(), "notice_period_days": n, "days_left": (by - today).num_days()}) } _ => Value::Null };
    json!({"country": "BE", "family": fam, "can_cancel_now": can, "earliest_effective_date": earliest.map(|x| x.to_string()), "notice_period_days": notice, "method": method, "conditions": conds, "unknown": unknown, "legal_basis": basis, "last_reviewed": "2026-11-23",
        "verify": VERIFY, "disclaimer": DISC, "disclaimer_key": "contract", "verify_key": "contract_verify", "early_termination_cost": cost, "anniversary_route": anniv, "contract_notice_deadline": deadline,
        "first_request_date": today.to_string(), "send_notice_by": send_by.map(|x| x.to_string())})
}
fn usage(c: Option<&Contract>) -> Value {
    let u = c.and_then(|c| c.usage.as_ref()); let (f, last, note) = match u { Some(serde_yaml::Value::Mapping(m)) => (m.get("frequency").and_then(|x| x.as_str()).unwrap_or("unknown").to_string(), m.get("last_used").and_then(|x| x.as_str()).map(String::from), m.get("note").and_then(|x| x.as_str()).map(String::from)),
        Some(serde_yaml::Value::String(s)) => ("unknown".into(), None, Some(s.clone())), _ => ("unknown".into(), None, None) };
    json!({"frequency": f, "last_used": last, "note": note, "recorded": u.is_some(), "measurable": false, "question_asked": false, "note_not_measurable": "the bank data cannot tell how often a service is used", "signals": []})
}
fn matches(c: &Contract, key: &str, entity: &str, amount: i64) -> bool {
    let Some(rx) = c.merchant_match.as_deref().and_then(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()) else { return false };
    (rx.is_match(key).unwrap_or(false) || rx.is_match(entity).unwrap_or(false)) && c.amount_match.as_ref().is_none_or(|m| { let t = m.tolerance_pct.unwrap_or(3.0); (amount.abs() as f64 / 100.0 - m.amount).abs() <= m.amount * t / 100.0 })
}
fn monthly_of(c: &Contract) -> Option<i64> { let b = c.billing.as_ref()?; let a = to_cents(b.amount?); Some(match b.period.as_deref() { Some("yearly") => div_cents(a, 12), Some("quarterly") => div_cents(a, 3), Some("bimonthly") => div_cents(a, 2), _ => a }) }
/// Every recurring cost, with its contract, usage, price history and the Belgian cancellation rule.
pub fn inventory(ds: &Dataset, recur: &crate::analytics::recurring::Recurring, include_ended: bool, today: NaiveDate) -> Value {
    let cs = contracts(&ds.memory_dir); let pcs = crate::analytics::recurring::price_changes(ds, recur, None, false, &Default::default(), false); let rec = &recur.json;
    let loan_rx: Vec<fancy_regex::Regex> = crate::loans::liabilities(&ds.memory_dir).0.iter().filter_map(|l| l.payment_match.as_deref()).filter_map(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()).collect();
    let mut used: Vec<String> = vec![]; let mut rows = vec![];
    for x in rec["series"].as_array().into_iter().flatten().filter(|x| x["direction"] == "out" && (include_ended || x["status"] == "active")) {
        let (key, ent, cat) = (x["key"].as_str().unwrap_or(""), x["entity"].as_str().unwrap_or(""), x["category"].as_str().unwrap_or("")); let exp = x["expected_amount"].as_str().map(|s| to_cents(s.parse().unwrap_or(0.0))).unwrap_or(0);
        if cat.starts_with("transfer.") || cat.starts_with("debt.") || cat.starts_with("taxes.") || cat == "housing.rent" || cat == "housing.property_tax" || cat.contains("mortgage") || cat.contains("loan") || loan_rx.iter().any(|r| r.is_match(key).unwrap_or(false) || r.is_match(ent).unwrap_or(false)) { continue; }
        let c = cs.iter().find(|c| matches(c, key, ent, exp)); if let Some(c) = c { used.push(c.id.clone()); }
        let yearly = x["yearly_cost"].as_str().map(|s| to_cents(s.parse().unwrap_or(0.0))); let monthly = yearly.map(|y| div_cents(y, 12));
        let occ: Vec<&Value> = x["occurrences"].as_array().map(|a| a.iter().collect()).unwrap_or_default(); let mut hist = vec![]; let mut last: Option<&str> = None;
        for o in &occ { let a = o["amount"].as_str(); if a != last { hist.push(json!({"from": o["date"], "amount": a.map(|s| s.trim_start_matches('-').to_string())})); last = a; } }
        let pch: Vec<Value> = pcs["changes"].as_array().into_iter().flatten().filter(|p| p["series_id"] == x["id"]).map(|p| json!({"id": p["id"], "date": p["date"], "old": p["old"], "new": p["new"], "direction": p["direction"], "pct": p["pct"], "yearly_impact": p["yearly_impact"], "confirmed": p["confirmed"]})).collect();
        let next = x["next_expected"].as_str(); let overdue = next.and_then(d).map_or(0, |n| (today - n).num_days().max(0));
        rows.push(row(x["id"].as_str().unwrap_or(""), if ent.is_empty() { key } else { ent }, ent, cat, c, "series", x["cadence"].as_str(), x["expected_amount"].as_str().map(|s| s.trim_start_matches('-').to_string()), monthly, yearly, x["status"].as_str().unwrap_or("active"),
            occ.first().and_then(|o| o["date"].as_str()), x["last_date"].as_str().or(occ.last().and_then(|o| o["date"].as_str())), next, overdue, x["account_label"].as_str(), hist, pch, today));
    }
    for c in cs.iter().filter(|c| !used.contains(&c.id)) { let m = monthly_of(c); rows.push(row(&format!("ctr_{}", c.id), c.provider.as_deref().unwrap_or(&c.id), c.provider.as_deref().unwrap_or(""), "", Some(c), if m.is_some() { "contract" } else { "unknown" }, c.billing.as_ref().and_then(|b| b.period.as_deref()), m.map(money_str), m, m.map(|m| m * 12), "contract_only", None, None, None, 0, None, vec![], vec![], today)); }
    let mut groups: BTreeMap<&str, (usize, i64, i64, usize, usize)> = BTreeMap::new();
    for r in rows.iter().filter(|r| r["status"] != "ended") { let e = groups.entry(GROUPS.iter().find(|g| g.0 == r["group"]).map_or("other", |g| g.0)).or_default(); e.0 += 1; e.1 += r["monthly"].as_str().map_or(0, |s| to_cents(s.parse().unwrap_or(0.0))); e.2 += r["yearly"].as_str().map_or(0, |s| to_cents(s.parse().unwrap_or(0.0))); if r["contract"]["status"] == "missing" { e.3 += 1; } if r["monthly"].is_null() { e.4 += 1; } }
    let tot = |i: usize| groups.values().map(|g| [g.1, g.2][i]).sum::<i64>(); let active: Vec<&Value> = rows.iter().filter(|r| r["status"] != "ended").collect();
    json!({"as_of": today.to_string(), "country": "BE", "rows": rows, "groups_meta": GROUPS.iter().map(|(i, l)| json!({"id": i, "label": l})).collect::<Vec<_>>(),
        "groups": groups.iter().map(|(g, v)| (g.to_string(), json!({"label": GROUPS.iter().find(|x| x.0 == *g).unwrap().1, "count": v.0, "monthly": money_str(v.1), "yearly": money_str(v.2), "without_contract": v.3, "unknown_cost": v.4}))).collect::<serde_json::Map<_, _>>(),
        "totals": {"services": active.len(), "monthly": money_str(tot(0)), "yearly": money_str(tot(1)), "without_contract": active.iter().filter(|r| r["contract"]["status"] == "missing").count(), "expired_contracts": active.iter().filter(|r| r["contract"]["status"] == "expired").count(),
            "with_cancellation_rule": active.len(), "cancellation_decidable": active.iter().filter(|r| !r["cancellation"]["can_cancel_now"].is_null()).count(), "usage_unknown": active.iter().filter(|r| r["usage"]["frequency"] == "unknown").count(), "outdated_alternatives": 0, "reminders": 0},
        "savings": savings_summary(), "notes": ["Cancellation rules: Belgian consumer law, general information; alternatives and decisions are not ported to finch yet."]})
}
#[allow(clippy::too_many_arguments)]
fn row(rf: &str, name: &str, ent: &str, cat: &str, c: Option<&Contract>, src: &str, cadence: Option<&str>, expected: Option<String>, monthly: Option<i64>, yearly: Option<i64>, status: &str, first: Option<&str>, last: Option<&str>, next: Option<&str>, overdue: i64, account: Option<&str>, hist: Vec<Value>, pch: Vec<Value>, today: NaiveDate) -> Value {
    let g = group_of(c.and_then(|c| c.kind.as_deref()), cat); let expired = c.and_then(|c| dd(&c.commitment_end).or(dd(&c.renewal))).is_some_and(|e| e < today && c.is_some_and(|c| c.renewal.as_deref().and_then(d).is_some_and(|r| r < today)));
    let contract = match c { Some(c) => json!({"status": if expired { "expired" } else { "on_file" }, "id": c.id, "provider": c.provider, "renewal": c.renewal, "commitment_end": c.commitment_end, "notice_period_days": c.notice_period_days, "keep": c.keep.as_ref().map(crate::memstore::jsonify), "contract_number_on_file": c.contract_number.is_some()}), None => json!({"status": "missing", "id": null}) };
    let empty = Contract::default();
    json!({"ref": rf, "name": name, "entity": ent, "group": g, "group_label": GROUPS.iter().find(|x| x.0 == g).unwrap().1, "category": (!cat.is_empty()).then_some(cat), "kind": c.and_then(|c| c.kind.clone()).unwrap_or_else(|| "other".into()), "cost_source": src, "cadence": cadence,
        "expected_amount": expected, "monthly": monthly.map(money_str), "yearly": yearly.map(money_str), "status": status, "first_seen": first, "last_payment": last, "next_charge": next, "overdue_days": overdue, "account": account, "price_history": hist, "price_changes": pch,
        "contract": contract, "series_id": (src == "series").then_some(rf), "series": if src == "series" { vec![rf] } else { vec![] }, "contract_id": c.map(|c| c.id.clone()), "linked_series": [], "draftable": c.is_none(), "usage": usage(c),
        "cancellation": cancellability(c.unwrap_or(&empty), cat, monthly, today), "alternatives": {"count": 0, "current": 0, "outdated": 0, "best": null, "items": [], "note": "alternatives are recorded in the Python app"}, "decision": null, "proposed_decisions": []})
}
pub fn savings_summary() -> Value { json!({"realised_monthly": "0.00", "realised_since_decisions": "0.00", "realised_yearly_run_rate": "0.00", "verified": 0, "pending": 0, "contradicted": 0, "claimed_monthly_unverified": "0.00"}) }
pub fn savings_view(today: NaiveDate) -> Value { let mut v = savings_summary(); v["as_of"] = today.to_string().into(); v["decisions"] = json!([]); v["proposed"] = json!([]); v["reminders"] = json!([]); v["note"] = "decisions (cancel / switch / downgrade) and their verified savings are not ported to finch yet".into(); v }

#[cfg(test)]
mod tests {
    use super::*;
    fn c(y: &str) -> Contract { serde_yaml::from_str(y).unwrap() }
    #[test] fn belgian_rules() {
        let t = d("2026-03-10").unwrap();
        let tel = cancellability(&c("id: gsm\nkind: telecom\nstart_date: 2026-01-15\n"), "", Some(2500), t); assert_eq!(tel["can_cancel_now"], true); assert_eq!(tel["early_termination_cost"]["free_exit_date"], "2026-07-15"); assert_eq!(tel["early_termination_cost"]["remaining_months"], 5);
        let ins = cancellability(&c("id: home\nkind: insurance_home\nrenewal: 2026-05-01\n"), "", None, t); assert_eq!(ins["anniversary_route"]["effective"], "2027-05-01", "too late for this year: 3 months before is past");
        let ins = cancellability(&c("id: home\nkind: insurance_home\nrenewal: 2026-08-01\n"), "", None, t); assert_eq!((ins["earliest_effective_date"].as_str(), ins["send_notice_by"].as_str()), (Some("2026-08-01"), Some("2026-05-01")));
        let en = cancellability(&c("id: e\nkind: energy\n"), "", None, t); assert_eq!(en["earliest_effective_date"], "2026-04-10");
        let st = cancellability(&c("id: s\nkind: streaming\n"), "", None, t); assert!(st["can_cancel_now"].is_null()); assert!(!st["unknown"].as_array().unwrap().is_empty());
    }
}
