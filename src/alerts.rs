//! Alerts (E10): the signals (consents, failing syncs, the insights cards, kid budgets) become stored events (`alert_events`, shared
//! with the Python app): one event per situation (the id is a hash of its dedupe key), escalation when the severity rises, resolved
//! when the signal goes (the same event if it returns within 7 days). The in-app feed is the table. Channels (ntfy, Telegram) are
//! OFF by default, go through the egress policy, respect quiet hours and a weekly budget, and say only a minimal, guarded sentence
//! ("Coach: 2 new alerts (1 high). Open the app."); a summary gives fixed kind labels and rounded amounts. E-mail and the macOS
//! notification are not ported. Kid budgets and LLM usage stay in the local feed only.
use crate::{analytics::{recurring::Recurring, Dataset, Scope}, config::Config, ingest::bank, net::Http, util::*};
use anyhow::{bail, Result};
use chrono::{DateTime, Timelike, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

pub const KINDS: [&str; 16] = ["consent", "sync_failing", "unusual_charge", "price_increase", "low_balance", "budget", "loan_alert", "loa_end", "unused_subscription", "contract_notice", "decision_contradicted", "llm_usage_high", "kid_budget", "scheme_end", "scheme_check", "rent_missing"];
const LOCAL_ONLY: [&str; 2] = ["llm_usage_high", "kid_budget"];
const SEV: [&str; 3] = ["low", "medium", "high"];
fn rank(s: &str) -> usize { SEV.iter().position(|x| *x == s).unwrap_or(0) }
pub const CHANNELS: [&str; 4] = ["macos", "ntfy", "email", "telegram"];
const TELEGRAM_API: &str = "https://api.telegram.org";
fn label(kind: &str) -> &'static str {
    match kind { "consent" => "Bank consent expiring or expired", "sync_failing" => "Bank sync failing", "unusual_charge" => "Unusual charge", "price_increase" => "Price increase", "low_balance" => "Low balance forecast", "budget" => "Budget over or at risk",
        "loan_alert" => "Loan payment alert", "loa_end" => "Lease end reminder", "unused_subscription" => "Unused subscription", "contract_notice" => "Contract notice deadline", "decision_contradicted" => "Cancelled service still charged", "llm_usage_high" => "LLM usage above your monthly threshold",
        "scheme_end" => "Rental scheme commitment ending", "scheme_check" => "Rental scheme condition to check", "rent_missing" => "Rent not received", "kid_budget" => "Kid budget limit", _ => "Alert" }
}

#[derive(Debug, Clone)]
pub struct Settings { pub enabled: bool, pub min_severity: String, pub external_min_severity: String, pub external_detail: String, pub quiet: Option<(u32, u32)>, pub quiet_hours: String, pub max_per_week: i64, pub disabled: Vec<String>, pub digest_only: Vec<String>,
    pub weekly_digest: bool, pub app_url: String, pub sync_failures: i64, pub anomaly_min_severity: String, pub ntfy: (bool, String), pub telegram: (bool, String), pub email: bool, pub macos: bool }
