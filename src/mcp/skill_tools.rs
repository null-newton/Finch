//! MCP tools of the coach's skills (E7): the savings calculator, the monthly review, a spike explained, the subscription audit,
//! the onboarding checklist and why a transaction has its category. Numbers come from code; refs and pseudonyms only.
use crate::{analytics::{recurring::{self, Recurring}, review, Dataset}, classify::rules::{annotation_mismatch, resolve, Lookups, Rules}, config::Config, memstore::Store, privacy::{stable_hash, Redactor}, questions, subs_extra::saving_of, util::*};
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub const MARKET_MAX_AGE_DAYS: i64 = 30;
const SAVINGS_DISC: &str = "Estimate from the prices you gave, not a quote: the provider's offer decides. General information, not financial advice.";

/// Staleness of a dated market figure (a price, a rate): older than 30 days is not presented as current.
pub fn age(label: &str, day: Option<&str>, today: NaiveDate) -> Result<Value, String> {
    let Some(s) = day else { return Ok(json!({"dated": false, "warning": format!("the date of the {label} was not given: state its source and date, and treat it as possibly outdated")})) };
    let x = d(s).ok_or(format!("{s:?} is not a date (YYYY-MM-DD)"))?; let n = (today - x).num_days(); let mut o = json!({"dated": true, "date": x.to_string(), "age_days": n});
    if n > MARKET_MAX_AGE_DAYS { o["warning"] = format!("the {label} is {n} days old (more than {MARKET_MAX_AGE_DAYS}): do not present it as current").into(); } else if n < 0 { o["warning"] = format!("the date of the {label} is in the future").into(); }
    Ok(o)
}
/// What switching a recurring service to a cheaper offer saves, net of the switching costs (a pure calculator over the given prices).
pub fn savings_estimate(a: &Value, today: NaiveDate) -> Result<Value, String> {
    let c = |k: &str| to_cents(a[k].as_f64().unwrap_or(0.0)); let (cur, alt, sw, months) = (c("current_monthly"), c("alternative_monthly"), c("switching_costs"), a["months"].as_i64().unwrap_or(12));
    let (m, net, be) = saving_of(cur, alt, sw, months); let mut notes: Vec<String> = vec![];
    let verdict = if m <= 0 { notes.push("the alternative is not cheaper than the current price".into()); "no_saving" } else if net <= 0 { notes.push(format!("the switching costs are paid back after {} month(s); the period asked is {months}", be.unwrap_or(0))); "costs_exceed_saving_over_the_period" } else { "saving" };
    if sw > 0 && be.is_some_and(|b| b > months) { notes.push("break-even is later than the horizon: a longer horizon would turn the result positive".into()); }
    let quote = age("alternative's price", a["quote_date"].as_str(), today)?; if let Some(w) = quote["warning"].as_str() { notes.push(w.into()); }
    notes.push("a promotional first-year price is not modelled: give the full price, or call the tool twice (promo months, then full price)".into());
    Ok(json!({"months": months, "current_monthly": money_str(cur), "alternative_monthly": money_str(alt), "switching_costs": money_str(sw), "monthly_saving": money_str(m), "yearly_saving": money_str(m * 12), "gross_saving": money_str(m * months),
        "net_saving": money_str(net), "break_even_months": be, "verdict": verdict, "quote": quote, "notes": notes, "computed_by": "finch", "disclaimer": SAVINGS_DISC}))
}

fn ids(con: &Connection, sql: &str) -> HashSet<String> { con.prepare(sql).and_then(|mut s| s.query_map([], |r| r.get(0)).map(|i| i.flatten().collect())).unwrap_or_default() }
fn fail(name: &str) -> impl Fn(anyhow::Error) -> String + '_ { move |e| format!("{name} failed ({})", e.root_cause()) }

// ---------------------------------------------------------------- why a transaction has its category
const STEPS: [(&str, &str); 7] = [("override", "a per-transaction override"), ("transfer link", "the leg of an internal transfer"), ("type rule", "a rule on the transaction type"), ("user merchant label", "the user's own label for this merchant"),
    ("rule", "a merchant rule"), ("entity", "the canonical merchant's default category"), ("automatic label", "an automatic (model or similarity) label")];
