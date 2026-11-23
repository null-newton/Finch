//! The household tools of the MCP server: pseudonyms and roles only (never a name, an alias, a birth year or a rule's text).
use crate::{analytics::Dataset, household::{self, JOINT}, privacy::Redactor};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const MAX_ITEMS: usize = 30;
pub fn fwd(red: &Redactor, id: &str) -> String { if id == JOINT || ["unassigned", "other", "unknown"].contains(&id) { id.into() } else { red.sc.pseudo.get(id).cloned().unwrap_or_else(|| "[person]".into()) } }
/// A member pseudonym (adult-1, kid-1, joint) back to the member id.
pub fn rev(red: &Redactor, p: &str) -> Option<String> { if p == JOINT { return Some(JOINT.into()); } red.sc.pseudo.iter().find(|(_, v)| *v == p).map(|(k, _)| k.clone()) }
fn rekey(red: &Redactor, v: &Value) -> Value { Value::Object(v.as_object().cloned().unwrap_or_default().into_iter().map(|(k, x)| (fwd(red, &k), x)).collect::<Map<_, _>>()) }

pub fn overview(ds: &Dataset, red: &Redactor, con: &Connection) -> Value {
    let p = &ds.people; let whole = ds.whole(); let mut counts: BTreeMap<String, usize> = BTreeMap::new(); let mut per: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for t in &whole { let who = t.person.as_deref().map_or("unassigned".into(), |x| fwd(red, x)); *counts.entry(who.clone()).or_default() += 1; *per.entry(t.account.clone()).or_default().entry(who).or_default() += 1; }
    let members: Vec<Value> = p.members.iter().map(|m| json!({"member": fwd(red, &m.id), "role": m.role, "accounts": ds.accounts.values().filter(|a| p.resolve(a.owner.as_deref()).as_deref() == Some(m.id.as_str())).map(|a| a.uid.clone()).collect::<Vec<_>>(),
        "attributed_transactions": whole.iter().filter(|t| t.person.as_deref() == Some(m.id.as_str())).count()})).collect();
    let accounts: Vec<Value> = ds.accounts.values().map(|a| json!({"account": a.uid, "purpose": a.purpose, "owner": p.resolve(a.owner.as_deref()).map(|w| fwd(red, &w)), "attributed": per.get(&a.uid).cloned().unwrap_or_default()})).collect();
    let manual: i64 = con.query_row("SELECT COUNT(*) FROM tx_person", [], |r| r.get(0)).unwrap_or(0);
    let raw = json!({"as_of": ds.today.to_string(), "members": members, "accounts": accounts, "transactions_by_person": counts, "attribution_rules": p.attribution.len(), "manual_reassignments": manual, "kid_budgets": p.kid_budgets.len(), "allocation_rules": p.allocations.len(),
        "note": "Members are pseudonyms; the transactions of a joint account belong to `joint` unless a rule or the user attributed them to a person. Pass a member pseudonym as `member` to any analytics tool to see only that person's money."});
    // `owner` / `accounts` are mapped by the redactor; members are already pseudonyms
    let mut out = red.walk(&raw, None); for (i, m) in p.members.iter().enumerate() { out["members"][i]["member"] = fwd(red, &m.id).into(); } out
}
pub fn kids(ds: &Dataset, red: &Redactor, con: &Connection, a: &Value) -> Result<Value, String> {
    let p = &ds.people; let months = a["months"].as_u64().unwrap_or(6) as usize;
    let targets = match a["member"].as_str() { Some(x) => vec![rev(red, x).filter(|m| p.get(m).is_some()).ok_or("that member is not a person: use a member pseudonym (adult-N, kid-N) from household_overview")?], None => p.children() };
    if targets.is_empty() { return Err("no child is declared in the household".into()); }
    let mut out = vec![];
    for m in &targets {
        let mut j = household::kid_report(ds, con, m, months).map_err(|e| e.to_string())?; j["member"] = fwd(red, m).into();
        if let Some(items) = j["extra_topups"]["items"].as_array_mut() { if items.len() > MAX_ITEMS { items.truncate(MAX_ITEMS); } for it in items.iter_mut() { it["source"] = fwd(red, it["source"].as_str().unwrap_or("")).into(); } }
        j["extra_topups"]["by_source"] = rekey(red, &j["extra_topups"]["by_source"]);
        for s in j["pocket_money"]["series"].as_array_mut().into_iter().flatten() { s["source"] = fwd(red, s["source"].as_str().unwrap_or("")).into(); }
        out.push(j);
    }
    let budgets: Vec<Value> = household::kid_budget_status(ds, None).into_iter().enumerate().filter(|(_, b)| targets.iter().any(|t| b["member"] == t.as_str())).map(|(n, mut b)| { let o = b.as_object_mut().unwrap(); o.remove("id"); o.remove("note"); b["member"] = fwd(red, b["member"].as_str().unwrap_or("")).into(); b["ref"] = format!("kid-budget-{}", n + 1).into(); b }).collect();
    let raw = json!({"as_of": ds.today.to_string(), "children": out, "kid_budgets": budgets, "note": "Pocket money = a regular credit (3 or more, same amount, weekly / fortnightly / monthly rhythm) or the declared amount; everything else is an extra top-up with its source. The balance trend is rebuilt backwards from the newest balance (estimated). A transfer from a parent is an internal transfer of the household: not income or spending for the household, but it is the child's income here."});
    Ok(fix_members(red.walk(&raw, None), &raw))
}
pub fn who_pays(ds: &Dataset, red: &Redactor, a: &Value) -> Value {
    let mut rep = household::who_pays(ds, a["months"].as_u64().unwrap_or(12) as usize, None);
    for (n, r) in rep["rules"].as_array_mut().into_iter().flatten().enumerate() {
        let o = r.as_object_mut().unwrap(); o.remove("id"); o.remove("title"); o.insert("ref".into(), format!("allocation-{}", n + 1).into());
        r["among"] = json!(r["among"].as_array().into_iter().flatten().map(|m| fwd(red, m.as_str().unwrap_or(""))).collect::<Vec<_>>());
        for m in r["members"].as_array_mut().into_iter().flatten() { m["member"] = fwd(red, m["member"].as_str().unwrap_or("")).into(); }
        if r["income_basis"].is_object() { r["income_basis"] = rekey(red, &r["income_basis"]); }
    }
    for m in rep["by_member"].as_array_mut().into_iter().flatten() { m["member"] = fwd(red, m["member"].as_str().unwrap_or("")).into(); }
    rep["note"] = "Computed shares and settlement per rule: `owed` is the member's fair share of the whole cost, `paid` what they paid out of their own money, `net` the settlement (positive: the others owe them). Costs paid from the joint account settle nothing. Quote the numbers exactly; give no legal or tax advice on a couple's finances.".into();
    fix_members(red.walk(&rep, None), &rep)
}
/// The redactor's text pass must not touch the pseudonyms we set (`member`, `source`, `among`, keys of `by_source` / `income_basis`).
fn fix_members(mut out: Value, raw: &Value) -> Value {
    fn go(o: &mut Value, r: &Value) {
        match (o, r) {
            (Value::Object(om), Value::Object(rm)) => for (k, rv) in rm { if matches!(k.as_str(), "member" | "source" | "among" | "by_source" | "income_basis") { om.insert(k.clone(), rv.clone()); } else if let Some(ov) = om.get_mut(k) { go(ov, rv); } },
            (Value::Array(oa), Value::Array(ra)) => for (ov, rv) in oa.iter_mut().zip(ra) { go(ov, rv) },
            _ => {}
        }
    }
    go(&mut out, raw); out
}
