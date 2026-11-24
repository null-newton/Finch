//! Loans, assets and net worth (E9), for Belgium. Memory files are read as the Python app writes them (`assets.yaml`,
//! `liabilities/<id>.yaml`); every figure comes from them or from the bank data, nothing is estimated from an instalment.
//! Amortization (integer cents, half up): Belgian loans quote a yearly rate and charge the EQUIVALENT monthly rate
//! ((1 + i)^(1/12) - 1) unless the file says `rate.monthly_convention: proportional` (i / 12). Deferrals (partial: interest only;
//! total: interest added to the capital), flat or percentage borrower insurance, and two modes: the whole table from the principal,
//! or the future table from a dated outstanding capital. Early repayment: the legal caps of the Code of Economic Law (book VII).
use crate::{analytics::Dataset, util::*};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Rate { #[serde(rename = "type")] pub ty: Option<String>, pub nominal: Option<f64>, pub taeg: Option<f64>, pub index: Option<String>, pub margin: Option<f64>, pub cap: Option<f64>, pub monthly_convention: Option<String> }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Insurance { pub provider: Option<String>, pub monthly: Option<f64>, pub delegated: Option<bool>, pub rate_pct: Option<f64>, pub basis: Option<String> }
#[derive(Debug, Clone, Deserialize)]
pub struct Deferral { pub months: i64, #[serde(default = "partial")] pub kind: String }
fn partial() -> String { "partial".into() }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Liability { pub id: String, pub kind: String, pub lender: Option<String>, pub asset: Option<String>, pub holder: Option<String>, pub holders: Option<serde_yaml::Value>, pub start_date: Option<String>, pub end_date: Option<String>,
    pub first_payment_date: Option<String>, pub term_months: Option<i64>, pub payment_day: Option<u32>, pub principal: Option<f64>, pub outstanding: Option<f64>, pub outstanding_as_of: Option<String>, pub rate: Option<Rate>,
    pub monthly_payment: Option<f64>, pub insurance: Option<Insurance>, pub deferral: Option<Deferral>, pub debited_account: Option<String>, pub payment_match: Option<String>, pub early_repayment_penalty: Option<serde_yaml::Value>,
    pub first_payment: Option<f64>, pub residual_value: Option<f64>, pub mileage_limit_km: Option<i64>, pub excess_km_fee: Option<f64>, pub initial_km: Option<i64>, pub notes: Option<String>, #[serde(skip)] pub file: String }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Asset { pub id: String, pub kind: String, pub provider: Option<String>, pub holder: Option<String>, pub holders: Option<serde_yaml::Value>, pub balance: Option<f64>, pub value: Option<f64>, pub as_of: Option<String>, pub liquidity: Option<String>,
    pub connected: Option<bool>, pub contribution_monthly: Option<f64>, pub description: Option<String>, pub purchase_price: Option<f64>, pub purchase_date: Option<String>, pub rent_monthly: Option<f64>, pub account: Option<String>, pub loan: Option<String>,
    pub lease: Option<Lease>, pub vacancies: Vec<Vacancy>, pub market_rate: Option<serde_yaml::Value>, pub scheme: Option<String>, pub commitment: Option<serde_yaml::Value> }
/// A Belgian residential lease, as the owner states it (no index is ever looked up).
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Lease { pub start_date: Option<String>, pub base_rent: Option<f64>, pub base_index: Option<f64>, pub current_index: Option<f64>, pub current_index_month: Option<String>, pub registered: Option<bool> }
#[derive(Debug, Clone, Default, Deserialize)] #[serde(default)]
pub struct Vacancy { pub start: String, pub end: Option<String>, pub note: Option<String> }
impl Asset { pub fn amount(&self) -> Option<f64> { self.balance.or(self.value) } }

/// The liabilities of `liabilities/*.yaml` (an unreadable file is skipped with a warning).
pub fn liabilities(mem: &Path) -> (Vec<Liability>, Vec<String>) {
    let (mut out, mut warn) = (vec![], vec![]); let mut files: Vec<_> = std::fs::read_dir(mem.join("liabilities")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "yaml") && !p.file_name().unwrap().to_string_lossy().starts_with('_')).collect(); files.sort();
    for f in files { match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|t| serde_yaml::from_str::<Liability>(&t).map_err(|e| e.to_string())) {
        Ok(mut l) if !l.id.is_empty() => { l.file = format!("liabilities/{}", f.file_name().unwrap().to_string_lossy()); out.push(l); } Ok(_) => warn.push(format!("{}: no id", f.display())), Err(e) => warn.push(format!("{}: {e}", f.file_name().unwrap().to_string_lossy())) } }
    (out, warn)
}
pub fn assets(mem: &Path) -> Vec<Asset> { crate::memory::list_file::<Asset>(mem, "assets.yaml", "assets").0 }
fn dd(s: &Option<String>) -> Option<NaiveDate> { s.as_deref().and_then(d) }

