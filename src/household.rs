//! The household's people (E14): members, attribution of transactions to a member, the children's money, kid budgets and the
//! shared-cost split ("who pays what"). `household.yaml` is read-only here: changes are the Python app's memory proposals.
//! Attribution precedence, first hit wins: a manual reassignment (`tx_person`), an attribution rule (file order), the account's owner,
//! nobody. The result is a member id, `joint` (the household as a whole) or None. It never changes a category, an amount or a link.
use crate::{analytics::{is_income, is_spending, is_transfer, Dataset, Tx}, transfers, util::*};
use anyhow::{bail, Result};
use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet, HashMap}, path::Path};

pub const JOINT: &str = "joint";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PocketMoney { pub amount: f64, pub period: String, #[serde(skip_serializing_if = "Option::is_none")] pub day: Option<u32> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member { pub id: String, pub name: String, pub role: String, #[serde(default)] pub birth_year: Option<i64>, #[serde(default)] pub aliases: Vec<String>, #[serde(default)] pub pocket_money: Option<PocketMoney> }
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleMatch { #[serde(skip_serializing_if = "Option::is_none")] pub account: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub card_last4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub merchant_key: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub direction: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub amount_min: Option<f64>, #[serde(skip_serializing_if = "Option::is_none")] pub amount_max: Option<f64> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule { pub id: String, pub member: String, #[serde(rename = "match")] pub m: RuleMatch, #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KidBudget { pub id: String, pub member: String, pub period: String, pub limit: f64, #[serde(default, skip_serializing_if = "Option::is_none")] pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub group: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<String> }
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocMatch { #[serde(skip_serializing_if = "Option::is_none")] pub category: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub group: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub merchant_key: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub account: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation { pub id: String, #[serde(default, skip_serializing_if = "Option::is_none")] pub title: Option<String>, #[serde(rename = "match")] pub m: AllocMatch, #[serde(default = "equal")] pub method: String,
    #[serde(default)] pub among: Vec<String>, #[serde(default)] pub shares: BTreeMap<String, f64>, #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<String> }
fn equal() -> String { "equal".into() }

pub fn fold(s: &str) -> String { crate::util::fold(s).trim().to_string() }
fn tokens(s: &str) -> Vec<String> { crate::rs!(r"[^\w']+").split(&fold(s)).filter(|t| !t.is_empty()).map(String::from).collect() }

/// The members and the rules of household.yaml, with the lookups the rest of the code needs.
#[derive(Debug, Clone, Default)]
pub struct People { pub members: Vec<Member>, pub attribution: Vec<Rule>, pub kid_budgets: Vec<KidBudget>, pub allocations: Vec<Allocation>, pub warnings: Vec<String>, lookup: HashMap<String, String>, first: HashMap<String, String> }
impl People {
    pub fn new(members: Vec<Member>, attribution: Vec<Rule>, kid_budgets: Vec<KidBudget>, allocations: Vec<Allocation>) -> Self {
        let (mut lookup, mut firsts): (HashMap<String, String>, BTreeMap<String, BTreeSet<String>>) = Default::default();
        for m in &members { for k in std::iter::once(&m.id).chain([&m.name]).chain(&m.aliases) { let f = fold(k); if !f.is_empty() { lookup.entry(f).or_insert(m.id.clone()); } }
            if let Some(t) = tokens(&m.name).into_iter().next() { firsts.entry(t).or_default().insert(m.id.clone()); } }
        let mut first = HashMap::new();
        for (t, ids) in firsts { if ids.len() == 1 && t.chars().count() >= 3 { let id = ids.into_iter().next().unwrap(); lookup.entry(t.clone()).or_insert(id.clone()); first.insert(t, id); } }
        Self { members, attribution, kid_budgets, allocations, warnings: vec![], lookup, first }
    }
    /// Tolerant read of household.yaml: an invalid item is left out with a warning (a missing file is an empty household).
    pub fn load(memory_dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(memory_dir.join("household.yaml")) else { return Self::default() };
        let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { let mut p = Self::default(); p.warnings.push("household.yaml is not valid YAML: no member is used".into()); return p };
        let mut warn = vec![];
        fn list<T: serde::de::DeserializeOwned>(v: &serde_yaml::Value, key: &str, warn: &mut Vec<String>) -> Vec<T> {
            v.get(key).and_then(|x| x.as_sequence()).into_iter().flatten().filter_map(|x| serde_yaml::from_value::<T>(x.clone()).map_err(|e| warn.push(format!("household.yaml {key}: an item is left out ({e})"))).ok()).collect()
        }
        let mut members: Vec<Member> = list(&v, "members", &mut warn); members.retain(|m| !m.id.is_empty() && matches!(m.role.as_str(), "adult" | "child"));
        let rules: Vec<Rule> = list::<Rule>(&v, "attribution", &mut warn).into_iter().filter(|r| r.m.card_last4.as_ref().is_none_or(|c| crate::rs!(r"^(\d{4}|\d{6}\*\d)$").is_match(c)) || { warn.push(format!("attribution {}: card_last4 must be 4 digits", r.id)); false }).collect();
        let mut p = Self::new(members, rules, list(&v, "kid_budgets", &mut warn), list(&v, "allocations", &mut warn)); p.warnings = warn; p
    }
    pub fn is_empty(&self) -> bool { self.members.is_empty() }
    pub fn get(&self, id: &str) -> Option<&Member> { self.members.iter().find(|m| m.id == id) }
    pub fn ids(&self) -> Vec<String> { self.members.iter().map(|m| m.id.clone()).collect() }
    pub fn adults(&self) -> Vec<String> { self.members.iter().filter(|m| m.role == "adult").map(|m| m.id.clone()).collect() }
    pub fn children(&self) -> Vec<String> { self.members.iter().filter(|m| m.role == "child").map(|m| m.id.clone()).collect() }
    pub fn role(&self, id: &str) -> Option<&str> { self.get(id).map(|m| m.role.as_str()) }
    pub fn name(&self, id: Option<&str>) -> String { match id { None | Some("") => "unassigned".into(), Some(JOINT) => "Joint".into(), Some(i) => self.get(i).map_or(i.to_string(), |m| m.name.clone()) } }
    /// An account-owner value (member id, name, alias, 'joint') -> a member id, 'joint', or None when unknown.
    pub fn resolve(&self, v: Option<&str>) -> Option<String> {
        let v = v?.trim(); if v.is_empty() { return None; } if v.eq_ignore_ascii_case(JOINT) { return Some(JOINT.into()); } self.lookup.get(&fold(v)).cloned()
    }
    /// The member's full name (every word) or an alias appears in `text`.
    pub fn text_mentions(&self, text: &str, id: &str) -> bool {
        let Some(m) = self.get(id) else { return false }; let hay = format!(" {} ", tokens(text).join(" "));
        std::iter::once(&m.name).chain(&m.aliases).any(|p| { let t = tokens(p); !(t.len() == 1 && t[0].chars().count() < 4) && !t.is_empty() && t.iter().all(|x| hay.contains(&format!(" {x} "))) })
    }
    /// Members a bank text names: a full name or alias, or a first name only one member has.
    pub fn mentioned(&self, text: &str) -> BTreeSet<String> {
        let toks: BTreeSet<String> = tokens(text).into_iter().collect();
        self.members.iter().filter(|m| self.text_mentions(text, &m.id)).map(|m| m.id.clone()).chain(self.first.iter().filter(|(t, _)| toks.contains(*t)).map(|(_, id)| id.clone())).collect()
    }
    pub fn member_of_owner(&self, owner: Option<&str>) -> Option<String> { self.resolve(owner).filter(|r| self.get(r).is_some()) }
}

/// The last four digits of a card the bank printed (`4871 04XX XXXX 1234`, `CARTE X4242`, `****4242`), or `123456*7`.
pub fn card_last4(desc: &str) -> Option<String> {
    if let Some(c) = crate::rs!(r"\b(\d{6})\*{4,}(\d)\b").captures(desc) { return Some(format!("{}*{}", &c[1], &c[2])); }
    crate::rs!(r"(?i)(?:[X*\x{2022}]{2,}[ -]?|\b(?:CB|CARTE|CARD|CARTA|KAART)\s*[X*\x{2022}]+[ -]?|\b(?:CB|CARTE|CARD|CARTA|KAART)\s*(?:N°|N\.|NO\.?|NR\.?|#)\s*[X*\x{2022}]*[ -]?)(\d{4})\b").captures(desc).map(|c| c[1].to_string())
}
fn rx_ok(p: &str, s: &str) -> bool { regex::Regex::new(&format!("(?i){p}")).is_ok_and(|r| r.is_match(s)) }
/// The first key of a rule that fails (English), or None when every key matches.
pub fn why_not(r: &Rule, names: &BTreeSet<String>, desc: &str, mkey: &str, amount: f64) -> Option<String> {
    let m = &r.m;
    if let Some(a) = &m.account { if !names.contains(&fold(a)) { return Some(format!("the account is not {a:?}")); } }
    if let Some(c) = &m.card_last4 { if card_last4(desc).as_deref() != Some(c) { return Some(format!("the card ending {c} is not printed on it")); } }
    if let Some(p) = &m.merchant_key { if !rx_ok(p, mkey) { return Some("the merchant key does not match".into()); } }
    if let Some(p) = &m.description { if !rx_ok(p, desc) { return Some("the description does not match".into()); } }
    match m.direction.as_deref() { Some("in") if amount <= 0.0 => return Some("the direction is not \"in\"".into()), Some("out") if amount >= 0.0 => return Some("the direction is not \"out\"".into()), _ => {} }
    if m.amount_min.is_some_and(|x| amount.abs() < x) { return Some(format!("the amount is below {}", m.amount_min.unwrap())); }
    if m.amount_max.is_some_and(|x| amount.abs() > x) { return Some(format!("the amount is above {}", m.amount_max.unwrap())); }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribution { pub person: Option<String>, pub source: &'static str, pub rule: Option<String>, pub reason: String }
/// Resolves the person of every transaction of one load; `attribute` is a cheap lookup.
pub struct Attributor<'a> { pub people: &'a People, pub manual: HashMap<String, String>, acc: HashMap<String, (BTreeSet<String>, Option<String>)> }
impl<'a> Attributor<'a> {
    pub fn new(con: &Connection, people: &'a People) -> Result<Self> {
        let acc = con.prepare("SELECT uid, label, name, owner FROM accounts")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))?
            .map(|r| r.map(|(u, l, n, o)| { let names = [Some(u.clone()), l, n].into_iter().flatten().map(|x| fold(&x)).filter(|x| !x.is_empty()).collect(); (u, (names, o)) })).collect::<rusqlite::Result<_>>()?;
        Ok(Self { people, manual: load_manual(con), acc })
    }
    pub fn names(&self, uid: &str) -> BTreeSet<String> { self.acc.get(uid).map(|a| a.0.clone()).unwrap_or_default() }
    pub fn attribute(&self, key: &str, uid: &str, desc: &str, mkey: &str, amount: f64) -> Attribution {
        if let Some(m) = self.manual.get(key) { return Attribution { person: Some(m.clone()), source: "manual", rule: None, reason: "reassigned by hand".into() }; }
        let (names, owner) = self.acc.get(uid).cloned().unwrap_or_default();
        if let Some(r) = self.people.attribution.iter().find(|r| why_not(r, &names, desc, mkey, amount).is_none()) { return Attribution { person: Some(r.member.clone()), source: "rule", rule: Some(r.id.clone()), reason: format!("attribution rule {}", r.id) }; }
        match self.people.resolve(owner.as_deref()) { Some(w) => Attribution { reason: if w == JOINT { "joint account".into() } else { "owner of the account".into() }, person: Some(w), source: "account", rule: None },
            None => Attribution { person: None, source: "none", rule: None, reason: "the account has no known owner".into() } }
    }
}
pub fn load_manual(con: &Connection) -> HashMap<String, String> { con.prepare("SELECT tx_key, member FROM tx_person").and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|i| i.flatten().collect())).unwrap_or_default() }

