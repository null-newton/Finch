//! The Belgian tax helper (B9): CANDIDATE items for the personal income tax return of one income year, found in the household's own
//! data: categories (`assets/rules.yaml`), memory tags (annotations of categorization.yaml), the loans, assets and rental property of
//! the memory, and two descriptor patterns (`PENSION_RX`, `LONG_TERM_RX`, to verify with real exports). A reminder list, never a return
//! and never advice: no rate or ceiling is quoted (they change every year and some items are regional), a figure finch does not have
//! is listed as missing, and every candidate names the certificate that holds the figure to declare.
use crate::{analytics::{Dataset, Tx}, disclaimers, loans, rental, util::*};
use chrono::Datelike;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Memory tags that mark a payment as a candidate (add them with an annotation of categorization.yaml).
pub const TAGS: [(&str, &[&str]); 6] = [("be-pension-saving", &["pension_saving", "pensioensparen", "epargne_pension"]), ("be-long-term-saving", &["long_term_saving", "langetermijnsparen"]),
    ("be-childcare", &["childcare"]), ("be-gifts", &["approved_gift"]), ("be-service-vouchers", &["service_vouchers"]), ("be-home-loan", &["home_loan"])];
const PENSION_RX: &str = r"(?i)\bPENSIOENSPA(AR|REN)\w*|\bEPARGNE[\s-]*PENSION\b";
const LONG_TERM_RX: &str = r"(?i)\bLANGE?\s*TERMIJN\s*SPA(AR|REN)\w*|\bEPARGNE\s+(A\s+)?LONG\s+TERME\b";
const CEILINGS: &str = "Ceilings, rates, age limits and minimums change every year and some items are regional: finch quotes none. The figures of the year are published by the FPS Finance (FOD Financiën / SPF Finances); the return is filed on Tax-on-web (MyMinfin).";
const VERIFY: &str = "Many of these items are pre-filled on Tax-on-web from the certificates the bank, insurer, institution or provider sends: compare with the pre-filled return rather than add a figure twice. The certificate, not the bank statement, is the figure to declare.";
const UNSEEN: &str = "payments from an account finch does not read are not counted: the certificate has the complete figure";

fn tags_of(id: &str) -> &'static [&'static str] { TAGS.iter().find(|t| t.0 == id).map_or(&[], |t| t.1) }
fn pick<'a>(txs: &[&'a Tx], cats: &[&str], id: &str, rx: Option<&str>) -> Vec<&'a Tx> {
    let rx = rx.and_then(|p| fancy_regex::Regex::new(p).ok()); let tags = tags_of(id);
    txs.iter().copied().filter(|t| cats.contains(&t.category.as_str()) || tags.iter().any(|g| t.tags.contains(*g)) || (t.amount_c < 0 && rx.as_ref().is_some_and(|r| is(r, &t.desc) || is(r, &t.mkey)))).collect()
}
fn spent(txs: &[&Tx]) -> i64 { -txs.iter().map(|t| t.amount_c).sum::<i64>() }
fn evidence(txs: &[&Tx]) -> Vec<String> { let mut v: Vec<&&Tx> = txs.iter().collect(); v.sort_by(|a, b| (a.amount_c, &a.key).cmp(&(b.amount_c, &b.key))); let mut out: Vec<String> = vec![]; for t in v { if !out.contains(&t.key) { out.push(t.key.clone()); } } out.truncate(5); out }
fn covered_note(ds: &Dataset, year: i32, txs: &[&Tx]) -> Option<String> {
    let mut acc: Vec<String> = txs.iter().map(|t| t.account.clone()).collect(); acc.sort(); acc.dedup(); if acc.is_empty() { return None; }
    let (closed, cov) = (last_closed_month(ds.today), ds.coverage.common_months(&acc));
    let miss: Vec<String> = (1..=12).map(|m| format!("{year:04}-{m:02}")).filter(|m| *m <= closed && !cov.contains(m)).collect();
    (!miss.is_empty()).then(|| format!("months of {year} not fully covered by the accounts carrying these payments: {}", miss.join(", ")))
}
fn entities(txs: &[&Tx]) -> Vec<Value> {
    let mut by: BTreeMap<&str, (i64, usize)> = BTreeMap::new(); for t in txs { let e = by.entry(t.entity.as_str()).or_default(); e.0 -= t.amount_c; e.1 += 1; }
    let mut v: Vec<_> = by.into_iter().collect(); v.sort_by(|a, b| (-a.1 .0, a.0).cmp(&(-b.1 .0, b.0))); v.into_iter().take(6).map(|(e, (c, n))| json!({"entity": e, "total": money_str(c), "n_tx": n})).collect()
}
fn merge(mut a: Value, b: Value) -> Value { if let (Some(o), Value::Object(m)) = (a.as_object_mut(), b) { o.extend(m); } a }
/// One candidate from the payments found (`spent` is null when there are none, e.g. a contract known only from the memory).
fn cand(ds: &Dataset, year: i32, id: &str, heading: &str, mechanism: &str, txs: &[&Tx], extra: Value) -> Value {
    let notes: Vec<String> = covered_note(ds, year, txs).into_iter().chain([UNSEEN.to_string()]).collect();
    let mut c = merge(json!({"id": id, "heading": heading, "mechanism": mechanism, "spent": (!txs.is_empty()).then(|| money_str(spent(txs))), "n_tx": txs.len(), "evidence": evidence(txs), "notes": notes}), extra);
    if let Some(x) = c.get("extra_notes").cloned() { c.as_object_mut().unwrap().remove("extra_notes"); if let (Some(n), Some(x)) = (c["notes"].as_array_mut(), x.as_array()) { n.extend(x.iter().cloned()); } } c
}
/// `region` of household.yaml (the region of residence decides the regional items), normalised; None when absent or unknown.
pub fn region(memory_dir: &std::path::Path) -> Option<&'static str> {
    let v: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(memory_dir.join("household.yaml")).ok()?).ok()?;
    match v.get("region")?.as_str()?.trim().to_lowercase().as_str() { "flanders" | "vlaanderen" | "flandre" => Some("flanders"), "wallonia" | "wallonie" | "wallonië" | "walloon region" => Some("wallonia"), "brussels" | "brussel" | "bruxelles" => Some("brussels"), _ => None }
}