/// The redacted decision chain of one transaction (any `h_` ref of the dataset): the classification steps in order, the memory annotations, the split and whose it is.
pub fn explain_transaction(ds: &Dataset, red: &Redactor, con: &Connection, cfg: &Config, a: &Value) -> Result<Value, String> {
    let rf = a["tx_ref"].as_str().unwrap_or(""); let f = fail("explain_transaction");
    let key = ds.txs.iter().map(|t| t.key.as_str()).find(|k| stable_hash(k) == rf).map(String::from).ok_or("unknown transaction ref: use a `ref` returned by transactions_search or an evidence list")?;
    type R = (Option<String>, Option<f64>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let (bdate, amount, ty, mkey, desc, op, purpose): R = con.query_row("SELECT t.booking_date, t.amount, e.tx_type, e.merchant_key, t.description, e.op_date, a.purpose FROM transactions t JOIN tx_enriched e USING(tx_key) LEFT JOIN accounts a ON a.uid=t.account_uid WHERE t.tx_key=?", [&key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).optional().map_err(|e| f(e.into()))?.ok_or("unknown transaction ref")?;
    let (bdate, amount, ty, mkey, desc) = (bdate.unwrap_or_default(), amount.unwrap_or(0.0), ty.unwrap_or_default(), mkey.unwrap_or_default(), desc.unwrap_or_default());
    let rules = Rules::load(cfg).map_err(&f)?; let lk = Lookups::load(con).map_err(&f)?; let m = lk.merchants.get(&mkey);
    let found: [(Option<String>, Option<&str>); 7] = [(lk.overrides.get(&key).cloned(), None), (lk.linked.contains(&key).then(|| "transfer.internal".to_string()), None),
        (rules.by_purpose.get(purpose.as_deref().unwrap_or("")).and_then(|d| d.get(&ty)).or_else(|| rules.type_rules.get(&ty)).cloned(), None), (m.filter(|x| x.1 == "user").map(|x| x.0.clone()), Some("user")),
        (rules.rule_category(&mkey, Some(if amount > 0.0 { "in" } else { "out" })).map(String::from), None), (lk.entity.get(&mkey).and_then(|(c, user, _)| c.clone().filter(|_| m.is_none() || *user)), None),
        (m.filter(|x| x.1 != "user").map(|x| x.0.clone()), m.map(|x| x.1.as_str()))];
    let first = found.iter().position(|x| x.0.is_some());
    let chain: Vec<Value> = STEPS.iter().zip(&found).enumerate().map(|(i, ((step, meaning), (c, src)))| json!({"step": step, "applies": c.is_some(), "decides": first == Some(i), "category": c, "source": c.as_ref().and(*src), "meaning": meaning})).collect();
    let (pre, src) = resolve(&lk, &rules, &key, Some(&ty), &mkey, purpose.as_deref(), Some(amount));
    let aliases: Vec<String> = con.prepare("SELECT old_key FROM tx_key_remap WHERE new_key=?").and_then(|mut s| s.query_map([&key], |r| r.get(0)).map(|i| i.flatten().collect())).unwrap_or_default();
    let anns = crate::memory::load_annotations(&cfg.memory_dir).0; let hit: Vec<&crate::memory::Annotation> = anns.iter().filter(|x| annotation_mismatch(x, &key, &mkey, &desc, &bdate, amount, &pre, op.as_deref(), &aliases).is_none()).collect();
    let parts: Vec<&crate::analytics::Tx> = ds.txs.iter().filter(|t| t.key == key).collect(); let t = parts[0]; let split = parts.len() > 1 || t.split_index.is_some();
    let attribution = if ds.people.is_empty() { json!({"person": null, "source": "none", "reason": "no household member is declared"}) } else { match crate::household::explain(con, &ds.people, &key) {
        Ok(x) => { let s = x["source"].as_str().unwrap_or("none"); json!({"person": x["person"].as_str().map(|p| super::people::fwd(red, p)), "source": s, "reason": match s { "manual" => "reassigned by the user", "rule" => "an attribution rule of the household memory", "account" => "owner of the account", _ => "the account has no known owner" }}) }
        Err(_) => json!({"person": null, "source": "none"}) } };
    let raw = json!({"ref": rf, "date": bdate, "amount": money_str(to_cents(amount)), "account": t.account, "account_purpose": purpose, "merchant": red.text(&t.entity), "tx_type": (!ty.is_empty()).then_some(ty), "decision_chain": chain,
        "pipeline": {"category": pre, "source": src, "consistent": first.map_or("other.uncategorized".to_string(), |i| found[i].0.clone().unwrap()) == pre},
        "memory_annotations": hit.iter().enumerate().map(|(i, x)| json!({"id": red.text(&x.id), "matched": true, "winner": i == 0, "sets_category": x.category.is_some(), "tags": x.tags})).collect::<Vec<_>>(),
        "split": split.then(|| parts.iter().map(|p| json!({"amount": money_str(p.amount_c), "category": p.category})).collect::<Vec<_>>()),
        "final": if split { json!({"category": "split", "source": "split", "tags": t.tags, "event": t.event.as_deref().map(|e| red.text(e))}) } else { json!({"category": t.category, "source": t.source, "tags": t.tags, "event": t.event.as_deref().map(|e| red.text(e))}) },
        "attribution": attribution, "how_to_change": "Only the user changes a category (the web app, `finch classify correct` or the memory); you may propose a memory annotation with memory_propose."});
    Ok(red.walk(&raw, None))
}

// ---------------------------------------------------------------- the month against usual, a spike explained
pub fn monthly_review(ds: &Dataset, red: &Redactor, con: &Connection, rec: &Recurring, a: &Value) -> Result<Value, String> {
    let raw = review::monthly_review(ds, a["month"].as_str(), rec, a["top"].as_u64().unwrap_or(8) as usize, &ids(con, "SELECT id FROM anomalies WHERE dismissed_at IS NOT NULL"), &ids(con, "SELECT id FROM price_change_dismissals"))?;
    let mut out = red.walk(&raw, None); out["note"] = "Numbers are computed from the data. 'usual' is the average of the earlier covered months without one-offs; movers carry evidence refs (transactions, rec_ series, anm_ anomalies, chg_ price changes).".into(); Ok(out)
}
pub fn explain_spike(ds: &Dataset, red: &Redactor, cfg: &Config, rec: &Recurring, a: &Value) -> Result<Value, String> {
    let (leaves, acct, target) = match (a["category"].as_str().map(|c| c.trim().trim_end_matches('.')), a["account"].as_str()) {
        (Some(c), None) => { let r = Rules::load(cfg).map_err(fail("explain_spike"))?; let l: Vec<String> = r.categories.keys().filter(|k| *k == c || k.starts_with(&format!("{c}."))).cloned().collect();
            if l.is_empty() { return Err("unknown category or group: use a category id or a group, e.g. 'food.groceries' or 'food'".into()); } (Some(l), None, c.to_string()) }
        (None, Some(p)) => (None, Some(red.account.iter().find(|(_, v)| *v == p).map(|(k, _)| k.clone()).ok_or("unknown account: use an account pseudonym from a tool result (for example from `coverage`)")?), p.to_string()),
        _ => return Err("give exactly one of category (a category or a group) or account".into()) };
    let raw = review::explain_spike(ds, leaves.as_deref(), acct.as_deref(), &target, a["month"].as_str(), rec, 8)?;
    let mut out = red.walk(&raw, None); out["scope"]["target"] = target.into();
    out["note"] = "Classes: one_off (tagged), recurring (a detected series), new_merchant (first payment ever that month), habitual. delta = month - usual per merchant.".into(); Ok(out)
}

// ---------------------------------------------------------------- the subscription audit
const DISCRETIONARY: (i64, i64) = (30, 100);
const COMPARE: [(&str, i64, i64); 3] = [("telecom", 10, 40), ("insurance", 5, 25), ("utilities", 5, 15)];
const OVERLAP: [&str; 5] = ["subscriptions.video_streaming", "subscriptions.music_streaming", "subscriptions.news_media", "subscriptions.software_cloud", "subscriptions.telecom"];
const HEURISTIC: &str = "savings ranges are fixed shares of the current yearly cost (subscriptions 30-100 %, telecom 10-40 %, insurance 5-25 %, energy and water 5-15 %), not market quotes";
/// Every active recurring cost by group (subs::inventory groups), price rises, duplicates and overlaps, contracts on file, unknown usage and ranked review candidates.
pub fn subscription_audit(ds: &Dataset, red: &Redactor, con: &Connection, cfg: &Config, rec: &Recurring, a: &Value) -> Result<Value, String> {
    let today = ds.today; let (lim, ilim) = (a["limit"].as_u64().unwrap_or(10) as usize, a["items_limit"].as_u64().unwrap_or(20) as usize);
    let mut inv = crate::subs::inventory(ds, rec, true, today); crate::subs_extra::enrich(&mut inv, con, rec, today).map_err(fail("subscription_audit"))?;
    let chg = recurring::price_changes(ds, rec, None, false, &ids(con, "SELECT id FROM price_change_dismissals"), false); let mut rises: BTreeMap<String, Value> = BTreeMap::new();
    for c in chg["changes"].as_array().into_iter().flatten().filter(|c| c["effect"] == "costs_more") { rises.entry(c["series_id"].as_str().unwrap_or("").to_string()).or_insert_with(|| c.clone()); }
    let have = Store::new(cfg, "coach-llm").map(|s| s.items(questions::YAML)).unwrap_or_default(); let asked: BTreeSet<String> = have.iter().flat_map(|q| [q["key"].as_str(), q["id"].as_str()]).flatten().map(String::from).collect();
    let cy = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents);
    struct It<'a> { v: Value, y: i64, x: &'a crate::analytics::recurring::Series, keep: Option<bool>, unknown: bool, on_file: bool, rise: Option<i64> }
    let (mut items, mut ended, mut other) = (vec![], vec![], 0);
    for r in inv["rows"].as_array().into_iter().flatten() {
        let Some(x) = r["series_id"].as_str().and_then(|s| rec.series.iter().find(|x| x.id == s)) else { continue }; let g = r["group"].as_str().unwrap_or("other"); if g == "other" { other += 1; continue; }
        if r["status"] == "ended" { ended.push(json!({"series_id": x.id, "entity": x.entity, "group": g, "yearly": r["yearly"], "last_payment": x.last_date.to_string()})); continue; }
        let pc = rises.get(&x.id); let keep = r["contract"]["keep"].as_bool(); let on_file = r["contract"]["status"] != "missing";
        let v = json!({"series_id": x.id, "entity": x.entity, "group": g, "category": x.category, "cadence": x.cadence, "expected": money_str(x.expected_c.abs()), "monthly": r["monthly"], "yearly": r["yearly"], "since": x.first_date.to_string(), "last_payment": x.last_date.to_string(), "account": x.account, "day_of_month": x.day_of_month,
            "contract": {"on_file": on_file, "keep": keep, "renewal": r["contract"]["renewal"], "commitment_end": r["contract"]["commitment_end"]}, "usage": if r["usage"]["recorded"] == true { "recorded" } else { "unknown" },
            "price_change": pc.map(|c| json!({"id": c["id"], "date": c["date"], "old": c["old"], "new": c["new"], "yearly_impact": c["yearly_impact"], "confirmed": c["confirmed"]}))});
        items.push(It { y: cy(&r["yearly"]), x, keep, unknown: r["usage"]["recorded"] != true, on_file, rise: pc.map(|c| cy(&c["yearly_impact"]).abs()), v });
    }
    items.sort_by(|a, b| (-a.y, &a.x.id).cmp(&(-b.y, &b.x.id)));
    let mut flags: Vec<Vec<&str>> = vec![vec![]; items.len()]; let (mut dups, mut overlaps) = (vec![], vec![]);
    for i in 0..items.len() { for j in i + 1..items.len() { let (xa, xb) = (items[i].x, items[j].x);
        let same_amt = (xa.expected_c.abs() - xb.expected_c.abs()).abs() <= 1 && xa.cadence == xb.cadence; let same_day = matches!((xa.day_of_month, xb.day_of_month), (Some(p), Some(q)) if p.abs_diff(q) <= 1);
        let kind = if same_amt && same_day && xa.expected_c.abs() >= 100 { "same_amount_same_day" } else if xa.key == xb.key && xa.account != xb.account { "same_merchant_two_accounts" } else { continue };
        dups.push(json!({"series_id": [xa.id, xb.id], "kind": kind, "amount": money_str(xa.expected_c.abs()), "cadence": xa.cadence, "yearly_each": money_str(xa.yearly_c.abs()), "merchants": [xa.entity, xb.entity]})); flags[i].push("duplicate_amount"); flags[j].push("duplicate_amount"); } }
    for cat in OVERLAP { let idx: Vec<usize> = (0..items.len()).filter(|i| items[*i].x.category == cat).collect(); let ents: BTreeSet<&str> = idx.iter().map(|i| items[*i].x.entity.as_str()).collect(); let info = cat == "subscriptions.telecom";
        if idx.len() >= 2 && (ents.len() >= 2 || info) { overlaps.push(json!({"category": cat, "series_id": idx.iter().map(|i| items[*i].x.id.clone()).collect::<Vec<_>>(), "merchants": ents, "combined_yearly": money_str(idx.iter().map(|i| items[*i].y).sum()), "dropping_the_cheapest_saves_yearly": money_str(idx.iter().map(|i| items[*i].y).min().unwrap_or(0)), "informational": info, "note": info.then_some("several lines can be legitimate (a family plan)")}));
            if !info { for i in idx { flags[i].push("overlap"); } } } }
    let mut cands: Vec<(i64, i64, Value)> = vec![];
    for (i, it) in items.iter().enumerate() {
        let g = it.v["group"].as_str().unwrap_or(""); let mut reasons: Vec<&str> = vec![]; for f in &flags[i] { if !reasons.contains(f) { reasons.push(f); } }
        let share = |p: i64| div_cents(it.y * p, 100); let (mut lo, mut hi) = (0, 0);
        if g == "subscriptions" && it.keep != Some(true) { (lo, hi) = (share(DISCRETIONARY.0), share(DISCRETIONARY.1)); if it.unknown && it.keep != Some(false) { reasons.insert(0, "usage_unknown"); } }
        else if let Some((_, l, h)) = COMPARE.iter().find(|c| c.0 == g) { (lo, hi) = (share(*l), share(*h)); reasons.push("compare_offers"); }
        if let Some(r) = it.rise { reasons.push("price_increase"); hi = hi.max(r); } if reasons.contains(&"duplicate_amount") { (lo, hi) = (0, hi.max(it.y)); }
        if it.keep == Some(false) { reasons.push("marked_to_cancel"); (lo, hi) = (it.y, it.y); } if !it.on_file { reasons.push("no_contract"); }
        if reasons.iter().all(|r| *r == "no_contract") { continue; }
        let step = ["marked_to_cancel", "duplicate_amount", "usage_unknown", "price_increase"].iter().zip(["stop_payment_marked_in_contract", "check_possible_duplicate", "ask_about_usage", "review_price_increase"]).find(|(r, _)| reasons.contains(r)).map_or("compare_offers", |x| x.1);
        cands.push((hi, lo, json!({"series_id": it.x.id, "entity": it.x.entity, "group": g, "category": it.x.category, "yearly": it.v["yearly"], "monthly": it.v["monthly"], "reasons": reasons, "next_step": step, "expected_yearly_savings": {"low": money_str(lo), "high": money_str(hi)}})));
    }
    cands.sort_by(|a, b| (-a.0, -a.1, a.2["series_id"].as_str()).cmp(&(-b.0, -b.1, b.2["series_id"].as_str())));
    let needs: Vec<Value> = cands.iter().filter(|c| c.2["reasons"].as_array().is_some_and(|r| r.iter().any(|x| x == "usage_unknown"))).filter(|c| { let s = c.2["series_id"].as_str().unwrap_or(""); rec.json["series"].as_array().into_iter().flatten().find(|x| x["id"] == s)
        .is_some_and(|x| { let q = crate::qgen::usage_question(x, false, None, today); !asked.contains(q["key"].as_str().unwrap_or("")) && !asked.contains(q["id"].as_str().unwrap_or("")) }) }).take(lim).map(|c| json!({"series_id": c.2["series_id"], "group": c.2["group"], "monthly": c.2["monthly"]})).collect();
    let mut groups: BTreeMap<&str, (usize, i64)> = BTreeMap::new(); for it in &items { let e = groups.entry(it.v["group"].as_str().unwrap_or("")).or_default(); e.0 += 1; e.1 += it.y; }
    let mut gs: Vec<(&str, (usize, i64))> = groups.into_iter().collect(); gs.sort_by_key(|g| -g.1 .1); ended.sort_by(|a, b| b["last_payment"].as_str().cmp(&a["last_payment"].as_str()));
    let total: i64 = items.iter().map(|i| i.y).sum();
    let raw = json!({"as_of": today.to_string(), "country": "BE", "totals": {"active_series": items.len(), "monthly": money_str(div_cents(total, 12)), "yearly": money_str(total), "not_audited_other": other},
        "groups": gs.iter().map(|(g, v)| (g.to_string(), json!({"count": v.0, "monthly": money_str(div_cents(v.1, 12)), "yearly": money_str(v.1)}))).collect::<serde_json::Map<_, _>>(),
        "items": items.iter().take(ilim).map(|i| i.v.clone()).collect::<Vec<_>>(), "items_not_listed": items.len().saturating_sub(ilim), "duplicates": dups, "overlaps": overlaps,
        "price_increases": items.iter().filter_map(|i| rises.get(&i.x.id).map(|c| json!({"series_id": i.x.id, "id": c["id"], "yearly_impact": c["yearly_impact"], "confirmed": c["confirmed"]}))).collect::<Vec<_>>(),
        "no_contract_on_file": {"count": items.iter().filter(|i| !i.on_file).count(), "series_id": items.iter().filter(|i| !i.on_file).map(|i| i.x.id.clone()).collect::<Vec<_>>()}, "candidates_total": cands.len(),
        "candidates_savings_range_sum": {"low": money_str(cands.iter().map(|c| c.1).sum()), "high": money_str(cands.iter().map(|c| c.0).sum()), "note": "upper-bound picture: the actions are alternatives, not a plan"},
        "candidates": cands.into_iter().take(lim).map(|c| c.2).collect::<Vec<_>>(), "usage_questions_needed": needs, "recently_ended": ended.into_iter().take(5).collect::<Vec<_>>(), "coverage": rec.json["coverage"],
        "notes": [HEURISTIC, "the household's use of a service is not in the bank data: unknown usage is asked, never guessed", "nothing is cancelled by the coach: review candidates only"],
        "next": "Ask the user about usage with `questions_propose` (series from usage_questions_needed); for telecom, insurance or energy a cheaper offer can be recorded with alternatives_record. Never tell the user to cancel: list review candidates."});
    Ok(red.walk(&raw, None))
}

