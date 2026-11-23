//! Enable Banking: the REST client (RS256 JWT, the private key never leaves the machine), the consent lifecycle, the connect /
//! finish flow (the bank redirects the browser; you paste the redirect address: `finch bank finish '<url>'`, the code is single-use
//! and the /sessions answer is saved first so `--replay STATE` can store it again), session storage with one-to-one account
//! matching across reconnects (an ambiguous match is created `needs_review`, never merged silently), and the transaction sync
//! (pagination, dedup by bank reference or content, pending replacement, balances, a daily limit per account).
use super::{accounts::{ensure_hashes, ident_hash, normalize_iban, resolve_account}, fingerprint::*};
use crate::{config::Config, egress, net::Http, util::*};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct ApiError { pub status: u16, pub body: String }
impl std::fmt::Display for ApiError { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "HTTP {}: {}", self.status, truncate_chars(&self.body, 300)) } }
impl std::error::Error for ApiError {}

pub struct Client<'a> { pub app_id: String, pub key_pem: Vec<u8>, pub api_url: String, pub http: &'a dyn Http, pub cfg: &'a Config, pub con: &'a Connection, /** tests: a fixed bearer token instead of a signed JWT */ pub token_override: Option<String> }
fn purpose(method: &str, path: &str) -> &'static str { if method == "DELETE" { return "revoke"; } match path.trim_start_matches('/').split('/').next().unwrap_or("") { "auth" | "sessions" => "connect", "accounts" => "sync", "aspsps" => "banks", "application" => "check", _ => "api" } }
impl<'a> Client<'a> {
    pub fn from_config(cfg: &'a Config, con: &'a Connection, http: &'a dyn Http) -> Result<Self> {
        let (Some(app), Some(key)) = (&cfg.eb_app_id, &cfg.eb_private_key_path) else { bail!("Enable Banking is not configured: set [enable_banking] app_id and private_key_path in config.toml") };
        let key_pem = std::fs::read(crate::config::expand_path(key)).with_context(|| format!("cannot read the Enable Banking private key {key}"))?;
        Ok(Self { app_id: app.clone(), key_pem, api_url: cfg.eb_api_url.trim_end_matches('/').into(), http, cfg, con, token_override: None })
    }
    pub fn token(&self) -> Result<String> {
        if let Some(t) = &self.token_override { return Ok(t.clone()); }
        let now = Utc::now().timestamp(); let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256); h.kid = Some(self.app_id.clone());
        Ok(jsonwebtoken::encode(&h, &json!({"iss": "enablebanking.com", "aud": "api.enablebanking.com", "iat": now, "exp": now + 3600}), &jsonwebtoken::EncodingKey::from_rsa_pem(&self.key_pem).context("the Enable Banking private key is not an RSA PEM key")?)?)
    }
    pub fn call(&self, method: &str, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Value> {
        egress::allow(Some(self.con), self.cfg, "enable_banking", &egress::host_of(&self.api_url), body.map_or(0, |b| b.to_string().len()), purpose(method, path))?;
        let mut u = url::Url::parse(&format!("{}{path}", self.api_url))?; if !query.is_empty() { u.query_pairs_mut().extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str()))); }
        let r = self.http.send(method, u.as_str(), &[("Authorization", format!("Bearer {}", self.token()?))], body)?;
        if r.status >= 400 { return Err(ApiError { status: r.status, body: r.body }.into()); }
        r.json()
    }
}

