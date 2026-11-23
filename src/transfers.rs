//! Internal transfer matching: money moving between the household's own accounts. A pair is a debit on account A and a credit
//! on account B (A != B), same currency, same amount to the cent, booking dates at most `window_days` apart (`cross_window_days`
//! between two banks), neither leg linked. Linked automatically only when unambiguous both ways, both legs look like transfers,
//! neither is income.*, and the confidence reaches `min_confidence`; the rest are PROPOSALS or AMBIGUOUS. Card payments / refunds
//! and cash withdrawals are never candidates (except a card payment to a top-up merchant, only ever proposed).
//! Strong evidence: the other account's IBAN or holder name in a description, a member who owns the other account named in a leg,
//! or both legs recognised as own-account transfers.
use crate::{classify::{parsers::{belgian::transfer_counterparty, common::{account_holder_tokens, lead_tokens, Toks}}, rules::{annotation_mismatch, Lookups, resolve, Rules}}, memory::{self, Annotation, Member}, re, util::*};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

const NOT_TRANSFER: [&str; 3] = ["card", "card_refund", "atm"];
const TRANSFER_TYPES: [&str; 10] = ["transfer_in", "transfer_out", "internal_transfer", "wero_in", "wero_out", "person_transfer_in", "person_transfer_out", "savings_internal", "topup", "fx_exchange"];
const OWN_TYPES: [&str; 4] = ["internal_transfer", "savings_internal", "topup", "fx_exchange"];
const HINT: &str = r"(?i)\b(OVERSCHRIJVING|STORTING|DOORLOPENDE\s+OPDRACHT|SPAAROPDRACHT|VIR|VIREMENT|ORDRE\s+PERMANENT|TRANSFER|REVOLUT|TOP[- ]?UP|SPAARREKENING|COMPTE\s+D'?EPARGNE)\b";

#[derive(Debug, Clone)]
pub struct Leg { pub tx_key: String, pub account_uid: String, pub account: String, pub date: String, pub amount: f64, pub currency: String, pub description: String, pub tx_type: Option<String>, pub merchant_key: String, pub topup: bool, pub bank: Option<String> }
impl Leg {
    pub fn short(&self) -> String { format!("{} {:>10.2} {} {:<22} {}", self.date, self.amount, self.currency, truncate_chars(&self.account, 22), truncate_chars(&self.description, 48)) }
    fn looks_like_transfer(&self) -> bool { self.topup || self.tx_type.as_deref().is_some_and(|t| TRANSFER_TYPES.contains(&t)) || is(re!(HINT), &self.description) }
    fn own(&self) -> bool { self.tx_type.as_deref().is_some_and(|t| OWN_TYPES.contains(&t)) }
}
#[derive(Debug, Clone)]
pub struct Pair { pub debit: Leg, pub credit: Leg, pub days: i64, pub strong: bool, pub topup: bool, pub confidence: f64, pub household: bool, pub to_child: bool }
#[derive(Debug, Default)]
pub struct MatchResult { pub linked: Vec<Pair>, pub proposals: Vec<Pair>, pub ambiguous: Vec<(Leg, Vec<Leg>, Vec<String>)> }

pub struct Opts { pub window_days: i64, pub cross_window_days: i64, pub min_confidence: f64, pub topups: Vec<String> }
impl Opts { pub fn from(cfg: &crate::config::Config) -> Self { Self { window_days: cfg.transfer_window_days, cross_window_days: cfg.transfer_cross_bank_window_days, min_confidence: cfg.transfer_min_confidence, topups: cfg.transfer_topup_merchants.clone() } } }

/// Strong evidence 0.98 - 0.03 / day, weak (both legs merely look like transfers) 0.70 - 0.05 / day, clamped to [0.3, 0.99].
pub fn confidence(days: i64, strong: bool) -> f64 { let c = if strong { 0.98 - 0.03 * days as f64 } else { 0.70 - 0.05 * days as f64 }; (c.clamp(0.3, 0.99) * 100.0).round() / 100.0 }