fn check_member(p: &People, m: &str) -> Result<String> {
    if m == JOINT || p.get(m).is_some() { return Ok(m.into()); }
    p.resolve(Some(m)).ok_or_else(|| anyhow::anyhow!("{m:?} is not a household member (ids: {}, or 'joint')", if p.is_empty() { "none declared".into() } else { p.ids().join(", ") }))
}
/// Reassign one transaction to a member (or 'joint') by hand; recorded in tx_person_log and reversible.
pub fn assign(con: &Connection, p: &People, tx_ref: &str, member: &str, by: &str, note: Option<&str>) -> Result<Value> {
    let key = transfers::resolve_tx(con, tx_ref)?; let member = check_member(p, member)?;
    let old: Option<String> = con.query_row("SELECT member FROM tx_person WHERE tx_key=?", [&key], |r| r.get(0)).optional()?;
    if old.as_deref() == Some(member.as_str()) { return Ok(json!({"tx_key": key, "member": member, "changed": false})); }
    let ts = now_iso();
    con.execute("INSERT OR REPLACE INTO tx_person(tx_key, member, set_at, set_by, note) VALUES (?,?,?,?,?)", params![key, member, ts, by, note])?;
    con.execute("INSERT INTO tx_person_log(tx_key, old_member, new_member, action, at, by, note) VALUES (?,?,?,'set',?,?,?)", params![key, old, member, ts, by, note])?;
    Ok(json!({"tx_key": key, "member": member, "previous": old, "changed": true}))
}
/// Remove the manual reassignment: the rules and the account owner decide again.
pub fn clear(con: &Connection, tx_ref: &str, by: &str) -> Result<Value> {
    let key = transfers::resolve_tx(con, tx_ref)?; let Some(old): Option<String> = con.query_row("SELECT member FROM tx_person WHERE tx_key=?", [&key], |r| r.get(0)).optional()? else { return Ok(json!({"tx_key": key, "changed": false})) };
    con.execute("DELETE FROM tx_person WHERE tx_key=?", [&key])?;
    con.execute("INSERT INTO tx_person_log(tx_key, old_member, new_member, action, at, by, note) VALUES (?,?,NULL,'clear',?,?,NULL)", params![key, old, now_iso(), by])?;
    Ok(json!({"tx_key": key, "previous": old, "changed": true}))
}
pub fn log(con: &Connection, tx_key: Option<&str>, limit: i64) -> Result<Vec<Value>> {
    let sql = format!("SELECT id, tx_key, old_member, new_member, action, at, by, note FROM tx_person_log{} ORDER BY id DESC LIMIT ?", if tx_key.is_some() { " WHERE tx_key=?" } else { "" });
    let f = |r: &rusqlite::Row| Ok(json!({"id": r.get::<_, i64>(0)?, "tx_key": r.get::<_, String>(1)?, "old_member": r.get::<_, Option<String>>(2)?, "new_member": r.get::<_, Option<String>>(3)?, "action": r.get::<_, String>(4)?, "at": r.get::<_, String>(5)?, "by": r.get::<_, Option<String>>(6)?, "note": r.get::<_, Option<String>>(7)?}));
    let mut st = con.prepare(&sql)?; Ok(match tx_key { Some(k) => st.query_map(params![k, limit], f)?.collect::<rusqlite::Result<_>>()?, None => st.query_map(params![limit], f)?.collect::<rusqlite::Result<_>>()? })
}
/// Undo one log line (a new line records the revert); refused when the transaction changed again since.
pub fn revert(con: &Connection, p: &People, id: i64, by: &str) -> Result<Value> {
    let (key, old, new): (String, Option<String>, Option<String>) = con.query_row("SELECT tx_key, old_member, new_member FROM tx_person_log WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or_else(|| anyhow::anyhow!("no attribution log line #{id}"))?;
    let cur: Option<String> = con.query_row("SELECT member FROM tx_person WHERE tx_key=?", [&key], |r| r.get(0)).optional()?;
    if cur != new { bail!("the transaction was changed again since this line: revert the newest change first"); }
    let (ts, note) = (now_iso(), format!("revert of #{id}"));
    match &old { None => { con.execute("DELETE FROM tx_person WHERE tx_key=?", [&key])?; } Some(o) => { check_member(p, o)?; con.execute("INSERT OR REPLACE INTO tx_person(tx_key, member, set_at, set_by, note) VALUES (?,?,?,?,?)", params![key, o, ts, by, note])?; } }
    con.execute("INSERT INTO tx_person_log(tx_key, old_member, new_member, action, at, by, note) VALUES (?,?,?,'revert',?,?,?)", params![key, cur, old, ts, by, note])?;
    Ok(json!({"tx_key": key, "member": old, "reverted": id}))
}
/// WHY a transaction belongs to whom: the manual line, every rule with whether it matched, the account owner.
pub fn explain(con: &Connection, p: &People, tx_key: &str) -> Result<Value> {
    let (uid, desc, amount, mkey, label, owner): (String, Option<String>, Option<f64>, Option<String>, Option<String>, Option<String>) = con.query_row("SELECT t.account_uid, t.description, t.amount, e.merchant_key, COALESCE(a.label, a.name, a.uid), a.owner FROM transactions t LEFT JOIN tx_enriched e ON e.tx_key=t.tx_key LEFT JOIN accounts a ON a.uid=t.account_uid WHERE t.tx_key=?",
        [tx_key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?.ok_or_else(|| anyhow::anyhow!("no such transaction"))?;
    let (desc, mkey, amount) = (desc.unwrap_or_default(), mkey.unwrap_or_default(), amount.unwrap_or(0.0));
    let at = Attributor::new(con, p)?; let names = at.names(&uid); let res = at.attribute(tx_key, &uid, &desc, &mkey, amount);
    let man: Option<Value> = con.query_row("SELECT member, set_at, set_by, note FROM tx_person WHERE tx_key=?", [tx_key], |r| Ok(json!({"member": r.get::<_, String>(0)?, "set_at": r.get::<_, String>(1)?, "set_by": r.get::<_, Option<String>>(2)?, "note": r.get::<_, Option<String>>(3)?}))).optional()?;
    let rules: Vec<Value> = p.attribution.iter().map(|r| { let w = why_not(r, &names, &desc, &mkey, amount); json!({"id": r.id, "member": r.member, "matched": w.is_none(), "why_not": w, "why_not_msg": null}) }).collect();
    Ok(json!({"tx_key": tx_key, "person": res.person, "source": res.source, "rule": res.rule, "reason": res.reason, "manual": man, "rules": rules, "account": label, "account_owner": owner,
        "account_owner_member": p.member_of_owner(owner.as_deref()), "card_last4": card_last4(&desc), "history": log(con, Some(tx_key), 10)?}))
}

// ---------------------------------------------------------------- the children's money
const OWN_MOVE_TYPES: [&str; 2] = ["savings_internal", "fx_exchange"];
const CADENCES: [(&str, i64, i64); 3] = [("weekly", 5, 9), ("fortnightly", 12, 16), ("monthly", 25, 37)];
fn inflow(t: &Tx) -> bool { t.amount_c > 0 && (is_transfer(&t.category) || is_income(&t.category)) }
fn spend_like(t: &Tx) -> bool { is_spending(&t.category) || t.category == "income.refund" }
fn window(ds: &Dataset, months: usize) -> Vec<String> { let c = last_closed_month(ds.today); months_between(&add_months_key(&c, -(months.max(1) as i32 - 1)), &c) }
/// Who the money came from: a transfer link gives the owner of the paying account; otherwise the members a text names.
fn source_of(t: &Tx, linked: &HashMap<String, String>, p: &People, member: &str, cp: &str) -> (String, bool) {
    if let Some(s) = linked.get(&t.key) { return (s.clone(), true); }
    let named = p.mentioned(&format!("{} {cp}", t.desc)); let others: Vec<&String> = named.iter().filter(|x| *x != member).collect();
    if others.len() == 1 { return (others[0].clone(), false); }
    if others.len() > 1 { return (if others.iter().all(|o| p.role(o) == Some("adult")) { JOINT.into() } else { others[0].clone() }, false); }
    if named.contains(member) { return ("self".into(), false); }
    (if is_income(&t.category) { "other" } else { "unknown" }.into(), false)
}
fn cadence(dates: &[NaiveDate]) -> Option<&'static str> {
    let mut d = dates.to_vec(); d.sort(); let gaps: Vec<i64> = d.windows(2).map(|w| (w[1] - w[0]).num_days()).collect(); if gaps.len() < 2 { return None; }
    let mut s = gaps.clone(); s.sort(); let med = s[s.len() / 2];
    CADENCES.iter().find(|(_, lo, hi)| (*lo..=*hi).contains(&med) && gaps.iter().filter(|g| (*lo..=*hi).contains(*g)).count() as f64 / gaps.len() as f64 >= 0.6).map(|c| c.0)
}
/// Regular inflows: 3+ credits of nearly the same amount (5 %, at least 0.50) from one source at a weekly / fortnightly / monthly rhythm.
pub fn detect_pocket_money(items: &[(NaiveDate, i64, String, String)], member: &str) -> Vec<Value> {
    let mut by: BTreeMap<&str, Vec<(NaiveDate, i64, &str)>> = BTreeMap::new(); for (d, c, k, s) in items { by.entry(s).or_default().push((*d, *c, k)); }
    let mut out = vec![];
    for (src, mut its) in by {
        its.sort_by_key(|x| x.1); let mut groups: Vec<Vec<(NaiveDate, i64, &str)>> = vec![];
        for it in its { match groups.iter_mut().find(|g| (it.1 - g[0].1).abs() <= 50.max(g[0].1 * 5 / 100)) { Some(g) => g.push(it), None => groups.push(vec![it]) } }
        for mut g in groups.into_iter().filter(|g| g.len() >= 3) {
            let Some(cad) = cadence(&g.iter().map(|x| x.0).collect::<Vec<_>>()) else { continue }; g.sort_by(|a, b| (a.0, a.2).cmp(&(b.0, b.2)));
            let amount = median_c(&g.iter().map(|x| x.1).collect::<Vec<_>>()); let last = g.last().unwrap().0;
            let next = match cad { "weekly" => last + Duration::days(7), "fortnightly" => last + Duration::days(14), _ => add_months(last, 1) };
            out.push(json!({"id": format!("pm_{}", &sha1_hex(&format!("{member}|{src}|{cad}|{amount}"))[..8]), "source": src, "amount": money_str(amount), "cadence": cad, "count": g.len(), "first": g[0].0.to_string(), "last": last.to_string(),
                "next_expected": next.to_string(), "day": if cad == "monthly" { last.day() } else { last.weekday().number_from_monday() }, "evidence": g.iter().map(|x| x.2).collect::<Vec<_>>()}));
        }
    }
    out
}
fn owned<'a>(ds: &'a Dataset, member: &str) -> Vec<&'a crate::analytics::AccountInfo> { ds.accounts.values().filter(|a| ds.people.resolve(a.owner.as_deref()).as_deref() == Some(member)).collect() }
/// Month-end balances of the member's own accounts, rebuilt backwards from the newest balance with the later transactions (an estimate).
fn balance_trend(ds: &Dataset, member: &str, months: &[String]) -> Value {
    let own = owned(ds, member); let known: Vec<_> = own.iter().filter(|a| ds.balances.contains_key(&a.uid)).collect(); let unknown: Vec<&str> = own.iter().filter(|a| !ds.balances.contains_key(&a.uid)).map(|a| a.label.as_str()).collect();
    if known.is_empty() { return json!({"current": null, "as_of": null, "trend": [], "unknown": unknown, "estimated": true}); }
    let cur: i64 = known.iter().map(|a| ds.balances[&a.uid].amount_c).sum(); let as_of = known.iter().map(|a| ds.balances[&a.uid].as_of).max().unwrap();
    let mut trend = vec![];
    for m in months { let end = month_end(m); if end >= as_of { continue; }
        if known.iter().any(|a| ds.coverage.first(&a.uid).is_some_and(|f| f > end)) { continue; }
        let tot: i64 = known.iter().map(|a| { let b = &ds.balances[&a.uid]; b.amount_c - ds.txs.iter().filter(|t| t.account == a.uid && t.date > end && t.date <= b.as_of).map(|t| t.amount_c).sum::<i64>() }).sum();
        trend.push(json!({"month": m, "end_balance": money_str(tot)})); }
    trend.push(json!({"month": month_key(as_of), "end_balance": money_str(cur), "as_of": as_of.to_string()}));
    json!({"current": money_str(cur), "as_of": as_of.to_string(), "trend": trend, "unknown": unknown, "estimated": true})
}
/// The children's-money report of one member: pocket money vs extra top-ups (with their source), spending, balance trend. `ds` is the whole household.
pub fn kid_report(ds: &Dataset, con: &Connection, member: &str, months: usize) -> Result<Value> {
    let p = &ds.people; if p.get(member).is_none() { bail!("{member:?} is not a household member"); }
    let win = window(ds, months); let wset: BTreeSet<&String> = win.iter().collect(); let whole = ds.whole();
    let mine: Vec<&Tx> = whole.iter().filter(|t| t.person.as_deref() == Some(member)).collect();
    let in_win: Vec<&Tx> = mine.iter().copied().filter(|t| inflow(t) && wset.contains(&t.month())).collect();
    let mut linked: HashMap<String, String> = HashMap::new(); let mut cps: HashMap<String, String> = HashMap::new();
    for t in &in_win {
        if let Some(o) = con.query_row("SELECT a.owner FROM transfer_links l JOIN transactions o ON o.tx_key=l.out_tx_key LEFT JOIN accounts a ON a.uid=o.account_uid WHERE l.in_tx_key=?", [&t.key], |r| r.get::<_, Option<String>>(0)).optional()? { linked.insert(t.key.clone(), p.resolve(o.as_deref()).unwrap_or_else(|| "unknown".into())); }
        else if let Some(Some(c)) = con.query_row("SELECT counterparty FROM tx_parse_meta WHERE tx_key=?", [&t.key], |r| r.get::<_, Option<String>>(0)).optional()? { cps.insert(t.key.clone(), c); }
    }
    let (mut rows, mut own_moves) = (vec![], vec![]);
    for t in in_win { if t.ty.as_deref().is_some_and(|x| OWN_MOVE_TYPES.contains(&x)) { own_moves.push(t); continue; }
        let (src, lk) = source_of(t, &linked, p, member, cps.get(&t.key).map_or("", |s| s.as_str())); if src == "self" { own_moves.push(t); continue; } rows.push((t, src, lk)); }
    let series = detect_pocket_money(&rows.iter().map(|(t, s, _)| (t.date, t.amount_c, t.key.clone(), s.clone())).collect::<Vec<_>>(), member);
    let mut pocket: BTreeSet<String> = series.iter().flat_map(|s| s["evidence"].as_array().unwrap().iter().filter_map(|k| k.as_str().map(String::from))).collect();
    let declared = p.get(member).and_then(|m| m.pocket_money.clone()).map(|d| { let a = (d.amount * 100.0).round() as i64; let tol = 50.max(a * 5 / 100);
        for (t, _, _) in &rows { if (t.amount_c - a).abs() <= tol { pocket.insert(t.key.clone()); } } json!({"amount": money_str(a), "period": d.period, "day": d.day, "matches": rows.iter().filter(|(t, _, _)| pocket.contains(&t.key)).count()}) });
    let pocket_c: i64 = rows.iter().filter(|(t, _, _)| pocket.contains(&t.key)).map(|(t, _, _)| t.amount_c).sum();
    let mut extra: Vec<&(&Tx, String, bool)> = rows.iter().filter(|(t, _, _)| !pocket.contains(&t.key)).collect(); extra.sort_by(|a, b| (b.0.date, &b.0.key).cmp(&(a.0.date, &a.0.key)));
    let extra_c: i64 = extra.iter().map(|r| r.0.amount_c).sum(); let mut by_src: BTreeMap<String, i64> = BTreeMap::new(); for r in &extra { *by_src.entry(r.1.clone()).or_default() += r.0.amount_c; }
    let spend: Vec<&&Tx> = mine.iter().filter(|t| wset.contains(&t.month()) && spend_like(t)).collect(); let spent: i64 = -spend.iter().map(|t| t.amount_c).sum::<i64>();
    let mut cats: BTreeMap<&str, (i64, usize)> = BTreeMap::new(); let mut per_m: BTreeMap<String, i64> = BTreeMap::new();
    for t in &spend { let e = cats.entry(&t.category).or_default(); e.0 -= t.amount_c; e.1 += 1; *per_m.entry(t.month()).or_default() -= t.amount_c; }
    let mut cats: Vec<_> = cats.into_iter().collect(); cats.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let cur = month_key(ds.today); let to_date = -mine.iter().filter(|t| t.month() == cur && spend_like(t)).map(|t| t.amount_c).sum::<i64>();
    let own = owned(ds, member); let bal = balance_trend(ds, member, &win); let n = win.len() as i64; let total_in = pocket_c + extra_c;
    let mut notes = vec![]; if own.is_empty() { notes.push("the child owns no account: only the transactions attributed to them by a rule or by hand are counted".to_string()); }
    if let Some(u) = bal["unknown"].as_array().filter(|u| !u.is_empty()) { notes.push(format!("no balance known for: {}", u.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))); }
    Ok(json!({"member": member, "as_of": ds.today.to_string(), "window": {"months": win, "from": format!("{}-01", win[0]), "to": month_end(win.last().unwrap()).to_string()},
        "accounts": own.iter().map(|a| { let b = ds.balances.get(&a.uid); json!({"account": a.uid, "label": a.label, "balance": b.map(|b| money_str(b.amount_c)), "balance_as_of": b.map(|b| b.as_of.to_string())}) }).collect::<Vec<_>>(),
        "balance": bal, "pocket_money": {"series": series, "declared": declared, "total": money_str(pocket_c), "monthly_equivalent": money_str(if n > 0 { pocket_c / n } else { 0 })},
        "extra_topups": {"total": money_str(extra_c), "count": extra.len(), "by_source": by_src.iter().map(|(k, v)| (k.clone(), json!(money_str(*v)))).collect::<serde_json::Map<_, _>>(),
            "items": extra.iter().map(|(t, s, lk)| json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "source": s, "linked": lk, "category": t.category})).collect::<Vec<_>>()},
        "inflow": {"total": money_str(total_in), "pocket": money_str(pocket_c), "extra": money_str(extra_c)}, "own_moves": {"count": own_moves.len(), "total": money_str(own_moves.iter().map(|t| t.amount_c).sum())},
        "ratio": {"pocket_share": pct(pocket_c, total_in), "extra_share": pct(extra_c, total_in), "pocket_to_extra": if extra_c != 0 { json!(pct(pocket_c, extra_c)) } else { Value::Null }},
        "spending": {"total": money_str(spent), "monthly_avg": money_str(if n > 0 { spent / n } else { 0 }), "this_month_to_date": money_str(to_date),
            "by_category": cats.iter().map(|(c, (v, k))| json!({"category": c, "total": money_str(*v), "n": k, "share": pct(*v, spent)})).collect::<Vec<_>>(),
            "by_month": win.iter().map(|m| json!({"month": m, "total": money_str(*per_m.get(m).unwrap_or(&0))})).collect::<Vec<_>>()},
        "notes": notes}))
}
pub fn children_overview(ds: &Dataset, con: &Connection, months: usize) -> Result<Vec<Value>> { ds.people.children().iter().map(|c| kid_report(ds, con, c, months)).collect() }

