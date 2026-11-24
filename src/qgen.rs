//! Questions generated from the data (E3-4): what the coach cannot infer and the user can answer in a sentence. Merchants with
//! money at stake and an uncertain label, large unexplained one-off payments, recurring costs with no contract or loan file, the
//! usage of subscriptions, the empty key fields of loans / contracts / assets, and accounts without an owner or a purpose. A
//! person-like counterparty is never named (they are counted in one question). Already asked (open, answered, dismissed) = skipped.
use crate::{analytics::{recurring::Recurring, Dataset}, config::Config, memstore::Store, questions, util::*};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const CONTRACT_GROUPS: [&str; 4] = ["housing", "subscriptions", "insurance", "debt"];
pub fn qid(kind: &str, key: &str) -> String { format!("q-{kind}-{}", &sha1_hex(key)[..10]) }
fn mk(kind: &str, key: &str, topic: &str, code: &str, text: String, ev: Value, target: Option<Value>, stake: Option<f64>, today: chrono::NaiveDate) -> Value {
    questions::doc(&json!({"id": qid(kind, key), "status": "open", "topic": topic, "topic_code": code, "question": text, "evidence": ev, "suggested_target": target, "stake": stake.map(|s| (s * 100.0).round() / 100.0), "key": key, "origin": "generated", "created": today.to_string()}))
}
fn covered(pats: &[fancy_regex::Regex], key: &str) -> bool { pats.iter().any(|r| r.is_match(key).unwrap_or(false)) }
fn eur(x: f64) -> String { let s = format!("{:.0}", x.abs()); let g: Vec<String> = s.as_bytes().rchunks(3).rev().map(|c| String::from_utf8_lossy(c).into()).collect(); format!("{}{}", if x < 0.0 { "-" } else { "+" }, g.join(",")) }
/// The usage question about one recurring series (shared with the coach's `questions_propose`: key `usage:<series>`).
pub fn usage_question(x: &Value, person_like: bool, contract: Option<&str>, today: chrono::NaiveDate) -> Value {
    let id = x["id"].as_str().unwrap_or(""); let cat = x["category"].as_str().unwrap_or(""); let yearly = x["yearly_cost"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0).abs();
    let monthly = if x["cadence"] == "monthly" { x["expected_amount"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0).abs() } else { (yearly / 12.0 * 100.0).round() / 100.0 };
    let name = if person_like { format!("the {} payment", cat.rsplit('.').next().unwrap_or("").replace('_', " ")) } else { x["entity"].as_str().unwrap_or("").to_string() };
    let since = x["first_date"].as_str().or(x["occurrences"][0]["date"].as_str()).unwrap_or("?");
    mk("usage", &format!("usage:{id}"), "Subscriptions", "subscriptions", format!("Do you still use {name} (about {monthly:.2} EUR a month, {yearly:.2} EUR a year, paid since {since}; series {id})? How often, and by whom? Keep it, review it or stop it?"),
        json!({"series": id, "category": cat, "monthly": monthly, "yearly": yearly, "since": since}), Some(contract.map_or(json!({"file": "contracts/"}), |c| json!({"file": format!("contracts/{c}.yaml"), "field": "usage"}))), Some(yearly), today)
}
/// Every candidate question, already-asked ones removed, highest stake first.
pub fn generate(st: &Store, con: &Connection, cfg: &Config, ds: &Dataset, rec: &Recurring, min_stake: f64, big: f64) -> Result<Vec<Value>> {
    let today = ds.today; let mut out: Vec<Value> = vec![];
    let (ls, _) = crate::loans::liabilities(&st.root); let cs = crate::subs::contracts(&st.root);
    let pats: Vec<fancy_regex::Regex> = ls.iter().filter_map(|l| l.payment_match.clone()).chain(cs.iter().filter_map(|c| c.merchant_match.clone())).filter_map(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()).collect();
    let rules = crate::classify::rules::Rules::load(cfg)?; let rv = crate::classify::llm::review(con, cfg, &rules)?;
    let held: BTreeSet<String> = rv["held_back_person_like"].as_array().into_iter().flatten().filter_map(|h| h["key"].as_str().map(String::from)).collect();
    // merchants with an uncertain or missing label, by money at stake
    let mut m = vec![];
    for r in rv["low_confidence"].as_array().into_iter().flatten() { let key = r["key"].as_str().unwrap_or(""); let stake = r["total"].as_f64().unwrap_or(0.0); if stake < min_stake || covered(&pats, key) || held.contains(key) { continue; }
        let txs: Vec<&crate::analytics::Tx> = ds.txs.iter().filter(|t| t.mkey == key).collect(); if txs.is_empty() { continue; } let net: i64 = txs.iter().map(|t| t.amount_c).sum();
        let (first, last) = (txs.iter().map(|t| t.date).min().unwrap(), txs.iter().map(|t| t.date).max().unwrap()); let cat = r["category"].as_str().filter(|c| *c != "other.uncategorized");
        m.push(mk("merchant", &format!("merchant:{key}"), "Categorization (ordered by money at stake)", "categorization", format!("What is {key}? {} payment{}, {} EUR between {first} and {last} (currently {}).", txs.len(), if txs.len() == 1 { "" } else { "s" }, eur(net as f64 / 100.0), cat.unwrap_or("not categorized")),
            json!({"payments": txs.len(), "net_total": net as f64 / 100.0, "gross_total": stake, "first": first.to_string(), "last": last.to_string(), "current_category": cat.unwrap_or("none"), "label_source": r["source"], "confidence": r["confidence"]}), Some(json!({"file": "categorization.yaml"})), Some(stake), today)); }
    m.sort_by(|a, b| b["stake"].as_f64().unwrap_or(0.0).total_cmp(&a["stake"].as_f64().unwrap_or(0.0))); out.extend(m.into_iter().take(15));
    let (hn, ht): (usize, f64) = (held.len(), rv["held_back_person_like"].as_array().into_iter().flatten().map(|h| h["total"].as_f64().unwrap_or(0.0)).sum());
    if hn > 0 && ht >= min_stake { out.push(mk("people", "held-back:people", "Categorization (ordered by money at stake)", "categorization", format!("{hn} counterparties may be people (transfers, no business name): {ht:.0} EUR in total. They are never sent to an LLM; review them with `finch classify review` and say what they are (family, rent, babysitter...)."),
        json!({"counterparties": hn, "gross_total": (ht * 100.0).round() / 100.0}), Some(json!({"file": "categorization.yaml"})), Some(ht), today)); }
    // large payments that may be one-offs
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new(); for t in &ds.txs { *counts.entry(&t.mkey).or_default() += 1; }
    let mut large: Vec<&crate::analytics::Tx> = ds.txs.iter().filter(|t| (t.amount_c.abs() as f64) >= big * 100.0 && !["memory", "split", "override"].contains(&t.source.as_str()) && !t.category.starts_with("transfer.") && !t.category.starts_with("income.") && counts.get(t.mkey.as_str()).copied().unwrap_or(0) <= 2 && !held.contains(&t.mkey) && !t.is_one_off()).collect();
    large.sort_by_key(|t| std::cmp::Reverse(t.amount_c.abs()));
    for t in large.into_iter().take(10) { let a = t.amount_c as f64 / 100.0; out.push(mk("large", &format!("large:{}", t.key), "Large one-off payments", "largePayments", format!("Is the payment of {} EUR on {} to {} a one-off? (counted as {}; it is in the monthly averages unless tagged)", eur(a), t.date, t.mkey, t.category),
        json!({"amount": a, "date": t.date.to_string(), "category": t.category, "label_source": t.source, "merchant": t.mkey}), Some(json!({"file": "categorization.yaml"})), Some(a.abs()), today)); }
    // recurring costs without a contract or loan file
    for x in rec.json["series"].as_array().into_iter().flatten().filter(|x| x["direction"] == "out" && x["status"] == "active") { let (key, cat) = (x["key"].as_str().unwrap_or(""), x["category"].as_str().unwrap_or(""));
        if !CONTRACT_GROUPS.contains(&cat.split('.').next().unwrap_or("")) || cat == "housing.rent" || covered(&pats, key) || covered(&pats, x["entity"].as_str().unwrap_or("")) || held.contains(key) { continue; }
        let yearly = x["yearly_cost"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0).abs();
        out.push(mk("recurring", &format!("recurring:{key}"), "Recurring payments without a contract or loan file", "recurringNoFile", format!("{key}: a {} payment of about {} EUR ({}), {yearly:.0} EUR a year, with no contract or loan file. Is it a contract (which provider, renewal, notice) or a loan?", x["cadence"].as_str().unwrap_or(""), x["expected_amount"].as_str().unwrap_or("?").trim_start_matches('-'), cat),
            json!({"series": x["id"], "category": cat, "yearly": yearly}), Some(json!({"file": if cat.starts_with("debt") { "liabilities/" } else { "contracts/" }})), Some(yearly), today)); }
    // the usage of subscriptions (the bank cannot tell)
    for x in rec.json["series"].as_array().into_iter().flatten().filter(|x| x["direction"] == "out" && x["status"] == "active" && x["category"].as_str().is_some_and(|c| c.starts_with("subscriptions."))).take(10) {
        let key = x["key"].as_str().unwrap_or(""); let c = cs.iter().find(|c| c.merchant_match.as_deref().and_then(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()).is_some_and(|r| r.is_match(key).unwrap_or(false)));
        if c.is_some_and(|c| c.usage.is_some()) { continue; } out.push(usage_question(x, held.contains(key), c.map(|c| c.id.as_str()), today)); }
    // empty key fields
    for l in &ls { let lease = matches!(l.kind.as_str(), "loa" | "lld"); let v = serde_json::to_value(serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(st.root.join(&l.file)).unwrap_or_default()).unwrap_or_default()).unwrap_or_default();
        let keys: &[&str] = if lease { &["lender", "start_date", "end_date", "residual_value", "mileage_limit_km", "excess_km_fee"] } else { &["lender", "start_date", "end_date", "outstanding", "outstanding_as_of", "rate.nominal"] };
        let miss: Vec<&str> = keys.iter().copied().filter(|k| k.split('.').fold(&v, |c, p| &c[p]).is_null()).collect(); if miss.is_empty() { continue; }
        out.push(mk("fill", &format!("fill:liability:{}", l.id), "Liabilities", "liabilities", format!("Liability {} ({}): {} key fields are empty ({}). The loan contract or the latest statement has them.", l.id, l.kind, miss.len(), miss.join(", ")), json!({"missing": miss, "monthly_payment": l.monthly_payment}), Some(json!({"file": l.file, "field": miss.join(",")})), Some(l.monthly_payment.unwrap_or(0.0) * 12.0), today)); }
    for c in &cs { let miss: Vec<&str> = [("provider", c.provider.is_none()), ("billing.amount", c.billing.as_ref().and_then(|b| b.amount).is_none()), ("renewal", c.renewal.is_none()), ("notice_period_days", c.notice_period_days.is_none())].into_iter().filter(|x| x.1).map(|x| x.0).collect();
        if !miss.is_empty() { out.push(mk("fill", &format!("fill:contract:{}", c.id), "Contracts", "contracts", format!("Contract {}: {} unknown.", c.id, miss.join(", ")), json!({"missing": miss}), Some(json!({"file": c.file, "field": miss.join(",")})), None, today)); } }
    for a in crate::loans::assets(&st.root).into_iter().filter(|a| a.amount().is_none()) { out.push(mk("fill", &format!("fill:asset:{}", a.id), "Assets", "assets", format!("Asset {} ({}) has no value or balance yet: what is it today (and as of when)?", a.id, a.kind), json!({"missing": [if ["vehicle", "real_estate", "real_estate_rental"].contains(&a.kind.as_str()) { "value" } else { "balance" }]}), Some(json!({"file": "assets.yaml", "field": format!("assets[{}].balance", a.id)})), None, today)); }
    // accounts without an owner or a purpose
    for r in con.prepare("SELECT a.uid, COALESCE(a.bank,''), a.owner, a.purpose, COUNT(t.tx_key), MIN(t.booking_date), MAX(t.booking_date) FROM accounts a LEFT JOIN transactions t ON t.account_uid=a.uid WHERE a.exclude=0 AND (a.owner IS NULL OR a.purpose IS NULL) GROUP BY a.uid")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?)))? {
        let (uid, bank, owner, purpose, n, first, last) = r?; let miss: Vec<&str> = [("owner", owner.is_none()), ("purpose", purpose.is_none())].into_iter().filter(|x| x.1).map(|x| x.0).collect();
        out.push(mk("acct", &format!("acct:{uid}"), "Accounts", "accounts", format!("Account {} at {} ({n} transactions): {} not set. Whose is it and what is it for?", &uid[..uid.len().min(8)], if bank.is_empty() { "an unknown bank" } else { &bank }, miss.join(" and ")), json!({"bank": if bank.is_empty() { "?" } else { &bank }, "transactions": n, "first": first, "last": last, "missing": miss}), Some(json!({"file": "db:accounts", "field": miss.join(",")})), None, today)); }
    // one question per subject (recurring > merchant > large), never twice
    let have: Vec<Value> = st.items(questions::YAML); let keys: BTreeSet<String> = have.iter().filter_map(|q| q["key"].as_str().map(String::from)).collect(); let ids: BTreeSet<String> = have.iter().filter_map(|q| q["id"].as_str().map(String::from)).collect();
    let subject = |q: &Value| -> Option<(usize, String)> { let k = q["key"].as_str()?; let (kind, rest) = k.split_once(':')?; match kind { "recurring" => Some((0, rest.into())), "merchant" => Some((1, rest.into())), "large" => Some((2, q["evidence"]["merchant"].as_str()?.into())), _ => None } };
    let mut best: BTreeMap<String, usize> = BTreeMap::new(); for q in &out { if let Some((r, s)) = subject(q) { let e = best.entry(s).or_insert(r); *e = (*e).min(r); } }
    let mut seen = BTreeSet::new();
    let mut new: Vec<Value> = out.into_iter().filter(|q| subject(q).is_none_or(|(r, s)| best[&s] == r) && !keys.contains(q["key"].as_str().unwrap_or("")) && !ids.contains(q["id"].as_str().unwrap_or("")) && seen.insert(q["key"].as_str().unwrap_or("").to_string())).collect();
    new.sort_by(|a, b| b["stake"].as_f64().unwrap_or(0.0).total_cmp(&a["stake"].as_f64().unwrap_or(0.0))); Ok(new)
}
/// Add the generated questions (one recorded change).
pub fn add(st: &Store, qs: &[Value], dry_run: bool) -> Result<crate::memstore::EditResult> {
    let mut ops: Vec<Value> = qs.iter().map(|q| json!({"op": "append", "path": "questions", "value": q})).collect(); if !st.exists(questions::YAML) { ops.insert(0, json!({"op": "create", "value": {"questions": []}})); }
    let extra = questions::companion_after(st, &ops)?; st.edit_with(questions::YAML, &ops, &crate::memstore::EditOpts { action: "generate-questions", reason: Some(&format!("{} question(s) generated from the data", qs.len())), dry_run, ..Default::default() }, &extra)
}