impl Settings {
    /// `[alerts]` of config.toml: every external channel is OFF by default; a wrong value refuses to load.
    pub fn load(cfg: &Config) -> Result<Self> {
        let t = &cfg.alerts; let s = |k: &str, d: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or(d).trim().to_string(); let b = |k: &str, d: bool| t.get(k).and_then(|v| v.as_bool()).unwrap_or(d);
        let kinds = |k: &str, d: &[&str]| -> Result<Vec<String>> { let v: Vec<String> = t.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_else(|| d.iter().map(|x| x.to_string()).collect());
            if let Some(x) = v.iter().find(|x| !KINDS.contains(&x.as_str())) { bail!("[alerts] {k}: unknown kind {x:?}"); } Ok(v) };
        let sub = |k: &str| t.get(k).and_then(|v| v.as_table()).cloned().unwrap_or_default(); let (th, nt, tg, em, mc) = (sub("thresholds"), sub("ntfy"), sub("telegram"), sub("email"), sub("macos"));
        let sev = |v: String, k: &str| -> Result<String> { if SEV.contains(&v.as_str()) { Ok(v) } else { bail!("[alerts] {k} must be low, medium or high") } };
        let qh = s("quiet_hours", "22:00-08:00"); let quiet = if qh.is_empty() { None } else { let c = crate::rs!(r"^([01]\d|2[0-3]):([0-5]\d)-([01]\d|2[0-3]):([0-5]\d)$").captures(&qh).ok_or_else(|| anyhow::anyhow!("[alerts] quiet_hours must look like \"22:00-08:00\" or be empty"))?;
            let m = |i: usize| c[i].parse::<u32>().unwrap(); Some((m(1) * 60 + m(2), m(3) * 60 + m(4))) };
        let url = nt.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(); if !url.is_empty() && (!url.starts_with("https://") || url::Url::parse(&url).map(|u| !u.username().is_empty() || u.password().is_some()).unwrap_or(true)) { bail!("[alerts.ntfy] url must be an https:// URL without a user name or password"); }
        let detail = s("external_detail", "minimal"); if !["minimal", "summary"].contains(&detail.as_str()) { bail!("[alerts] external_detail must be minimal or summary"); }
        Ok(Self { enabled: b("enabled", true), min_severity: sev(s("min_severity", "low"), "min_severity")?, external_min_severity: sev(s("external_min_severity", "medium"), "external_min_severity")?, external_detail: detail, quiet, quiet_hours: qh,
            max_per_week: t.get("max_per_week").and_then(|v| v.as_integer()).unwrap_or(7).clamp(0, 100), disabled: kinds("disabled_kinds", &[])?, digest_only: kinds("digest_only_kinds", &["price_increase", "unusual_charge"])?, weekly_digest: b("weekly_digest", true),
            app_url: s("app_url", ""), sync_failures: th.get("sync_failures").and_then(|v| v.as_integer()).unwrap_or(3).max(1), anomaly_min_severity: sev(th.get("anomaly_min_severity").and_then(|v| v.as_str()).unwrap_or("medium").into(), "thresholds.anomaly_min_severity")?,
            ntfy: (nt.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false), url), telegram: (tg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false), tg.get("chat_id").and_then(|v| v.as_str()).unwrap_or("").to_string()),
            email: em.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false), macos: mc.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) })
    }
    pub fn enabled(&self, ch: &str) -> bool { match ch { "ntfy" => self.ntfy.0, "telegram" => self.telegram.0, "email" => self.email, "macos" => self.macos, _ => false } }
    pub fn in_quiet(&self, now: DateTime<Utc>) -> bool { let Some((a, b)) = self.quiet else { return false }; let l = now.with_timezone(&chrono::Local); let m = l.hour() * 60 + l.minute(); if a < b { a <= m && m < b } else { m >= a || m < b } }
}

