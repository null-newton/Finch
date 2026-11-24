//! The cards of the insights feed (anomalies, confirmed price changes, forecast risks, budgets over or at risk), shared by the web
//! app and the alert engine. Every figure comes from the analytics; this module only words them.
use crate::{analytics::{anomalies, planning, recurring::{self, Recurring}, Dataset, Scope}, util::*};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashSet;

fn iid(parts: &[&str]) -> String { format!("ins_{}", &sha1_hex(&parts.join("|"))[..10]) }
fn cents_of(v: &Value) -> i64 { v.as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents) }
pub fn cards(ds: &Dataset, rec: &Recurring, con: &Connection, sc: &Scope) -> anyhow::Result<(Vec<Value>, usize)> {
    let an = anomalies::detect(ds, sc, rec, &anomalies::dismissed_ids(con)?, false);
    let dis: HashSet<String> = con.prepare("SELECT id FROM price_change_dismissals")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>().map_err(anyhow::Error::from)?;
    let pcs = recurring::price_changes(ds, rec, None, false, &dis, false); let fc = planning::forecast(ds, rec, sc, 90, false); let bs = planning::status(ds, rec, None);
    let mut cards = vec![];
    for x in an["anomalies"].as_array().into_iter().flatten() {
        let title = match x["type"].as_str().unwrap_or("") { "category_spike" => "Spending spike", "duplicate_charge" => "Possible duplicate charge", "new_merchant" => "New merchant", "large_transaction" => "Large payment", t => t }.to_string();
        cards.push(json!({"id": x["id"], "kind": "anomaly", "subtype": x["type"], "severity": x["severity"], "title": title, "body": x["message"], "amount": x["amount"], "date": x["period"], "subject": x["subject"], "evidence": x["evidence"].as_array().map(|e| e.iter().take(20).cloned().collect::<Vec<_>>()), "persist": "anomaly"}));
    }
    for c in pcs["changes"].as_array().into_iter().flatten() {
        let up = c["direction"] == "increase"; let pct = c["pct"].as_f64().unwrap_or(0.0).abs();
        cards.push(json!({"id": c["id"], "kind": "price_change", "subtype": c["direction"], "severity": if up { "medium" } else { "low" }, "title": format!("{}: price {} {:.0}%", c["entity"].as_str().unwrap_or(""), if up { "up" } else { "down" }, pct * 100.0),
            "body": format!("{} -> {} EUR per {} payment, about {} EUR a year{}.", c["old"].as_str().unwrap_or(""), c["new"].as_str().unwrap_or(""), c["cadence"].as_str().unwrap_or(""), money_str(cents_of(&c["yearly_impact"]).abs()), if c["confirmed"] == true { "" } else { " (not yet confirmed by a second payment)" }),
            "amount": c["yearly_impact"], "date": c["date"], "subject": c["entity"], "evidence": [c["tx_key"].clone()], "persist": "price_change"}));
    }
    for acc in std::iter::once(&fc["household"]).chain(fc["accounts"].as_array().into_iter().flatten()) {
        let (neg, risk) = (acc["first_negative"].as_str(), acc["first_at_risk"].as_str()); let flags: Vec<String> = acc["flags"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
        if neg.is_none() && risk.is_none() && flags.is_empty() { continue; }
        let label = acc["label"].as_str().unwrap_or("Household"); let when = neg.or(risk);
        let lowest = acc["min_balance"].as_str().map(|m| format!("Lowest expected {m} EUR on {}.", acc["min_date"].as_str().unwrap_or(""))).unwrap_or_default();
        let body = match (neg, risk) { (Some(n), _) => format!("Projected below zero from {n}. {lowest}"), (_, Some(r)) => format!("Could run short around {r} (low end of the band). {lowest}"), _ => lowest };
        cards.push(json!({"id": iid(&["forecast", acc["account"].as_str().unwrap_or("household"), when.unwrap_or(""), &flags.join(",")]), "kind": "forecast", "subtype": if neg.is_some() { "negative" } else { "at_risk" }, "severity": if neg.is_some() { "high" } else { "medium" },
            "title": format!("{label}: balance {}", if neg.is_some() { "projected negative" } else { "at risk" }), "body": body, "amount": acc["min_balance"], "date": when, "subject": label, "evidence": [], "persist": "ui", "flags": flags}));
    }
    for p in bs["budgets"].as_array().into_iter().flatten().filter(|p| p["status"] == "over" || p["status"] == "at_risk") {
        let over = p["status"] == "over"; let g = |k: &str| p[k].as_str().unwrap_or("").to_string();
        cards.push(json!({"id": iid(&["budget", &g("id"), &g("month"), &g("status")]), "kind": "budget", "subtype": p["status"], "severity": if over { "high" } else { "medium" }, "title": format!("Budget {}: {}", g("target"), if over { "over" } else { "on track to overrun" }),
            "body": format!("{} EUR spent of {} EUR in {}; projected {} EUR by month end.", g("spent"), g("available"), g("month"), g("projected")), "amount": p["remaining"], "date": p["month"], "subject": p["target"], "evidence": p["evidence"], "persist": "ui"}));
    }
    let sev = |c: &Value| ["high", "medium", "low"].iter().position(|x| c["severity"] == *x).unwrap_or(3);
    cards.sort_by(|x, y| y["date"].as_str().unwrap_or("").cmp(x["date"].as_str().unwrap_or(""))); cards.sort_by_key(sev);
    Ok((cards, an["dismissed"].as_u64().unwrap_or(0) as usize + dis.len()))
}

/// The coach's stored answers and digests (newest first): (visible items, hidden count). Snoozed ones come back on their date.
pub fn coach_items(con: &Connection, today: chrono::NaiveDate, include_hidden: bool) -> anyhow::Result<(Vec<Value>, usize)> {
    let rows: Vec<Value> = con.prepare("SELECT id, created, kind, title, body, findings, evidence, skill, backend, model, usage_ref, status, snoozed_until, unverified_numbers, suspicious, question FROM insights ORDER BY created DESC LIMIT 200")?
        .query_map([], |r| { let j = |i: usize| -> rusqlite::Result<Value> { Ok(r.get::<_, Option<String>>(i)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!([]))) };
            Ok(json!({"id": r.get::<_, String>(0)?, "created": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "title": r.get::<_, String>(3)?, "body": r.get::<_, String>(4)?, "findings": j(5)?, "evidence": j(6)?, "skill": r.get::<_, Option<String>>(7)?, "backend": r.get::<_, Option<String>>(8)?, "model": r.get::<_, Option<String>>(9)?,
                "usage_ref": r.get::<_, Option<i64>>(10)?, "status": r.get::<_, String>(11)?, "snoozed_until": r.get::<_, Option<String>>(12)?, "unverified_numbers": j(13)?, "suspicious": r.get::<_, i64>(14)? != 0, "question": r.get::<_, Option<String>>(15)?, "ai_generated": true, "ai_label": "AI-generated: check the figures against the evidence", "ai_label_short": "AI-generated"})) })?.collect::<rusqlite::Result<_>>()?;
    let visible = |x: &Value| match x["status"].as_str() { Some("dismissed" | "done") => false, Some("snoozed") => x["snoozed_until"].as_str().and_then(crate::util::d).is_none_or(|d| d <= today), _ => true };
    let n = rows.len(); let items: Vec<Value> = rows.into_iter().filter(|x| include_hidden || visible(x)).collect(); let hidden = n - items.len(); Ok((items, hidden))
}
pub fn coach_set(con: &Connection, id: &str, action: &str, days: i64, today: chrono::NaiveDate) -> anyhow::Result<bool> {
    let (st, until) = match action { "read" => ("read", None), "done" => ("done", None), "dismiss" => ("dismissed", None), "restore" => ("new", None), "snooze" => ("snoozed", Some((today + chrono::Duration::days(days.clamp(1, 365))).to_string())), _ => anyhow::bail!("unknown action {action:?}") };
    Ok(con.execute("UPDATE insights SET status=?, snoozed_until=? WHERE id=?", rusqlite::params![st, until, id])? > 0)
}