fn legs(con: &Connection, topups: &[String]) -> Result<Vec<Leg>> {
    let top = (!topups.is_empty()).then(|| regex::Regex::new(&format!(r"\b({})\b", topups.iter().map(|t| escape(&t.to_uppercase())).collect::<Vec<_>>().join("|"))).unwrap());
    let mut st = con.prepare("SELECT t.tx_key, t.account_uid, COALESCE(a.label, a.name, a.uid), t.booking_date, t.amount, t.currency, t.description, e.tx_type, e.merchant_key, a.bank
        FROM transactions t LEFT JOIN accounts a ON a.uid=t.account_uid JOIN tx_enriched e ON e.tx_key=t.tx_key
        WHERE t.amount <> 0 AND t.booking_date IS NOT NULL AND e.tx_type IS NOT NULL AND t.tx_key NOT IN (SELECT out_tx_key FROM transfer_links) AND t.tx_key NOT IN (SELECT in_tx_key FROM transfer_links)")?;
    let rows = st.query_map([], |r| Ok(Leg { tx_key: r.get(0)?, account_uid: r.get(1)?, account: r.get::<_, Option<String>>(2)?.unwrap_or_default(), date: r.get(3)?, amount: r.get(4)?, currency: r.get::<_, Option<String>>(5)?.unwrap_or_default(), description: r.get::<_, Option<String>>(6)?.unwrap_or_default(), tx_type: r.get(7)?, merchant_key: r.get::<_, Option<String>>(8)?.unwrap_or_default(), topup: false, bank: r.get(9)? }))?;
    let mut out = vec![];
    for l in rows { let mut l = l?; let ty = l.tx_type.clone().unwrap_or_default();
        if ty == "card" && top.as_ref().is_some_and(|t| t.is_match(&if l.merchant_key.is_empty() { l.description.to_uppercase() } else { l.merchant_key.to_uppercase() })) { l.topup = true; } else if NOT_TRANSFER.contains(&ty.as_str()) { continue; }
        out.push(l); }
    Ok(out)
}
fn days(a: &str, b: &str) -> Option<i64> { Some((d(a)? - d(b)?).num_days().abs()) }

struct Ctx { ident: HashMap<String, (String, Toks)>, owners: HashMap<String, Option<String>>, members: Vec<Member> }
impl Ctx {
    fn load(con: &Connection, memory_dir: &std::path::Path) -> Result<Self> {
        let mut ident = HashMap::new(); let mut owners = HashMap::new();
        let mut st = con.prepare("SELECT uid, iban, name, owner, cash_account_type FROM accounts")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?)))? {
            let (u, iban, n, o, c) = r?; let toks = account_holder_tokens(n.as_deref(), o.as_deref(), c.as_deref());
            ident.insert(u.clone(), (iban.unwrap_or_default().split_whitespace().collect::<String>().to_uppercase(), if toks.len() < 2 { Toks::new() } else { toks })); owners.insert(u, o);
        }
        Ok(Self { ident, owners, members: memory::members(memory_dir) })
    }
    fn member_of(&self, uid: &str) -> Option<&Member> { let o = self.owners.get(uid)?.as_deref()?; self.members.iter().find(|m| m.id == o || m.name.eq_ignore_ascii_case(o) || m.aliases.iter().any(|a| a.eq_ignore_ascii_case(o))) }
    /// Every word of the member's name, or of one alias, appears in the text.
    fn mentions(text: &str, m: &Member) -> bool { let t: HashSet<String> = rs_words(text); std::iter::once(&m.name).chain(&m.aliases).any(|n| { let w = rs_words(n); !w.is_empty() && w.is_subset(&t) }) }
    fn member_evidence(&self, dl: &Leg, cl: &Leg) -> bool {
        let (od, oc) = (self.member_of(&dl.account_uid), self.member_of(&cl.account_uid));
        if let Some(c) = oc { if od.is_none_or(|x| x.id != c.id) && Self::mentions(&dl.description, c) { return true; } }
        if let Some(dm) = od { if oc.is_none_or(|x| x.id != dm.id) && Self::mentions(&cl.description, dm) { return true; } }
        false
    }
    fn strong(&self, dl: &Leg, cl: &Leg) -> bool {
        if dl.own() && cl.own() { return true; }
        if self.member_evidence(dl, cl) { return true; }
        for (leg, mine, other) in [(dl, &dl.account_uid, &cl.account_uid), (cl, &cl.account_uid, &dl.account_uid)] {
            let empty = (String::new(), Toks::new()); let (iban, toks) = self.ident.get(other).unwrap_or(&empty);
            let text = leg.description.to_uppercase();
            if !iban.is_empty() && text.split_whitespace().collect::<String>().contains(iban.as_str()) { return true; }
            let lead: Toks = lead_tokens(&transfer_counterparty(&text)).into_iter().take(toks.len() + 1).collect();
            if !toks.is_empty() && Some(toks) != self.ident.get(mine).map(|x| &x.1) && toks.is_subset(&lead) { return true; }
        }
        false
    }
}
fn rs_words(s: &str) -> HashSet<String> { crate::rs!(r"[^\w']+").split(&strip_accents(s).to_uppercase()).filter(|w| w.chars().count() >= 2).map(String::from).collect() }