pub struct Cand { pub kind: String, pub key: String, pub severity: String, pub title: String, pub body: String, pub payload: Value }
pub fn event_id(key: &str) -> String { format!("alr_{}", &sha1_hex(key)[..12]) }
/// Every signal of the moment, one candidate per dedupe key.
pub fn candidates(con: &Connection, ds: &Dataset, rec: &Recurring, s: &Settings) -> Result<Vec<Cand>> {
    let mut out = vec![];
    for c in bank::list_consents(con, false)? {
        let top = if c.status == "expired" || c.status == "revoked" { "expired" } else if c.days_left.is_some_and(|d| d <= 3) { "d3" } else if c.days_left.is_some_and(|d| d <= 14) { "d14" } else { continue };
        let title = if top == "expired" { format!("{} ({}): consent EXPIRED", c.bank, c.country) } else { format!("{} ({}): consent {}, {} day(s) left (until {})", c.bank, c.country, c.status.to_uppercase(), c.days_left.unwrap_or(0), c.valid_until.clone().unwrap_or_default()) };
        out.push(Cand { kind: "consent".into(), key: format!("consent:{}:{top}", c.session_id), severity: if top == "d14" { "medium" } else { "high" }.into(), title, body: "Reconnect the bank from the app (Connections) or with `finch bank reconnect`. Until then the data goes stale.".into(),
            payload: json!({"session_id": c.session_id, "bank": c.bank, "threshold": top, "days_left": c.days_left, "valid_until": c.valid_until})});
    }
    for (uid, lbl) in con.prepare("SELECT uid, COALESCE(label, name, uid) FROM accounts WHERE source='api' AND COALESCE(exclude,0)=0")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()? {
        let rows: Vec<(String, i64)> = con.prepare("SELECT ran_at, ok FROM sync_log WHERE account_uid=? ORDER BY ran_at DESC, rowid DESC")?.query_map([&uid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let streak: Vec<&String> = rows.iter().take_while(|r| r.1 == 0).map(|r| &r.0).collect(); if (streak.len() as i64) < s.sync_failures { continue; }
        out.push(Cand { kind: "sync_failing".into(), key: format!("sync:{uid}:{}", streak.last().unwrap()), severity: if streak.len() as i64 >= 2 * s.sync_failures { "high" } else { "medium" }.into(), title: format!("{lbl}: the last {} syncs failed", streak.len()),
            body: "The transactions of this account are not up to date. Run `finch sync` to see the error; an expired consent needs `finch bank reconnect`.".into(), payload: json!({"account": lbl, "failures": streak.len(), "since": streak.last()})});
    }
    let (cards, _) = crate::insights::cards(ds, rec, con, &Scope::default())?;
    for c in cards { let (kind, sub, sev) = (c["kind"].as_str().unwrap_or(""), c["subtype"].as_str().unwrap_or(""), c["severity"].as_str().unwrap_or("low"));
        let payload = json!({"card": c["id"], "subtype": sub, "subject": c["subject"], "date": c["date"], "amount_c": c["amount"].as_str().and_then(|a| a.parse::<f64>().ok()).map(|a| to_cents(a).abs()), "evidence": c["evidence"].as_array().map(|e| e.iter().take(10).cloned().collect::<Vec<_>>())});
        let mk = |k: &str, key: String, sev: &str| Cand { kind: k.into(), key, severity: sev.into(), title: c["title"].as_str().unwrap_or("").into(), body: c["body"].as_str().unwrap_or("").into(), payload: payload.clone() };
        match kind { "anomaly" if rank(sev) >= rank(&s.anomaly_min_severity) && sev != "low" => out.push(mk("unusual_charge", c["id"].as_str().unwrap_or("").into(), sev)),
            "price_change" if sub == "increase" => out.push(mk("price_increase", c["id"].as_str().unwrap_or("").into(), "medium")),
            "forecast" if !c["date"].is_null() => out.push(mk("low_balance", format!("forecast:{}:{sub}", c["subject"].as_str().unwrap_or("")), sev)),
            "budget" => out.push(mk("budget", format!("budget:{}:{}", c["subject"].as_str().unwrap_or(""), c["date"].as_str().unwrap_or("")), sev)), _ => {} } }
    for b in crate::household::kid_budget_status(ds, None).into_iter().filter(|b| b["status"] != "ok") { let name = ds.people.get(b["member"].as_str().unwrap_or("")).map_or("A child".to_string(), |m| m.name.split(' ').next().unwrap_or("").to_string()); let over = b["status"] == "over";
        out.push(Cand { kind: "kid_budget".into(), key: format!("kidbudget:{}:{}", b["id"].as_str().unwrap_or(""), b["period_start"].as_str().unwrap_or("")), severity: if over { "medium" } else { "low" }.into(),
            title: format!("{name} {} the {} limit", if over { "went over" } else { "is close to" }, b["period"].as_str().unwrap_or("")), body: format!("{} EUR of {} EUR used. A friendly chat, not a telling-off.", b["spent"].as_str().unwrap_or(""), b["limit"].as_str().unwrap_or("")), payload: json!({"subtype": "limit", "budget": b["id"]})}); }
    let mut seen = std::collections::HashSet::new(); Ok(out.into_iter().filter(|c| seen.insert(event_id(&c.key))).collect())
}
fn now_iso(n: DateTime<Utc>) -> String { n.format("%Y-%m-%dT%H:%M:%S+00:00").to_string() }
fn prefs(con: &Connection) -> Result<HashMap<String, (bool, Option<String>)>> { Ok(con.prepare("SELECT kind, muted, snoozed_until FROM alert_kind_prefs")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)? != 0, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?) }
fn initial(kind: &str, sev: &str, s: &Settings, p: &HashMap<String, (bool, Option<String>)>, today: &str) -> (String, Option<String>) {
    let pr = p.get(kind); if pr.is_some_and(|x| x.0) || rank(sev) < rank(&s.min_severity) { return ("suppressed".into(), None); }
    match pr.and_then(|x| x.1.clone()).filter(|u| u.as_str() >= today) { Some(u) => ("snoozed".into(), Some(u)), None => ("new".into(), None) }
}
/// Upsert the candidates; resolve the events whose signal is gone. Returns {new, escalated, reopened, resolved, unchanged}.
pub fn sync_events(con: &Connection, cands: &[Cand], s: &Settings, now: DateTime<Utc>, today: &str) -> Result<Value> {
    let ts = now_iso(now); let p = prefs(con)?; let have: HashMap<String, Value> = listing(con, None)?.into_iter().map(|e| (e["id"].as_str().unwrap().to_string(), e)).collect();
    let mut res: BTreeMap<&str, Vec<String>> = ["new", "escalated", "reopened", "resolved", "unchanged"].iter().map(|k| (*k, vec![])).collect(); let mut seen = std::collections::HashSet::new();
    for c in cands.iter().filter(|c| !s.disabled.contains(&c.kind)) { let id = event_id(&c.key); seen.insert(id.clone()); let payload = c.payload.to_string(); let title: String = c.title.chars().take(300).collect(); let body: String = c.body.chars().take(1500).collect();
        let Some(old) = have.get(&id) else { let (st, until) = initial(&c.kind, &c.severity, s, &p, today);
            con.execute("INSERT INTO alert_events(id, kind, severity, created, updated, last_seen, title, body, payload, status, snoozed_until) VALUES (?,?,?,?,?,?,?,?,?,?,?)", params![id, c.kind, c.severity, ts, ts, ts, title, body, payload, st, until])?; res.get_mut("new").unwrap().push(id); continue };
        let (mut st, mut until, mut sev, mut esc) = (old["status"].as_str().unwrap().to_string(), old["snoozed_until"].as_str().map(String::from), old["severity"].as_str().unwrap().to_string(), old["escalations"].as_i64().unwrap_or(0));
        let (mut created, mut sent) = (old["created"].as_str().unwrap().to_string(), old["channels_sent"].to_string()); let mut changed: Option<&str> = None; let muted = p.get(&c.kind).is_some_and(|x| x.0);
        if let Some(r) = old["resolved_at"].as_str() { let age = DateTime::parse_from_rfc3339(r).map(|d| (now - d.with_timezone(&Utc)).num_days()).unwrap_or(99);
            if age >= 7 { let (a, b) = initial(&c.kind, &c.severity, s, &p, today); st = a; until = b; sev = c.severity.clone(); esc = 0; created = ts.clone(); sent = "{}".into(); changed = Some("reopened"); } else { changed = Some("unresolved"); } }
        if rank(&c.severity) > rank(&sev) { sev = c.severity.clone(); esc += 1; changed = changed.or(Some("escalated")); if st == "acked" || st == "sent" { st = "new".into(); } }
        if st == "suppressed" && !muted && rank(&sev) >= rank(&s.min_severity) { let (a, b) = initial(&c.kind, &sev, s, &p, today); st = a; until = b; changed = changed.or(Some("unsuppressed")); }
        if st == "snoozed" && until.as_deref().unwrap_or("") < today { st = if old["channels_sent"].as_object().is_some_and(|o| !o.is_empty()) { "sent".into() } else { "new".into() }; until = None; }
        con.execute("UPDATE alert_events SET severity=?, created=?, updated=?, last_seen=?, resolved_at=NULL, title=?, body=?, payload=?, status=?, snoozed_until=?, channels_sent=?, escalations=?, acked_at=CASE WHEN ?='acked' THEN acked_at END WHERE id=?",
            params![sev, created, if changed.is_some() { ts.clone() } else { old["updated"].as_str().unwrap().to_string() }, ts, title, body, payload, st, until, sent, esc, st, id])?;
        res.get_mut(match changed { Some("escalated") => "escalated", Some("reopened") => "reopened", _ => "unchanged" }).unwrap().push(id);
    }
    for (id, old) in &have { if !seen.contains(id) && old["resolved_at"].is_null() { con.execute("UPDATE alert_events SET resolved_at=?, updated=? WHERE id=?", params![ts, ts, id])?; res.get_mut("resolved").unwrap().push(id.clone()); } }
    Ok(json!(res))
}
pub fn evaluate(con: &Connection, ds: &Dataset, rec: &Recurring, s: &Settings, now: DateTime<Utc>, dry_run: bool) -> Result<Value> {
    let cands = candidates(con, ds, rec, s)?; con.execute_batch("BEGIN")?;
    let r = sync_events(con, &cands, s, now, &ds.today.to_string()); match &r { Ok(_) if !dry_run => con.execute_batch("COMMIT")?, _ => con.execute_batch("ROLLBACK")? }
    let mut v = r?; v["candidates"] = cands.len().into(); Ok(v)
}
const COLS: &str = "id, kind, severity, created, updated, last_seen, resolved_at, title, body, payload, status, snoozed_until, acked_at, channels_sent, escalations";
fn row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let payload: Value = serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or(json!({})); let resolved: Option<String> = r.get(6)?;
    Ok(json!({"id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "severity": r.get::<_, String>(2)?, "created": r.get::<_, String>(3)?, "updated": r.get::<_, String>(4)?, "last_seen": r.get::<_, String>(5)?, "resolved": resolved.is_some(), "resolved_at": resolved,
        "title": r.get::<_, String>(7)?, "body": r.get::<_, String>(8)?, "title_msg": null, "body_msg": null, "payload": payload, "status": r.get::<_, String>(10)?, "snoozed_until": r.get::<_, Option<String>>(11)?, "acked_at": r.get::<_, Option<String>>(12)?,
        "channels_sent": serde_json::from_str::<Value>(&r.get::<_, String>(13)?).unwrap_or(json!({})), "escalations": r.get::<_, i64>(14)?}))
}
pub fn listing(con: &Connection, include_resolved: Option<bool>) -> Result<Vec<Value>> {
    let sql = format!("SELECT {COLS} FROM alert_events{} ORDER BY CASE severity WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END, updated DESC", if include_resolved == Some(false) { " WHERE resolved_at IS NULL" } else { "" });
    Ok(con.prepare(&sql)?.query_map([], row)?.collect::<rusqlite::Result<_>>()?)
}
pub fn summary(con: &Connection) -> Result<Value> {
    let open = |extra: &str| con.query_row(&format!("SELECT COUNT(*) FROM alert_events WHERE resolved_at IS NULL AND status IN ('new','sent'){extra}"), [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    Ok(json!({"ready": true, "open": open(""), "new": open(" AND status='new'"), "high": open(" AND severity='high'")}))
}
pub fn kinds(con: &Connection, s: &Settings) -> Result<Vec<Value>> { let p = prefs(con)?; Ok(KINDS.iter().map(|k| { let x = p.get(*k); json!({"kind": k, "label": label(k), "muted": x.is_some_and(|x| x.0), "snoozed_until": x.and_then(|x| x.1.clone()), "disabled_in_config": s.disabled.iter().any(|d| d == k), "digest_only": s.digest_only.iter().any(|d| d == k)}) }).collect()) }
/// ack | restore | snooze (days): the user's own handling of one event.
pub fn act(con: &Connection, id: &str, what: &str, days: Option<i64>, today: chrono::NaiveDate) -> Result<bool> {
    let ts = now_iso(Utc::now());
    Ok(match what {
        "ack" => con.execute("UPDATE alert_events SET status='acked', acked_at=?, snoozed_until=NULL, updated=? WHERE id=?", params![ts, ts, id])?,
        "snooze" => con.execute("UPDATE alert_events SET status='snoozed', snoozed_until=?, updated=? WHERE id=?", params![(today + chrono::Duration::days(days.unwrap_or(7).clamp(1, 365))).to_string(), ts, id])?,
        "restore" => con.execute("UPDATE alert_events SET status=CASE WHEN channels_sent<>'{}' THEN 'sent' ELSE 'new' END, snoozed_until=NULL, acked_at=NULL, updated=? WHERE id=?", params![ts, id])?,
        _ => bail!("unknown action"),
    } > 0)
}
/// mute | unmute | snooze (days) | wake a whole kind.
pub fn kind_act(con: &Connection, kind: &str, what: &str, days: Option<i64>, today: chrono::NaiveDate) -> Result<()> {
    if !KINDS.contains(&kind) { bail!("unknown alert kind {kind:?}"); } let ts = now_iso(Utc::now());
    let (muted, until): (Option<bool>, Option<Option<String>>) = match what { "mute" => (Some(true), None), "unmute" => (Some(false), None), "snooze" => (None, Some(Some((today + chrono::Duration::days(days.unwrap_or(7).clamp(1, 365))).to_string()))), "wake" => (None, Some(None)), _ => bail!("unknown action") };
    con.execute("INSERT INTO alert_kind_prefs(kind, muted, snoozed_until, updated) VALUES (?,0,NULL,?) ON CONFLICT(kind) DO NOTHING", params![kind, ts])?;
    if let Some(m) = muted { con.execute("UPDATE alert_kind_prefs SET muted=?, updated=? WHERE kind=?", params![m as i64, ts, kind])?; if m { con.execute("UPDATE alert_events SET status='suppressed', updated=? WHERE kind=? AND status IN ('new','sent')", params![ts, kind])?; } }
    if let Some(u) = until { con.execute("UPDATE alert_kind_prefs SET snoozed_until=?, updated=? WHERE kind=?", params![u, ts, kind])?; } Ok(())
}

// ---------------------------------------------------------------- messages and channels
fn round_amount(c: i64) -> i64 { let e = c.abs() as f64 / 100.0; if e < 5.0 { return 0; } let step = if e >= 1000.0 { 100.0 } else { 10.0 }; (step as i64).max(((e / step).round() * step) as i64) }
pub fn minimal_text(ev: &[Value], url: &str) -> String { let n = ev.len(); let high = ev.iter().filter(|e| e["severity"] == "high").count(); format!("Coach: {n} new alert{}{}. {}", if n == 1 { "" } else { "s" }, if high > 0 { format!(" ({high} high)") } else { String::new() }, if url.is_empty() { "Open the app.".into() } else { format!("Open the app: {url}") }) }
pub fn summary_text(ev: &[Value], url: &str) -> String {
    let mut o: Vec<&Value> = ev.iter().collect(); o.sort_by(|a, b| rank(b["severity"].as_str().unwrap_or("")).cmp(&rank(a["severity"].as_str().unwrap_or(""))).then(a["kind"].as_str().cmp(&b["kind"].as_str())));
    let mut lines: Vec<String> = o.iter().take(5).map(|e| { let k = e["kind"].as_str().unwrap_or(""); let mut l = format!("- {} ({})", label(k), e["severity"].as_str().unwrap_or(""));
        if ["unusual_charge", "price_increase", "budget", "unused_subscription"].contains(&k) { if let Some(a) = e["payload"]["amount_c"].as_i64().filter(|a| round_amount(*a) > 0) { l += &format!(", about {} EUR", round_amount(a)); } } l }).collect();
    if o.len() > 5 { lines.push(format!("- and {} more", o.len() - 5)); }
    format!("{}.\n{}\n{}", minimal_text(ev, "").trim_end_matches(". Open the app."), lines.join("\n"), if url.is_empty() { "Open the app.".into() } else { format!("Open the app: {url}") })
}
/// The text an external channel may send: built from fixed words and rounded numbers, then checked by the privacy guard.
pub fn compose(ev: &[Value], s: &Settings, guard: &crate::privacy::Guard) -> Option<(String, String, Option<&'static str>)> {
    if s.external_detail == "summary" { let t = summary_text(ev, &s.app_url); if guard.violations(&json!(t)).is_empty() { return Some(("Coach alerts".into(), t, None)); } }
    let m = minimal_text(ev, &s.app_url); guard.violations(&json!(m)).is_empty().then_some(("Coach alerts".into(), m, (s.external_detail == "summary").then_some("the summary was refused by the privacy filter: sent as minimal")))
}
fn secret(n: &str) -> Option<String> { crate::secrets::get_secret(n, false).ok().flatten() }
pub fn problems(cfg: &Config, s: &Settings, ch: &str) -> Vec<String> {
    let mut p = vec![]; match ch { "email" => p.push("the e-mail channel is not ported to finch yet".into()), "macos" => p.push("the macOS notification is not ported to finch".into()), _ => {} }
    if let Some(w) = crate::egress::evaluate(cfg, &format!("alerts.{ch}"), "") { p.push(w); }
    if ch == "ntfy" && s.ntfy.1.is_empty() { p.push("missing setting: url".into()); } if ch == "telegram" { if s.telegram.1.is_empty() { p.push("missing setting: chat_id".into()); } if secret("telegram_bot_token").is_none() { p.push("secret 'telegram_bot_token' not set".into()); } } p
}
/// The request a send would make, safe to show: tokens are masked.
pub fn render(s: &Settings, ch: &str, title: &str, body: &str) -> Value {
    match ch { "ntfy" => json!({"channel": ch, "method": "POST", "url": s.ntfy.1, "headers": {"Title": title, "Priority": "default", "Tags": "bell", "Authorization": if secret("ntfy_token").is_some() { "Bearer ********" } else { "(none)" }}, "body": body}),
        "telegram" => json!({"channel": ch, "method": "POST", "url": format!("{TELEGRAM_API}/bot********/sendMessage"), "json": {"chat_id": s.telegram.1, "text": format!("{title}\n{body}"), "disable_web_page_preview": true}}), _ => json!({"channel": ch}) }
}
fn send(con: &Connection, cfg: &Config, s: &Settings, ch: &str, title: &str, body: &str, http: &dyn Http) -> Result<()> {
    match ch {
        "ntfy" => { crate::egress::allow(Some(con), cfg, "alerts.ntfy", &crate::egress::host_of(&s.ntfy.1), body.len(), "alert")?; let mut h = vec![("Title", title.to_string()), ("Priority", "default".to_string()), ("Tags", "bell".to_string())];
            if let Some(t) = secret("ntfy_token") { h.push(("Authorization", format!("Bearer {t}"))); } let r = http.send("POST", &s.ntfy.1, &h, Some(&json!(body)))?; if r.status >= 300 { bail!("ntfy answered {}", r.status); } }
        "telegram" => { let tok = secret("telegram_bot_token").ok_or_else(|| anyhow::anyhow!("telegram_bot_token not set"))?; crate::egress::allow(Some(con), cfg, "alerts.telegram", "api.telegram.org", body.len(), "alert")?;
            let r = http.send("POST", &format!("{TELEGRAM_API}/bot{tok}/sendMessage"), &[], Some(&json!({"chat_id": s.telegram.1, "text": format!("{title}\n{body}"), "disable_web_page_preview": true})))?; if r.status >= 300 { bail!("Telegram answered {}", r.status); } }
        _ => bail!("channel {ch} is not available"),
    } Ok(())
}
/// Hand each enabled channel the events it has not had yet (dry_run: show what would go, send nothing, write nothing).
pub fn dispatch(con: &Connection, cfg: &Config, s: &Settings, now: DateTime<Utc>, dry_run: bool, http: &dyn Http) -> Result<Vec<Value>> {
    let events = listing(con, Some(false))?; let mut out = vec![]; let guard = crate::privacy::Guard::new(cfg, Some(con)); let ts = now_iso(now);
    for ch in CHANNELS.iter().filter(|c| s.enabled(c)) {
        let mut r = json!({"channel": ch, "status": "nothing", "reason": null, "n": 0, "message": null, "request": null});
        let pr = problems(cfg, s, ch); if !pr.is_empty() { r["status"] = "not_ready".into(); r["reason"] = pr.join("; ").into(); out.push(r); continue; }
        let mut pend: Vec<Value> = events.iter().filter(|e| ["new", "sent"].contains(&e["status"].as_str().unwrap_or("")) && !s.digest_only.iter().any(|k| e["kind"] == k.as_str()) && !LOCAL_ONLY.contains(&e["kind"].as_str().unwrap_or(""))
            && rank(e["severity"].as_str().unwrap_or("")) >= rank(&s.min_severity).max(rank(&s.external_min_severity)) && e["channels_sent"][ch].as_object().is_none_or(|g| rank(g["severity"].as_str().unwrap_or("")) < rank(e["severity"].as_str().unwrap_or("")))).cloned().collect();
        let based: bool = con.query_row("SELECT 1 FROM alert_channel_state WHERE channel=?", [ch], |_| Ok(())).optional()?.is_some();
        if !based { r["baselined"] = pend.len().into(); if !dry_run { for e in &pend { let mut sent = e["channels_sent"].clone(); sent[*ch] = json!({"at": ts, "severity": e["severity"], "baseline": true}); con.execute("UPDATE alert_events SET channels_sent=? WHERE id=?", params![sent.to_string(), e["id"].as_str()])?; }
            con.execute("INSERT OR REPLACE INTO alert_channel_state(channel, baselined_at, n_events) VALUES (?,?,?)", params![ch, ts, pend.len() as i64])?; } pend.clear(); }
        r["n"] = pend.len().into(); if pend.is_empty() { out.push(r); continue; }
        let used: i64 = con.query_row("SELECT COUNT(*) FROM alert_deliveries WHERE channel=? AND ok=1 AND sent_at>=?", params![ch, now_iso(now - chrono::Duration::days(7))], |x| x.get(0))?;
        if s.in_quiet(now) { r["status"] = "deferred".into(); r["reason"] = "quiet hours".into(); out.push(r); continue; }
        if used >= s.max_per_week { r["status"] = "deferred".into(); r["reason"] = format!("weekly limit reached ({used}/{} messages in 7 days)", s.max_per_week).into(); out.push(r); continue; }
        let Some((title, body, note)) = compose(&pend, s, &guard) else { r["status"] = "failed".into(); r["reason"] = "the message was refused by the privacy filter".into(); out.push(r); continue };
        r["message"] = json!({"title": title, "body": body, "detail": s.external_detail, "note": note}); r["request"] = render(s, ch, &title, &body);
        if dry_run { r["status"] = "would_send".into(); out.push(r); continue; }
        match send(con, cfg, s, ch, &title, &body, http) {
            Err(e) => { con.execute("INSERT INTO alert_deliveries(sent_at, channel, what, n_events, ok, error) VALUES (?,?,'alert',?,0,?)", params![ts, ch, pend.len() as i64, "send failed"])?; r["status"] = "failed".into(); r["reason"] = e.root_cause().to_string().chars().take(120).collect::<String>().into(); }
            Ok(()) => { con.execute("INSERT INTO alert_deliveries(sent_at, channel, what, n_events, ok) VALUES (?,?,'alert',?,1)", params![ts, ch, pend.len() as i64])?;
                for e in &pend { let mut sent = e["channels_sent"].clone(); sent[*ch] = json!({"at": ts, "severity": e["severity"]}); con.execute("UPDATE alert_events SET channels_sent=?, status=CASE WHEN status='new' THEN 'sent' ELSE status END, updated=? WHERE id=?", params![sent.to_string(), ts, e["id"].as_str()])?; }
                r["status"] = "sent".into(); }
        } out.push(r);
    }
    Ok(out)
}
pub fn channels_view(con: &Connection, cfg: &Config, s: &Settings) -> Result<Value> {
    let since = now_iso(Utc::now() - chrono::Duration::days(7));
    let chs: Vec<Value> = CHANNELS.iter().map(|ch| { let pr = if s.enabled(ch) { problems(cfg, s, ch) } else { vec![] }; let n: i64 = con.query_row("SELECT COUNT(*) FROM alert_deliveries WHERE channel=? AND ok=1 AND sent_at>=?", params![ch, since], |r| r.get(0)).unwrap_or(0);
        json!({"channel": ch, "enabled": s.enabled(ch), "ready": s.enabled(ch) && pr.is_empty(), "problems": pr, "external": *ch != "macos", "target": match *ch { "ntfy" => if s.ntfy.1.is_empty() { String::new() } else { format!("{}/****", crate::egress::host_of(&s.ntfy.1)) }, "telegram" => if s.telegram.1.is_empty() { String::new() } else { "chat ****".into() }, _ => String::new() }, "sent_last_7_days": n}) }).collect();
    let recent: Vec<Value> = con.prepare("SELECT sent_at, channel, what, ok, error FROM alert_deliveries ORDER BY id DESC LIMIT 20")?.query_map([], |r| Ok(json!({"sent_at": r.get::<_, String>(0)?, "channel": r.get::<_, String>(1)?, "what": r.get::<_, String>(2)?, "ok": r.get::<_, i64>(3)? != 0, "error": r.get::<_, Option<String>>(4)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(json!({"in_app": {"enabled": s.enabled, "note": "the in-app feed keeps every alert; nothing leaves the machine unless a channel is enabled"}, "channels": chs, "external_detail": s.external_detail, "min_severity": s.min_severity, "quiet_hours": s.quiet_hours, "max_per_week": s.max_per_week,
        "weekly_digest": s.weekly_digest, "weekly_digest_to_channels": false, "recent_deliveries": recent, "how_to_enable": "enable a channel in [alerts.ntfy] / [alerts.telegram] of config.toml; tokens go to the secret store (ntfy_token, telegram_bot_token)"}))
}
/// The weekly summary of the in-app feed (local, nothing stored or sent).
pub fn digest(con: &Connection, today: chrono::NaiveDate) -> Result<Value> {
    let start = today - chrono::Duration::days(today.weekday_from_monday() as i64 + 7); let end = start + chrono::Duration::days(6);
    let evs: Vec<Value> = listing(con, None)?.into_iter().filter(|e| e["created"].as_str().is_some_and(|c| c[..10] >= *start.to_string() && c[..10] <= *end.to_string())).collect();
    let mut by: BTreeMap<String, usize> = BTreeMap::new(); for e in &evs { *by.entry(label(e["kind"].as_str().unwrap_or("")).to_string()).or_default() += 1; }
    let md = format!("## The week of {start} to {end}\n\n{}\n", if evs.is_empty() { "No new alert.".to_string() } else { by.iter().map(|(k, n)| format!("- {k}: {n}")).collect::<Vec<_>>().join("\n") });
    Ok(json!({"week_start": start.to_string(), "week_end": end.to_string(), "markdown": md}))
}
trait Wd { fn weekday_from_monday(&self) -> u32; }
impl Wd for chrono::NaiveDate { fn weekday_from_monday(&self) -> u32 { chrono::Datelike::weekday(self).num_days_from_monday() } }
pub fn alerts_view(con: &Connection, s: &Settings, status: Option<&str>, kind: Option<&str>, severity: Option<&str>, include_resolved: bool) -> Result<Value> {
    let all = listing(con, Some(include_resolved))?; let mut counts: Map<String, Value> = Map::new(); let sm = summary(con)?; for k in ["open", "new", "high"] { counts.insert(k.into(), sm[k].clone()); }
    for st in ["snoozed", "acked", "suppressed"] { counts.insert(st.into(), json!(all.iter().filter(|e| e["status"] == st && !e["resolved"].as_bool().unwrap_or(false)).count())); }
    let items: Vec<Value> = all.into_iter().filter(|e| status.is_none_or(|x| e["status"] == x) && kind.is_none_or(|x| e["kind"] == x) && severity.is_none_or(|x| e["severity"] == x)).collect();
    Ok(json!({"ready": true, "enabled": s.enabled, "items": items, "counts": counts, "kinds": kinds(con, s)?, "settings": {"min_severity": s.min_severity, "external_detail": s.external_detail, "quiet_hours": s.quiet_hours, "max_per_week": s.max_per_week}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn texts() {
        let ev = vec![json!({"kind": "unusual_charge", "severity": "high", "payload": {"amount_c": 123456}}), json!({"kind": "consent", "severity": "medium", "payload": {}})];
        assert_eq!(minimal_text(&ev, ""), "Coach: 2 new alerts (1 high). Open the app.");
        assert_eq!(summary_text(&ev, ""), "Coach: 2 new alerts (1 high).\n- Unusual charge (high), about 1200 EUR\n- Bank consent expiring or expired (medium)\nOpen the app.");
        assert_eq!(round_amount(1234), 10); assert_eq!(round_amount(300), 0);
    }
}
