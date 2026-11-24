//! Subscriptions (E8), the user's side: alternative offers (sourced, dated; savings computed here), decisions (cancelled, switched,
//! downgraded, kept) checked against the bank data, cancellation letters filled locally from templates (nothing is sent), the
//! household's contact block (local only), and a contract file drafted from a recurring series.
use crate::{analytics::recurring::Recurring, memstore::{EditOpts, Store}, util::*};
use anyhow::{bail, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn rid(p: &str) -> String { format!("{p}_{:010x}", rand::random::<u64>() & 0xff_ffff_ffff) }
/// (contract id, series id) of an inventory ref: `rec_...`, `ctr_<contract>` or a contract id.
pub fn split_ref(r: &str) -> (Option<String>, Option<String>) { if r.starts_with("rec_") { (None, Some(r.into())) } else { (Some(r.strip_prefix("ctr_").unwrap_or(r).into()), None) } }

// ---------------------------------------------------------------- alternatives
pub fn record_alternative(con: &Connection, rf: &str, provider: &str, offer: &str, monthly: f64, switching: f64, features: Option<&str>, url: Option<&str>, retrieved: Option<NaiveDate>, method: &str, source: &str, today: NaiveDate) -> Result<String> {
    let (c, s) = split_ref(rf); if provider.trim().is_empty() || offer.trim().is_empty() { bail!("provider and offer_name are required"); } if monthly.is_nan() || monthly <= 0.0 { bail!("the monthly price must be positive"); } if switching < 0.0 { bail!("switching costs must not be negative"); }
    if let Some(u) = url { if !u.starts_with("https://") || url::Url::parse(u).is_err() { bail!("source_url must be an https:// address"); } } if method == "find-cheaper" && url.is_none() { bail!("an offer found on the web needs its source_url"); }
    let day = retrieved.unwrap_or(today); if day > today { bail!("retrieved_at cannot be in the future"); } let id = rid("alt");
    con.execute("INSERT INTO alternatives(id, contract_id, series_id, provider, offer_name, monthly_price_c, switching_costs_c, features, source_url, retrieved_at, method, notes, source, created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,NULL,?,?)",
        params![id, c, s, provider.trim(), offer.trim(), to_cents(monthly), to_cents(switching), features, url, day.to_string(), method, source, now_iso()])?; Ok(id)
}
pub fn delete_alternative(con: &Connection, id: &str) -> Result<bool> { Ok(con.execute("DELETE FROM alternatives WHERE id=?", [id])? > 0) }
/// The stored offers of a service with their savings against what it costs now (an offer older than 30 days is outdated).
pub fn alternatives_for(con: &Connection, contract: Option<&str>, series: Option<&str>, current_c: Option<i64>, today: NaiveDate) -> Result<Value> {
    let rows: Vec<Value> = con.prepare("SELECT id, provider, offer_name, monthly_price_c, switching_costs_c, features, source_url, retrieved_at, method, source FROM alternatives WHERE (contract_id=?1 AND ?1 IS NOT NULL) OR (series_id=?2 AND ?2 IS NOT NULL) ORDER BY monthly_price_c")?
        .query_map(params![contract, series], |r| { let (p, sw, ret): (i64, i64, String) = (r.get(3)?, r.get(4)?, r.get(7)?); let age = d(&ret).map_or(0, |x| (today - x).num_days()); let stale = age > 30;
            let sav = current_c.map(|c| { let m = c - p; let be = (m > 0).then(|| ((sw as f64) / m as f64).ceil() as i64); json!({"monthly": money_str(m), "yearly": money_str(m * 12), "net_12m": money_str(m * 12 - sw), "break_even_months": be, "verdict": if m * 12 - sw > 0 { "cheaper over 12 months" } else { "not cheaper over 12 months" }, "computed_by": "finch", "stale_warning": stale.then(|| format!("the price was seen {age} days ago: check it is still offered"))}) });
            Ok(json!({"id": r.get::<_, String>(0)?, "provider": r.get::<_, String>(1)?, "offer": r.get::<_, String>(2)?, "monthly_price": money_str(p), "switching_costs": money_str(sw), "features": r.get::<_, Option<String>>(5)?, "source_url": r.get::<_, Option<String>>(6)?, "retrieved_at": ret, "age_days": age,
                "method": r.get::<_, String>(8)?, "source": r.get::<_, String>(9)?, "status": if stale { "outdated" } else { "current" }, "label": format!("{} {}", r.get::<_, String>(1)?, r.get::<_, String>(2)?), "stale": stale, "savings": sav})) })?.collect::<rusqlite::Result<_>>()?;
    let best = rows.iter().filter(|x| x["stale"] == false).max_by_key(|x| x["savings"]["net_12m"].as_str().map_or(i64::MIN, |s| to_cents(s.parse().unwrap_or(0.0)))).cloned();
    Ok(json!({"count": rows.len(), "current": rows.iter().filter(|x| x["stale"] == false).count(), "outdated": rows.iter().filter(|x| x["stale"] == true).count(), "best": best, "items": rows, "note": "prices you recorded, with their date and source; finch computes the savings, nothing is switched"}))
}

// ---------------------------------------------------------------- decisions
pub fn record_decision(con: &Connection, rf: &str, name: Option<&str>, decision: &str, before: f64, after: f64, decided: Option<NaiveDate>, effective: Option<NaiveDate>, note: Option<&str>, source: &str, state: &str, today: NaiveDate, dry_run: bool) -> Result<Value> {
    let (c, s) = split_ref(rf); if !["cancelled", "renegotiated", "switched", "downgraded", "kept"].contains(&decision) { bail!("decision must be cancelled, renegotiated, switched, downgraded or kept"); }
    if before < 0.0 || after < 0.0 { bail!("amounts must not be negative"); } let dec = decided.unwrap_or(today); if dec > today { bail!("decided_on cannot be in the future"); }
    let out = json!({"valid": true, "monthly_saving": ((before - after) * 100.0).round() / 100.0}); if dry_run { return Ok(out); }
    let id = rid("dec"); con.execute("INSERT INTO decisions(id, contract_id, series_id, name, decision, decided_on, effective_on, before_monthly_c, after_monthly_c, note, source, state, created_at, confirmed_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![id, c, s, name, decision, dec.to_string(), effective.unwrap_or(dec).to_string(), to_cents(before), to_cents(after), note, source, state, now_iso(), (state == "confirmed").then(now_iso)])?;
    let mut o = out; o["id"] = id.into(); Ok(o)
}
pub fn set_decision(con: &Connection, id: &str, what: &str) -> Result<bool> {
    Ok(match what { "confirm" => con.execute("UPDATE decisions SET state='confirmed', confirmed_at=? WHERE id=? AND state='proposed'", params![now_iso(), id])?, "reject" => con.execute("UPDATE decisions SET state='rejected' WHERE id=? AND state='proposed'", [id])?, "delete" => con.execute("DELETE FROM decisions WHERE id=?", [id])?, _ => bail!("unknown action") } > 0)
}
/// A decision checked against the bank data: a cancelled service still charged after its effective date (+ a cycle) is contradicted.
fn check(d: &Value, rec: &Recurring, today: NaiveDate) -> Value {
    let eff = d["effective_on"].as_str().and_then(crate::util::d).unwrap_or(today); let x = d["series_id"].as_str().and_then(|s| rec.json["series"].as_array().into_iter().flatten().find(|x| x["id"] == s));
    let (b, a) = (d["before_monthly_c"].as_i64().unwrap_or(0), d["after_monthly_c"].as_i64().unwrap_or(0)); let save = b - a; let grace = eff + chrono::Duration::days(35);
    let months = if today > eff { ((today - eff).num_days() / 30).max(0) } else { 0 };
    let (st, why) = if d["decision"] == "kept" { ("not_applicable", "kept: no saving to check".to_string()) } else if let Some(x) = x {
        let after: Vec<i64> = x["occurrences"].as_array().into_iter().flatten().filter(|o| o["date"].as_str().and_then(crate::util::d).is_some_and(|dd| dd > grace)).filter_map(|o| o["amount"].as_str().and_then(|s| s.parse::<f64>().ok())).map(|v| to_cents(v).abs()).collect();
        if today <= grace { ("pending", format!("checked after {grace} (one billing cycle after the effective date)")) }
        else if after.is_empty() { if a == 0 { ("verified", "no charge since the effective date".into()) } else { ("pending", "the new, lower charge has not been seen yet".into()) } }
        else if a == 0 { ("contradicted", "still charged after the cancellation".into()) } else if after.iter().all(|v| (*v - a).abs() <= 100.max(a / 20)) { ("verified", "the new amount is charged".into()) } else { ("contradicted", "the charge did not drop as expected".into()) }
    } else { ("ambiguous", "no recurring series to check it against".into()) };
    json!({"id": d["id"], "decision": d["decision"], "decided_on": d["decided_on"], "effective_on": d["effective_on"], "before_monthly": money_str(b), "after_monthly": money_str(a), "monthly_saving": money_str(save), "months_counted": if st == "verified" { months } else { 0 },
        "since_decision": money_str(if st == "verified" { save * months } else { 0 }), "note": d["note"], "source": d["source"], "status": st, "reason": why, "check_on": (st == "pending").then(|| grace.to_string()), "reminder": st == "contradicted",
        "contract": d["contract_id"], "series": d["series_id"], "name": d["name"], "state": d["state"]})
}
pub fn decisions(con: &Connection, rec: &Recurring, today: NaiveDate) -> Result<Vec<Value>> {
    Ok(con.prepare("SELECT id, contract_id, series_id, name, decision, decided_on, effective_on, before_monthly_c, after_monthly_c, note, source, state FROM decisions ORDER BY decided_on DESC")?.query_map([], |r| Ok(json!({"id": r.get::<_, String>(0)?, "contract_id": r.get::<_, Option<String>>(1)?, "series_id": r.get::<_, Option<String>>(2)?, "name": r.get::<_, Option<String>>(3)?, "decision": r.get::<_, String>(4)?, "decided_on": r.get::<_, String>(5)?, "effective_on": r.get::<_, Option<String>>(6)?, "before_monthly_c": r.get::<_, i64>(7)?, "after_monthly_c": r.get::<_, i64>(8)?, "note": r.get::<_, Option<String>>(9)?, "source": r.get::<_, String>(10)?, "state": r.get::<_, String>(11)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?.iter().map(|d| check(d, rec, today)).collect())
}
pub fn savings(con: &Connection, rec: &Recurring, today: NaiveDate) -> Result<Value> {
    let all = decisions(con, rec, today)?; let conf: Vec<&Value> = all.iter().filter(|d| d["state"] == "confirmed").collect(); let c = |v: &Value, k: &str| to_cents(v[k].as_str().unwrap_or("0").parse().unwrap_or(0.0));
    let ver: Vec<&&Value> = conf.iter().filter(|d| d["status"] == "verified").collect(); let monthly: i64 = ver.iter().map(|d| c(d, "monthly_saving")).sum();
    Ok(json!({"as_of": today.to_string(), "realised_monthly": money_str(monthly), "realised_since_decisions": money_str(ver.iter().map(|d| c(d, "since_decision")).sum()), "realised_yearly_run_rate": money_str(monthly * 12), "verified": ver.len(), "pending": conf.iter().filter(|d| d["status"] == "pending").count(), "contradicted": conf.iter().filter(|d| d["status"] == "contradicted").count(),
        "claimed_monthly_unverified": money_str(conf.iter().filter(|d| d["status"] != "verified" && d["status"] != "not_applicable").map(|d| c(d, "monthly_saving")).sum()), "decisions": conf, "proposed": all.iter().filter(|d| d["state"] == "proposed").map(|d| json!({"id": d["id"], "decision": d["decision"], "name": d["name"], "source": d["source"], "before_monthly": c(d, "before_monthly") as f64 / 100.0, "after_monthly": c(d, "after_monthly") as f64 / 100.0, "note": d["note"]})).collect::<Vec<_>>(),
        "reminders": conf.iter().filter(|d| d["status"] == "contradicted").map(|d| format!("{} is still charged after it was {}", d["name"].as_str().unwrap_or("a service"), d["decision"].as_str().unwrap_or(""))).collect::<Vec<_>>(), "note": "a saving counts once the bank data confirms it"}))
}

// ---------------------------------------------------------------- contact, letters, drafts
pub fn contact(st: &Store) -> Value { let h = crate::memstore::jsonify(&st.load("household.yaml").unwrap_or_default()); let c = h["contact"].clone(); json!({"contact": if c.is_object() { c.clone() } else { json!({}) }, "set": c.as_object().is_some_and(|o| !o.is_empty()), "local_only": true,
    "members": h["members"].as_array().into_iter().flatten().map(|m| json!({"id": m["id"], "name": m["name"]})).collect::<Vec<_>>()}) }
pub fn set_contact(st: &Store, address: Option<&str>, email: Option<&str>, phone: Option<&str>, dry_run: bool) -> Result<crate::memstore::EditResult> {
    let mut c = serde_json::Map::new(); for (k, v) in [("address", address), ("email", email), ("phone", phone)] { if let Some(v) = v.filter(|v| !v.trim().is_empty()) { c.insert(k.into(), v.trim().into()); } }
    if let Some(e) = c.get("email").and_then(|v| v.as_str()) { if !crate::rs!(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").is_match(e) { bail!("that is not an e-mail address"); } }
    let mut ops = vec![]; if !st.exists("household.yaml") { ops.push(json!({"op": "create", "value": {"members": []}})); } ops.push(if c.is_empty() { json!({"op": "unset", "path": "contact"}) } else { json!({"op": "set", "path": "contact", "value": c}) });
    st.edit("household.yaml", &ops, &EditOpts { action: "set-contact", reason: Some("contact details for letters"), dry_run, ..Default::default() })
}
/// A cancellation letter or e-mail (Dutch, French or English), filled from the contract and the contact block; placeholders mark what is missing.
pub fn letter(st: &Store, c: &crate::subs::Contract, lang: &str, channel: &str, today: NaiveDate) -> Value {
    let rule = crate::subs::cancellability(c, "", None, today); let ct = contact(st); let who = c.holder.as_deref().and_then(|h| ct["members"].as_array().and_then(|m| m.iter().find(|x| x["id"] == h)).and_then(|m| m["name"].as_str().map(String::from)));
    let mut ph = vec![]; let mut v = |x: Option<String>, p: &str| x.unwrap_or_else(|| { ph.push(p.to_string()); format!("[{p}]") });
    let (name, addr, num, prov) = (v(who, "your name"), v(ct["contact"]["address"].as_str().map(String::from), "your address"), v(c.contract_number.clone(), "contract number"), v(c.provider.clone(), "provider"));
    let eff = rule["earliest_effective_date"].as_str().map(String::from); let law = rule["legal_basis"][0]["law"].as_str().unwrap_or("");
    let (subject, body) = match lang {
        "nl" => (format!("Opzegging contract {num}"), format!("{name}\n{addr}\n\nAan {prov}\n\n{}\n\nBetreft: opzegging van contract {num}\n\nGeachte heer, mevrouw,\n\nHierbij zeg ik het contract {num} op{}.\nWilt u de opzegging en de einddatum schriftelijk bevestigen?\n\nMet vriendelijke groeten,\n\n{name}", today, eff.as_ref().map(|e| format!(" met ingang van {e} of de vroegst mogelijke datum ({law})")).unwrap_or_default())),
        "fr" => (format!("Résiliation du contrat {num}"), format!("{name}\n{addr}\n\nÀ {prov}\n\n{}\n\nObjet : résiliation du contrat {num}\n\nMadame, Monsieur,\n\nPar la présente, je résilie le contrat {num}{}.\nPouvez-vous me confirmer par écrit la résiliation et sa date de prise d'effet ?\n\nVeuillez agréer mes salutations distinguées,\n\n{name}", today, eff.as_ref().map(|e| format!(" à partir du {e} ou de la première date possible ({law})")).unwrap_or_default())),
        _ => (format!("Termination of contract {num}"), format!("{name}\n{addr}\n\nTo {prov}\n\n{}\n\nSubject: termination of contract {num}\n\nDear Sir or Madam,\n\nI hereby terminate contract {num}{}.\nPlease confirm the termination and its end date in writing.\n\nKind regards,\n\n{name}", today, eff.as_ref().map(|e| format!(" with effect from {e} or the earliest possible date ({law})")).unwrap_or_default())),
    };
    json!({"contract": c.id, "lang": lang, "channel": channel, "subject": subject, "text": body, "filename": format!("termination-{}-{today}.txt", c.id), "placeholders": ph, "legal_basis": rule["legal_basis"], "can_cancel_now": rule["can_cancel_now"], "send_on_or_after": today.to_string(), "send_by": rule["send_notice_by"],
        "notes": [if channel == "lrar" { "for an insurance contract, send it by registered letter (aangetekend / recommandé)" } else { "keep a copy and the provider's confirmation" }], "pdf": null, "pdf_note": "print it from the text; nothing is sent by finch", "sent": false, "country": "BE"})
}
/// A contract file drafted from a recurring series: (file, id, value, kind, provider, yearly).
fn draft_value(st: &Store, rec: &Recurring, series: &str) -> Result<(String, String, Value, String, String, Value)> {
    let x = rec.json["series"].as_array().into_iter().flatten().find(|x| x["id"] == series).ok_or_else(|| anyhow::anyhow!("no such recurring series"))?; let (key, cat, ent) = (x["key"].as_str().unwrap_or(""), x["category"].as_str().unwrap_or(""), x["entity"].as_str().unwrap_or(""));
    let base: String = crate::rs!(r"[^a-z0-9]+").replace_all(&ent.to_lowercase(), "-").trim_matches('-').chars().take(40).collect(); let base = if base.is_empty() { "contract".to_string() } else { base };
    let id = std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find(|i| !st.exists(&format!("contracts/{i}.yaml"))).unwrap();
    let kind = if cat.contains("telecom") { "telecom" } else if cat.contains("energy") { "energy" } else if cat.contains("water") { "water" } else if cat.starts_with("insurance.") { if cat.contains("car") { "insurance_car" } else if cat.contains("health") { "insurance_health" } else if cat.contains("home") { "insurance_home" } else { "insurance_other" } } else if cat.contains("stream") { "streaming" } else if cat.contains("software") { "software" } else if cat.starts_with("subscriptions.") { "membership" } else { "other" };
    let period = x["cadence"].as_str().filter(|p| ["monthly", "bimonthly", "quarterly", "yearly"].contains(p)).unwrap_or("monthly"); let amount = x["expected_amount"].as_str().and_then(|s| s.parse::<f64>().ok()).map(f64::abs);
    let value = json!({"id": id, "provider": ent, "kind": kind, "merchant_match": format!("^{}$", regex::escape(key)), "start_date": x["first_date"].as_str().or(x["occurrences"][0]["date"].as_str()), "billing": {"amount": amount, "period": period}, "renewal": null, "notice_period_days": null, "documents": [], "notes": ""});
    Ok((format!("contracts/{id}.yaml"), id, value, kind.into(), ent.into(), x["yearly_cost"].clone()))
}
fn draft_meta(series: &str, d: &(String, String, Value, String, String, Value)) -> Value { json!({"series": series, "contract_id": d.1, "file": d.0, "kind": d.3, "provider": d.4, "missing": ["renewal", "notice_period_days", "contract_number"], "yearly": d.5, "warnings": [], "value": d.2}) }
pub fn draft(st: &Store, rec: &Recurring, series: &str, dry_run: bool) -> Result<Value> {
    let dv = draft_value(st, rec, series)?; let res = st.edit(&dv.0, &[json!({"op": "create", "value": dv.2})], &EditOpts { action: "draft-contract", reason: Some(&format!("contract drafted from series {series}")), detail: Some(&dv.1), dry_run, ..Default::default() })?;
    Ok(json!({"dry_run": dry_run, "changed": res.changed(), "diff": res.diff, "change_id": res.change_id, "warnings": res.warnings, "contract": draft_meta(series, &dv)}))
}
/// The same draft as a sealed memory proposal (for the coach): nothing is written until the user accepts it.
pub fn draft_propose(st: &Store, rec: &Recurring, series: &str, source: &str) -> Result<Value> {
    let dv = draft_value(st, rec, series)?; let p = crate::proposals::create(st, &dv.0, &[json!({"op": "create", "value": dv.2})], &format!("contract drafted from series {series}"), source, vec![])?;
    Ok(json!({"proposal_id": p.id, "status": "pending", "contract": draft_meta(series, &dv)}))
}
/// Fills the inventory rows with their stored alternatives and decisions, and the totals with the verified savings.
pub fn enrich(inv: &mut Value, con: &Connection, rec: &Recurring, today: NaiveDate) -> Result<()> {
    let ds = decisions(con, rec, today)?; let (mut out, mut rem) = (0, 0);
    for r in inv["rows"].as_array_mut().into_iter().flatten() {
        let (c, s) = (r["contract_id"].as_str().map(String::from), r["series_id"].as_str().map(String::from)); let cur = r["monthly"].as_str().and_then(|m| m.parse::<f64>().ok()).map(to_cents);
        let alt = alternatives_for(con, c.as_deref(), s.as_deref(), cur, today)?; out += alt["outdated"].as_u64().unwrap_or(0); r["alternatives"] = alt;
        let mine: Vec<&Value> = ds.iter().filter(|d| (c.is_some() && d["contract"].as_str() == c.as_deref()) || (s.is_some() && d["series"].as_str() == s.as_deref())).collect();
        if let Some(d) = mine.iter().find(|d| d["state"] == "confirmed") { if d["status"] == "contradicted" { rem += 1; } r["decision"] = (*d).clone(); }
        r["proposed_decisions"] = mine.iter().filter(|d| d["state"] == "proposed").map(|d| (*d).clone()).collect::<Vec<_>>().into();
    }
    let sv = savings(con, rec, today)?; inv["totals"]["outdated_alternatives"] = out.into(); inv["totals"]["reminders"] = rem.into();
    inv["savings"] = json!({"realised_monthly": sv["realised_monthly"], "realised_since_decisions": sv["realised_since_decisions"], "realised_yearly_run_rate": sv["realised_yearly_run_rate"], "verified": sv["verified"], "pending": sv["pending"], "contradicted": sv["contradicted"], "claimed_monthly_unverified": sv["claimed_monthly_unverified"]});
    inv["notes"] = json!(["Cancellation rules: Belgian consumer law, general information. Alternatives are the prices you recorded; a saving counts once the bank data confirms it."]); Ok(())
}
pub fn alternative_exists(con: &Connection, id: &str) -> bool { con.query_row("SELECT 1 FROM alternatives WHERE id=?", [id], |_| Ok(())).optional().ok().flatten().is_some() }