/// `<data_dir>/ui-state.json` (owner-only), shared with the Python app: the cards the user snoozed (id -> until) or dismissed.
/// Presentation state only; anomalies and price changes are dismissed in the database.
pub struct UiState(pub std::path::PathBuf);
static UI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl UiState {
    pub fn new(cfg: &crate::config::Config) -> Self { Self(cfg.data_dir.join("ui-state.json")) }
    fn load(&self) -> Value { std::fs::read_to_string(&self.0).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).unwrap_or(json!({})) }
    fn update(&self, f: impl FnOnce(&mut Value)) -> anyhow::Result<()> {
        let _g = UI_LOCK.lock().unwrap_or_else(|e| e.into_inner()); let mut d = self.load(); f(&mut d);
        let mut m: Vec<(String, Value)> = d.as_object().cloned().unwrap_or_default().into_iter().collect(); m.sort_by(|x, y| x.0.cmp(&y.0));
        crate::memstore::atomic_write(&self.0, &serde_json::to_string_pretty(&m.into_iter().collect::<serde_json::Map<_, _>>())?)
    }
    pub fn snoozed(&self, today: chrono::NaiveDate) -> std::collections::BTreeMap<String, String> { let t = today.to_string(); self.load()["snoozed"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().filter(|v| *v >= t.as_str()).map(|v| (k.clone(), v.to_string()))).collect() }
    pub fn dismissed(&self) -> HashSet<String> { self.load()["dismissed"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() }
    pub fn snooze(&self, id: &str, until: chrono::NaiveDate) -> anyhow::Result<()> { self.update(|d| { if !d["snoozed"].is_object() { d["snoozed"] = json!({}); } d["snoozed"][id] = until.to_string().into(); }) }
    pub fn dismiss(&self, id: &str) -> anyhow::Result<()> { self.update(|d| { let mut s: Vec<String> = d["dismissed"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).chain([id.to_string()]).collect(); s.sort(); s.dedup(); d["dismissed"] = json!(s); }) }
    pub fn unsnooze(&self, id: &str) -> anyhow::Result<()> { self.update(|d| { if let Some(m) = d["snoozed"].as_object_mut() { m.remove(id); } let s: Vec<Value> = d["dismissed"].as_array().into_iter().flatten().filter(|x| *x != id).cloned().collect(); d["dismissed"] = s.into(); }) }
}