pub fn find_pairs(con: &Connection, rules: &Rules, annotations: &[Annotation], memory_dir: &std::path::Path, o: &Opts) -> Result<MatchResult> {
    let legs = legs(con, &o.topups)?; let ctx = Ctx::load(con, memory_dir)?; let lk = Lookups::load(con)?;
    let cross = o.cross_window_days.max(o.window_days);
    let rejected: HashSet<(String, String)> = con.prepare("SELECT out_tx_key, in_tx_key FROM transfer_rejections")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut income: HashMap<String, bool> = HashMap::new();
    let mut is_income = |l: &Leg| *income.entry(l.tx_key.clone()).or_insert_with(|| {
        let (cat, _) = resolve(&lk, rules, &l.tx_key, l.tx_type.as_deref(), &l.merchant_key, None, Some(l.amount));
        let m = lk.merchants.get(&l.merchant_key).map(|x| x.0.clone()).unwrap_or_default();
        let ann = annotations.iter().find(|a| annotation_mismatch(a, &l.tx_key, &l.merchant_key, &l.description, &l.date, l.amount, &cat, None, &[]).is_none()).and_then(|a| a.category.clone()).unwrap_or_default();
        [cat, m, ann].iter().any(|c| c.starts_with("income."))
    });
    let pair = |dl: &Leg, cl: &Leg, n: i64, strong: bool| {
        let (od, oc) = (ctx.member_of(&dl.account_uid), ctx.member_of(&cl.account_uid));
        let topup = dl.topup || cl.topup; let mut c = confidence(n, strong); if topup { c = c.min(0.6); }
        Pair { debit: dl.clone(), credit: cl.clone(), days: n, strong, topup, confidence: c, household: matches!((od, oc), (Some(a), Some(b)) if a.id != b.id), to_child: oc.is_some_and(|m| m.role == "child") }
    };
    let mut credits: HashMap<(i64, String), Vec<&Leg>> = HashMap::new();
    for l in legs.iter().filter(|l| l.amount > 0.0) { credits.entry(((l.amount * 100.0).round() as i64, l.currency.clone())).or_default().push(l); }
    let mut cands = |pool: &HashSet<&str>, lo: i64, hi_same: i64, hi_cross: i64| -> Vec<(Leg, Leg, i64, bool)> {
        let mut out = vec![];
        for dl in legs.iter().filter(|l| l.amount < 0.0 && pool.contains(l.tx_key.as_str())) {
            for cl in credits.get(&((-dl.amount * 100.0).round() as i64, dl.currency.clone())).into_iter().flatten().filter(|c| pool.contains(c.tx_key.as_str())) {
                if cl.account_uid == dl.account_uid || rejected.contains(&(dl.tx_key.clone(), cl.tx_key.clone())) { continue; }
                let Some(n) = days(&dl.date, &cl.date) else { continue };
                let hi = if dl.bank.is_some() && cl.bank.is_some() && dl.bank != cl.bank { hi_cross } else { hi_same };
                if n > hi || n <= lo || !(dl.looks_like_transfer() && cl.looks_like_transfer()) || is_income(dl) || is_income(cl) { continue; }
                out.push((dl.clone(), (*cl).clone(), n, ctx.strong(dl, cl)));
            }
        }
        out
    };
    let all: HashSet<&str> = legs.iter().map(|l| l.tx_key.as_str()).collect();
    let first = cands(&all, -1, o.window_days, o.window_days);
    let (mut out_c, mut in_c): (BTreeMap<String, Vec<(Leg, i64, bool)>>, HashMap<String, Vec<String>>) = Default::default();
    let mut debits: HashMap<String, Leg> = HashMap::new();
    for (dl, cl, n, s) in first { in_c.entry(cl.tx_key.clone()).or_default().push(dl.tx_key.clone()); debits.insert(dl.tx_key.clone(), dl.clone()); out_c.entry(dl.tx_key).or_default().push((cl, n, s)); }
    let mut res = MatchResult::default(); let mut locked: HashSet<String> = HashSet::new();
    let mut order: Vec<&String> = out_c.keys().collect(); order.sort_by(|a, b| debits[*a].date.cmp(&debits[*b].date).then(a.cmp(b)));
    for dk in order {
        let cs = &out_c[dk]; let dl = &debits[dk];
        if cs.len() == 1 && in_c[&cs[0].0.tx_key].len() == 1 {
            let p = pair(dl, &cs[0].0, cs[0].1, cs[0].2); locked.extend([dl.tx_key.clone(), cs[0].0.tx_key.clone()]);
            if p.confidence >= o.min_confidence && !p.topup { res.linked.push(p) } else { res.proposals.push(p) }
        } else {
            let mut comp: Vec<String> = cs.iter().flat_map(|c| in_c[&c.0.tx_key].iter().filter(|x| *x != dk).cloned()).collect(); comp.sort(); comp.dedup();
            res.ambiguous.push((dl.clone(), cs.iter().map(|c| c.0.clone()).collect(), comp));
        }
    }
    if cross > o.window_days {
        let pool: HashSet<&str> = all.iter().copied().filter(|k| !locked.contains(*k)).collect();
        let mut wide: Vec<_> = cands(&pool, o.window_days, o.window_days, cross).into_iter().filter(|c| c.3).collect();
        wide.sort_by(|a, b| (a.2, &a.0.date, &a.0.tx_key, &a.1.tx_key).cmp(&(b.2, &b.0.date, &b.0.tx_key, &b.1.tx_key)));
        let mut used: HashSet<String> = HashSet::new();
        for (dl, cl, n, _) in wide {
            if used.contains(&dl.tx_key) || used.contains(&cl.tx_key) { continue; }
            let p = pair(&dl, &cl, n, true); used.extend([dl.tx_key.clone(), cl.tx_key.clone()]);
            if p.confidence >= o.min_confidence && !p.topup { res.linked.push(p) } else { res.proposals.push(p) }
        }
        res.ambiguous.retain(|g| !used.contains(&g.0.tx_key));
    }
    Ok(res)
}

