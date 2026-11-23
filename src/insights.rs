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