/// The candidates of income `year` (default: the year before today's). Err for a year outside 2000..=this year.
pub fn candidates(ds: &Dataset, year: Option<i32>) -> Result<Value, String> {
    let year = year.unwrap_or(ds.today.year() - 1); if !(2000..=ds.today.year()).contains(&year) { return Err("year must be between 2000 and the current year".into()); }
    let txs: Vec<&Tx> = ds.txs.iter().filter(|t| t.date.year() == year).collect(); let reg = region(&ds.memory_dir);
    let kids: Vec<Value> = ds.people.members.iter().filter(|m| m.role == "child").map(|m| json!({"member": m.id, "birth_year_known": m.birth_year.is_some()})).collect();
    let no_birth = ds.people.members.iter().filter(|m| m.role == "child" && m.birth_year.is_none()).count();
    let assets = loans::assets(&ds.memory_dir); let kind_items = |k: &str| assets.iter().filter(|a| a.kind == k).map(|a| a.id.clone()).collect::<Vec<_>>();
    let reg_missing = || if reg.is_none() { vec!["region of residence on 1 January of the tax year (household.yaml `region`: flanders, wallonia or brussels)".to_string()] } else { vec![] };
    let (mut out, mut nodata) = (vec![], vec![]);
    let mut none = |id: &str, how: &str| nodata.push(json!({"id": id, "how": how}));
    // pension saving
    let p = pick(&txs, &[], "be-pension-saving", Some(PENSION_RX)); let items = kind_items("pension");
    if (!p.is_empty() && spent(&p) > 0) || !items.is_empty() {
        out.push(cand(ds, year, "be-pension-saving", "Pension saving (pensioensparen / épargne-pension): tax reduction", "reduction", &p, json!({"memory_items": items,
            "conditions": ["a pension savings fund or insurance contract in your own name; the reduction is per taxpayer, not per household", "two yearly ceilings exist, each with its own reduction rate: you choose one per year (finch quotes neither)"],
            "documents_to_keep": ["the yearly fiscal certificate of the bank or insurer (it states what was paid and the holder)"],
            "missing_info": if p.is_empty() { vec!["the payments of the year: none is seen on the accounts finch reads".to_string()] } else { vec!["which partner holds the contract".to_string()] },
            "legal_basis": "WIB 92 / CIR 92 art. 145/8 and following (federal)"})));
    } else { none("be-pension-saving", "payments whose description says PENSIOENSPAREN / EPARGNE-PENSION, tag pension_saving, or an asset of kind pension"); }
    // long-term saving (individual life insurance)
    let p = pick(&txs, &["insurance.life"], "be-long-term-saving", Some(LONG_TERM_RX)); let items = kind_items("life_insurance_savings");
    if (!p.is_empty() && spent(&p) > 0) || !items.is_empty() {
        out.push(cand(ds, year, "be-long-term-saving", "Long-term saving (langetermijnsparen / épargne à long terme): life insurance premiums", "reduction", &p, json!({"memory_items": items,
            "conditions": ["only an individual life insurance that meets the legal conditions (taken out before a legal age, for a minimum term); not every life insurance qualifies", "a premium can count only once: a life insurance tied to a home loan may belong to the home-loan regime instead"],
            "documents_to_keep": ["the insurer's yearly fiscal certificate"], "missing_info": ["whether the contract qualifies (the insurer's certificate says so)"],
            "legal_basis": "WIB 92 / CIR 92 art. 145/1, 2° (federal)"})));
    } else { none("be-long-term-saving", "category insurance.life, tag long_term_saving, or an asset of kind life_insurance_savings"); }
    // childcare
    let p = pick(&txs, &["kids.childcare"], "be-childcare", None);
    if !p.is_empty() && spent(&p) > 0 {
        let mut miss = vec!["the deductible days and amounts (the provider's certificate)".to_string()]; if kids.is_empty() { miss.push("no child in household.yaml".into()); } if no_birth > 0 { miss.push(format!("the birth year of {no_birth} child(ren) (`birth_year_known: false` in `children`): the deduction depends on the child's age")); }
        out.push(cand(ds, year, "be-childcare", "Childcare (kinderopvang / garde d'enfants): tax reduction", "reduction", &p, json!({"children": kids,
            "conditions": ["childcare for a dependent child below an age limit set by law (higher for a child with a disability)", "paid to an approved, subsidised or supervised provider (crèche, after-school care, holiday care...); a babysitter or a family member usually does not count", "a ceiling per child and per day applies (finch quotes none)"],
            "documents_to_keep": ["the fiscal certificate of each childcare provider"], "missing_info": miss, "legal_basis": "WIB 92 / CIR 92 art. 145/35 (federal)"})));
    } else { none("be-childcare", "category kids.childcare or tag childcare"); }
    // gifts
    let p = pick(&txs, &["charity.donations"], "be-gifts", None);
    if !p.is_empty() && spent(&p) > 0 {
        out.push(cand(ds, year, "be-gifts", "Gifts to approved institutions (giften / libéralités): tax reduction", "reduction", &p, json!({"organisations": entities(&p),
            "conditions": ["only gifts to an institution approved by the tax administration count, as stated on its fiscal certificate", "a minimum per institution per year and an overall ceiling apply (finch quotes none)", "a collection through a crowdfunding platform, a gift to a person, or a purchase (raffle, membership, ticket) usually does not count"],
            "documents_to_keep": ["the fiscal certificate of each institution (usually sent to the tax administration and pre-filled)"], "missing_info": ["which of these institutions are approved"], "legal_basis": "WIB 92 / CIR 92 art. 145/33 (federal)"})));
    } else { none("be-gifts", "category charity.donations or tag approved_gift"); }
    // service vouchers (regional)
    let p = pick(&txs, &["housing.household_help"], "be-service-vouchers", None);
    if !p.is_empty() && spent(&p) > 0 {
        out.push(cand(ds, year, "be-service-vouchers", "Service vouchers (dienstencheques / titres-services): regional tax reduction, if your region still has one", "check", &p, json!({"region": reg,
            "conditions": ["only vouchers bought from the issuer (paper or electronic) count, not other cleaning help paid directly", "the reduction is regional: whether it still exists, and how much, depends on your region of residence and the year (some regions reduced or ended it): check your region's current rule"],
            "documents_to_keep": ["the issuer's yearly fiscal certificate"], "missing_info": reg_missing(), "legal_basis": "regional competence since the sixth state reform (special financing act); the regional tax codes set the rule"})));
    } else { none("be-service-vouchers", "category housing.household_help or tag service_vouchers"); }
    // home loan (regional regimes: flag only)
    let p = pick(&txs, &["housing.mortgage", "insurance.borrower"], "be-home-loan", None);
    let morts: Vec<loans::Liability> = loans::liabilities(&ds.memory_dir).0.into_iter().filter(|l| l.kind == "mortgage").collect();
    if (!p.is_empty() && spent(&p) > 0) || !morts.is_empty() {
        let ls: Vec<Value> = morts.iter().map(|l| { let j = loans::compute(l, ds.today).to_json(false); let row = j["by_year"].as_array().and_then(|a| a.iter().find(|r| r["year"] == year)).cloned();
            let mut miss: Vec<String> = vec![]; if row.is_none() { miss.push(if j["status"] == "computed" { format!("no instalment of {year} in the schedule") } else { format!("the schedule of {year} cannot be computed: {}", j["missing"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")) }); }
            if l.start_date.is_none() { miss.push("the date the loan was signed (start_date): the regime depends on it".into()); }
            json!({"loan": l.id, "kind": l.kind, "signed": l.start_date, "schedule_status": j["status"], "year_from_schedule": row.map(|r| json!({"interest": r["interest"], "principal": r["principal"], "insurance": r["insurance"], "instalments": r["instalments"], "partial": r["partial"]})), "missing": miss}) }).collect();
        let mut miss = reg_missing(); miss.push("whether the loan finances your own home (eigen woning / habitation propre) and whether it is your only home".into()); if morts.is_empty() { miss.push("the loan in memory/liabilities (kind mortgage): interest and capital of the year come from its schedule".into()); }
        out.push(cand(ds, year, "be-home-loan", "Home loan (woonbonus / chèque habitat / federal rules): flag only, the regime depends on region and loan date", "check", &p, json!({"loans": ls, "region": reg,
            "conditions": ["which regime applies (the Flemish, Walloon or Brussels regional regime for the own home, or the federal rules for another home) depends on the region of residence, the date the loan was signed and its purpose: finch does not decide it", "several regions closed their regime to loans signed after a given date; an older loan may keep the rules of its time", "capital, interest and borrower-insurance premiums can count under the same regime: the bank's yearly certificate splits them"],
            "documents_to_keep": ["the bank's yearly certificate (interest and capital paid)", "the borrower insurance certificate", "the loan deed and, for an older loan, the first-year certificate (attest for the first tax year)"], "missing_info": miss,
            "legal_basis": "regional competence for the own home since the sixth state reform; WIB 92 / CIR 92 for the federal rules", "extra_notes": ["the yearly figures are computed from the loan figures recorded in memory: the bank's certificate is the figure to declare"]})));
    } else { none("be-home-loan", "category housing.mortgage, tag home_loan, or a liability of kind mortgage"); }
    // rental property: cadastral income
    let props = rental::properties(ds);
    for a in &props {
        let lk = rental::links(ds, a); let p: Vec<&Tx> = txs.iter().copied().filter(|t| t.category == "housing.property_tax" && lk.accounts.contains(&t.account)).collect();
        let mut miss = vec!["the indexed cadastral income (KI / RC) of the property: not recorded in finch (it is on the property-tax assessment, aanslagbiljet onroerende voorheffing / avertissement-extrait de rôle du précompte immobilier)".to_string()]; miss.extend(lk.notes.iter().cloned());
        out.push(cand(ds, year, "be-rental-cadastral", "Rental property: declare its cadastral income", "declaration", &p, json!({"asset": a.id, "rule": rental::tax(year)["note"],
            "conditions": ["the property tax paid in the year (shown as spent, if any) is not the taxable base: its assessment shows the cadastral income", "professional or furnished letting, or letting to a company, follow other rules: ask an adviser"],
            "documents_to_keep": ["the property-tax assessment of the year", "the lease"], "missing_info": miss, "legal_basis": "WIB 92 / CIR 92 (income from real estate)"})));
    }
    if props.is_empty() { none("be-rental-cadastral", "an asset of kind real_estate_rental"); }
    let adults = ds.people.members.iter().filter(|m| m.role == "adult").count();
    Ok(json!({"year": year, "country": "BE", "income_year_note": format!("{year} is the income year (inkomstenjaar / année des revenus): it is declared in tax year {} (aanslagjaar / exercice d'imposition), on Tax-on-web (MyMinfin) or on paper", year + 1),
        "year_in_progress": year == ds.today.year(), "candidates": out, "no_data_for": nodata, "tags_used": TAGS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "household": {"adults": adults, "children": kids.len(), "region": reg}, "ceilings": CEILINGS, "verify": VERIFY, "disclaimer": disclaimers::TAX, "disclaimer_key": "tax"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn descriptors() { let (p, l) = (fancy_regex::Regex::new(PENSION_RX).unwrap(), fancy_regex::Regex::new(LONG_TERM_RX).unwrap());
        for s in ["DEMOBANK PENSIOENSPAREN", "STORTING PENSIOENSPAARFONDS DEMO", "VERSEMENT EPARGNE-PENSION", "epargne pension demo"] { assert!(is(&p, s), "{s}"); }
        for s in ["PENSIOEN DEMO", "OVERSCHRIJVING VAN PENSIOENFONDS", "SPAREN"] { assert!(!is(&p, s), "{s}"); }
        for s in ["PREMIE LANGE TERMIJN SPAREN", "LANGETERMIJNSPAREN DEMO", "EPARGNE A LONG TERME"] { assert!(is(&l, s), "{s}"); } assert!(!is(&l, "TERMIJNREKENING")); }
    #[test] fn regions() { let t = tempfile::tempdir().unwrap(); let w = |s: &str| { std::fs::write(t.path().join("household.yaml"), s).unwrap(); region(t.path()) };
        assert_eq!(w("region: Bruxelles\n"), Some("brussels")); assert_eq!(w("region: wallonie\n"), Some("wallonia")); assert_eq!(w("region: Mars\n"), None); assert_eq!(w("members: []\n"), None); }
}
