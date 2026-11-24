//! Loan scenarios (a calculator on the real schedule, Belgian rules), net-worth snapshots and history, the lease view, and loan
//! facts inferred from the bank payments (offered as a memory PROPOSAL, never written). Estimates only: the lender's figures decide.
use crate::{analytics::Dataset, loans::{self, cents, early_repayment, table, Liability, Sched}, util::*};
use anyhow::{bail, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub const DISC: &str = "Estimate from the figures you gave, not an offer: your bank or a broker gives the binding numbers. General information, not financial advice; no product is recommended.";
fn verdict(net: i64) -> Value { json!({"verdict": if net > 0 { "saves money" } else { "costs more than it saves" }, "verdict_code": if net > 0 { "saves" } else { "costs" }}) }
fn needs(s: &Sched) -> Option<Value> { (s.status != "computed").then(|| json!({"status": "needs_fields", "missing": s.missing, "alternative": s.alternative})) }
fn break_even(base: &[(i64, i64, i64, i64)], alt: &[(i64, i64, i64, i64)], pen: i64) -> Option<usize> { if pen == 0 { return Some(0); } let mut acc = 0; for (i, b) in base.iter().enumerate() { acc += b.0 - alt.get(i).map_or(0, |a| a.0); if acc >= pen { return Some(i + 1); } } None }
fn ins_total(l: &Liability, p0: i64, rows: &[(i64, i64, i64, i64)]) -> i64 { let Some(i) = &l.insurance else { return 0 }; rows.iter().map(|r| match (i.monthly, i.rate_pct) { (Some(m), _) => cents(m), (None, Some(p)) => ((if i.basis.as_deref() == Some("outstanding") { r.3 + r.1 } else { p0 }) as f64 * p / 1200.0).round() as i64, _ => 0 }).sum() }
/// Repay `amount` early on `on`: the two options (keep the instalment and end earlier, or keep the term and pay less), net of the legal indemnity.
pub fn prepay(l: &Liability, s: &Sched, today: NaiveDate, amount: f64, on: Option<NaiveDate>, penalty: Option<f64>) -> Result<Value> {
    if let Some(n) = needs(s) { return Ok(n); } let day = on.unwrap_or(today).max(today); let rows: Vec<&loans::Row> = s.rows.iter().filter(|r| r.due > day).collect();
    let cap = s.rows.iter().rfind(|r| r.due <= day).map_or(s.capital, |r| r.balance); let amt = cents(amount); if amt <= 0 { bail!("the amount must be positive"); }
    if rows.is_empty() { return Ok(json!({"status": "nothing_left", "note": "no instalment remains after that date"})); }
    if amt >= cap { return Ok(json!({"status": "payoff", "capital_due": money_str(cap), "note": "the amount covers the whole capital: ask the lender for a payoff quote (capital + accrued interest + indemnity)"})); }
    if rows.iter().any(|r| r.kind == "deferral") { return Ok(json!({"status": "needs_fields", "missing": ["a prepayment during a deferral is not modelled: ask the lender"]})); }
    let er = early_repayment(l, s, Some(amt), today); let pen = penalty.map(cents).or_else(|| er["max_indemnity"].as_str().map(|x| to_cents(x.parse().unwrap_or(0.0)))).unwrap_or(0);
    let base = table(cap, s.monthly_rate, None, Some(s.payment)).unwrap_or_default(); let bi: i64 = base.iter().map(|r| r.0).sum(); let p0 = l.principal.map_or(cap, cents);
    let kp = table(cap - amt, s.monthly_rate, None, Some(s.payment)).unwrap_or_default(); let kp_i: i64 = kp.iter().map(|r| r.0).sum(); let (ib, ik) = (ins_total(l, p0, &base), ins_total(l, p0, &kp)); let net1 = bi - kp_i + ib - ik - pen;
    let pay1 = loans::annuity(cap - amt, s.monthly_rate, base.len() as i64); let kt = table(cap - amt, s.monthly_rate, Some(base.len() as i64), Some(pay1)).unwrap_or_default(); let kt_i: i64 = kt.iter().map(|r| r.0).sum(); let it = ins_total(l, p0, &kt); let net2 = bi - kt_i + ib - it - pen;
    let mut o1 = json!({"mode": "keep_payment", "label": "Keep the instalment, finish earlier", "new_instalment": money_str(s.payment), "monthly_change": "0.00", "months_saved": base.len() - kp.len(), "new_last_instalment": add_months(rows[0].due, kp.len() as i32 - 1).to_string(),
        "interest_saved": money_str(bi - kp_i), "insurance_saved": money_str(ib - ik), "penalty": money_str(pen), "net_saving": money_str(net1), "break_even_months": break_even(&base, &kp, pen)});
    let mut o2 = json!({"mode": "keep_term", "label": "Keep the end date, pay less each month", "new_instalment": money_str(pay1), "monthly_change": money_str(pay1 - s.payment), "months_saved": 0, "new_last_instalment": rows.last().unwrap().due.to_string(),
        "interest_saved": money_str(bi - kt_i), "insurance_saved": money_str(ib - it), "penalty": money_str(pen), "net_saving": money_str(net2), "break_even_months": break_even(&base, &kt, pen)});
    for (o, n) in [(&mut o1, net1), (&mut o2, net2)] { if let Some(m) = o.as_object_mut() { m.extend(verdict(n).as_object().unwrap().clone()); } }
    let mut notes = s.assumptions.clone(); if l.insurance.as_ref().is_some_and(|i| i.monthly.is_some()) { notes.push("a flat insurance premium is not reduced by a partial prepayment unless the insurer recalculates it".into()); } notes.push(format!("estimate from the stored schedule; the lender's table decides. {DISC}"));
    Ok(json!({"status": "computed", "country": "BE", "date": day.to_string(), "amount": money_str(amt), "capital_before": money_str(cap), "capital_after": money_str(cap - amt), "remaining_instalments": rows.len(), "instalment": money_str(s.payment), "penalty": money_str(pen),
        "penalty_basis": if penalty.is_some() { "the indemnity you entered".to_string() } else { er["rule"].as_str().unwrap_or("").to_string() }, "approximate": s.approximate, "options": [o1, o2], "notes": notes, "disclaimer_key": "loan"}))
}
/// A lower rate on the capital still due: internal (same lender: its fees) or external (another lender: the reinvestment indemnity + new mortgage costs).
pub fn renegotiate(l: &Liability, s: &Sched, today: NaiveDate, new_rate: f64, variant: Option<&str>, bank_fees: f64, guarantee_fees: f64, other_fees: f64, penalty: Option<f64>) -> Result<Value> {
    if let Some(n) = needs(s) { return Ok(n); } let (cap, months) = (s.remaining.unwrap_or(0), s.rows.iter().filter(|r| !r.made).count() as i64); if months == 0 || cap <= 0 { return Ok(json!({"status": "nothing_left"})); }
    let variant = variant.unwrap_or(if l.kind == "mortgage" { "external" } else { "internal" }); let cur = l.rate.as_ref().and_then(|r| r.nominal).unwrap_or(0.0);
    let r1 = loans::monthly_rate(new_rate, l.rate.as_ref().and_then(|r| r.monthly_convention.as_deref())); let (p0, p1) = (loans::annuity(cap, s.monthly_rate, months), loans::annuity(cap, r1, months));
    let (i0, i1): (i64, i64) = (table(cap, s.monthly_rate, Some(months), None).unwrap_or_default().iter().map(|r| r.0).sum(), table(cap, r1, Some(months), None).unwrap_or_default().iter().map(|r| r.0).sum());
    let pen = penalty.map(cents).unwrap_or(if variant == "external" { early_repayment(l, s, Some(cap), today)["max_indemnity"].as_str().map_or(0, |x| to_cents(x.parse().unwrap_or(0.0))) } else { 0 });
    let costs = pen + cents(bank_fees) + cents(guarantee_fees) + cents(other_fees); let saving = p0 - p1; let net = i0 - i1 - costs; let be = (saving > 0).then(|| ((costs as f64) / saving as f64).ceil() as i64).filter(|b| *b <= months);
    Ok(json!({"status": "computed", "basis": {"capital": money_str(cap), "remaining_instalments": months, "current_rate_pct": cur, "from": "the amortization schedule"}, "variant": variant, "country": "BE", "current_rate_pct": cur, "new_rate_pct": new_rate, "current_payment": money_str(p0), "new_payment": money_str(p1),
        "monthly_saving": money_str(saving), "gross_interest_saving": money_str(i0 - i1), "penalty": money_str(pen), "total_costs": money_str(costs), "net_saving": money_str(net), "break_even_months": be, "verdict": if net > 0 && be.is_some() { "worth asking the lenders" } else { "the costs eat the saving" },
        "notes": [if variant == "external" { "refinancing with another lender: the reinvestment indemnity (at most 3 months of interest on a mortgage), a new mortgage registration or mandate, notary and file costs; ask for the binding figures" } else { "an internal renegotiation: the lender's file fee; no reinvestment indemnity when the lender agrees" }, DISC], "disclaimer_key": "loan"}))
}
/// Switching the borrower insurance (schuldsaldoverzekering) to another insurer for the remaining months.
pub fn insurance(s: &Sched, alt_monthly: f64, fees: f64) -> Result<Value> {
    if let Some(n) = needs(s) { return Ok(n); } let Some(nx) = s.rows.iter().find(|r| !r.made) else { return Ok(json!({"status": "nothing_left"})) };
    if nx.insurance <= 0 { return Ok(json!({"status": "needs_fields", "missing": ["insurance.monthly (or insurance.rate_pct and basis)"]})); }
    let months = s.rows.iter().filter(|r| !r.made).count() as i64; let (cur, alt) = (nx.insurance, cents(alt_monthly)); let net = (cur - alt) * months - cents(fees);
    Ok(json!({"status": "computed", "basis": {"current_monthly_from": "the next instalment of the schedule", "remaining_instalments": months}, "country": "BE", "current_monthly": money_str(cur), "alternative_monthly": money_str(alt), "remaining_months": months, "monthly_saving": money_str(cur - alt),
        "total_saving": money_str((cur - alt) * months), "total_costs": money_str(cents(fees)), "net_saving": money_str(net), "verdict": if net > 0 { "saves money" } else { "costs more than it saves" },
        "notes": ["in Belgium you may take the borrower insurance from another insurer; the lender may require equivalent cover, and a health questionnaire applies", DISC], "disclaimer_key": "loan"}))
}
/// Store a computed scenario in the insights feed (no loan id, no lender).
pub fn save_insight(con: &Connection, kind: &str, loan_kind: &str, res: &Value) -> Result<String> {
    let label = match kind { "prepay" => "Early repayment", "renegotiate" => "Renegotiation", _ => "Insurance change" }; let id = format!("cin_{:010x}", rand::random::<u64>() & 0xff_ffff_ffff);
    let body = format!("**{label}** estimate on the stored amortization schedule. Estimates only, not an offer.\n\nNet saving {} EUR.\n\nGeneral information, not financial advice.", res["net_saving"].as_str().or(res["options"][0]["net_saving"].as_str()).unwrap_or("?"));
    con.execute("INSERT INTO insights(id, created, kind, title, body, findings, skill, backend, model) VALUES (?,?,'finding',?,?,?,'loan-scenario','local','none')", params![id, now_iso(), format!("{label} scenario ({})", loan_kind.replace('_', " ")), body, json!([res]).to_string()])?; Ok(id)
}

// ---------------------------------------------------------------- net-worth history
/// Record today's net worth (one snapshot per day, replaced when taken again).
pub fn snapshot(con: &Connection, ds: &Dataset, stale_months: i32) -> Result<Value> {
    let nw = loans::net_worth(ds, ds.today, stale_months); let c = |k: &str| to_cents(nw["by_category"][k].as_str().unwrap_or("0").parse().unwrap_or(0.0)); let t = |v: &Value| to_cents(v.as_str().unwrap_or("0").parse().unwrap_or(0.0));
    let assets = c("cash") + c("savings") + c("investments") + c("real_estate") + c("vehicles") + c("other");
    con.execute("INSERT OR REPLACE INTO net_worth_history(as_of, month, source, net_worth_c, assets_c, liabilities_c, cash_c, savings_c, investments_c, real_estate_c, vehicles_c, other_c, n_unknown, complete, detail, created_at) VALUES (?,?,'snapshot',?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![ds.today.to_string(), month_key(ds.today), t(&nw["net_worth"]), assets, t(&nw["liabilities"]["total"]), c("cash"), c("savings"), c("investments"), c("real_estate"), c("vehicles"), c("other"), nw["n_unknown"].as_i64().unwrap_or(0), nw["complete"] == true, json!({"by_owner": nw["by_owner"], "unknown": nw["unknown"]}).to_string(), now_iso()])?;
    Ok(json!({"as_of": ds.today.to_string(), "net_worth": nw["net_worth"], "complete": nw["complete"]}))
}
pub fn history(con: &Connection, months: i64) -> Result<Vec<Value>> {
    let mut st = con.prepare("SELECT month, as_of, source, net_worth_c, assets_c, liabilities_c, cash_c, savings_c, investments_c, real_estate_c, vehicles_c, other_c, n_unknown, complete, detail FROM net_worth_history ORDER BY as_of DESC")?;
    let mut by: std::collections::BTreeMap<String, Value> = Default::default();
    for r in st.query_map([], |r| { let d: Value = serde_json::from_str(&r.get::<_, String>(14)?).unwrap_or(json!({})); Ok(json!({"month": r.get::<_, String>(0)?, "as_of": r.get::<_, String>(1)?, "source": r.get::<_, String>(2)?, "net_worth": money_str(r.get(3)?), "assets": money_str(r.get(4)?), "liabilities": money_str(r.get(5)?),
        "by_category": {"cash": money_str(r.get(6)?), "savings": money_str(r.get(7)?), "investments": money_str(r.get(8)?), "real_estate": money_str(r.get(9)?), "vehicles": money_str(r.get(10)?), "other": money_str(r.get(11)?)}, "n_unknown": r.get::<_, i64>(12)?, "complete": r.get::<_, i64>(13)? != 0, "unknown": d["unknown"]})) })? {
        let v = r?; by.entry(v["month"].as_str().unwrap().to_string()).or_insert(v); }
    Ok(by.into_values().rev().take(months as usize).collect::<Vec<_>>().into_iter().rev().collect())
}

// ---------------------------------------------------------------- lease (LOA / LLD) and inference
pub fn lease_view(l: &Liability, mem: &std::path::Path, today: NaiveDate) -> Option<Value> {
    if !matches!(l.kind.as_str(), "loa" | "lld") { return None; }
    let v: Value = serde_json::to_value(serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(mem.join(&l.file)).unwrap_or_default()).unwrap_or_default()).unwrap_or_default();
    let reads: Vec<(NaiveDate, i64)> = v["odometer"].as_array().into_iter().flatten().filter_map(|o| Some((d(o["date"].as_str()?)?, o["km"].as_i64()?))).collect(); let end = l.end_date.as_deref().and_then(d);
    let mut miss = vec![]; for (f, x) in [("end_date", end.is_none()), ("residual_value", l.residual_value.is_none()), ("mileage_limit_km", l.mileage_limit_km.is_none())] { if x { miss.push(f); } }
    let start = l.start_date.as_deref().and_then(d); let init = l.initial_km.unwrap_or(0);
    let pace = match (reads.last(), start.or(reads.first().map(|r| r.0))) { (Some(last), Some(s)) if (last.0 - s).num_days() >= 30 => { let per = (last.1 - if start.is_some() { init } else { reads[0].1 }) as f64 / (last.0 - s).num_days() as f64; Some(per) } _ => None };
    let proj = match (pace, end, reads.last()) { (Some(p), Some(e), Some(last)) => Some(last.1 + (p * (e - last.0).num_days().max(0) as f64).round() as i64), _ => None };
    let excess = match (proj, l.mileage_limit_km) { (Some(p), Some(lim)) => Some((p - init - lim).max(0)), _ => None };
    Some(json!({"id": l.id, "kind": l.kind, "missing": miss, "checklist": ["the return condition report (damage beyond normal wear is charged)", "the kilometres against the contract's limit", "whether buying at the residual value makes sense against the car's market value"],
        "end": {"end_date": end.map(|e| e.to_string()), "known": end.is_some(), "days_left": end.map(|e| (e - today).num_days()), "ended": end.is_some_and(|e| e < today)},
        "decision": {"residual_value": l.residual_value.map(|x| money_str(cents(x))), "needs": if l.residual_value.is_none() { vec!["residual_value"] } else { vec![] }, "note": "compare the residual value with the car's market value near the end of the contract; you decide"},
        "mileage": {"limit_km": l.mileage_limit_km, "excess_km_fee": l.excess_km_fee.map(|x| money_str(cents(x))), "readings": reads.len(), "status": if reads.is_empty() { "no_reading" } else if pace.is_none() { "too_early" } else { "projected" }, "needs": if l.mileage_limit_km.is_none() { vec!["mileage_limit_km"] } else { vec![] },
            "latest": reads.last().map(|r| json!({"date": r.0.to_string(), "km": r.1, "age_days": (today - r.0).num_days()})), "pace": pace.map(|p| json!({"km_per_year": (p * 365.0).round(), "km_per_month": (p * 30.4).round(), "since": start.or(reads.first().map(|r| r.0)).unwrap().to_string()})),
            "projected_odometer_at_end": proj, "projected_contract_km": proj.map(|p| p - init), "excess_km": excess, "excess_cost": match (excess, l.excess_km_fee) { (Some(x), Some(f)) => Some(money_str(cents(x as f64 * f))), _ => None }}}))
}
/// Loan facts the bank payments show (the instalment and its day, the first and last payment), offered for the user to confirm.
pub fn infer(ds: &Dataset, l: &Liability) -> Value {
    let p = loans::payments(ds, l); if p.len() < 3 { return json!({"status": "too_few_payments", "fields": [], "notes": ["at least 3 matching payments are needed (set payment_match on the loan)"]}); }
    let mut amts: Vec<i64> = p.iter().map(|x| x.1).collect(); amts.sort(); let med = median_c(&amts); let same = p.iter().filter(|x| (x.1 - med).abs() <= 100).count();
    let mut days: Vec<u32> = p.iter().map(|x| chrono::Datelike::day(&x.0)).collect(); days.sort(); let day = days[days.len() / 2];
    let conf = |ok: bool| if ok { "high" } else { "medium" }; let mut f = vec![];
    if l.monthly_payment.is_none() { f.push(json!({"field": "monthly_payment", "value": med as f64 / 100.0, "confidence": conf(same * 10 >= p.len() * 8), "method": format!("median of {} payments", p.len())})); }
    if l.payment_day.is_none() { f.push(json!({"field": "payment_day", "value": day, "confidence": conf(days.iter().filter(|x| (**x as i64 - day as i64).abs() <= 2).count() * 10 >= days.len() * 8), "method": "the usual day of the payments"})); }
    if l.first_payment_date.is_none() && l.start_date.is_none() { f.push(json!({"field": "first_payment_date", "value": p[0].0.to_string(), "confidence": "low", "method": "the first payment seen (the loan may be older than the data)"})); }
    json!({"status": "inferred", "fields": f, "notes": ["inferred from the bank payments: confirm against the loan contract before accepting"]})
}
