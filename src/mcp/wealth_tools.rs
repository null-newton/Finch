//! MCP tools of phase 5c: net worth, loans, rental property, subscriptions and cancellability. Loans and assets reach a model by KIND
//! and a ref (`asset-1`, `loan-1`) only: never a lender, a provider of an asset, a description, a file name or a contract number.
use crate::{analytics::{recurring::Recurring, Dataset}, loans, privacy::Redactor, rental, subs};
use serde_json::{json, Value};

fn refs(ds: &Dataset) -> (Vec<(String, String)>, Vec<(String, String)>) {
    (loans::assets(&ds.memory_dir).iter().enumerate().map(|(i, a)| (a.id.clone(), format!("asset-{}", i + 1))).collect(), loans::liabilities(&ds.memory_dir).0.iter().enumerate().map(|(i, l)| (l.id.clone(), format!("loan-{}", i + 1))).collect())
}
fn rf(map: &[(String, String)], id: &str) -> String { map.iter().find(|x| x.0 == id).map_or("[item]".into(), |x| x.1.clone()) }
fn back(map: &[(String, String)], r: &str) -> Option<String> { map.iter().find(|x| x.1 == r).map(|x| x.0.clone()) }
const NOTE: &str = "Items are given by kind and ref only. An item listed under `unknown` has no recorded figure: never estimate it; say it is missing and offer to propose it (memory_propose).";