// ---------------------------------------------------------------- the amortization schedule
#[derive(Debug, Clone)]
pub struct Row { pub k: usize, pub due: NaiveDate, pub kind: &'static str, pub interest: i64, pub principal: i64, pub insurance: i64, pub payment: i64, pub balance: i64, pub made: bool }
#[derive(Debug, Clone, Default)]
pub struct Sched { pub status: &'static str, pub mode: Option<&'static str>, pub missing: Vec<String>, pub alternative: Option<String>, pub approximate: bool, pub rows: Vec<Row>, pub capital: i64, pub payment: i64, pub remaining: Option<i64>,
    pub source: Option<&'static str>, pub assumptions: Vec<String>, pub payment_check: Option<Value>, pub outstanding_check: Option<Value>, pub monthly_rate: f64, pub partial_first: bool }
fn round_c(x: f64) -> i64 { (x + 0.5 * x.signum() * 1e-9).round() as i64 }
pub fn cents(x: f64) -> i64 { to_cents(x) }
/// The monthly rate of a yearly rate: equivalent (Belgian practice) or proportional.
pub fn monthly_rate(rate_pct: f64, conv: Option<&str>) -> f64 { let i = rate_pct / 100.0; if conv == Some("proportional") { i / 12.0 } else { (1.0 + i).powf(1.0 / 12.0) - 1.0 } }
pub fn annuity(p: i64, r: f64, n: i64) -> i64 { if r == 0.0 { round_c(p as f64 / n as f64) } else { round_c(p as f64 * r / (1.0 - (1.0 + r).powi(-(n as i32)))) } }
/// (interest, principal, payment, balance) rows of an annuity (the last row clears the capital); with `fixed` the instalment is kept and the term follows.
pub fn table(p: i64, r: f64, n: Option<i64>, fixed: Option<i64>) -> Option<Vec<(i64, i64, i64, i64)>> {
    let pay = fixed.unwrap_or_else(|| annuity(p, r, n.unwrap_or(1))); let (mut bal, mut out) = (p, vec![]); let max = n.unwrap_or(1200);
    for k in 1..=max { let i = round_c(bal as f64 * r); let (pr, paid) = if Some(k) == n || pay - i >= bal { (bal, bal + i) } else { (pay - i, pay) }; if pr <= 0 { return None; } bal -= pr; out.push((i, pr, paid, bal)); if bal <= 0 { return Some(out); } }
    if n.is_some() { Some(out) } else { None }
}
fn months_between(a: NaiveDate, b: NaiveDate) -> i64 { (b.year() as i64 - a.year() as i64) * 12 + b.month() as i64 - a.month() as i64 }
fn on_day(x: NaiveDate, day: Option<u32>) -> NaiveDate { match day { Some(dy) => { let last = month_end(&month_key(x)).day(); NaiveDate::from_ymd_opt(x.year(), x.month(), dy.min(last)).unwrap() } None => x } }
pub fn term_of(l: &Liability) -> Option<i64> { l.term_months.or_else(|| Some(months_between(dd(&l.start_date)?, dd(&l.end_date)?)).filter(|n| *n > 0)) }
pub fn due(l: &Liability, k: i64) -> Option<NaiveDate> { Some(on_day(match dd(&l.first_payment_date) { Some(f) => add_months(f, k as i32 - 1), None => add_months(dd(&l.start_date)?, k as i32) }, l.payment_day)) }
fn insurance(l: &Liability, principal: i64, before: i64) -> i64 {
    let Some(ins) = &l.insurance else { return 0 }; if let Some(m) = ins.monthly { return cents(m); }
    ins.rate_pct.map_or(0, |p| round_c((if ins.basis.as_deref() == Some("outstanding") { before } else { principal }) as f64 * p / 1200.0))
}
fn payment_check(l: &Liability, regular: i64, ins: i64) -> Option<Value> {
    let declared = cents(l.monthly_payment?); let close = |a: i64, b: i64| (a - b).abs() <= 100.max(b / 100);
    Some(if close(declared, regular + ins) { json!({"status": "matches", "declared": money_str(declared), "computed_total": money_str(regular + ins)}) } else if ins > 0 && close(declared, regular) { json!({"status": "matches_without_insurance", "declared": money_str(declared), "computed": money_str(regular)}) }
        else { json!({"status": "differs", "declared": money_str(declared), "computed_total": money_str(regular + ins), "hint": "variable rate, modulated instalments, fees, the monthly-rate convention or a wrong figure: check the loan table"}) })
}
/// The schedule of one liability, or the exact list of what is missing.
pub fn compute(l: &Liability, today: NaiveDate) -> Sched {
    let mut s = Sched { status: "not_computable", ..Default::default() };
    if matches!(l.kind.as_str(), "loa" | "lld" | "leasing") { s.status = "not_applicable"; s.alternative = Some("a lease has rents and a residual value, not an amortization table and no capital owed".into()); return s; }
    let rate = l.rate.as_ref().and_then(|r| r.nominal); let conv = l.rate.as_ref().and_then(|r| r.monthly_convention.as_deref());
    let mut miss = vec![]; if l.principal.is_none() { miss.push("principal"); } if rate.is_none() { miss.push("rate.nominal"); } if l.start_date.is_none() { miss.push("start_date"); } if term_of(l).is_none() { miss.push("end_date or term_months"); }
    if let Some(rate) = rate { s.monthly_rate = monthly_rate(rate, conv); }
    let note_conv = |s: &mut Sched| s.assumptions.push(if conv == Some("proportional") { "monthly rate = yearly rate / 12 (rate.monthly_convention: proportional)".into() } else { format!("monthly rate = the equivalent of the yearly rate ((1 + i)^(1/12) - 1 = {:.5} %), the usual Belgian convention; set rate.monthly_convention: proportional if the loan table uses i / 12", s.monthly_rate * 100.0) });
    if miss.is_empty() { s = from_principal(l, s, today); note_conv(&mut s); if s.status == "computed" && s.outstanding_check.is_some() { if let Some(mut r) = from_outstanding(l, Sched { monthly_rate: s.monthly_rate, ..Default::default() }, today) { r.outstanding_check = s.outstanding_check.clone(); r.assumptions.insert(0, "the declared capital differs from the theoretical table: the declared figure is rolled forward from its date".into()); note_conv(&mut r); return r; } } return finish(l, s, today); }
    s.missing = miss.iter().map(|m| m.to_string()).collect();
    if rate.is_some() && l.outstanding.is_some_and(|o| o > 0.0) && l.outstanding_as_of.is_some() && (l.end_date.is_some() || l.monthly_payment.is_some() || (l.start_date.is_some() && term_of(l).is_some())) {
        if let Some(mut r) = from_outstanding(l, s.clone(), today) { r.missing = s.missing.clone(); note_conv(&mut r); return finish(l, r, today); }
    } else if l.outstanding.is_some() && rate.is_some() { s.alternative = Some("outstanding + outstanding_as_of + rate.nominal + (end_date or monthly_payment) give the FUTURE table".into()); }
    s
}
fn from_principal(l: &Liability, mut s: Sched, today: NaiveDate) -> Sched {
    let n = term_of(l).unwrap(); let p = cents(l.principal.unwrap()); if p <= 0 { s.status = "invalid"; s.missing = vec!["principal (must be positive)".into()]; return s; }
    let dn = l.deferral.as_ref().map_or(0, |d| d.months); if dn >= n { s.status = "invalid"; s.missing = vec!["deferral (not shorter than the term)".into()]; return s; }
    let r = s.monthly_rate; let mut bal = p; let mut rows = vec![];
    for k in 1..=dn { let i = round_c(bal as f64 * r); let ins = insurance(l, p, bal); let du = due(l, k).unwrap();
        if l.deferral.as_ref().unwrap().kind == "total" { bal += i; rows.push(Row { k: k as usize, due: du, kind: "deferral", interest: i, principal: -i, insurance: ins, payment: 0, balance: bal, made: du <= today }); }
        else { rows.push(Row { k: k as usize, due: du, kind: "deferral", interest: i, principal: 0, insurance: ins, payment: i, balance: bal, made: du <= today }); } }
    let Some(am) = table(bal, r, Some(n - dn), None) else { s.status = "invalid"; return s };
    for (j, (i, pr, pay, b)) in am.iter().enumerate() { let k = dn + j as i64 + 1; let du = due(l, k).unwrap(); rows.push(Row { k: k as usize, due: du, kind: "regular", interest: *i, principal: *pr, insurance: insurance(l, p, b + pr), payment: *pay, balance: *b, made: du <= today }); }
    s.status = "computed"; s.mode = Some("from_principal"); s.source = Some("schedule"); s.capital = p; s.payment = am[0].2;
    s.assumptions.push("annuity loan: constant instalment, interest = capital x monthly rate per instalment".into()); s.assumptions.push(format!("first instalment due {}", rows[0].due));
    if let Some(dv) = &l.deferral { s.assumptions.push(if dv.kind == "total" { format!("total deferral of {} month(s): the interest is added to the capital; the insurance is still paid", dv.months) } else { format!("interest-only deferral of {} month(s): the capital does not move; the insurance is still paid", dv.months) }); }
    s.payment_check = payment_check(l, s.payment, rows[dn as usize].insurance);
    if let (Some(o), Some(at)) = (l.outstanding, dd(&l.outstanding_as_of)) { let comp = rows.iter().rfind(|x| x.due <= at).map_or(p, |x| x.balance); let decl = cents(o);
        if (decl - comp).abs() > 5000.max(comp / 50) { s.outstanding_check = Some(json!({"status": "differs", "as_of": at.to_string(), "declared": money_str(decl), "computed": money_str(comp), "hint": "the table differs from the declared capital: the rate, the term, a deferral or an early repayment may not be what the memory says"})); } }
    s.rows = rows; s
}
fn from_outstanding(l: &Liability, mut s: Sched, today: NaiveDate) -> Option<Sched> {
    let at = dd(&l.outstanding_as_of)?; let bal = cents(l.outstanding?); let day = l.payment_day.unwrap_or_else(|| [dd(&l.first_payment_date), dd(&l.start_date), dd(&l.end_date), Some(at)].into_iter().flatten().next().unwrap().day());
    let anchor = month_start(&month_key(at)); let first = (0..3).map(|k| on_day(add_months(anchor, k), Some(day))).find(|c| *c > at)?;
    let mut notes = vec![format!("the capital still due on {at} is the starting point; earlier instalments are not known"), format!("instalments assumed on day {day}{}", if l.payment_day.is_some() { "" } else { " (payment_day is not recorded)" })];
    let end = dd(&l.end_date).or_else(|| term_of(l).and_then(|t| due(l, t)));
    let ins_flat = l.insurance.as_ref().and_then(|i| i.monthly).map_or(0, cents);
    let am = match end { Some(e) => { let n = (0..600).take_while(|k| on_day(add_months(first, *k), Some(day)) <= e).count() as i64; if n == 0 { return None; } table(bal, s.monthly_rate, Some(n), None)? }
        None => { notes.push("end_date unknown: the term follows from monthly_payment minus the flat insurance".into()); table(bal, s.monthly_rate, None, Some(cents(l.monthly_payment?) - ins_flat))? } };
    let p0 = l.principal.map_or(bal, cents);
    s.rows = am.iter().enumerate().map(|(j, (i, pr, pay, b))| { let du = on_day(add_months(first, j as i32), Some(day)); Row { k: j + 1, due: du, kind: "regular", interest: *i, principal: *pr, insurance: insurance(l, p0, b + pr), payment: *pay, balance: *b, made: du <= today } }).collect();
    s.status = "computed"; s.mode = Some("from_outstanding"); s.source = Some("declared_rolled"); s.capital = bal; s.payment = am[0].2; s.alternative = None; s.partial_first = true;
    s.assumptions = notes; s.payment_check = payment_check(l, s.payment, s.rows[0].insurance); Some(s)
}
fn finish(l: &Liability, mut s: Sched, _today: NaiveDate) -> Sched {
    if s.status != "computed" { return s; } s.remaining = Some(s.rows.iter().rfind(|r| r.made).map_or(s.capital, |r| r.balance));
    s.approximate = l.rate.as_ref().is_some_and(|r| matches!(r.ty.as_deref(), Some("variable" | "mixed")));
    if s.approximate { s.assumptions.push("variable / mixed rate: the current rate is applied to the whole term (the bank's table will differ when the index moves)".into()); }
    if s.mode == Some("from_outstanding") { s.assumptions.push("totals and interest paid to date are unknown (the table starts at the declared capital); remaining interest and the yearly figures of the future are exact".into()); }
    s
}
impl Sched {
    pub fn to_json(&self, rows: bool) -> Value {
        let made: Vec<&Row> = self.rows.iter().filter(|r| r.made).collect(); let fut = self.mode == Some("from_outstanding");
        let sum = |f: &dyn Fn(&Row) -> i64, only: Option<bool>| self.rows.iter().filter(|r| only.is_none_or(|m| r.made == m)).map(f).sum::<i64>();
        let mut by: BTreeMap<i32, [i64; 4]> = BTreeMap::new(); for r in &self.rows { let e = by.entry(r.due.year()).or_default(); e[0] += 1; e[1] += r.interest; e[2] += r.principal; e[3] += r.insurance; }
        let first = by.keys().next().copied();
        let by_year: Vec<Value> = by.iter().map(|(y, v)| json!({"year": y, "instalments": v[0], "interest": money_str(v[1]), "principal": money_str(v[2]), "insurance": money_str(v[3]), "total": money_str(v[1] + v[2] + v[3]), "partial": self.partial_first && Some(*y) == first && self.rows[0].due.month() > 1})).collect();
        let tot_i = sum(&|r| r.interest, None); let tot_s = sum(&|r| r.insurance, None);
        json!({"status": self.status, "mode": self.mode, "missing": self.missing, "alternative": self.alternative, "approximate": self.approximate, "source": self.source,
            "payment": (self.status == "computed").then(|| money_str(self.payment)), "remaining_capital": self.remaining.map(money_str), "next_due": self.rows.iter().find(|r| !r.made).map(|r| r.due.to_string()),
            "first_due": self.rows.first().map(|r| r.due.to_string()), "last_due": self.rows.last().map(|r| r.due.to_string()), "remaining_instalments": self.rows.len() - made.len(), "payments_made": made.len(),
            "term_instalments": (self.status == "computed").then_some(self.rows.len()), "total_interest": (!fut && self.status == "computed").then(|| money_str(tot_i)), "total_insurance": (!fut && self.status == "computed").then(|| money_str(tot_s)),
            "total_cost": (!fut && self.status == "computed").then(|| money_str(tot_i + tot_s)), "interest_paid": (!fut && self.status == "computed").then(|| money_str(sum(&|r| r.interest, Some(true)))), "remaining_interest": (self.status == "computed").then(|| money_str(sum(&|r| r.interest, Some(false)))),
            "payment_check": self.payment_check, "outstanding_check": self.outstanding_check, "assumptions": self.assumptions, "by_year": by_year, "rows_count": self.rows.len(),
            "rows": if rows { self.rows.iter().map(|r| json!({"k": r.k, "due": r.due.to_string(), "kind": r.kind, "interest": money_str(r.interest), "principal": money_str(r.principal), "insurance": money_str(r.insurance), "payment": money_str(r.payment), "total": money_str(r.payment + r.insurance), "balance": money_str(r.balance), "made": r.made})).collect::<Vec<_>>() } else { vec![] }})
    }
}

/// The legal maximum indemnity for repaying `amount` early (Belgian Code of Economic Law, book VII): general information only.
pub fn early_repayment(l: &Liability, s: &Sched, amount: Option<i64>, today: NaiveDate) -> Value {
    let amt = amount.or(s.remaining); let rate = l.rate.as_ref().and_then(|r| r.nominal); let variable = l.rate.as_ref().is_some_and(|r| r.ty.as_deref() == Some("variable"));
    let disc = "General information, not financial or legal advice: the contract may ask less; your lender gives the binding figure.";
    match l.kind.as_str() {
        "mortgage" => json!({"rule": "mortgage credit: the reinvestment indemnity is capped at 3 months of interest on the capital repaid, at the loan's rate", "legal_basis": "Code of Economic Law, book VII (mortgage credit)",
            "max_indemnity": match (amt, rate) { (Some(a), Some(r)) => json!(money_str(round_c(a as f64 * r / 100.0 / 12.0 * 3.0))), _ => Value::Null }, "needs": if rate.is_none() { vec!["rate.nominal"] } else { vec![] },
            "other_costs": "the deed of release of the mortgage registration (notary and registration costs) is separate", "disclaimer": disc}),
        "consumer_loan" | "car_loan" => { let left = s.rows.iter().filter(|r| !r.made).count() as i64; let pct = if variable { 0.0 } else if left > 12 { 1.0 } else { 0.5 };
            json!({"rule": if variable { "consumer credit at a variable rate: no indemnity for an early repayment".to_string() } else { format!("consumer credit at a fixed rate: at most {pct} % of the capital repaid ({} a year of term left)", if left > 12 { "more than" } else { "at most" }) },
                "legal_basis": "Code of Economic Law, book VII (consumer credit)", "max_indemnity": amt.map(|a| money_str(round_c(a as f64 * pct / 100.0))), "needs": if s.status != "computed" { vec!["a computable schedule (the term left)"] } else { vec![] }, "disclaimer": disc}) }
        _ => json!({"rule": "no general legal cap is known for this kind of credit: read the contract", "legal_basis": null, "max_indemnity": null, "needs": [], "disclaimer": disc}),
    }.as_object().map(|o| { let mut o = o.clone(); o.insert("as_of".into(), today.to_string().into()); o.insert("amount".into(), json!(amt.map(money_str))); Value::Object(o) }).unwrap()
}

// ---------------------------------------------------------------- liabilities list, loan detail, net worth
fn holder(h: &Option<serde_yaml::Value>, one: &Option<String>) -> String {
    match h { Some(serde_yaml::Value::Sequence(s)) if s.len() > 1 => "joint".into(), Some(serde_yaml::Value::Sequence(s)) if s.len() == 1 => s[0].as_str().unwrap_or("unassigned").into(),
        Some(serde_yaml::Value::String(s)) if !s.is_empty() => if ["joint", "both", "all"].contains(&s.to_lowercase().as_str()) { "joint".into() } else { s.clone() }, _ => one.clone().unwrap_or_else(|| "unassigned".into()) }
}
/// Bank payments that match a loan's `payment_match` (regex on the merchant key or description).
pub fn payments(ds: &Dataset, l: &Liability) -> Vec<(NaiveDate, i64, String, String)> {
    let Some(rx) = l.payment_match.as_deref().and_then(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()) else { return vec![] };
    let mut v: Vec<_> = ds.whole().into_iter().filter(|t| t.amount_c < 0 && (rx.is_match(&t.mkey).unwrap_or(false) || rx.is_match(&t.desc).unwrap_or(false))).map(|t| (t.date, -t.amount_c, ds.label(&t.account), t.key)).collect(); v.sort(); v
}
pub fn liability_json(ds: &Dataset, l: &Liability, today: NaiveDate, rows: bool) -> Value {
    let s = compute(l, today); let pays = payments(ds, l); let mut miss: Vec<String> = s.missing.clone(); if l.payment_match.is_none() { miss.push("payment_match".into()); }
    let stale = dd(&l.outstanding_as_of).is_some_and(|a| a < add_months(today, -6));
    json!({"id": l.id, "file": l.file, "kind": l.kind, "lender": l.lender, "asset": l.asset, "holder": l.holder, "start_date": l.start_date, "end_date": l.end_date, "first_payment_date": l.first_payment_date, "term_months": l.term_months, "payment_day": l.payment_day,
        "principal": l.principal.map(|x| money_str(cents(x))), "outstanding": l.outstanding.map(|x| money_str(cents(x))), "outstanding_as_of": l.outstanding_as_of, "outstanding_stale": stale,
        "rate": l.rate.as_ref().map(|r| json!({"type": r.ty, "nominal": r.nominal, "taeg": r.taeg, "index": r.index, "margin": r.margin, "cap": r.cap, "monthly_convention": r.monthly_convention.clone().unwrap_or_else(|| "equivalent".into())})),
        "monthly_payment": l.monthly_payment.map(|x| money_str(cents(x))), "insurance": l.insurance.as_ref().map(|i| json!({"provider": i.provider, "monthly": i.monthly.map(|x| money_str(cents(x))), "delegated": i.delegated, "rate_pct": i.rate_pct, "basis": i.basis})),
        "deferral": l.deferral.as_ref().map(|d| json!({"months": d.months, "kind": d.kind})), "debited_account": l.debited_account, "debited_account_label": l.debited_account.as_ref().map(|a| ds.label(a)), "payment_match": l.payment_match,
        "early_repayment_penalty": l.early_repayment_penalty.as_ref().map(crate::memstore::jsonify), "first_payment": l.first_payment.map(|x| money_str(cents(x))), "residual_value": l.residual_value.map(|x| money_str(cents(x))), "mileage_limit_km": l.mileage_limit_km, "excess_km_fee": l.excess_km_fee.map(|x| money_str(cents(x))), "initial_km": l.initial_km, "odometer": [], "notes": l.notes,
        "missing": miss, "open_questions": [], "payments": pays.last().map(|p| json!({"series_id": null, "last_date": p.0.to_string(), "next_expected": s.rows.iter().find(|r| !r.made).map(|r| r.due.to_string()), "amount": money_str(p.1), "status": "observed"})),
        "schedule": s.to_json(rows), "remaining_capital": s.remaining.or(l.outstanding.map(cents)).map(money_str), "remaining_capital_source": if s.remaining.is_some() { json!("schedule") } else if l.outstanding.is_some() { json!("declared") } else { Value::Null },
        "alerts": [], "payments_seen": pays.len(), "lease": crate::loans_extra::lease_view(l, &ds.memory_dir, today), "inferred": crate::loans_extra::infer(ds, l)["fields"], "early_repayment": early_repayment(l, &s, None, today)})
}
pub fn liabilities_view(ds: &Dataset, today: NaiveDate) -> Value {
    let (ls, warn) = liabilities(&ds.memory_dir); let items: Vec<Value> = ls.iter().map(|l| liability_json(ds, l, today, false)).collect();
    let known: i64 = items.iter().filter_map(|i| i["remaining_capital"].as_str()).map(|s| to_cents(s.parse().unwrap_or(0.0))).sum(); let monthly: i64 = ls.iter().filter_map(|l| l.monthly_payment).map(cents).sum();
    json!({"liabilities": items, "totals": {"outstanding_known": money_str(known), "monthly_payments": money_str(monthly), "n_unknown_outstanding": items.iter().filter(|i| i["remaining_capital"].is_null() && i["kind"] != "loa" && i["kind"] != "lld").count()}, "warnings": warn,
        "note": "Capital still due from the amortization schedule when the loan's facts allow it, else the declared outstanding; never estimated from an instalment."})
}
pub fn loan_detail(ds: &Dataset, id: &str, today: NaiveDate) -> Option<Value> {
    let (ls, _) = liabilities(&ds.memory_dir); let l = ls.iter().find(|l| l.id == id)?; let s = compute(l, today); let pays = payments(ds, l);
    let mut amts: Vec<i64> = pays.iter().map(|p| p.1).collect(); amts.sort();
    Some(json!({"id": l.id, "kind": l.kind, "schedule": s.to_json(true), "lease": crate::loans_extra::lease_view(l, &ds.memory_dir, today), "alerts": [], "early_repayment": early_repayment(l, &s, None, today),
        "payments": {"count": pays.len(), "first": pays.first().map(|p| p.0.to_string()), "last": pays.last().map(|p| p.0.to_string()), "last_amount": pays.last().map(|p| money_str(p.1)), "median_amount": (!amts.is_empty()).then(|| money_str(median_c(&amts))),
            "recent": pays.iter().rev().take(12).map(|p| json!({"date": p.0.to_string(), "amount": money_str(p.1), "account": p.2})).collect::<Vec<_>>()},
        "inference": crate::loans_extra::infer(ds, l)}))
}
const CATS: [&str; 6] = ["cash", "savings", "investments", "real_estate", "vehicles", "other"];
pub fn asset_category(kind: &str) -> &'static str { match kind { "regulated_savings" | "savings_account" => "savings", "employee_savings_plan" | "life_insurance_savings" | "securities" | "pension" | "crypto" => "investments", "real_estate" | "real_estate_rental" => "real_estate", "vehicle" => "vehicles", "cash" => "cash", _ => "other" } }
/// Bank balances + assets of the memory - capital still due, by category and owner; unknown items are listed, never counted as zero.
pub fn net_worth(ds: &Dataset, today: NaiveDate, asset_stale_months: i32) -> Value {
    let (mut assets_c, mut liab_c, mut unknown, mut stale) = (0i64, 0i64, vec![], vec![]); let mut by_cat: BTreeMap<String, i64> = CATS.iter().map(|c| (c.to_string(), 0)).collect(); let mut owners: BTreeMap<String, [i64; 3]> = BTreeMap::new();
    let mut accs = vec![]; let mut bank = 0;
    for a in ds.accounts.values() { let owner = a.owner.clone().unwrap_or_else(|| "unassigned".into()); let cat = if a.purpose.as_deref() == Some("savings") { "savings" } else { "cash" };
        match ds.balances.get(&a.uid) { Some(b) => { let st = (today - b.as_of).num_days() > 7; bank += b.amount_c; assets_c += b.amount_c; *by_cat.get_mut(cat).unwrap() += b.amount_c; owners.entry(owner).or_default()[0] += b.amount_c; if st { stale.push(json!({"kind": "account", "id": a.uid})); }
                accs.push(json!({"uid": a.uid, "label": a.label, "bank": a.bank, "owner": a.owner, "purpose": a.purpose, "balance": money_str(b.amount_c), "as_of": b.as_of.to_string(), "balance_type": b.ty, "stale": st})); }
            None => { owners.entry(owner).or_default()[2] += 1; unknown.push(json!({"kind": "account", "id": a.uid, "label": a.label, "reason": "no balance synced yet"})); accs.push(json!({"uid": a.uid, "label": a.label, "bank": a.bank, "owner": a.owner, "purpose": a.purpose, "balance": null, "as_of": null})); } } }
    let cut = add_months(today, -asset_stale_months); let mut items = vec![]; let (mut atot, mut connected) = (0i64, 0);
    for a in assets(&ds.memory_dir) { let cat = asset_category(&a.kind); let owner = holder(&a.holders, &a.holder);
        if a.connected == Some(true) { connected += 1; items.push(json!({"id": a.id, "kind": a.kind, "provider": a.provider, "holder": owner, "value": a.amount().map(|v| money_str(cents(v))), "as_of": a.as_of, "stale": false, "unknown_value": a.amount().is_none(), "liquidity": a.liquidity, "connected": true, "contribution_monthly": a.contribution_monthly.map(|x| money_str(cents(x))), "description": a.description, "counted": false, "category": cat, "note": "counted through the balance of its bank account"})); continue; }
        let st = dd(&a.as_of).is_none_or(|x| x < cut);
        match a.amount() { Some(v) => { let c = cents(v); assets_c += c; atot += c; *by_cat.get_mut(cat).unwrap() += c; owners.entry(owner.clone()).or_default()[0] += c; if st { stale.push(json!({"kind": "asset", "id": a.id})); } }
            None => { owners.entry(owner.clone()).or_default()[2] += 1; unknown.push(json!({"kind": "asset", "id": a.id, "label": a.description.clone().unwrap_or(a.id.clone()), "reason": "no value recorded"})); } }
        items.push(json!({"id": a.id, "kind": a.kind, "provider": a.provider, "holder": owner, "value": a.amount().map(|v| money_str(cents(v))), "as_of": a.as_of, "stale": st, "unknown_value": a.amount().is_none(), "liquidity": a.liquidity, "connected": a.connected, "contribution_monthly": a.contribution_monthly.map(|x| money_str(cents(x))), "description": a.description, "counted": a.amount().is_some(), "category": cat}));
    }
    let mut litems = vec![];
    for l in liabilities(&ds.memory_dir).0 { let owner = holder(&l.holders, &l.holder); let s = compute(&l, today);
        if s.status == "not_applicable" { litems.push(json!({"id": l.id, "kind": l.kind, "lender": l.lender, "outstanding": null, "as_of": null, "stale": false, "counted": false, "excluded": true, "note": "lease: no capital owed, not counted"})); continue; }
        if dd(&l.end_date).is_some_and(|e| e < today) && l.outstanding.is_none() && s.status != "computed" { continue; }
        let (amt, src, as_of, st) = match (s.remaining, l.outstanding) { (Some(r), _) => (Some(r), "schedule", Some(today.to_string()), false), (None, Some(o)) => (Some(cents(o)), "declared", l.outstanding_as_of.clone(), dd(&l.outstanding_as_of).is_none_or(|x| x < add_months(today, -6))), _ => (None, "", None, false) };
        match amt { Some(c) => { liab_c += c; owners.entry(owner).or_default()[1] += c; if st { stale.push(json!({"kind": "liability", "id": l.id})); } } None => { owners.entry(owner).or_default()[2] += 1; unknown.push(json!({"kind": "liability", "id": l.id, "label": l.kind, "reason": "capital still due unknown: the loan's facts do not allow a schedule and no outstanding is recorded"})); } }
        litems.push(json!({"id": l.id, "kind": l.kind, "lender": l.lender, "outstanding": amt.map(money_str), "as_of": as_of, "stale": st, "counted": amt.is_some(), "source": (!src.is_empty()).then_some(src), "excluded": false}));
    }
    let mut bc: serde_json::Map<String, Value> = by_cat.iter().map(|(k, v)| (k.clone(), json!(money_str(*v)))).collect(); bc.insert("liabilities".into(), json!(money_str(liab_c)));
    json!({"as_of": today.to_string(), "net_worth": money_str(assets_c - liab_c), "complete": unknown.is_empty(), "n_unknown": unknown.len(), "unknown": unknown, "stale": stale,
        "bank": {"total": money_str(bank), "accounts": accs}, "assets": {"total": money_str(atot), "items": items, "connected_not_counted": connected}, "liabilities": {"total": money_str(liab_c), "items": litems},
        "by_category": bc, "by_owner": owners.iter().map(|(o, v)| (o.clone(), json!({"assets": money_str(v[0]), "liabilities": money_str(v[1]), "net_worth": money_str(v[0] - v[1]), "n_unknown": v[2]}))).collect::<serde_json::Map<_, _>>(),
        "history": [], "note": "Known assets minus known debts. An item without a value is listed as unknown, never counted as zero; the figure is then the known part."})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lb(y: &str) -> Liability { serde_yaml::from_str(y).unwrap() }
    #[test] fn belgian_annuity() {
        let t = d("2026-01-15").unwrap();
        let l = lb("id: house\nkind: mortgage\nprincipal: 200000\nrate: {type: fixed, nominal: 3}\nstart_date: 2025-01-10\nterm_months: 240\n");
        let s = compute(&l, t); assert_eq!(s.status, "computed"); assert_eq!(s.rows.len(), 240); assert_eq!(s.rows.last().unwrap().balance, 0);
        assert_eq!(money_str(s.payment), "1105.15", "equivalent monthly rate of 3 %"); assert_eq!(s.rows[0].interest, 49325);
        let p = compute(&lb("id: house\nkind: mortgage\nprincipal: 200000\nrate: {nominal: 3, monthly_convention: proportional}\nstart_date: 2025-01-10\nterm_months: 240\n"), t); assert_eq!(money_str(p.payment), "1109.20");
        assert_eq!(s.rows.iter().filter(|r| r.made).count(), 12); assert_eq!(s.rows[0].due.to_string(), "2025-02-10");
        let e = early_repayment(&l, &s, Some(1_000_000), t); assert_eq!(e["max_indemnity"], "75.00", "3 months of interest at 3 % on 10 000");
        let m = compute(&lb("id: x\nkind: mortgage\nrate: {nominal: 3}\nstart_date: 2025-01-10\n"), t); assert_eq!(m.missing, vec!["principal", "end_date or term_months"]);
        let o = compute(&lb("id: car\nkind: car_loan\nrate: {nominal: 5}\noutstanding: 10000\noutstanding_as_of: 2025-12-31\nend_date: 2028-12-05\n"), t);
        assert_eq!((o.status, o.mode), ("computed", Some("from_outstanding"))); assert_eq!(o.rows[0].due.to_string(), "2026-01-05"); assert_eq!(o.rows.last().unwrap().due.to_string(), "2028-12-05");
        let c = compute(&lb("id: c\nkind: consumer_loan\nprincipal: 6000\nrate: {nominal: 6}\nstart_date: 2025-06-01\nterm_months: 24\n"), t);
        assert_eq!(early_repayment(&lb("id: c\nkind: consumer_loan\nprincipal: 6000\nrate: {nominal: 6}\nstart_date: 2025-06-01\nterm_months: 24\n"), &c, Some(100000), t)["max_indemnity"], "10.00", "1 % of 1000 with more than a year left");
    }
}