/// Kid budgets: spending attributed to the member in the current week (Mon-Sun) or month; ok < 80 % <= at_risk <= 100 % < over.
pub fn kid_budget_status(ds: &Dataset, member: Option<&str>) -> Vec<Value> {
    ds.people.kid_budgets.iter().filter(|b| member.is_none_or(|m| b.member == m)).map(|b| {
        let (lo, hi) = if b.period == "weekly" { let s = ds.today - Duration::days(ds.today.weekday().num_days_from_monday() as i64); (s, s + Duration::days(6)) } else { let m = month_key(ds.today); (month_start(&m), month_end(&m)) };
        let spent = (-ds.txs.iter().filter(|t| t.person.as_deref() == Some(&b.member) && t.date >= lo && t.date <= hi.min(ds.today) && spend_like(t) && b.category.as_ref().is_none_or(|c| &t.category == c) && b.group.as_ref().is_none_or(|g| t.category.split('.').next() == Some(g))).map(|t| t.amount_c).sum::<i64>()).max(0);
        let limit = (b.limit * 100.0).round() as i64; let ratio = if limit > 0 { spent as f64 / limit as f64 } else { 0.0 };
        let (elapsed, len) = ((hi.min(ds.today) - lo).num_days() + 1, (hi - lo).num_days() + 1);
        json!({"id": b.id, "member": b.member, "period": b.period, "category": b.category, "group": b.group, "limit": money_str(limit), "spent": money_str(spent), "remaining": money_str((limit - spent).max(0)), "ratio": round4(ratio),
            "status": if spent > limit { "over" } else if ratio >= 0.8 { "at_risk" } else { "ok" }, "period_start": lo.to_string(), "period_end": hi.to_string(), "days_left": (len - elapsed).max(0),
            "projected": money_str(if elapsed > 0 { (spent as f64 / elapsed as f64 * len as f64).round() as i64 } else { spent }), "note": b.note})
    }).collect()
}