// ---------------------------------------------------------------- consents
pub fn parse_ts(v: Option<&str>) -> Option<DateTime<Utc>> { let v = v?; DateTime::parse_from_rfc3339(&v.replace(' ', "T")).map(|d| d.with_timezone(&Utc)).ok().or_else(|| chrono::NaiveDateTime::parse_from_str(v.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok().map(|n| n.and_utc())) }
/// (status, days left): revoked | expired | urgent (<= 3 days) | unknown (live check not AUTHORIZED) | expiring (<= 14) | ok | replaced.
pub fn classify_consent(valid_until: Option<&str>, status: &str, live: Option<&str>, now: DateTime<Utc>) -> (&'static str, Option<i64>) {
    let left = parse_ts(valid_until).map(|d| (d - now).num_seconds().div_euclid(86400)); let live = live.unwrap_or("").to_uppercase();
    if status == "replaced" { return ("replaced", left); }
    if status == "revoked" || ["REVOKED", "CANCELLED", "CLOSED"].contains(&live.as_str()) { return ("revoked", left); }
    if ["EXPIRED", "INVALID"].contains(&live.as_str()) || left.is_some_and(|l| l < 0) { return ("expired", left); }
    let unverified = !live.is_empty() && live != "AUTHORIZED";
    match left { None => (if unverified { "unknown" } else { "ok" }, None), Some(l) if l <= 3 => ("urgent", left), Some(_) if unverified => ("unknown", left), Some(l) if l <= 14 => ("expiring", left), _ => ("ok", left) }
}
#[derive(Debug, Clone)]
pub struct Consent { pub session_id: String, pub bank: String, pub country: String, pub valid_until: Option<String>, pub days_left: Option<i64>, pub status: &'static str, pub live_status: Option<String>, pub accounts: i64 }
pub fn list_consents(con: &Connection, include_replaced: bool) -> Result<Vec<Consent>> {
    let now = Utc::now();
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, String, Option<String>, i64)> = con.prepare("SELECT s.session_id, s.aspsp_name, s.aspsp_country, s.valid_until, s.status, s.live_status, (SELECT COUNT(*) FROM accounts a WHERE a.session_id=s.session_id) FROM sessions s ORDER BY s.aspsp_name, s.created_at")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "active".into()), r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows.into_iter().filter_map(|(sid, b, c, vu, st, live, n)| { let (s, left) = classify_consent(vu.as_deref(), &st, live.as_deref(), now); (include_replaced || s != "replaced").then(|| Consent { session_id: sid, bank: b.unwrap_or_else(|| "?".into()), country: c.unwrap_or_default(), valid_until: vu, days_left: left, status: s, live_status: live, accounts: n }) }).collect())
}
/// GET /sessions/{id} of every non-replaced session; a failure on one never stops the others.
pub fn refresh_live(c: &Client) -> Result<Vec<String>> {
    let mut out = vec![];
    for (sid, name) in c.con.prepare("SELECT session_id, aspsp_name FROM sessions WHERE status <> 'replaced'")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()? {
        match c.call("GET", &format!("/sessions/{sid}"), &[], None) {
            Ok(live) => { let st = live["status"].as_str().map(str::to_uppercase); c.con.execute("UPDATE sessions SET live_status=?, live_checked_at=?, valid_until=COALESCE(?, valid_until) WHERE session_id=?", params![st, now_iso(), live["access"]["valid_until"].as_str(), sid])?; }
            Err(e) => { let note = e.downcast_ref::<ApiError>().map_or_else(|| "network error".to_string(), |a| format!("error {}", a.status)); c.con.execute("UPDATE sessions SET live_status=?, live_checked_at=? WHERE session_id=?", params![note, now_iso(), sid])?; out.push(format!("consent {}: live check failed ({note})", name.unwrap_or_default())); }
        }
    }
    Ok(out)
}
pub fn format_consents(cs: &[Consent]) -> String {
    let mut l = vec![format!("{:<30}{:<5}{:<10}{:>10}  valid until / session", "bank", "cty", "status", "days left")];
    l.extend(cs.iter().map(|c| format!("{:<30}{:<5}{:<10}{:>10}  {}  {}{}", truncate_chars(&c.bank, 29), c.country, c.status, c.days_left.map_or("?".into(), |d| d.to_string()), c.valid_until.as_deref().unwrap_or("-"), c.session_id, c.live_status.as_ref().filter(|l| *l != "AUTHORIZED").map_or(String::new(), |l| format!("  [bank says: {l}]")))));
    l.join("\n")
}