// ---------------------------------------------------------------- the onboarding checklist
const MORTGAGE_FIELDS: [&str; 9] = ["lender", "principal", "start_date", "end_date", "rate.nominal", "monthly_payment", "outstanding", "outstanding_as_of", "insurance.monthly"];
const LOAN_FIELDS: [&str; 6] = ["lender", "start_date", "end_date", "monthly_payment", "outstanding", "outstanding_as_of"];
const LEASE_FIELDS: [&str; 7] = ["lender", "start_date", "end_date", "monthly_payment", "residual_value", "mileage_limit_km", "excess_km_fee"];
const LOAN_CATEGORIES: [&str; 6] = ["housing.mortgage", "housing.rental_property_loan", "transport.car_loan_lease", "debt.personal_loan", "debt.loan_repayment", "debt.bnpl"];
fn st(todo: usize, total: usize) -> &'static str { if todo == 0 { "done" } else if todo >= total { "todo" } else { "partial" } }
fn act(step: &str, what: &str, command: &str) -> Value { json!({"step": step, "note": what, "hint": command}) }
/// What the coach still does not know about the household, as an ordered checklist with the next actions (read-only).
pub fn onboarding_status(ds: &Dataset, red: &Redactor, cfg: &Config, rec: &Recurring) -> Result<Value, String> {
    let p = &ds.people; let fwd = |id: &str| super::people::fwd(red, id); let list = |f: &str| crate::memory::household_list(&ds.memory_dir, f).len(); let store = Store::new(cfg, "coach-llm").map_err(fail("onboarding_status"))?;
    let (mut steps, mut actions) = (vec![], vec![]); let prop = "a memory proposal (the coach proposes it, the user accepts it with `finch memory accept <id>`)";
    // household
    let (adults, kids) = (p.adults(), p.children()); let no_birth: Vec<String> = p.members.iter().filter(|m| m.birth_year.is_none()).map(|m| fwd(&m.id)).collect(); let (emp, places, schools) = (list("employers"), list("places"), list("schools"));
    let mut miss: Vec<String> = vec![]; if adults.is_empty() { miss.push("members: no adult declared".into()); actions.push(act("household", "add each member (the id is what the coach sees, the name stays local)", prop)); }
    if !no_birth.is_empty() { miss.push(format!("birth year of {} member(s) (members_without_birth_year)", no_birth.len())); }
    if rec.series.iter().any(|x| x.category == "income.salary") && emp == 0 { miss.push("employer not declared (it is masked in everything a model sees only when declared)".into()); actions.push(act("household", "declare the employer(s), places and schools in household.yaml (privacy declarations)", prop)); }
    if places == 0 { miss.push("towns / places not declared".into()); } if !kids.is_empty() && schools == 0 { miss.push("schools / nurseries not declared".into()); }
    steps.push(json!({"id": "household", "heading": "Household and privacy declarations", "status": st(miss.len(), if p.members.is_empty() { 1 } else { 4 }), "have": {"adults": adults.len(), "children": kids.len(), "employers": emp, "places": places, "schools": schools}, "missing": miss, "members_without_birth_year": {"members": no_birth}}));
    // accounts
    let unset: Vec<Value> = ds.accounts.values().filter(|a| a.owner.is_none() || a.purpose.is_none()).map(|a| json!({"account": a.uid, "missing": ([("owner", a.owner.is_none()), ("purpose", a.purpose.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect::<Vec<_>>())})).collect();
    if !unset.is_empty() { actions.push(act("accounts", "say whose each account is and what it is for", "finch accounts set <account> --owner <member-id|joint> --purpose main|cards|rental|kids|savings")); }
    steps.push(json!({"id": "accounts", "heading": "Accounts: owner and purpose", "status": st(unset.len(), ds.accounts.len().max(1)), "have": {"accounts": ds.accounts.len()}, "missing": unset}));
    // loans
    let (ls, _) = crate::loans::liabilities(&ds.memory_dir); let rxs: Vec<fancy_regex::Regex> = ls.iter().filter_map(|l| l.payment_match.as_deref()).filter_map(|x| fancy_regex::Regex::new(&format!("(?i){x}")).ok()).collect();
    let loans: Vec<Value> = ls.iter().enumerate().map(|(i, l)| { let v = serde_json::to_value(serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(ds.memory_dir.join(&l.file)).unwrap_or_default()).unwrap_or_default()).unwrap_or_default();
        let fields: &[&str] = match l.kind.as_str() { "mortgage" => &MORTGAGE_FIELDS, "loa" | "lld" => &LEASE_FIELDS, _ => &LOAN_FIELDS }; let mut m: Vec<&str> = fields.iter().copied().filter(|k| k.split('.').fold(&v, |c, p| &c[p]).is_null()).collect(); if l.payment_match.is_none() { m.push("payment_match"); }
        json!({"ref": format!("loan-{}", i + 1), "kind": l.kind, "missing": m, "matched_in_bank_data": !crate::loans::payments(ds, l).is_empty()}) }).collect();
    let orphans: Vec<String> = rec.series.iter().filter(|x| x.status == "active" && LOAN_CATEGORIES.contains(&x.category.as_str()) && !x.links.iter().any(|l| l.0 == "liability") && !rxs.iter().any(|r| r.is_match(&x.key).unwrap_or(false) || r.is_match(&x.entity).unwrap_or(false))).map(|x| x.id.clone()).collect();
    let todo = loans.iter().filter(|l| l["missing"].as_array().is_some_and(|m| !m.is_empty())).count() + orphans.len();
    if todo > 0 { actions.push(act("loans", "fill the empty loan fields from the loan contract or the latest statement (questions_propose with the loan-N refs asks the user); a loan payment without a file needs a liabilities/<id>.yaml", "finch loans list   (or attach the offer: finch memory doc add <file> --kind loan)")); }
    steps.push(json!({"id": "loans", "heading": "Loans and mortgage (the fields mortgage_check needs)", "status": st(todo, (loans.len() + orphans.len()).max(1)), "have": {"liabilities": loans.len()}, "liabilities": loans, "loan_payments_without_file": {"count": orphans.len(), "series_id": orphans.iter().take(10).collect::<Vec<_>>()}}));
    // contracts
    let cs = crate::subs::contracts(&ds.memory_dir); let no_file: Vec<&str> = rec.series.iter().filter(|x| x.missing_contract && x.status == "active").map(|x| x.id.as_str()).collect();
    let thin: Vec<Value> = cs.iter().filter_map(|c| { let m: Vec<&str> = [("provider", c.provider.is_none()), ("billing.amount", c.billing.as_ref().and_then(|b| b.amount).is_none()), ("renewal", c.renewal.is_none()), ("notice_period_days", c.notice_period_days.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect(); (!m.is_empty()).then(|| json!({"contract": red.text(&c.id), "missing": m})) }).collect();
    if !no_file.is_empty() || !thin.is_empty() { actions.push(act("contracts", "draft a contract file per recurring cost (contracts_draft PROPOSES one from a rec_ series), or attach the contract PDF", "finch subs draft <rec_id> --dry-run   (or: finch memory doc add <file> --kind contract)")); }
    steps.push(json!({"id": "contracts", "heading": "Contracts behind the recurring payments", "status": st(no_file.len() + thin.len(), (no_file.len() + cs.len()).max(1)), "have": {"contracts": cs.len()}, "recurring_without_contract": {"count": no_file.len(), "series_id": no_file.iter().take(15).collect::<Vec<_>>()}, "contracts_with_empty_fields": thin.into_iter().take(15).collect::<Vec<_>>()}));
    // preferences
    let lines = |f: &str| store.read_text(f).lines().filter(|l| { let t = l.trim(); !t.is_empty() && !t.starts_with('#') && !t.starts_with("<!--") && !t.starts_with('_') }).count(); let (prof, pref) = (lines("profile.md"), lines("preferences.md"));
    let mut mp: Vec<&str> = vec![]; if prof < 3 { mp.push("profile.md is empty: household background, goals, situation"); } if pref < 4 { mp.push("preferences.md has few coach rules: language, tone, topics to avoid"); }
    if !mp.is_empty() { actions.push(act("preferences", "write the coach rules (language, tone, goals)", prop)); }
    let goals = crate::memory::list_file::<crate::memory::Goal>(&ds.memory_dir, "goals.yaml", "goals").0.len();
    steps.push(json!({"id": "preferences", "heading": "Preferences and goals", "status": st(mp.len(), 2), "have": {"profile_lines": prof, "preference_lines": pref, "goals": goals}, "missing": mp}));
    // budgets
    let nb = crate::analytics::planning::budgets(ds).0.len(); let sug = crate::analytics::planning::suggest(ds, &Default::default(), None, Some(50))["suggestions"].as_array().map_or(0, |s| s.len());
    if nb == 0 && sug > 0 { actions.push(act("budgets", "start from the suggested budgets (the median of recent months); budget_suggestions lists them", "finch budgets suggest")); }
    steps.push(json!({"id": "budgets", "heading": "Budgets", "status": st(usize::from(nb == 0), 1), "have": {"budgets": nb, "suggestions_available": sug}, "missing": if nb == 0 { vec!["no budget yet"] } else { vec![] }}));
    // questions
    let open = questions::listing(&store, Some("open")).len();
    actions.push(if open > 0 { act("questions", "answer the open questions, biggest stake first (open_questions)", "finch questions list") } else { act("questions", "refresh the question list from the data", "finch questions generate --dry-run") });
    steps.push(json!({"id": "questions", "heading": "Open questions", "status": if open == 0 { "done" } else { "partial" }, "have": {"open": open}}));
    let done = steps.iter().filter(|s| s["status"] == "done").count(); let next = steps.iter().find(|s| s["status"] != "done").map(|s| s["id"].clone());
    let raw = json!({"as_of": ds.today.to_string(), "progress": {"done": done, "total": steps.len(), "next_step": next}, "steps": steps, "next_actions": actions,
        "notes": ["answers become memory PROPOSALS (memory_propose / questions_propose) that the user accepts themselves", "finish with `finch memory check` (0 errors)"], "note": "members, accounts and loans are pseudonyms (adult-1, account-main-1, loan-1): the user applies the exact id from their own memory"});
    Ok(red.walk(&raw, None))
}