pub fn net_worth(ds: &Dataset, red: &Redactor, con: &rusqlite::Connection, a: &Value) -> Result<Value, String> {
    let (am, lm) = refs(ds); let v = loans::net_worth(ds, ds.today, ds.settings.asset_stale_months as i32);
    let raw = json!({"as_of": v["as_of"], "net_worth": v["net_worth"], "complete": v["complete"], "n_unknown": v["n_unknown"], "by_category": v["by_category"], "by_owner": v["by_owner"], "bank_total": v["bank"]["total"],
        "assets": v["assets"]["items"].as_array().into_iter().flatten().map(|a| json!({"ref": rf(&am, a["id"].as_str().unwrap_or("")), "kind": a["kind"], "category": a["category"], "value": a["value"], "as_of": a["as_of"], "stale": a["stale"], "counted": a["counted"]})).collect::<Vec<_>>(),
        "liabilities": v["liabilities"]["items"].as_array().into_iter().flatten().map(|l| json!({"ref": rf(&lm, l["id"].as_str().unwrap_or("")), "kind": l["kind"], "outstanding": l["outstanding"], "source": l["source"], "stale": l["stale"], "counted": l["counted"]})).collect::<Vec<_>>(),
        "unknown": v["unknown"].as_array().into_iter().flatten().map(|u| { let k = u["kind"].as_str().unwrap_or(""); json!({"type": k, "ref": match k { "asset" => rf(&am, u["id"].as_str().unwrap_or("")), "liability" => rf(&lm, u["id"].as_str().unwrap_or("")), _ => "account".into() }, "reason": u["reason"]}) }).collect::<Vec<_>>(), "note": NOTE});
    let mut out = red.walk(&raw, None);
    if a["history"] == true { let h = crate::loans_extra::history(con, a["months"].as_i64().unwrap_or(24)).map_err(|e| format!("net_worth failed ({})", e.root_cause()))?;
        out["history"] = h.iter().map(|p| json!({"month": p["month"], "as_of": p["as_of"], "source": p["source"], "net_worth": p["net_worth"], "assets": p["assets"], "liabilities": p["liabilities"], "by_category": p["by_category"], "n_unknown": p["n_unknown"], "complete": p["complete"]})).collect::<Vec<_>>().into();
        out["history_note"] = "one point per month: the latest snapshot recorded that month (`finch loans snapshot`); a month with n_unknown > 0 is the known part only. Nothing is recorded by this tool.".into(); }
    Ok(out)
}
pub fn loans_overview(ds: &Dataset, red: &Redactor, a: &Value) -> Result<Value, String> {
    let (_, lm) = refs(ds); let only = a["loan"].as_str().map(|r| back(&lm, r).ok_or("unknown loan: use a ref from net_worth or loans_overview (loan-N)")).transpose()?;
    let amount = a["prepayment"].as_f64().map(loans::cents);
    let out: Vec<Value> = loans::liabilities(&ds.memory_dir).0.iter().filter(|l| only.as_ref().is_none_or(|o| *o == l.id)).map(|l| { let s = loans::compute(l, ds.today); let j = s.to_json(false); let e = loans::early_repayment(l, &s, amount, ds.today);
        json!({"ref": rf(&lm, &l.id), "kind": l.kind, "status": j["status"], "mode": j["mode"], "missing": j["missing"], "rate": l.rate.as_ref().map(|r| json!({"type": r.ty, "nominal": r.nominal, "monthly_convention": r.monthly_convention.clone().unwrap_or_else(|| "equivalent".into())})),
            "payment": j["payment"], "remaining_capital": j["remaining_capital"].as_str().map(String::from).or(l.outstanding.map(|o| crate::util::money_str(loans::cents(o)))), "next_due": j["next_due"], "last_due": j["last_due"], "remaining_instalments": j["remaining_instalments"], "remaining_interest": j["remaining_interest"],
            "by_year": j["by_year"], "payment_check": j["payment_check"], "assumptions": j["assumptions"], "early_repayment": {"amount": e["amount"], "rule": e["rule"], "legal_basis": e["legal_basis"], "max_indemnity": e["max_indemnity"]}}) }).collect();
    Ok(red.walk(&json!({"as_of": ds.today.to_string(), "loans": out, "note": NOTE, "disclaimer": "Estimates from the figures recorded, not an offer: the lender gives the binding numbers. This is general information, not financial advice."}), None))
}
pub fn rental_overview(ds: &Dataset, red: &Redactor) -> Value {
    let (am, _) = refs(ds);
    let props: Vec<Value> = rental::properties(ds).iter().map(|a| { let lk = rental::links(ds, a); let cf = rental::cashflow(ds, a, &lk, 13); let ix = rental::indexation(a);
        json!({"ref": rf(&am, &a.id), "kind": "rental property", "account_linked": !lk.accounts.is_empty(), "loans_linked": lk.loans.len(), "expected_rent": cf["rent"]["expected"], "rent_source": cf["rent"]["source"], "vacancy": cf["vacancy"], "average": cf["average"],
            "months": cf["months"].as_array().into_iter().flatten().map(|m| json!({"month": m["month"], "rent": m["rent"], "costs": m["costs"], "net": m["net"], "effort": m["effort"], "rent_status": m["rent_status"], "complete": m["complete"]})).collect::<Vec<_>>(),
            "indexation": {"status": ix["status"], "indexed_rent": ix["indexed_rent"], "change": ix["change"], "missing": ix["missing"], "rules": ix["rules"]}, "pnl_this_year": rental::pnl(&cf, chrono::Datelike::year(&ds.today), ds.today)["totals"]}) }).collect();
    red.walk(&json!({"as_of": ds.today.to_string(), "properties": props, "tax": rental::tax(chrono::Datelike::year(&ds.today))["note"], "note": "A property reaches you as asset-N: never an address, a tenant or a lender. Every index or rent is the owner's own figure: never look one up and never estimate a missing one."}), None)
}
pub fn subscriptions(ds: &Dataset, rec: &Recurring, red: &Redactor, con: &rusqlite::Connection, a: &Value) -> Value {
    let mut v = subs::inventory(ds, rec, a["include_ended"].as_bool().unwrap_or(false), ds.today); let _ = crate::subs_extra::enrich(&mut v, con, rec, ds.today);
    let rows: Vec<Value> = v["rows"].as_array().into_iter().flatten().map(|r| json!({"ref": r["ref"], "name": r["name"], "group": r["group"], "category": r["category"], "cadence": r["cadence"], "monthly": r["monthly"], "yearly": r["yearly"], "status": r["status"], "last_payment": r["last_payment"], "next_charge": r["next_charge"],
        "price_changes": r["price_changes"], "contract": {"status": r["contract"]["status"], "id": r["contract"]["id"], "renewal": r["contract"]["renewal"], "commitment_end": r["contract"]["commitment_end"]}, "usage": r["usage"]["frequency"],
        "alternatives": {"count": r["alternatives"]["count"], "outdated": r["alternatives"]["outdated"], "best": r["alternatives"]["best"].get("savings").map(|s| json!({"offer": r["alternatives"]["best"]["label"], "monthly_price": r["alternatives"]["best"]["monthly_price"], "retrieved_at": r["alternatives"]["best"]["retrieved_at"], "net_12m": s["net_12m"]}))}, "decision": r["decision"].get("status").map(|_| json!({"decision": r["decision"]["decision"], "status": r["decision"]["status"]})),
        "cancellation": {"family": r["cancellation"]["family"], "can_cancel_now": r["cancellation"]["can_cancel_now"], "earliest_effective_date": r["cancellation"]["earliest_effective_date"], "send_notice_by": r["cancellation"]["send_notice_by"]}})).collect();
    red.walk(&json!({"as_of": v["as_of"], "country": "BE", "rows": rows, "groups": v["groups"], "totals": v["totals"], "savings": v["savings"], "note": "Recurring costs and their contracts; usage is what the user states (the bank cannot tell). Nothing is ever cancelled: the user decides."}), None)
}
pub fn cancellability(ds: &Dataset, rec: &Recurring, red: &Redactor, a: &Value) -> Result<Value, String> {
    let r = a["ref"].as_str().unwrap_or(""); let v = subs::inventory(ds, rec, true, ds.today);
    let row = v["rows"].as_array().into_iter().flatten().find(|x| x["ref"] == r || x["contract_id"] == r).ok_or("unknown ref: use a `ref` or contract id from subscriptions_inventory")?;
    let mut c = row["cancellation"].clone(); if let Some(o) = c.as_object_mut() { o.remove("first_request_date"); }
    Ok(red.walk(&json!({"ref": row["ref"], "name": row["name"], "cancellation": c, "contract_on_file": row["contract"]["status"] != "missing"}), None))
}
fn row_of(ds: &Dataset, rec: &Recurring, rf: &str) -> Result<Value, String> { subs::inventory(ds, rec, true, ds.today)["rows"].as_array().into_iter().flatten().find(|x| x["ref"] == rf || x["contract_id"] == rf).cloned().ok_or("unknown ref: use a `ref` from subscriptions_inventory".into()) }
fn canon(row: &Value) -> String { row["series_id"].as_str().map(String::from).or(row["contract_id"].as_str().map(|c| format!("ctr_{c}"))).unwrap_or_default() }
/// Stores an offer found on the web (https source, the date it was seen); finch computes the savings.
pub fn alternatives_record(ds: &Dataset, rec: &Recurring, red: &Redactor, con: &rusqlite::Connection, a: &Value) -> Result<Value, String> {
    let row = row_of(ds, rec, a["ref"].as_str().unwrap_or(""))?; let ret = a["retrieved_at"].as_str().and_then(crate::util::d).ok_or("retrieved_at: the date the price was seen (YYYY-MM-DD)")?;
    let id = crate::subs_extra::record_alternative(con, &canon(&row), a["provider"].as_str().unwrap_or(""), a["offer_name"].as_str().unwrap_or(""), a["monthly_price"].as_f64().unwrap_or(0.0), a["switching_costs"].as_f64().unwrap_or(0.0), a["features"].as_str(), a["source_url"].as_str(), Some(ret), "find-cheaper", "coach-llm", ds.today).map_err(|e| format!("not recorded: {e}"))?;
    let alts = crate::subs_extra::alternatives_for(con, row["contract_id"].as_str(), row["series_id"].as_str(), row["monthly"].as_str().and_then(|m| m.parse::<f64>().ok()).map(crate::util::to_cents), ds.today).map_err(|e| e.to_string())?;
    let me = alts["items"].as_array().into_iter().flatten().find(|x| x["id"] == id.as_str()).cloned().unwrap_or_default();
    Ok(red.walk(&json!({"id": id, "ref": row["ref"], "savings": me["savings"], "status": me["status"], "note": "stored with its source and date; nothing is switched"}), None))
}
/// A decision PROPOSED by the coach: it counts only after the user confirms it in the web app.
pub fn decision_propose(ds: &Dataset, rec: &Recurring, red: &Redactor, con: &rusqlite::Connection, a: &Value) -> Result<Value, String> {
    let row = row_of(ds, rec, a["ref"].as_str().unwrap_or(""))?; let eff = a["effective_on"].as_str().map(|v| crate::util::d(v).ok_or("effective_on: YYYY-MM-DD")).transpose()?;
    let note = a["note"].as_str().map(|n| red.text(n).chars().take(300).collect::<String>()); let before = a["before_monthly"].as_f64().or(row["monthly"].as_str().and_then(|m| m.parse().ok())).unwrap_or(0.0);
    let v = crate::subs_extra::record_decision(con, &canon(&row), row["name"].as_str(), a["decision"].as_str().unwrap_or(""), before, a["after_monthly"].as_f64().unwrap_or(0.0), None, eff, note.as_deref(), "coach-llm", "proposed", ds.today, false).map_err(|e| format!("not proposed: {e}"))?;
    Ok(red.walk(&json!({"decision_id": v["id"], "state": "proposed", "monthly_saving": v["monthly_saving"], "next": "Tell the user: they confirm or reject it in the web app (Subscriptions). A saving counts only once the bank data confirms it."}), None))
}
pub fn contracts_draft(ds: &Dataset, rec: &Recurring, red: &Redactor, cfg: &crate::config::Config, a: &Value) -> Result<Value, String> {
    let row = row_of(ds, rec, a["ref"].as_str().unwrap_or(""))?; let s = row["series_id"].as_str().ok_or("draft only from a rec_ series without a contract file")?; if row["contract"]["status"] != "missing" { return Err("that service already has a contract file".into()); }
    let st = crate::memstore::Store { history: false, ..crate::memstore::Store::new(cfg, "coach-llm").map_err(|e| e.to_string())? };
    let v = crate::subs_extra::draft_propose(&st, rec, s, "coach-llm").map_err(|e| format!("not drafted: {}", red.text(&e.to_string())))?;
    Ok(red.walk(&json!({"proposal_id": v["proposal_id"], "status": "pending", "kind": v["contract"]["kind"], "missing": v["contract"]["missing"], "next": "Tell the user the proposal id: they review it in the web app (Memory > Proposals) and accept it themselves with `finch memory accept <id>`."}), None))
}
pub fn savings_tracker(rec: &Recurring, red: &Redactor, con: &rusqlite::Connection, today: chrono::NaiveDate) -> Result<Value, String> {
    let mut v = crate::subs_extra::savings(con, rec, today).map_err(|e| e.to_string())?; if let Some(o) = v.as_object_mut() { o.remove("reminders"); }
    for d in v["decisions"].as_array_mut().into_iter().flatten() { if let Some(o) = d.as_object_mut() { o.remove("note"); } }
    Ok(red.walk(&v, None))
}