/// Detect pairs; with `link` store those at or above the threshold (the default only proposes).
pub fn match_transfers(con: &Connection, rules: &Rules, ann: &[Annotation], memory_dir: &std::path::Path, o: &Opts, link: bool) -> Result<MatchResult> {
    let res = find_pairs(con, rules, ann, memory_dir, o)?;
    if link { for p in &res.linked { con.execute("INSERT INTO transfer_links(out_tx_key, in_tx_key, amount, confidence, method, created_at) VALUES (?,?,?,?, 'auto', ?)", params![p.debit.tx_key, p.credit.tx_key, p.debit.amount.abs(), p.confidence, now_iso()])?; } }
    Ok(res)
}
pub fn resolve_tx(con: &Connection, r: &str) -> Result<String> {
    if con.query_row("SELECT 1 FROM transactions WHERE tx_key=?", [r], |_| Ok(())).optional()?.is_some() { return Ok(r.into()); }
    let esc = r.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let rows: Vec<String> = con.prepare("SELECT tx_key FROM transactions WHERE tx_key LIKE ? ESCAPE '\\' LIMIT 3")?.query_map([format!("%{esc}%")], |x| x.get(0))?.collect::<rusqlite::Result<_>>()?;
    match rows.len() { 1 if r.len() >= 6 => Ok(rows[0].clone()), n if n > 1 => bail!("{r:?} matches several transactions; give more of the key"), _ => bail!("No transaction matches {r:?}") }
}
pub fn link(con: &Connection, out_ref: &str, in_ref: &str, allow_mismatch: bool) -> Result<i64> {
    let (ok, ik) = (resolve_tx(con, out_ref)?, resolve_tx(con, in_ref)?);
    let q = |k: &str| con.query_row("SELECT account_uid, amount FROM transactions WHERE tx_key=?", [k], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)));
    let (o, i) = (q(&ok)?, q(&ik)?);
    if o.1 >= 0.0 || i.1 <= 0.0 { bail!("the first transaction must be the debit (negative) and the second the credit (positive)"); }
    if o.0 == i.0 { bail!("both transactions are on the same account"); }
    if (-o.1 * 100.0).round() != (i.1 * 100.0).round() && !allow_mismatch { bail!("amounts differ ({} vs {}); use --allow-amount-mismatch for a transfer with fees or a currency change", o.1, i.1); }
    for k in [&ok, &ik] { if let Some(id) = con.query_row("SELECT id FROM transfer_links WHERE out_tx_key=? OR in_tx_key=?", [k, k], |r| r.get::<_, i64>(0)).optional()? { bail!("{k} is already part of link #{id} (unlink it first)"); } }
    con.execute("INSERT INTO transfer_links(out_tx_key, in_tx_key, amount, confidence, method, created_at) VALUES (?,?,?,1.0,'manual',?)", params![ok, ik, o.1.abs(), now_iso()])?;
    let id = con.last_insert_rowid(); con.execute("DELETE FROM transfer_rejections WHERE out_tx_key=? AND in_tx_key=?", [&ok, &ik])?; Ok(id)
}
/// Remove a link (by id or either leg); the pair is remembered so matching never proposes it again.
pub fn unlink(con: &Connection, r: &str) -> Result<(i64, String, String)> {
    let q = "SELECT id, out_tx_key, in_tx_key FROM transfer_links WHERE ";
    let row = r.parse::<i64>().ok().and_then(|id| con.query_row(&format!("{q}id=?"), [id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional().ok().flatten())
        .or(con.query_row(&format!("{q}out_tx_key=? OR in_tx_key=?"), [r, r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?);
    let Some(row): Option<(i64, String, String)> = row else { bail!("No transfer link matches {r:?} (see `finch transfers list`)") };
    con.execute("DELETE FROM transfer_links WHERE id=?", [row.0])?; con.execute("INSERT OR REPLACE INTO transfer_rejections VALUES (?,?,?)", params![row.1, row.2, now_iso()])?; Ok(row)
}
pub fn list_links(con: &Connection) -> Result<Vec<Value>> {
    let mut st = con.prepare("SELECT l.id, l.amount, l.confidence, l.method, o.booking_date, COALESCE(ao.label, ao.name, ao.uid), i.booking_date, COALESCE(ai.label, ai.name, ai.uid) FROM transfer_links l
        LEFT JOIN transactions o ON o.tx_key=l.out_tx_key LEFT JOIN accounts ao ON ao.uid=o.account_uid LEFT JOIN transactions i ON i.tx_key=l.in_tx_key LEFT JOIN accounts ai ON ai.uid=i.account_uid ORDER BY COALESCE(o.booking_date, ''), l.id")?;
    let r = st.query_map([], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "amount": r.get::<_, f64>(1)?, "confidence": r.get::<_, f64>(2)?, "method": r.get::<_, String>(3)?, "out_date": r.get::<_, Option<String>>(4)?, "out_account": r.get::<_, Option<String>>(5)?, "in_date": r.get::<_, Option<String>>(6)?, "in_account": r.get::<_, Option<String>>(7)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(r)
}
pub fn format_result(r: &MatchResult, linked_now: bool) -> String {
    let mut l = vec![];
    for (kind, p) in r.linked.iter().map(|p| (if linked_now { "linked" } else { "auto-linkable" }, p)).chain(r.proposals.iter().map(|p| ("proposal", p))) {
        l.push(format!("{:.2} {kind} ({} evidence{}{}, {}d apart)\n  OUT {}\n      key {}\n  IN  {}\n      key {}", p.confidence, if p.strong { "strong" } else { "weak" }, if p.topup { ", card top-up" } else { "" },
            if p.household { if p.to_child { ", household transfer to a child" } else { ", household transfer" } } else { "" }, p.days, p.debit.short(), p.debit.tx_key, p.credit.short(), p.credit.tx_key));
    }
    for (dl, cs, comp) in &r.ambiguous { l.push(format!("ambiguous\n  OUT {}\n      key {}{}{}", dl.short(), dl.tx_key, cs.iter().map(|c| format!("\n    IN  {}\n      key {}", c.short(), c.tx_key)).collect::<String>(), if comp.is_empty() { String::new() } else { format!("\n      (also claimed by {} other debit(s))", comp.len()) })); }
    if l.is_empty() { "no transfer candidates".into() } else { l.join("\n\n") }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap(); crate::db::apply_migrations(&c, false, None).unwrap();
        c.execute_batch("INSERT INTO accounts(uid, iban, bank, label, source) VALUES ('a','BE68539007547034','KBC','Zicht','import'),('b','BE12345678901234','KBC','Spaar','import'),('c',NULL,'Belfius','Andere','import');").unwrap(); c
    }
    fn tx(c: &Connection, k: &str, acc: &str, date: &str, amt: f64, desc: &str, ty: &str) {
        c.execute("INSERT INTO transactions(tx_key, account_uid, booking_date, amount, currency, description) VALUES (?,?,?,?, 'EUR', ?)", params![k, acc, date, amt, desc]).unwrap();
        c.execute("INSERT INTO tx_enriched(tx_key, tx_type, merchant_key) VALUES (?,?,'')", [k, ty]).unwrap();
    }
    #[test] fn pairs() {
        let c = db(); let dir = std::env::temp_dir(); let o = Opts { window_days: 3, cross_window_days: 5, min_confidence: 0.85, topups: vec!["REVOLUT".into()] };
        tx(&c, "o1", "a", "2025-03-01", -200.0, "OVERSCHRIJVING NAAR BE12 3456 7890 1234", "transfer_out"); tx(&c, "i1", "b", "2025-03-01", 200.0, "OVERSCHRIJVING VAN ZICHT", "transfer_in");
        tx(&c, "o2", "a", "2025-03-05", -50.0, "OVERSCHRIJVING NAAR X", "transfer_out"); tx(&c, "i2", "c", "2025-03-06", 50.0, "STORTING", "transfer_in");
        tx(&c, "o3", "a", "2025-03-07", -30.0, "BETALING MET DEBETKAART SHOP", "card"); tx(&c, "i3", "b", "2025-03-07", 30.0, "OVERSCHRIJVING VAN Y", "transfer_in");
        let r = match_transfers(&c, &Rules::defaults(), &[], &dir, &o, true).unwrap();
        assert_eq!(r.linked.len(), 1, "strong (IBAN of the other account) pair is linked"); assert_eq!(r.linked[0].debit.tx_key, "o1"); assert_eq!(r.linked[0].confidence, 0.98);
        assert_eq!(r.proposals.len(), 1, "weak pair only proposed"); assert_eq!(r.proposals[0].credit.tx_key, "i2");
        assert_eq!(c.query_row("SELECT COUNT(*) FROM transfer_links", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let (id, ..) = unlink(&c, "o1").unwrap(); assert!(id > 0);
        assert!(match_transfers(&c, &Rules::defaults(), &[], &dir, &o, false).unwrap().linked.is_empty(), "a rejected pair is never proposed again");
        assert!(link(&c, "i3", "o3", false).is_err());
    }
}
