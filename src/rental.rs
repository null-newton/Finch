//! A rental property (E15), for Belgium: an `assets.yaml` item of kind `real_estate_rental`, linked to its bank account (`account`, or
//! the accounts with purpose `rental`) and its loan (`loan`, or the liabilities whose `asset` is it). Month by month: rent received,
//! the costs by kind, the loan instalments, the net and the owner's effort; missing rents against the expected rent (declared, the
//! indexed lease rent, or the observed median); the yearly totals. Belgian specifics: the yearly rent indexation with the health
//! index (base rent x new index / base index), computed ONLY from the indexes the owner typed in (nothing is looked up). No
//! French scheme (Pinel) is modelled: the scheme block shows the commitment the owner recorded (dates, figures, extension decision),
//! and the indicators are neutral facts (rate vs the owner's market rate, commitment end, net equity). Tax: private letting is taxed on
//! the cadastral income, not the rents.
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
fn end_of(start: NaiveDate, years: i64) -> NaiveDate { add_months(start, 12 * years as i32) - chrono::Duration::days(1) }
fn whole_months(a: NaiveDate, b: NaiveDate) -> i64 { (b.year() - a.year()) as i64 * 12 + b.month() as i64 - a.month() as i64 - i64::from(b.day() < a.day()) }
/// The commitment the owner recorded on the property (`commitment:` of the asset: dates, figures, the extension decision). No Belgian
/// tax-incentive scheme is modelled: nothing is looked up, a missing figure is listed, and the dates are the owner's own.
pub fn scheme(ds: &Dataset, a: &Asset, observed_rent: Option<i64>) -> Value {
    let c = a.commitment.as_ref().map(crate::memstore::jsonify).unwrap_or(Value::Null); let today = ds.today; let declared = a.scheme.is_some() || c.is_object();
    let dv = |k: &str| c[k].as_str().and_then(d); let (start, years) = (dv("start_date"), c["years"].as_i64().filter(|y| (1..=30).contains(y)));
    let end = dv("end_date").or_else(|| Some(end_of(start?, years?))); let ext = &c["extension"];
    let eff = end.map(|e| match (ext["decision"].as_str(), ext["years"].as_i64()) { (Some("extend"), Some(y)) => end_of(e + chrono::Duration::days(1), y), _ => e });
    let need = |f: &str, why: &str| json!({"field": f, "needed_for": why}); let mut missing = vec![];
    if declared { if start.is_none() { missing.push(need("commitment.start_date", "the end date of the commitment and its reminders")); } if years.is_none() && dv("end_date").is_none() { missing.push(need("commitment.years", "the end date of the commitment")); } }
    let s = |x: Option<NaiveDate>| x.map(|x| x.to_string());
    let mut out = json!({"declared": declared, "scheme": a.scheme, "start_date": s(start), "years": years, "end_date": s(end), "end_source": end.map(|_| if dv("end_date").is_some() { "declared" } else { "start + years" }), "effective_end_date": s(eff), "state": "unknown",
        "extension": if ext.is_object() { json!({"decision": ext["decision"].as_str().unwrap_or("undecided"), "years": ext["years"], "additional_rate_pct": ext["additional_rate_pct"], "decided_on": ext["decided_on"], "note_recorded": ext["note"].is_string()}) } else { json!({"decision": "undecided"}) },
        "missing": missing, "warnings": ["no Belgian tax-incentive rental scheme is modelled: the dates and figures shown are the ones you recorded"]});
    if let (Some(st), Some(e), Some(ef)) = (start, end, eff) {
        out["state"] = (if today < st { "not_started" } else if today <= ef { "active" } else { "ended" }).into(); let total = (e - st).num_days().max(1);
        out["progress_pct"] = json!((((today - st).num_days() as f64 / total as f64).clamp(0.0, 1.0) * 1000.0).round() / 10.0); out["days_left"] = (e - today).num_days().into(); out["months_left"] = (if e >= today { whole_months(today, e) } else { 0 }).into();
        let rm = ds.settings.rental_reminder_months as i32; let mut steps: Vec<i32> = [rm, 12, 6, 3].into_iter().filter(|m| *m <= rm).collect(); steps.sort_by(|x, y| y.cmp(x)); steps.dedup();
        let rem: Vec<(i32, NaiveDate)> = steps.iter().map(|m| (*m, add_months(e, -m))).collect(); out["reminders"] = rem.iter().map(|(m, dt)| json!({"months_before": m, "date": dt.to_string()})).collect::<Vec<_>>().into();
        out["decision_needed"] = (out["extension"]["decision"] == "undecided" && rem.iter().any(|r| r.1 <= today) && today <= add_months(e, 6)).into(); out["next_reminder_date"] = json!(rem.iter().map(|r| r.1).filter(|x| *x > today).min().map(|x| x.to_string()));
    }
    let f = |k: &str| c[k].as_f64().filter(|x| x.is_finite() && *x >= 0.0);
    let (cap, basis) = match (f("rent_cap_monthly"), f("rent_cap_m2"), f("surface_m2")) { (Some(m), ..) => (Some(to_cents(m)), Some("the monthly cap you declared")), (None, Some(m2), Some(s)) if s > 0.0 => (Some(to_cents(m2 * s)), Some("the cap per m2 you declared x the surface you declared")), _ => (None, None) };
    let (rent, rb) = match (a.rent_monthly, observed_rent) { (Some(r), _) => (Some(to_cents(r)), Some("the lease rent you declared")), (None, Some(o)) => (Some(o), Some("the last rent received (it may be net of management fees: the cap applies to the lease rent)")), _ => (None, None) };
    out["rent_cap"] = json!({"cap": cap.map(money_str), "cap_basis": basis, "rent": rent.map(money_str), "rent_basis": rb, "status": "unknown"});
    if let (Some(cp), Some(r)) = (cap, rent) { out["rent_cap"]["gap"] = money_str(r - cp).into(); out["rent_cap"]["status"] = (if r > cp { "above_cap" } else { "within_cap" }).into(); }
    let (lim, inc) = (f("tenant_income_limit").map(to_cents), f("tenant_income").map(to_cents)); out["tenant_income"] = json!({"limit": lim.map(money_str), "tenant_income": inc.map(money_str), "status": "unknown"});
    if let (Some(l), Some(i)) = (lim, inc) { out["tenant_income"]["headroom"] = money_str(l - i).into(); out["tenant_income"]["status"] = (if i > l { "above_limit" } else { "within_limit" }).into(); }
    out
}
const GENERAL: &str = "This is general information, not financial advice.";
const MARKET_MAX_AGE_DAYS: i64 = 30;
/// Renegotiate-or-sell indicators: the loan rate against the market rate THE OWNER ENTERED (never looked up), the end of the commitment,
/// the net equity (declared value less the capital still due; an unknown loan leaves it unknown). Facts and a neutral reading only.
pub fn indicators(ds: &Dataset, a: &Asset, rate: Option<f64>, rate_date: Option<NaiveDate>, fees: [Option<f64>; 4]) -> Value {
    let today = ds.today; let lk = links(ds, a); let mr = a.market_rate.as_ref().map(crate::memstore::jsonify).filter(|m| m["rate_pct"].is_number());
    let market = match (rate, mr) {
        (Some(r), _) => Some((r, rate_date, "given for this call")), (None, Some(m)) => Some((m["rate_pct"].as_f64().unwrap_or(0.0), m["as_of"].as_str().and_then(d), "recorded on the property")), _ => None };
    let mk = match market { None => json!({"status": "missing", "note": "enter the current market rate for a loan of similar duration (your own quote or a comparator you looked at, with its date): the coach never looks it up"}),
        Some((r, day, src)) => { let mut o = json!({"status": "given", "rate_pct": r, "date": day.map(|x| x.to_string()), "basis": src});
            match day { None => o["warning"] = "the date of the market rate was not given: treat it as possibly outdated".into(), Some(x) => { let age = (today - x).num_days(); o["age_days"] = age.into();
                if age > MARKET_MAX_AGE_DAYS { o["warning"] = format!("the market rate is {age} days old (more than {MARKET_MAX_AGE_DAYS}): do not present it as current").into(); } else if age < 0 { o["warning"] = "the date of the market rate is in the future".into(); } } } o } };
    // the loan rate against it
    let mut lr = json!({"status": "unknown"});
    match lk.loans.iter().find(|l| l.kind == "mortgage" || l.kind == "consumer_loan" || l.rate.is_some()) {
        None => lr["missing"] = json!(["the loan and its nominal rate"]),
        Some(l) => match l.rate.as_ref().and_then(|r| r.nominal) { None => lr["missing"] = json!(["rate.nominal of the loan"]), Some(nom) => {
            lr["loan_rate_pct"] = nom.into(); lr["variable"] = l.rate.as_ref().and_then(|r| r.ty.as_deref()).is_some_and(|t| t == "variable" || t == "mixed").into();
            match market { None => lr["missing"] = json!(["a market rate you entered"]), Some((m, ..)) => {
                let gap = ((nom - m) * 100.0).round() / 100.0; let thr = ds.settings.rental_rate_gap_pts; lr["market_rate_pct"] = m.into(); lr["gap_pts"] = gap.into(); lr["threshold_pts"] = thr.into();
                lr["status"] = (if gap >= thr { "above_market" } else if gap > -thr { "close_to_market" } else { "below_market" }).into();
                lr["reading"] = (if gap >= thr { format!("the loan rate is {gap:.2} point(s) above the market rate you entered: a quote may be worth asking for. {GENERAL}") } else if gap > -thr { "the loan rate is close to the market rate you entered: little to gain from a renegotiation on the rate alone".into() } else { "the loan rate is below the market rate you entered: a renegotiation would raise it".into() }).into();
                if gap >= thr { let [b, g, o, p] = fees; let sc = loans::compute(l, today);
                    lr["renegotiation"] = crate::loans_extra::renegotiate(l, &sc, today, m, None, b.unwrap_or(0.0), g.unwrap_or(0.0), o.unwrap_or(0.0), p).unwrap_or_else(|e| json!({"status": "invalid", "note": e.to_string()}));
                    if lr["renegotiation"]["status"] == "computed" && fees.iter().all(|x| x.is_none_or(|v| v == 0.0)) { lr["renegotiation_note"] = "no fee was given: add the bank fees, the guarantee fees and the penalty of your contract to price it".into(); } } } } } } }
    // the net equity
    let mut eq = json!({"status": "unknown"}); let mut miss: Vec<String> = vec![]; let value = a.amount();
    match value { None => miss.push("value (the valuation you believe, with as_of)".into()), Some(v) => { eq["value"] = money_str(to_cents(v)).into(); eq["value_as_of"] = json!(a.as_of);
        match a.as_of.as_deref().and_then(d) { None => eq["value_warning"] = "the valuation has no as_of date".into(), Some(x) if x < add_months(today, -(ds.settings.asset_stale_months as i32)) => eq["value_warning"] = format!("the valuation dates from {x}: update it").into(), _ => {} } } }
    if lk.loans.is_empty() { miss.push("the loan (link it with `loan`), or record that there is none".into()); } else {
        let caps: Vec<(Option<i64>, String, Option<usize>, bool)> = lk.loans.iter().map(|l| { let sc = loans::compute(l, today);
            if sc.status == "computed" && sc.remaining.is_some() { (sc.remaining, "amortization schedule".into(), Some(sc.rows.iter().filter(|r| !r.made).count()), sc.approximate) }
            else if let Some(o) = l.outstanding { (Some(loans::cents(o)), format!("declared outstanding as of {}", l.outstanding_as_of.as_deref().unwrap_or("an unknown date")), None, true) } else { (None, "unknown".into(), None, false) } }).collect();
        eq["loans"] = caps.iter().map(|c| json!({"capital": c.0.map(money_str), "source": c.1, "months_left": c.2})).collect::<Vec<_>>().into();
        if caps.iter().any(|c| c.0.is_none()) { miss.push("the capital still due of a loan (its terms or a declared outstanding)".into()); } else {
            let owed: i64 = caps.iter().filter_map(|c| c.0).sum(); eq["outstanding"] = money_str(owed).into(); eq["approximate"] = caps.iter().any(|c| c.3).into();
            if let Some(v) = value.map(to_cents) { eq["status"] = "computed".into(); eq["net_equity"] = money_str(v - owed).into(); let pct = |x: i64| (v != 0).then(|| ((x as f64 / v as f64) * 1000.0).round() / 10.0);
                eq["loan_to_value_pct"] = json!(pct(owed)); eq["equity_share_pct"] = json!(pct(v - owed)); eq["notes"] = json!(["before the selling costs, the early-repayment penalty and any tax on a capital gain: none of them is modelled"]); } } }
    if !miss.is_empty() { eq["missing"] = json!(miss); }
    let cf = cashflow(ds, a, &lk, 24); let done: Vec<&Value> = cf["months"].as_array().into_iter().flatten().filter(|m| m["complete"] == true).collect(); let done = &done[done.len().saturating_sub(12)..];
    let effort = (!done.is_empty()).then(|| money_str(div_cents(done.iter().map(|m| to_cents(m["effort"].as_str().unwrap_or("0").parse().unwrap_or(0.0))).sum(), done.len() as i64)));
    let last_rent = cf["months"].as_array().into_iter().flatten().rev().find(|m| m["rent"].as_str().is_some_and(|r| r != "0.00")).map(|m| to_cents(m["rent"].as_str().unwrap_or("0").parse().unwrap_or(0.0)));
    let sch = scheme(ds, a, last_rent); let mut sig = vec![];
    if lr["status"] == "above_market" { sig.push(json!({"id": "rate_above_market", "reading": lr["reading"]})); }
    match (sch["state"].as_str(), sch["months_left"].as_i64()) {
        (Some("active"), Some(m)) if m <= ds.settings.rental_reminder_months as i64 => sig.push(json!({"id": "commitment_ending", "reading": format!("the commitment ends in about {m} month(s) ({}): extend (if your arrangement allows it), keep renting under the ordinary rules, or sell. Check the consequences of each with the rules that apply or an adviser. {GENERAL}", sch["end_date"].as_str().unwrap_or(""))})),
        (Some("active"), _) => sig.push(json!({"id": "commitment_running", "reading": "the commitment is running: ending it early (selling, stopping the rental) can call its advantage into question. Verify with the rules that apply before any decision."})),
        (Some("ended"), _) => sig.push(json!({"id": "commitment_over", "reading": format!("the commitment ended on {}: its constraints may no longer apply; verify what changes for the property.", sch["effective_end_date"].as_str().unwrap_or(""))})), _ => {} }
    if eq["status"] == "computed" { sig.push(if eq["net_equity"].as_str().is_some_and(|x| !x.starts_with('-')) { json!({"id": "equity_positive", "reading": "the declared valuation is above the capital still due"}) } else { json!({"id": "equity_negative", "reading": "the declared valuation is below the capital still due: selling would not repay the loan"}) }); }
    let commitment: serde_json::Map<String, Value> = ["state", "start_date", "years", "end_date", "effective_end_date", "days_left", "months_left", "decision_needed", "extension"].iter().map(|k| (k.to_string(), sch[*k].clone())).collect();
    json!({"as_of": today.to_string(), "market_rate": mk, "loan_rate": lr, "commitment": commitment, "equity": eq, "trailing_effort": effort, "signals": sig,
        "scenarios": ["the loan's renegotiation scenario (Loans page): the new rate priced on the real schedule", "the loan's prepayment scenario (Loans page): a prepayment on the exact schedule"], "disclaimer": crate::loans_extra::DISC, "disclaimer_key": "loan"})
}
pub fn properties(ds: &Dataset) -> Vec<Asset> { loans::assets(&ds.memory_dir).into_iter().filter(|a| a.kind == "real_estate_rental").collect() }
pub fn pnl(cf: &Value, year: i32, today: NaiveDate) -> Value {
    let rows: Vec<&Value> = cf["months"].as_array().into_iter().flatten().filter(|m| m["month"].as_str().is_some_and(|x| x.starts_with(&year.to_string()))).collect();
    let keys = ["rent", "loan", "charges", "fees", "taxes", "insurance", "works", "other", "costs", "net", "effort", "owner_in", "owner_out"];
    let totals: serde_json::Map<String, Value> = keys.iter().map(|k| (k.to_string(), json!(money_str(rows.iter().map(|r| to_cents(r[*k].as_str().unwrap_or("0").parse().unwrap_or(0.0))).sum())))).collect();
    let n = rows.len() as i64; let eff = totals["effort"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
    json!({"year": year, "totals": totals, "n_months": n, "months_expected": if year == today.year() { today.month() as i64 } else { 12 }, "months_incomplete": rows.iter().filter(|r| r["complete"] == false).map(|r| r["month"].clone()).collect::<Vec<_>>(), "months_missing_data": [],
        "year_in_progress": year == today.year(), "complete": rows.iter().all(|r| r["complete"] == true) && n == 12, "monthly_average_effort": (n > 0).then(|| money_str(div_cents(to_cents(eff), n))), "vacancy": cf["vacancy"], "loan_split": null})
}
fn asset_json(a: &Asset) -> Value { json!({"id": a.id, "kind": a.kind, "account": a.account, "loan": a.loan, "scheme": a.scheme, "value": a.amount(), "as_of": a.as_of, "rent_monthly": a.rent_monthly, "purchase_price": a.purchase_price, "purchase_date": a.purchase_date, "commitment": a.commitment.as_ref().map(crate::memstore::jsonify),
    "market_rate": a.market_rate.as_ref().map(crate::memstore::jsonify), "vacancies": a.vacancies.iter().map(|v| json!({"start": v.start, "end": v.end, "note": v.note})).collect::<Vec<_>>()}) }
pub fn list(ds: &Dataset) -> Value {
    let ps = properties(ds); let linked: Vec<String> = ps.iter().flat_map(|a| links(ds, a).accounts).collect();
    let rows: Vec<Value> = ps.iter().map(|a| { let lk = links(ds, a); let cf = cashflow(ds, a, &lk, 13); let last = cf["months"].as_array().and_then(|m| m.iter().rev().nth(1)).cloned();
        let owed: Option<i64> = lk.loans.iter().map(|l| loans::compute(l, ds.today).remaining.or(l.outstanding.map(loans::cents))).sum();
        let mut miss = lk.notes.clone(); if a.amount().is_none() { miss.push("value".into()); } let sc = scheme(ds, a, None);
        json!({"id": a.id, "account_link": if lk.accounts.is_empty() { "none" } else { "linked" }, "loan_link": if lk.loans.is_empty() { "none" } else { "linked" }, "n_loans": lk.loans.len(), "expected_rent": cf["rent"]["expected"], "rent_source": cf["rent"]["source"],
            "last_month": last.map(|m| json!({"month": m["month"], "rent": m["rent"], "net": m["net"], "effort": m["effort"], "complete": m["complete"], "rent_status": m["rent_status"]})), "vacancy": cf["vacancy"],
            "scheme": {"declared": sc["declared"], "scheme": sc["scheme"], "state": sc["state"], "end_date": sc["end_date"], "effective_end_date": sc["effective_end_date"], "months_left": sc["months_left"], "decision_needed": sc["decision_needed"]},
            "net_equity": a.amount().and_then(|v| owed.map(|o| money_str(loans::cents(v) - o))), "missing": miss}) }).collect();
    json!({"as_of": ds.today.to_string(), "properties": rows, "unlinked_rental_accounts": ds.accounts.values().filter(|ac| ac.purpose.as_deref() == Some("rental") && !linked.contains(&ac.uid)).map(|ac| json!({"uid": ac.uid, "label": ac.label})).collect::<Vec<_>>(), "property_categories": KINDS})
}
pub fn detail(ds: &Dataset, id: &str, year: Option<i32>) -> Option<Value> {
    let a = properties(ds).into_iter().find(|a| a.id == id)?; let lk = links(ds, &a); let cf = cashflow(ds, &a, &lk, 24); let cur = cf["months"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
    let years: Vec<i32> = cf["months"].as_array().into_iter().flatten().filter(|m| m["n_tx"].as_u64().unwrap_or(0) > 0).filter_map(|m| m["month"].as_str()?.get(..4)?.parse().ok()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let y = year.unwrap_or(ds.today.year());
    Some(json!({"id": a.id, "kind": a.kind, "links": {"account": if lk.accounts.is_empty() { "none" } else { "linked" }, "accounts": lk.accounts.len(), "loan": if lk.loans.is_empty() { "none" } else { "linked" }, "loans": lk.loans.len(), "notes": lk.notes},
        "cashflow": cf, "current_month": {"month": cur["month"], "rent": cur["rent"], "expected": cur["expected_rent"], "status": cur["rent_status"]}, "pnl": pnl(&cf, y, ds.today), "scheme": scheme(ds, &a, cf["months"].as_array().into_iter().flatten().rev().find(|m| m["rent"].as_str().is_some_and(|r| r != "0.00")).map(|m| to_cents(m["rent"].as_str().unwrap_or("0").parse().unwrap_or(0.0)))), "years": years, "flows_to_label": 0, "asset": asset_json(&a), "tag": format!("rental:{}", a.id),
        "indexation": indexation(&a)}))
}
/// Belgium: private residential letting is taxed on the indexed cadastral income, not on the rents received.
pub fn tax(year: i32) -> Value {
    json!({"year": year, "country": "BE", "status": "not_applicable", "note": "In Belgium, letting a home privately to a private tenant is taxed on the property's indexed cadastral income (KI / RC), not on the rents: no rent-based figure is computed. Professional letting or furnished letting differ: ask an adviser.",
        "disclaimer": "General information, not tax advice: rules change and depend on your situation. Verify on the official site (FPS Finance / MyMinfin) or with an adviser.", "disclaimer_key": "tax"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn index() { let a: Asset = serde_yaml::from_str("id: flat\nkind: real_estate_rental\nlease: {start_date: 2022-09-01, base_rent: 800, base_index: 118.5, current_index: 130.2}\n").unwrap(); let v = indexation(&a);
        assert_eq!(v["indexed_rent"], "878.99", "800 x 130.2 / 118.5 = 878.987"); assert_eq!(v["change"], "78.99"); }
}