/// `total` cents shared in proportion to `w` by largest remainder: the parts add up exactly.
pub fn split_cents(total: i64, w: &BTreeMap<String, f64>) -> BTreeMap<String, i64> {
    let sum: f64 = w.values().sum(); if total == 0 || sum <= 0.0 { return w.keys().map(|k| (k.clone(), 0)).collect(); }
    let (sign, t) = (total.signum(), total.abs()); let raw: BTreeMap<&String, f64> = w.iter().map(|(k, v)| (k, t as f64 * v / sum)).collect();
    let mut out: BTreeMap<String, i64> = raw.iter().map(|(k, v)| ((*k).clone(), v.floor() as i64)).collect(); let rest = t - out.values().sum::<i64>();
    let mut order: Vec<&&String> = raw.keys().collect(); order.sort_by(|a, b| (raw[*b] - raw[*b].floor()).total_cmp(&(raw[*a] - raw[*a].floor())).then(a.cmp(b)));
    for k in order.into_iter().take(rest.max(0) as usize) { *out.get_mut(*k).unwrap() += 1; }
    out.into_iter().map(|(k, v)| (k, sign * v)).collect()
}
fn alloc_matches(a: &Allocation, t: &Tx, ds: &Dataset) -> bool {
    let m = &a.m; spend_like(t) && m.category.as_ref().is_none_or(|c| &t.category == c) && m.group.as_ref().is_none_or(|g| t.category.split('.').next() == Some(g)) && m.tag.as_ref().is_none_or(|g| t.tags.contains(g))
        && m.merchant_key.as_ref().is_none_or(|p| rx_ok(p, &t.mkey)) && m.account.as_ref().is_none_or(|x| fold(x) == fold(&t.account) || fold(x) == fold(&ds.label(&t.account)))
}
/// Shared costs split by rule: each member's fair share, what they paid personally, and the settlement (nets add up to zero).
/// Costs paid from the joint account are the common pot's; a cost attributed outside the rule is reported apart.
pub fn who_pays(ds: &Dataset, months: usize, rule_id: Option<&str>) -> Value {
    let p = &ds.people; let win: BTreeSet<String> = window(ds, months).into_iter().collect(); let txs: Vec<&Tx> = ds.txs.iter().filter(|t| win.contains(&t.month())).collect();
    let mut income: BTreeMap<String, i64> = BTreeMap::new(); for t in &txs { if let Some(x) = &t.person { if is_income(&t.category) { *income.entry(x.clone()).or_default() += t.amount_c; } } }
    let (mut claimed, mut rules, mut totals): (BTreeSet<(String, Option<usize>)>, Vec<Value>, BTreeMap<String, (i64, i64, i64)>) = Default::default();
    for a in p.allocations.iter().filter(|a| rule_id.is_none_or(|r| a.id == r)) {
        let mine: Vec<&&Tx> = txs.iter().filter(|t| !claimed.contains(&(t.key.clone(), t.split_index)) && alloc_matches(a, t, ds)).collect();
        for t in &mine { claimed.insert((t.key.clone(), t.split_index)); }
        let mut notes = vec![]; let mut among = if a.among.is_empty() { p.adults() } else { a.among.clone() };
        let weights: BTreeMap<String, f64> = match a.method.as_str() {
            "custom" => { among = a.shares.keys().cloned().collect(); a.shares.clone() }
            "income" => { let w: BTreeMap<String, f64> = among.iter().map(|m| (m.clone(), income.get(m).copied().unwrap_or(0).max(0) as f64)).collect(); if w.values().sum::<f64>() <= 0.0 { notes.push("no income is attributed to these members over the window: shared equally instead"); among.iter().map(|m| (m.clone(), 1.0)).collect() } else { w } }
            _ => among.iter().map(|m| (m.clone(), 1.0)).collect(),
        };
        let tw: f64 = weights.values().sum::<f64>().max(f64::MIN_POSITIVE);
        let cost = -mine.iter().map(|t| t.amount_c).sum::<i64>(); let joint = -mine.iter().filter(|t| t.person.as_deref() == Some(JOINT)).map(|t| t.amount_c).sum::<i64>();
        let paid: BTreeMap<String, i64> = among.iter().map(|m| (m.clone(), -mine.iter().filter(|t| t.person.as_deref() == Some(m)).map(|t| t.amount_c).sum::<i64>())).collect(); let personal: i64 = paid.values().sum();
        let (owed, fair, jshare) = (split_cents(cost, &weights), split_cents(personal, &weights), split_cents(joint, &weights));
        if mine.is_empty() { notes.push("no transaction matched this rule in the window"); }
        let rows: Vec<Value> = among.iter().map(|m| { let net = paid[m] - fair.get(m).copied().unwrap_or(0); let e = totals.entry(m.clone()).or_default(); e.0 += owed.get(m).copied().unwrap_or(0); e.1 += paid[m]; e.2 += net;
            json!({"member": m, "share_pct": (weights.get(m).copied().unwrap_or(0.0) / tw * 10000.0).round() / 100.0, "owed": money_str(owed.get(m).copied().unwrap_or(0)), "paid": money_str(paid[m]), "joint_share": money_str(jshare.get(m).copied().unwrap_or(0)), "net": money_str(net)}) }).collect();
        rules.push(json!({"id": a.id, "title": a.title, "method": a.method, "among": among, "members": rows, "total": money_str(cost), "joint_paid": money_str(joint), "personal_paid": money_str(personal), "unattributed": money_str(cost - joint - personal), "n": mine.len(),
            "income_basis": if a.method == "income" { json!(among.iter().map(|m| (m.clone(), json!(money_str(income.get(m).copied().unwrap_or(0))))).collect::<serde_json::Map<_, _>>()) } else { Value::Null },
            "evidence": mine.iter().map(|t| t.key.clone()).collect::<BTreeSet<_>>().into_iter().take(20).collect::<Vec<_>>(), "notes": notes}));
    }
    json!({"as_of": ds.today.to_string(), "window": {"months": win}, "rules": rules, "by_member": totals.iter().map(|(m, (o, pd, n))| json!({"member": m, "owed": money_str(*o), "paid": money_str(*pd), "net": money_str(*n)})).collect::<Vec<_>>(),
        "notes": if p.allocations.is_empty() { vec!["no allocation rule: add `allocations` to household.yaml (a memory proposal in the Python app)"] } else { vec![] }})
}

