//! Accounts: stable ids, IBAN hashes, labels / owner / purpose. `accounts.uid` is the STABLE internal id.
use crate::util::*;
use anyhow::{bail, Result};
use rusqlite::{params_from_iter, Connection, OptionalExtension};

pub const PURPOSES: [&str; 5] = ["main", "cards", "rental", "kids", "savings"];

pub fn normalize_iban(iban: Option<&str>) -> Option<String> { let s: String = iban.unwrap_or("").split_whitespace().collect::<String>().to_uppercase(); (!s.is_empty()).then_some(s) }
pub fn ident_hash(iban: Option<&str>, other: Option<&str>) -> Option<String> {
    if let Some(i) = normalize_iban(iban) { return Some(sha256_hex(format!("iban:{i}").as_bytes())); }
    other.map(|o| sha256_hex(format!("other:{}", o.trim().to_uppercase()).as_bytes()))
}
pub fn mask_iban(iban: Option<&str>) -> String { iban.map(|i| format!("…{}", &i[i.len().saturating_sub(4)..])).unwrap_or_default() }

pub fn ensure_hashes(con: &Connection) -> Result<()> {
    let rows: Vec<(String, String)> = con.prepare("SELECT uid, iban FROM accounts WHERE identification_hash IS NULL AND iban IS NOT NULL")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (uid, iban) in rows { if let Some(h) = ident_hash(Some(&iban), None) { con.execute("UPDATE accounts SET identification_hash=? WHERE uid=?", [&h, &uid])?; } }
    Ok(())
}

/// Stable uid for a uid, api_uid, label (case-insensitive) or unique uid prefix.
pub fn resolve_account(con: &Connection, ident: &str) -> Result<String> {
    if let Some(u) = con.query_row("SELECT uid FROM accounts WHERE uid=? OR api_uid=?", [ident, ident], |r| r.get(0)).optional()? { return Ok(u); }
    let col = |sql: &str, p: &str| -> Result<Vec<String>> { Ok(con.prepare(sql)?.query_map([p], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?) };
    let mut rows = col("SELECT uid FROM accounts WHERE LOWER(label)=LOWER(?)", ident)?;
    if rows.is_empty() && ident.chars().count() >= 4 { rows = col("SELECT uid FROM accounts WHERE uid LIKE ? ESCAPE '\\'", &format!("{}%", ident.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))?; }
    match rows.len() { 1 => Ok(rows.remove(0)), 0 => bail!("No account matches {ident:?} (see `finch accounts list`)"), _ => bail!("{ident:?} is ambiguous: {}", rows.join(", ")) }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountRow { pub uid: String, pub api_uid: Option<String>, pub bank: Option<String>, pub label: Option<String>, pub name: Option<String>, pub iban: Option<String>, pub currency: Option<String>, pub owner: Option<String>, pub purpose: Option<String>, pub exclude: bool, pub source: String, pub session_status: Option<String>, pub needs_review: bool, pub tx_count: i64 }

pub fn list_accounts(con: &Connection) -> Result<Vec<AccountRow>> {
    let mut st = con.prepare("SELECT a.uid, a.api_uid, a.bank, a.label, a.name, a.iban, a.currency, a.owner, a.purpose, a.exclude, a.source, s.status, a.needs_review,
        (SELECT COUNT(*) FROM transactions t WHERE t.account_uid=a.uid) FROM accounts a LEFT JOIN sessions s ON s.session_id=a.session_id ORDER BY COALESCE(a.bank, ''), COALESCE(a.label, a.name, a.uid)")?;
    let r = st.query_map([], |r| Ok(AccountRow { uid: r.get(0)?, api_uid: r.get(1)?, bank: r.get(2)?, label: r.get(3)?, name: r.get(4)?, iban: r.get(5)?, currency: r.get(6)?, owner: r.get(7)?, purpose: r.get(8)?, exclude: r.get::<_, i64>(9)? != 0, source: r.get(10)?, session_status: r.get(11)?, needs_review: r.get::<_, i64>(12)? != 0, tx_count: r.get(13)? }))?.collect::<rusqlite::Result<_>>()?;
    Ok(r)
}

#[derive(Default)]
pub struct AccountSet { pub label: Option<String>, pub owner: Option<String>, pub purpose: Option<String>, pub exclude: Option<bool>, pub resolve: bool }
pub fn set_account(con: &Connection, ident: &str, s: AccountSet, members: &[String]) -> Result<String> {
    let uid = resolve_account(con, ident)?;
    if let Some(p) = &s.purpose { if !PURPOSES.contains(&p.as_str()) { bail!("purpose must be one of {}", PURPOSES.join(", ")); } }
    let owner = match s.owner.as_deref().map(str::trim) {
        Some("") => bail!("owner must be 'joint' or a member id"), Some(o) if o.eq_ignore_ascii_case("joint") => Some("joint".to_string()),
        Some(o) if !members.is_empty() && !members.iter().any(|m| m == o) => bail!("owner {o:?} is neither 'joint' nor a household member ({})", members.join(", ")),
        o => o.map(String::from),
    };
    let label = s.label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
    if let Some(l) = &label { if let Some(o) = con.query_row("SELECT uid FROM accounts WHERE LOWER(label)=LOWER(?) AND uid<>?", [l, &uid], |r| r.get::<_, String>(0)).optional()? { bail!("label {l:?} is already used by account {o}"); } }
    let (mut sets, mut args): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for (c, v) in [("label", label), ("owner", owner), ("purpose", s.purpose)] { if let Some(v) = v { sets.push(format!("{c}=?")); args.push(v); } }
    if let Some(e) = s.exclude { sets.push(format!("exclude={}", e as i32)); }
    if s.resolve { sets.push("needs_review=0".into()); }
    if sets.is_empty() { bail!("nothing to set: pass --label, --owner, --purpose, --exclude, --include or --resolve"); }
    args.push(uid.clone()); con.execute(&format!("UPDATE accounts SET {} WHERE uid=?", sets.join(", ")), params_from_iter(args))?; Ok(uid)
}

pub fn format_accounts(rows: &[AccountRow]) -> String {
    let mut out = vec![format!("{:<38}{:<26}{:<22}{:<10}{:<9}{:<7}{:>6}  notes", "uid", "bank", "label", "owner", "purpose", "src", "tx")];
    for r in rows {
        let mut n = vec![];
        if r.exclude { n.push("EXCLUDED from analytics".to_string()); }
        if r.needs_review { n.push("NEEDS REVIEW (ambiguous after reconnect)".into()); }
        if let Some(a) = r.api_uid.as_ref().filter(|a| **a != r.uid) { n.push(format!("api uid {a}")); }
        if r.source == "api" && r.session_status.as_deref().is_some_and(|s| s != "active") { n.push(format!("session {}", r.session_status.as_deref().unwrap())); }
        if r.iban.is_some() { n.push(mask_iban(r.iban.as_deref())); }
        let t = |s: Option<&String>, w: usize| truncate_chars(s.map_or("-", |x| x.as_str()), w);
        out.push(format!("{:<38}{:<26}{:<22}{:<10}{:<9}{:<7}{:>6}  {}", r.uid, t(r.bank.as_ref(), 25), t(r.label.as_ref().or(r.name.as_ref()), 21), t(r.owner.as_ref(), 9), t(r.purpose.as_ref(), 8), r.source, r.tx_count, n.join("; ")));
    }
    out.join("\n")
}