// ---------------------------------------------------------------- connect / finish
const CLASH: &str = "A bank allows few concurrent authorisations per application and user: a second consent from the same Enable Banking app can invalidate the first one. Keep one consent owner per bank.";
pub fn find_session(con: &Connection, target: &str) -> Result<Consent> {
    let cs = list_consents(con, false)?; if let Some(c) = cs.iter().find(|c| c.session_id == target) { return Ok(c.clone()); }
    let low = target.to_lowercase(); let mut same: Vec<&Consent> = cs.iter().filter(|c| c.bank.to_lowercase() == low).collect(); if same.is_empty() { same = cs.iter().filter(|c| c.bank.to_lowercase().contains(&low)).collect(); }
    match same.len() { 1 => Ok(same[0].clone()), 0 => bail!("No bank session matches {target:?} (see `finch bank consents`)"), _ => bail!("{target:?} matches several sessions: {}; pass the session id", same.iter().map(|c| format!("{} ({})", c.bank, c.session_id)).collect::<Vec<_>>().join(", ")) }
}
/// Start an authorisation: (state, the bank's login URL). Refuses a second consent for a bank that already has a live one.
pub fn start_auth(c: &Client, bank: &str, country: &str, days: i64, replaces: Option<&str>, replace: bool) -> Result<(String, String)> {
    let redirect = c.cfg.eb_redirect_url.clone().ok_or_else(|| anyhow::anyhow!("Set [enable_banking] redirect_url in config.toml (it must be whitelisted in your Enable Banking app)"))?;
    if replaces.is_none() { if let Some(x) = list_consents(c.con, false)?.into_iter().find(|x| x.bank.eq_ignore_ascii_case(bank) && x.country.eq_ignore_ascii_case(country) && !["expired", "revoked"].contains(&x.status)) { if !replace { bail!("{} ({}) already has an active consent (session {}, valid until {}). Refusing to start a second one.\n{CLASH}\nRenew it with `finch bank reconnect \"{}\"`, or pass --replace.", x.bank, x.country, x.session_id, x.valid_until.unwrap_or_default(), x.bank); } } }
    let state = hex::encode(rand::random::<[u8; 16]>()); let until = (Utc::now() + Duration::days(days)).to_rfc3339();
    let res = c.call("POST", "/auth", &[], Some(&json!({"access": {"valid_until": until}, "aspsp": {"name": bank, "country": country}, "state": state, "redirect_url": redirect, "psu_type": "personal"})))?;
    c.con.execute("INSERT INTO pending_auth(state, aspsp_name, aspsp_country, created_at, replaces_session_id, valid_until_requested) VALUES (?,?,?,?,?,?)", params![state, bank, country, now_iso(), replaces, until])?;
    Ok((state, res["url"].as_str().ok_or_else(|| anyhow::anyhow!("the bank's /auth answer has no url"))?.into()))
}
/// (code, state) from the pasted redirect address (quotes, wraps and shell backslashes tolerated) or a bare code.
pub fn parse_finish_arg(con: &Connection, text: &str) -> Result<(String, Option<String>)> {
    let t = text.trim().trim_matches(['\'', '"', '`', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}']).trim();
    if !t.to_lowercase().starts_with("http") { if t.contains(['?', '=', '/']) { bail!("that is not a bare code and not an address starting with https://: paste the whole address the browser shows after the bank login, in single quotes"); } return Ok((t.into(), None)); }
    let clean: String = t.split_whitespace().collect::<String>().replace("\\?", "?").replace("\\=", "=").replace("\\&", "&");
    let u = url::Url::parse(&clean).context("not a valid address")?; let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
    let state = q.get("state").cloned();
    if let Some(s) = &state { if con.query_row("SELECT 1 FROM pending_auth WHERE state=?", [s], |_| Ok(())).optional()?.is_none() { bail!("Unknown state in the redirect address: not started by this tool"); } }
    if let Some(e) = q.get("error") { bail!("The bank returned an error ({}): nothing was connected. Run connect again.", truncate_chars(e, 60)); }
    Ok((q.get("code").cloned().ok_or_else(|| anyhow::anyhow!("the address has no ?code=... part: paste the WHOLE address the browser shows after the bank login"))?, state))
}
/// Exchange the single-use code for a session and store it; the raw answer is saved BEFORE it is interpreted (replayable).
pub fn complete_auth(c: &Client, code: &str, state: Option<&str>) -> Result<Value> {
    let (state, replaces, fallback) = match state {
        Some(s) => {
            let row: (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) = c.con.query_row("SELECT completed_at, replaces_session_id, created_at, aspsp_name, aspsp_country, valid_until_requested FROM pending_auth WHERE state=?", [s], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?.ok_or_else(|| anyhow::anyhow!("Unknown state: not started by this tool"))?;
            if let Some(d) = row.0 { bail!("This authorisation was already completed on {d}"); }
            if parse_ts(row.2.as_deref()).is_some_and(|d| Utc::now() - d > Duration::hours(24)) { bail!("This authorisation was started more than 24 h ago: it expired. Start again with `finch bank connect` / `reconnect`."); }
            if c.con.execute("UPDATE pending_auth SET completed_at=? WHERE state=? AND completed_at IS NULL", params![now_iso(), s])? != 1 { bail!("This authorisation is already being completed elsewhere"); }
            (s.to_string(), row.1, json!({"bank": row.3, "country": row.4, "valid_until": row.5}))
        }
        None => { let s = format!("adhoc-{}", hex::encode(rand::random::<[u8; 8]>())); c.con.execute("INSERT INTO pending_auth(state, created_at, completed_at) VALUES (?,?,?)", params![s, now_iso(), now_iso()])?; (s, None, json!({})) }
    };
    let s = match c.call("POST", "/sessions", &[], Some(&json!({"code": code}))) {
        Ok(s) => s,
        Err(e) => { if state.starts_with("adhoc-") { c.con.execute("DELETE FROM pending_auth WHERE state=?", [&state])?; } else { c.con.execute("UPDATE pending_auth SET completed_at=NULL WHERE state=?", [&state])?; }
            if e.downcast_ref::<ApiError>().is_some_and(|a| [400, 401, 403, 404, 422].contains(&a.status)) { bail!("the bank rejected the authorisation code: it is single-use and expires within minutes. Run `finch bank connect` again"); } return Err(e); }
    };
    c.con.execute("UPDATE pending_auth SET session_response=? WHERE state=?", params![s.to_string(), state])?;
    store_saved(c.con, &state, &s, replaces.as_deref(), &fallback)
}
fn store_saved(con: &Connection, state: &str, s: &Value, replaces: Option<&str>, fb: &Value) -> Result<Value> {
    con.execute_batch("BEGIN")?;
    match store_session(con, s, replaces, fb) {
        Ok(info) => { con.execute("UPDATE pending_auth SET session_response=NULL WHERE state=?", [state])?; con.execute_batch("COMMIT")?; Ok(info) }
        Err(e) => { let _ = con.execute_batch("ROLLBACK"); bail!("the bank created session {} but storing it failed ({e}). The answer is saved: fix the cause and run `finch bank finish --replay {state}`", s["session_id"].as_str().unwrap_or("?")) }
    }
}
/// Store the /sessions answer saved by a failed completion (no bank call).
pub fn replay_auth(con: &Connection, state: &str) -> Result<Value> {
    let (resp, rep, b, c, v): (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) = con.query_row("SELECT session_response, replaces_session_id, aspsp_name, aspsp_country, valid_until_requested FROM pending_auth WHERE state=?", [state], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?.ok_or_else(|| anyhow::anyhow!("Unknown state"))?;
    let resp = resp.ok_or_else(|| anyhow::anyhow!("Nothing to replay for this state (no saved session response)"))?;
    store_saved(con, state, &serde_json::from_str(&resp)?, rep.as_deref(), &json!({"bank": b, "country": c, "valid_until": v}))
}

fn fold_u(x: Option<&str>) -> String { collapse(x.unwrap_or("")).to_uppercase() }
/// Persist a session and its accounts; matching onto existing records is one-to-one and conservative.
pub fn store_session(con: &Connection, s: &Value, replaces: Option<&str>, fb: &Value) -> Result<Value> {
    ensure_hashes(con)?;
    let sid = s["session_id"].as_str().ok_or_else(|| anyhow::anyhow!("the bank's session response has no session_id"))?;
    let bank = s["aspsp"]["name"].as_str().or(fb["bank"].as_str()).ok_or_else(|| anyhow::anyhow!("the session response names no bank"))?; let country = s["aspsp"]["country"].as_str().or(fb["country"].as_str()).unwrap_or("");
    let until = s["access"]["valid_until"].as_str().or(fb["valid_until"].as_str());
    con.execute("INSERT INTO sessions(session_id, aspsp_name, aspsp_country, valid_until, created_at, raw, status) VALUES (?,?,?,?,?,?, 'active') ON CONFLICT(session_id) DO UPDATE SET aspsp_name=excluded.aspsp_name, aspsp_country=excluded.aspsp_country, valid_until=excluded.valid_until, raw=excluded.raw, status='active', replaced_by=NULL",
        params![sid, bank, country, until, now_iso(), s.to_string()])?;
    // sessions whose accounts this one may take over: itself, the one it replaces, dead sessions of the same bank and country
    let mut eligible: HashSet<String> = [sid.to_string()].into(); if let Some(r) = replaces { eligible.insert(r.into()); }
    for c in list_consents(con, true)? { if c.session_id != sid && fold_u(Some(&c.bank)) == fold_u(Some(bank)) && fold_u(Some(&c.country)) == fold_u(Some(country)) && ["expired", "revoked", "replaced"].contains(&c.status) { eligible.insert(c.session_id); } }
    let (mut out, mut touched, mut claimed, mut skipped) = (vec![], HashSet::new(), HashSet::new(), 0);
    for acc in s["accounts"].as_array().into_iter().flatten() {
        let Some(new_uid) = acc["uid"].as_str() else { skipped += 1; continue };
        let iban = acc["account_id"]["iban"].as_str(); let other = acc["account_id"]["other"]["identification"].as_str(); let h = ident_hash(iban, other);
        let name = acc["name"].as_str().or(acc["product"].as_str()); let (cur, typ) = (acc["currency"].as_str(), acc["cash_account_type"].as_str());
        let mut existing = con.query_row("SELECT uid FROM accounts WHERE (uid=? OR api_uid=?) AND source='api'", [new_uid, new_uid], |r| r.get::<_, String>(0)).optional()?.filter(|u| !claimed.contains(u));
        let mut ambiguous: Vec<String> = vec![];
        if existing.is_none() {
            let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = con.prepare("SELECT uid, name, currency, cash_account_type, identification_hash, session_id FROM accounts WHERE source='api'")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
            let (mut exact, mut wild) = (vec![], vec![]);
            for (u, n, c, t, oh, osid) in rows {
                if !osid.as_ref().is_some_and(|x| eligible.contains(x)) || claimed.contains(&u) || (cur.is_some() && c.is_some() && cur != c.as_deref()) || (typ.is_some() && t.is_some() && typ != t.as_deref()) { continue; }
                match &h { Some(h) => if oh.as_ref() != Some(h) { continue }, None => if oh.is_some() || name.is_none() || fold_u(n.as_deref()) != fold_u(name) { continue } }
                if (c.as_deref(), t.as_deref()) == (cur, typ) { exact.push(u) } else { wild.push(u) }
            }
            let cands = if exact.is_empty() { wild } else { exact };
            if cands.len() == 1 { existing = Some(cands[0].clone()); } else { ambiguous = cands; }
        }
        if let Some(ex) = existing {
            claimed.insert(ex.clone());
            if let Some(o) = con.query_row("SELECT session_id FROM accounts WHERE uid=?", [&ex], |r| r.get::<_, Option<String>>(0))?.filter(|o| o != sid) { touched.insert(o); }
            con.execute("UPDATE accounts SET session_id=?, api_uid=?, name=COALESCE(?, name), iban=COALESCE(?, iban), currency=COALESCE(?, currency), cash_account_type=COALESCE(?, cash_account_type), raw=?, bank=?, identification_hash=COALESCE(?, identification_hash) WHERE uid=?",
                params![sid, (new_uid != ex).then_some(new_uid), name, normalize_iban(iban), cur, typ, acc.to_string(), bank, h, ex])?;
            out.push(json!({"uid": ex, "api_uid": new_uid, "name": name, "remapped": new_uid != ex, "new": false, "needs_review": false}));
        } else {
            con.execute("INSERT INTO accounts(uid, session_id, name, iban, currency, cash_account_type, raw, bank, source, identification_hash, needs_review) VALUES (?,?,?,?,?,?,?,?, 'api', ?, ?)", params![new_uid, sid, name, normalize_iban(iban), cur, typ, acc.to_string(), bank, h, !ambiguous.is_empty() as i64])?;
            claimed.insert(new_uid.to_string()); for u in &ambiguous { con.execute("UPDATE accounts SET needs_review=1 WHERE uid=?", [u])?; }
            out.push(json!({"uid": new_uid, "api_uid": new_uid, "name": name, "remapped": false, "new": true, "needs_review": !ambiguous.is_empty(), "candidates": ambiguous}));
        }
    }
    let mut cands: Vec<String> = touched.into_iter().collect(); if let Some(r) = replaces.filter(|r| *r != sid) { cands.push(r.into()); } cands.sort(); cands.dedup();
    let mut retired = vec![];
    for old in &cands {
        if con.query_row("SELECT 1 FROM sessions WHERE session_id=?", [old], |_| Ok(())).optional()?.is_none() { continue; }
        let left: i64 = con.query_row("SELECT COUNT(*) FROM accounts WHERE session_id=?", [old], |r| r.get(0))?;
        if left == 0 || Some(old.as_str()) == replaces { con.execute("UPDATE sessions SET status='replaced', replaced_by=? WHERE session_id=?", [sid, old])?; retired.push(old.clone()); }
    }
    let mut orphans = vec![]; for old in &cands { orphans.extend(con.prepare("SELECT uid FROM accounts WHERE session_id=?")?.query_map([old], |r| r.get::<_, String>(0))?.flatten()); }
    Ok(json!({"session_id": sid, "bank": bank, "country": country, "valid_until": until, "accounts": out, "retired": retired, "orphans": orphans, "skipped_accounts": skipped}))
}

// ---------------------------------------------------------------- sync
fn amount(tx: &Value) -> (f64, String) { let ta = &tx["transaction_amount"]; let a = ta["amount"].as_f64().or_else(|| ta["amount"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0); (if tx["credit_debit_indicator"] == "DBIT" { -a.abs() } else { a }, ta["currency"].as_str().unwrap_or("").into()) }
fn counterparty(tx: &Value, a: f64) -> String { tx[if a < 0.0 { "creditor" } else { "debtor" }]["name"].as_str().unwrap_or("").into() }
fn description(tx: &Value) -> String { match &tx["remittance_information"] { Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | "), Value::String(s) => s.clone(), _ => String::new() } }
fn btc(tx: &Value) -> String { let b = &tx["bank_transaction_code"]; b["code"].as_str().or(b["description"].as_str()).or(b.as_str()).unwrap_or("").into() }
/// The transaction as a booked row (BOOK or no status; date from value / transaction date when needed), or None.
fn booked(tx: &Value) -> Option<Value> {
    if !matches!(tx["status"].as_str(), None | Some("") | Some("BOOK")) { return None; }
    if tx["booking_date"].as_str().is_some_and(|d| !d.is_empty()) { return Some(tx.clone()); }
    let day = tx["value_date"].as_str().or(tx["transaction_date"].as_str())?; let mut t = tx.clone(); t["booking_date"] = day.into(); Some(t)
}
fn enriches(old: Option<&str>, new: Option<&str>) -> bool { let (o, n) = (norm_desc(old), norm_desc(new)); !o.is_empty() && (n.starts_with(&o) || o.split(' ').all(|w| n.split(' ').any(|x| x == w))) }
fn same_tx(stored: &(Option<String>, f64, Option<String>), bd: Option<&str>, a: f64, desc: Option<&str>) -> bool {
    if (stored.1 * 100.0).round() != (a * 100.0).round() || !(enriches(stored.2.as_deref(), desc) || enriches(desc, stored.2.as_deref())) { return false; }
    matches!((stored.0.as_deref().and_then(d), bd.and_then(d)), (Some(x), Some(y)) if (x - y).num_days().abs() <= 1)
}
fn differs(s: &(Option<String>, f64, Option<String>), bd: Option<&str>, a: f64, desc: Option<&str>) -> bool { content_key(s.0.as_deref().unwrap_or(""), s.1, s.2.as_deref()) != content_key(bd.unwrap_or(""), a, desc) }
pub fn tx_key(uid: &str, tx: &Value) -> String {
    let rf = tx["entry_reference"].as_str().filter(|r| !r.is_empty()); let (a, cur) = amount(tx);
    match rf { Some(r) if !is_position_ref(Some(r)) => format!("{uid}:ref:{r}"), Some(_) => content_tx_key(uid, Some(&cur), &content_key(tx["booking_date"].as_str().unwrap_or(""), a, Some(&description(tx))), 1),
        None => format!("{uid}:fp:{}", &sha256_hex([uid, tx["booking_date"].as_str().unwrap_or(""), tx["value_date"].as_str().unwrap_or(""), &format!("{a:.2}"), &cur, &counterparty(tx, a), &description(tx)].join("|").as_bytes())[..24]) }
}
struct Keyer { uid: String, seen: HashMap<(String, Key), usize> }
impl Keyer {
    fn content(&mut self, tx: &Value) -> String { let (a, cur) = amount(tx); let ck = content_key(tx["booking_date"].as_str().unwrap_or(""), a, Some(&description(tx))); let n = self.seen.entry((cur.clone(), ck.clone())).or_default(); *n += 1; content_tx_key(&self.uid, Some(&cur), &ck, *n) }
    fn key(&mut self, tx: &Value) -> String { if is_position_ref(tx["entry_reference"].as_str()) { self.content(tx) } else { tx_key(&self.uid, tx) } }
}
pub fn syncs_today(con: &Connection, uid: &str) -> Result<i64> {
    let today = chrono::Local::now().date_naive(); let b = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::Local).single().map(|x| x.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S+00:00").to_string()).unwrap_or_default();
    Ok(con.query_row("SELECT COUNT(*) FROM sync_log WHERE account_uid=? AND ran_at>=? AND ran_at<?", params![uid, b(today), b(today + Duration::days(1))], |r| r.get(0))?)
}
/// Sync one account; returns {uid, status: ok | skipped | failed, ...}.
pub fn sync_account(c: &Client, uid: &str, full: bool, force: bool) -> Result<Value> {
    let con = c.con;
    if !force && syncs_today(con, uid)? >= c.cfg.sync_daily_limit { return Ok(json!({"uid": uid, "status": "skipped", "note": format!("already {} syncs today (use --force)", c.cfg.sync_daily_limit)})); }
    let api_uid: String = con.query_row("SELECT COALESCE(api_uid, uid) FROM accounts WHERE uid=?", [uid], |r| r.get(0)).optional()?.unwrap_or_else(|| uid.into());
    let last: Option<String> = con.query_row("SELECT MAX(booking_date) FROM transactions WHERE account_uid=?", [uid], |r| r.get(0))?;
    let mut query: Vec<(&str, String)> = match last.as_deref().and_then(d).filter(|_| !full) { Some(l) => vec![("date_from", (l - Duration::days(7)).to_string())], None => vec![("strategy", "longest".into())] };
    let ts = now_iso(); let count = || con.query_row("SELECT COUNT(*) FROM transactions WHERE account_uid=?", [uid], |r| r.get::<_, i64>(0)); let before = count()?;
    let mut imported: HashMap<Key, Vec<String>> = HashMap::new();
    for r in con.prepare("SELECT t.tx_key, t.booking_date, t.amount, t.description FROM transactions t JOIN import_rows r USING(tx_key) WHERE t.account_uid=?")?.query_map([uid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<String>>(3)?)))? { let (k, dd, a, de) = r?; imported.entry(content_key(dd.as_deref().unwrap_or(""), a.unwrap_or(0.0), de.as_deref())).or_default().push(k); }
    let (mut pages, mut seen_n, mut pending, mut replaced, mut conflicts, mut refreshed) = (0, 0, vec![], 0, 0, 0);
    let mut keyer = Keyer { uid: uid.into(), seen: HashMap::new() }; let mut seen_keys: HashMap<String, Key> = HashMap::new();
    con.execute_batch("BEGIN")?;
    let res = (|| -> Result<()> {
        loop {
            let res = c.call("GET", &format!("/accounts/{api_uid}/transactions"), &query, None)?; pages += 1;
            for raw in res["transactions"].as_array().into_iter().flatten() {
                let Some(tx) = booked(raw) else { pending.push(raw.clone()); continue };
                seen_n += 1; let (a, cur) = amount(&tx); let bd = tx["booking_date"].as_str(); let desc = description(&tx);
                let mut key = keyer.key(&tx);
                let get = |k: &str| con.query_row("SELECT booking_date, amount, description FROM transactions WHERE tx_key=?", [k], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0), r.get::<_, Option<String>>(2)?))).optional();
                let mut stored = get(&key)?;
                if stored.is_none() && key.starts_with(&format!("{uid}:ref:")) {   // the entry already stored under a content key (its reference was taken)
                    if let Some(rf) = tx["entry_reference"].as_str() { for (k, sd, sa, sde) in con.prepare("SELECT tx_key, booking_date, amount, description FROM transactions WHERE account_uid=? AND entry_reference=? AND tx_key LIKE ?")?.query_map(params![uid, rf, format!("{uid}:cfp:%")], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<f64>>(2)?.unwrap_or(0.0), r.get::<_, Option<String>>(3)?)))?.flatten() {
                        let s = (sd, sa, sde); if !differs(&s, bd, a, Some(&desc)) || same_tx(&s, bd, a, Some(&desc)) { key = k; stored = Some(s); break; } } }
                }
                let this = content_key(bd.unwrap_or(""), a, Some(&desc)); let dup = seen_keys.contains_key(&key);
                if dup && seen_keys.get(&key) == Some(&this) { continue; }
                seen_keys.entry(key.clone()).or_insert(this.clone());
                if let Some(s) = &stored { if dup || differs(s, bd, a, Some(&desc)) {
                    if !dup && same_tx(s, bd, a, Some(&desc)) {
                        let text = if enriches(s.2.as_deref(), Some(&desc)) { desc.clone() } else { s.2.clone().unwrap_or_default() };
                        con.execute("UPDATE transactions SET description=?, booking_date=?, value_date=?, counterparty=?, bank_tx_code=?, raw=?, last_seen=? WHERE tx_key=?", params![text, bd, tx["value_date"].as_str(), counterparty(&tx, a), btc(&tx), tx.to_string(), ts, key])?; refreshed += 1; continue;
                    }
                    let nk = keyer.content(&tx); if get(&nk)?.is_none() { conflicts += 1; } key = nk;
                } }
                let superseded = if !imported.is_empty() && get(&key)?.is_none() { imported.get_mut(&this).and_then(|v| v.pop()) } else { None };
                con.execute("INSERT INTO transactions(tx_key, account_uid, entry_reference, booking_date, value_date, amount, currency, counterparty, description, mcc, bank_tx_code, raw, first_seen, last_seen) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(tx_key) DO UPDATE SET last_seen=excluded.last_seen, raw=excluded.raw",
                    params![key, uid, tx["entry_reference"].as_str(), bd, tx["value_date"].as_str(), a, cur, counterparty(&tx, a), desc, tx["merchant_category_code"].as_str(), btc(&tx), tx.to_string(), ts, ts])?;
                if let Some(old) = superseded { replace_tx_key(con, &old, &key, "import superseded by bank feed")?; replaced += 1; }
            }
            match res["continuation_key"].as_str().filter(|k| !k.is_empty()) { Some(k) => { query.retain(|q| q.0 != "continuation_key"); query.push(("continuation_key", k.into())); } None => break }
        }
        con.execute("DELETE FROM pending_transactions WHERE account_uid=?", [uid])?;
        for tx in &pending { let (a, cur) = amount(tx); con.execute("INSERT INTO pending_transactions VALUES (?,?,?,?,?,?,?)", params![uid, tx["booking_date"].as_str().or(tx["transaction_date"].as_str()).or(tx["value_date"].as_str()), a, cur, counterparty(tx, a), description(tx), tx.to_string()])?; }
        for b in c.call("GET", &format!("/accounts/{api_uid}/balances"), &[], None)?["balances"].as_array().into_iter().flatten() {
            let ba = &b["balance_amount"]; con.execute("INSERT INTO balances VALUES (?,?,?,?,?,?)", params![uid, ts, b["balance_type"].as_str(), ba["amount"].as_f64().or_else(|| ba["amount"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0), ba["currency"].as_str(), b["reference_date"].as_str()])?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => { let new = count()? - before; con.execute("INSERT INTO sync_log VALUES (?,?,?,?,?,?)", params![uid, ts, 1, new, pages, format!("booked_seen={seen_n} pending={}{}", pending.len(), if conflicts > 0 { format!(" key_conflicts={conflicts}") } else { String::new() })])?; con.execute_batch("COMMIT")?;
            Ok(json!({"uid": uid, "status": "ok", "pages": pages, "new": new, "pending": pending.len(), "replaced_imports": replaced, "key_conflicts": conflicts, "refreshed": refreshed})) }
        Err(e) => { let _ = con.execute_batch("ROLLBACK"); let api = e.downcast_ref::<ApiError>();
            let note = match api { Some(a) if a.status == 429 => "rate limited by the bank (ASPSP_RATE_LIMIT_EXCEEDED)".to_string(), Some(a) => format!("HTTP {}", a.status), None => truncate_chars(&e.root_cause().to_string(), 200) };
            con.execute("INSERT INTO sync_log VALUES (?,?,?,?,?,?)", params![uid, ts, 0, 0, pages, note])?; Ok(json!({"uid": uid, "status": "failed", "note": note})) }
    }
}
/// Sync every account of every live session (dead consents are reported, not retried); one failure never stops the others.
pub fn sync_all(c: &Client, account: Option<&str>, full: bool, force: bool) -> Result<Vec<Value>> {
    let targets: Vec<(String, String, Option<String>)> = match account {
        Some(a) => { let uid = resolve_account(c.con, a)?; let (src, bank, sid, review): (String, String, Option<String>, i64) = c.con.query_row("SELECT source, COALESCE(bank, ''), session_id, needs_review FROM accounts WHERE uid=?", [&uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            if review != 0 { bail!("{a:?} needs review after a reconnect: `finch accounts merge OLD NEW` or `finch accounts set UID --resolve` first"); } if src != "api" { bail!("{a:?} is a manual (file import) account: nothing to sync"); } vec![(uid, bank, sid)] }
        None => c.con.prepare("SELECT a.uid, COALESCE(a.bank, s.aspsp_name, ''), a.session_id FROM accounts a JOIN sessions s ON s.session_id=a.session_id WHERE a.source='api' AND a.needs_review=0 AND s.status <> 'replaced' ORDER BY s.aspsp_name, a.uid")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?,
    };
    let mut notes = refresh_live(c).unwrap_or_else(|e| vec![format!("consent refresh failed: {}", e.root_cause())]);
    let status: HashMap<String, &'static str> = list_consents(c.con, false)?.into_iter().map(|x| (x.session_id, x.status)).collect();
    let mut out: Vec<Value> = notes.drain(..).map(|n| json!({"status": "note", "note": n})).collect();
    for (uid, bank, sid) in targets {
        if let Some(st) = sid.as_ref().and_then(|s| status.get(s)).filter(|s| ["expired", "revoked"].contains(s)) { out.push(json!({"uid": uid, "bank": bank, "status": "failed", "note": format!("{bank}: consent {st}: run `finch bank reconnect \"{bank}\"`")})); continue; }
        let mut r = sync_account(c, &uid, full, force).unwrap_or_else(|e| json!({"uid": uid, "status": "failed", "note": truncate_chars(&e.root_cause().to_string(), 200)})); r["bank"] = bank.into(); out.push(r);
    }
    Ok(out)
}

/// Fold account NEW into OLD (OLD keeps its uid, label, owner, purpose and history).
pub fn merge_accounts(con: &Connection, old_ref: &str, new_ref: &str) -> Result<String> {
    let (old, new) = (resolve_account(con, old_ref)?, resolve_account(con, new_ref)?); if old == new { bail!("OLD and NEW are the same account"); }
    let (oc, os): (Option<String>, String) = con.query_row("SELECT currency, source FROM accounts WHERE uid=?", [&old], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let (nsid, napi, nc, ns, niban): (Option<String>, String, Option<String>, String, Option<String>) = con.query_row("SELECT session_id, COALESCE(api_uid, uid), currency, source, iban FROM accounts WHERE uid=?", [&new], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
    if os != ns { bail!("cannot merge an API account with a manual one"); } if oc.is_some() && nc.is_some() && oc != nc { bail!("currencies differ: not the same account"); }
    con.execute_batch("BEGIN")?;
    let r = (|| -> Result<()> {
        for key in con.prepare("SELECT tx_key FROM transactions WHERE account_uid=?")?.query_map([&new], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()? {
            let nk = key.strip_prefix(&format!("{new}:")).map_or(key.clone(), |rest| format!("{old}:{rest}"));
            if nk != key && con.query_row("SELECT 1 FROM transactions WHERE tx_key=?", [&nk], |_| Ok(())).optional()?.is_some() { replace_tx_key(con, &key, &nk, "account merge: duplicate of the kept row")?; }
            else if nk != key { rekey_tables(con, &key, &nk)?; con.execute("INSERT OR REPLACE INTO tx_key_remap VALUES (?,?,'account merge',datetime('now'))", [&key, &nk])?; con.execute("UPDATE transactions SET account_uid=? WHERE tx_key=?", [&old, &nk])?; }
            else { con.execute("UPDATE transactions SET account_uid=? WHERE tx_key=?", [&old, &key])?; }
        }
        for t in ["balances", "sync_log", "pending_transactions", "imports"] { con.execute(&format!("UPDATE {t} SET account_uid=? WHERE account_uid=?"), [&old, &new])?; }
        con.execute("DELETE FROM accounts WHERE uid=?", [&new])?;
        con.execute("UPDATE accounts SET session_id=?, api_uid=?, needs_review=0, iban=COALESCE(iban, ?), identification_hash=COALESCE(identification_hash, ?) WHERE uid=?", params![nsid, (napi != old).then_some(napi), niban, ident_hash(niban.as_deref(), None), old])?; Ok(())
    })();
    match r { Ok(()) => { con.execute_batch("COMMIT")?; Ok(old) } Err(e) => { let _ = con.execute_batch("ROLLBACK"); Err(e) } }
}