/// Members, account owners, rules, kid budgets, allocations and logins (the household page).
pub fn overview(ds: &Dataset, con: &Connection) -> Result<Value> {
    let p = &ds.people; let whole = ds.whole(); let mut counts: BTreeMap<String, usize> = BTreeMap::new(); let mut per_acc: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for t in &whole { let who = t.person.clone().unwrap_or_else(|| "unassigned".into()); *counts.entry(who.clone()).or_default() += 1; *per_acc.entry(t.account.clone()).or_default().entry(who).or_default() += 1; }
    let members: Vec<Value> = p.members.iter().map(|m| json!({"id": m.id, "name": m.name, "role": m.role, "birth_year": m.birth_year, "aliases": m.aliases, "pocket_money": m.pocket_money,
        "accounts": ds.accounts.values().filter(|a| p.resolve(a.owner.as_deref()).as_deref() == Some(m.id.as_str())).map(|a| a.uid.clone()).collect::<Vec<_>>(), "attributed_transactions": counts.get(&m.id).copied().unwrap_or(0)})).collect();
    let mut accs: Vec<_> = ds.accounts.values().collect(); accs.sort_by(|a, b| (a.bank.clone().unwrap_or_default(), &a.label).cmp(&(b.bank.clone().unwrap_or_default(), &b.label)));
    let accounts: Vec<Value> = accs.iter().map(|a| { let who = p.resolve(a.owner.as_deref()); json!({"uid": a.uid, "label": a.label, "bank": a.bank, "owner": a.owner, "owner_member": who.clone().filter(|w| p.get(w).is_some()), "joint": who.as_deref() == Some(JOINT), "owner_known": who.is_some(), "purpose": a.purpose, "attributed": per_acc.get(&a.uid).cloned().unwrap_or_default()}) }).collect();
    let manual: i64 = con.query_row("SELECT COUNT(*) FROM tx_person", [], |r| r.get(0)).unwrap_or(0);
    let users: Vec<Value> = con.prepare("SELECT id, role, member_id, created_at, disabled_at, prefs FROM ui_users ORDER BY id")?.query_map([], |r| Ok(json!({"id": r.get::<_, String>(0)?, "role": r.get::<_, String>(1)?, "member_id": r.get::<_, Option<String>>(2)?, "created_at": r.get::<_, Option<String>>(3)?, "disabled": r.get::<_, Option<String>>(4)?.is_some(), "prefs": r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!({}))})))?.collect::<rusqlite::Result<_>>()?;
    let mut warnings = p.warnings.clone();
    if p.is_empty() { warnings.push("no member is declared: add the household's members to household.yaml".into()); } else { for a in &accounts { if a["owner_known"] == false { warnings.push(format!("account {}: owner {:?} is not 'joint' or a declared member", a["label"].as_str().unwrap_or(""), a["owner"].as_str().unwrap_or("(none)"))); } } }
    Ok(json!({"as_of": ds.today.to_string(), "members": members, "accounts": accounts, "purposes": crate::ingest::accounts::PURPOSES, "attribution": {"counts": counts, "manual": manual},
        "rules": p.attribution, "kid_budgets": p.kid_budgets, "allocations": p.allocations, "users": users, "warnings": warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn cards() {
        assert_eq!(card_last4("BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 SHOP").as_deref(), Some("1234"));
        assert_eq!(card_last4("CARTE X4242 SHOP").as_deref(), Some("4242")); assert_eq!(card_last4("PAIEMENT 123456******7 X").as_deref(), Some("123456*7"));
        assert_eq!(card_last4("CB 0210 PARIS"), None, "a bare CB + digits is a date");
    }
    #[test] fn split_is_exact() { let w: BTreeMap<String, f64> = [("a".into(), 1.0), ("b".into(), 1.0), ("c".into(), 1.0)].into(); let s = split_cents(100, &w); assert_eq!(s.values().sum::<i64>(), 100); assert_eq!(s["a"], 34); assert_eq!(split_cents(-100, &w).values().sum::<i64>(), -100); }
    #[test] fn resolve_names() {
        let m = |id: &str, n: &str, r: &str| Member { id: id.into(), name: n.into(), role: r.into(), birth_year: None, aliases: vec![], pocket_money: None };
        let p = People::new(vec![m("zorro", "Zorro Testmans", "adult"), m("kiki", "Kiki Testmans", "child")], vec![], vec![], vec![]);
        assert_eq!(p.resolve(Some("ZORRO TESTMANS")).as_deref(), Some("zorro")); assert_eq!(p.resolve(Some("Joint")).as_deref(), Some(JOINT)); assert_eq!(p.resolve(Some("nobody")), None);
        assert_eq!(p.mentioned("OVERSCHRIJVING VAN KIKI"), ["kiki".to_string()].into()); assert!(p.mentioned("TESTMANS").is_empty(), "a shared surname names nobody");
    }
}
