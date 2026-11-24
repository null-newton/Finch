//! Database access: every code path opens the DB through [`connect`]. Same schema and `schema_migrations` table as the
//! Python app, so both can open the same file. SQLCipher with the `db_key` secret when built with `--features sqlcipher`;
//! a plaintext DB is refused unless `--insecure` / `insecure_plaintext_db = true`.
use crate::{config::Config, ingest::fingerprint as fp, secrets, util::*};
use anyhow::{bail, Context, Result};
use rusqlite::{functions::FunctionFlags, params, Connection, OptionalExtension};
use std::{collections::{BTreeMap, HashMap, HashSet}, path::{Path, PathBuf}};

pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
pub enum Step { Sql(&'static str), Rust(fn(&Connection) -> Result<()>) }
pub struct Migration { pub version: i64, pub name: &'static str, pub step: Step }

pub fn migrations() -> Vec<Migration> {
    vec![
        Migration { version: 1, name: "initial", step: Step::Sql(include_str!("../migrations/0001_initial.sql")) },
        Migration { version: 2, name: "indexes", step: Step::Sql(include_str!("../migrations/0002_indexes.sql")) },
        Migration { version: 3, name: "multibank_consents_imports_transfers", step: Step::Sql(include_str!("../migrations/0003_multibank_consents_imports_transfers.sql")) },
        Migration { version: 4, name: "account_needs_review", step: Step::Sql(include_str!("../migrations/0004_account_needs_review.sql")) },
        Migration { version: 5, name: "pending_auth_hardening", step: Step::Sql(include_str!("../migrations/0005_pending_auth_hardening.sql")) },
        Migration { version: 6, name: "e2_parsers_llm_entities_splits", step: Step::Sql(include_str!("../migrations/0006_e2_parsers_llm_entities_splits.sql")) },
        Migration { version: 7, name: "rekey_position_refs", step: Step::Rust(m0007) },
        Migration { version: 8, name: "batches_entity_category_user", step: Step::Sql(include_str!("../migrations/0008_batches_entity_category_user.sql")) },
        Migration { version: 9, name: "batch_jobs", step: Step::Sql(include_str!("../migrations/0009_batch_jobs.sql")) },
        Migration { version: 10, name: "batch_purpose_usage_ref_rename_journal", step: Step::Rust(m0010) },
        Migration { version: 11, name: "analytics", step: Step::Sql(include_str!("../migrations/0011_analytics.sql")) },
        Migration { version: 12, name: "price_change_dismissals", step: Step::Sql(include_str!("../migrations/0012_price_change_dismissals.sql")) },
        Migration { version: 13, name: "insights", step: Step::Sql(include_str!("../migrations/0013_insights.sql")) },
        Migration { version: 14, name: "alternatives", step: Step::Sql(include_str!("../migrations/0014_alternatives.sql")) },
        Migration { version: 15, name: "decisions", step: Step::Sql(include_str!("../migrations/0015_decisions.sql")) },
        Migration { version: 16, name: "net_worth_history", step: Step::Sql(include_str!("../migrations/0016_net_worth_history.sql")) },
        Migration { version: 17, name: "alerts", step: Step::Sql(include_str!("../migrations/0017_alerts.sql")) },
        Migration { version: 18, name: "alert_channel_state", step: Step::Sql(include_str!("../migrations/0018_alert_channel_state.sql")) },
        Migration { version: 19, name: "egress_journal", step: Step::Sql(include_str!("../migrations/0019_egress_journal.sql")) },
        Migration { version: 20, name: "ai_label_compliance", step: Step::Rust(m0020) },
        Migration { version: 21, name: "quality_gold_eval", step: Step::Sql(include_str!("../migrations/0021_quality_gold_eval.sql")) },
        Migration { version: 22, name: "household_people", step: Step::Sql(include_str!("../migrations/0022_household_people.sql")) },
        Migration { version: 23, name: "dateless_rows_to_pending", step: Step::Rust(m0023) },
        Migration { version: 24, name: "passkeys", step: Step::Sql(include_str!("../migrations/0024_passkeys.sql")) },
        Migration { version: 25, name: "merge_parked_reference_duplicates", step: Step::Rust(m0025) },
    ]
}

/// Some(true) plain SQLite, Some(false) encrypted / unknown, None missing or empty.
pub fn is_plaintext(p: &Path) -> Option<bool> {
    let b = std::fs::read(p).ok().filter(|b| !b.is_empty())?;
    Some(b.len() >= 16 && &b[..16] == SQLITE_MAGIC)
}
fn sql_quote(s: &str) -> String { format!("'{}'", s.replace('\'', "''")) }

pub fn open_raw(p: &Path, key: Option<&str>) -> Result<Connection> {
    let con = Connection::open(p)?;
    if let Some(k) = key {
        if !cfg!(feature = "sqlcipher") { bail!("{} is encrypted, but this build of finch has no SQLCipher support: rebuild with `cargo build --release --features sqlcipher`", p.display()); }
        con.execute_batch(&format!("PRAGMA key = {};", sql_quote(k)))?;
    }
    con.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).with_context(|| format!("Cannot read {}: wrong db_key, or the file is not a database", p.display()))?;
    let cache = std::sync::Mutex::new(HashMap::<String, Option<fancy_regex::Regex>>::new());
    con.create_scalar_function("REGEXP", 2, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, move |ctx| {
        let (pat, val): (String, Option<String>) = (ctx.get(0)?, ctx.get(1)?);
        let mut c = cache.lock().unwrap();
        let r = c.entry(pat.clone()).or_insert_with(|| rx_i(&pat));
        Ok(val.is_some_and(|v| r.as_ref().is_some_and(|r| is(r, &v))))
    })?;
    con.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(con)
}

pub struct Opts { pub insecure: bool, pub migrate: bool, pub create: bool, pub path: Option<PathBuf> }
impl Default for Opts { fn default() -> Self { Self { insecure: false, migrate: true, create: false, path: None } } }

/// Open the application database (missing file = error unless `create`).
pub fn connect(cfg: &Config, o: Opts) -> Result<Connection> {
    let path = o.path.clone().unwrap_or_else(|| cfg.db_path.clone());
    let allow_plain = o.insecure || cfg.insecure_plaintext_db;
    let state = is_plaintext(&path);
    if state.is_none() && !o.create { bail!("No database at {} (check db_path / data_dir / COACH_HOME). Create one with `finch db migrate --create`.", path.display()); }
    if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
    let (con, key) = match state {
        Some(true) => { if !allow_plain { bail!("Refusing to open plaintext database {}.\n  - knowingly use it: pass --insecure, or set insecure_plaintext_db = true in config.toml\n  - encrypt it: `finch db encrypt` (needs --features sqlcipher)", path.display()); } (open_raw(&path, None)?, None) }
        Some(false) => { let k = secrets::get_secret("db_key", true)?; (open_raw(&path, k.as_deref())?, k) }
        None => { let k = if cfg!(feature = "sqlcipher") { secrets::get_secret("db_key", !allow_plain)? } else if allow_plain { None } else { bail!("this build has no SQLCipher support, so a new database would be plaintext: pass --insecure to create one knowingly, or rebuild with --features sqlcipher") }; (open_raw(&path, k.as_deref())?, k) }
    };
    if o.migrate {
        if state.is_some() && !cfg.db_auto_migrate { let done = applied(&con)?; if let Some(m) = migrations().iter().find(|m| !done.contains_key(&m.version)) { bail!("{} is behind this version ({:04}_{} pending) and [db] auto_migrate is false: run `finch db migrate`", path.display(), m.version, m.name); } }
        let ap = apply_migrations(&con, true, key.as_deref())?;
        if !ap.is_empty() && state.is_some() { eprintln!("finch: applied migration(s) {} to {} (disable with [db] auto_migrate = false)", ap.join(", "), path.display()); }
    }
    Ok(con)
}

pub fn applied(con: &Connection) -> Result<BTreeMap<i64, (String, String)>> {
    if !has_table(con, "schema_migrations")? { return Ok(BTreeMap::new()); }
    let mut st = con.prepare("SELECT version, name, applied_at FROM schema_migrations")?;
    let r = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?; Ok(r)
}
pub fn has_table(con: &Connection, t: &str) -> Result<bool> { Ok(con.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", [t], |_| Ok(())).optional()?.is_some()) }
pub fn has_column(con: &Connection, t: &str, c: &str) -> Result<bool> { let mut st = con.prepare(&format!("PRAGMA table_info({t})"))?; let cols: Vec<String> = st.query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?; Ok(cols.iter().any(|x| x == c)) }
pub fn user_tables(con: &Connection) -> Result<Vec<String>> { let mut st = con.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?; let r = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?; Ok(r) }
pub fn table_counts(con: &Connection) -> Result<Vec<(String, i64)>> { user_tables(con)?.into_iter().map(|t| Ok((t.clone(), con.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))?))).collect() }
fn db_file(con: &Connection) -> Option<PathBuf> { con.path().filter(|p| !p.is_empty()).map(PathBuf::from) }

/// Consistent copy via the SQLite backup API; `dest` gets the same key.
pub fn snapshot(con: &Connection, dest: &Path, key: Option<&str>) -> Result<()> {
    let mut dst = Connection::open(dest)?;
    if let Some(k) = key { dst.execute_batch(&format!("PRAGMA key = {};", sql_quote(k)))?; }
    rusqlite::backup::Backup::new(con, &mut dst)?.run_to_completion(256, std::time::Duration::ZERO, None)?; Ok(())
}

/// Apply every not-yet-applied migration in order (each in BEGIN IMMEDIATE, re-checked inside); returns the ones applied now.
pub fn apply_migrations(con: &Connection, safety_copy: bool, key: Option<&str>) -> Result<Vec<String>> {
    let all = migrations(); let known: HashSet<i64> = all.iter().map(|m| m.version).collect(); let done = applied(con)?;
    let unknown: Vec<_> = done.keys().filter(|v| !known.contains(v)).collect();
    if !unknown.is_empty() { bail!("Database was migrated by a newer version (unknown migrations {unknown:?}); upgrade finch."); }
    let pending: Vec<_> = all.iter().filter(|m| !done.contains_key(&m.version)).collect();
    if pending.is_empty() { return Ok(vec![]); }
    if let Some(f) = db_file(con).filter(|_| safety_copy) {
        if user_tables(con)?.iter().any(|t| t != "schema_migrations") {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let bak = f.with_file_name(format!("{name}.pre-migrate-{}.bak", chrono::Utc::now().format("%Y%m%dT%H%M%S")));
            if !bak.exists() { snapshot(con, &bak, key)?; }
            let mut olds: Vec<_> = std::fs::read_dir(f.parent().unwrap())?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| { let n = n.to_string_lossy(); n.starts_with(&format!("{name}.pre-migrate-")) && n.ends_with(".bak") })).collect();
            olds.sort(); for o in olds.iter().take(olds.len().saturating_sub(2)) { let _ = std::fs::remove_file(o); }
        }
    }
    con.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL)")?;
    let mut out = vec![];
    for m in pending {
        con.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<bool> {
            if applied(con)?.contains_key(&m.version) { return Ok(false); }
            match &m.step { Step::Sql(s) => con.execute_batch(s)?, Step::Rust(f) => f(con)? }
            con.execute("INSERT INTO schema_migrations(version, name, applied_at) VALUES (?,?,?)", params![m.version, m.name, now_iso()])?; Ok(true)
        })();
        match r { Ok(true) => { con.execute_batch("COMMIT")?; out.push(format!("{:04}_{}", m.version, m.name)); } Ok(false) => con.execute_batch("ROLLBACK")?, Err(e) => { let _ = con.execute_batch("ROLLBACK"); return Err(e.context(format!("migration {:04}_{}", m.version, m.name))); } }
    }
    Ok(out)
}

pub fn status(con: &Connection) -> Result<(Vec<(i64, String, String)>, Vec<(i64, String)>)> {
    let done = applied(con)?;
    Ok(migrations().iter().fold((vec![], vec![]), |(mut a, mut p), m| { match done.get(&m.version) { Some((_, at)) => a.push((m.version, m.name.into(), at.clone())), None => p.push((m.version, m.name.into())) } (a, p) }))
}

// ---------------------------------------------------------------- data migrations (ports of the Python ones)
fn m0007(con: &Connection) -> Result<()> {
    let mut st = con.prepare("SELECT tx_key, account_uid, entry_reference, booking_date, amount, currency, description FROM transactions WHERE tx_key LIKE '%:ref:%'")?;
    let rows: Vec<(String, String, String, Option<String>, f64, Option<String>, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?, r.get::<_, Option<f64>>(4)?.unwrap_or(0.0), r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut groups: BTreeMap<(String, String, fp::Key), Vec<(i64, String)>> = BTreeMap::new();
    for (key, uid, rf, bd, amt, cur, desc) in rows {
        if key != format!("{uid}:ref:{rf}") || !fp::is_position_ref(Some(&rf)) { continue; }
        let idx = rf.rsplit('-').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        groups.entry((uid, cur.unwrap_or_default(), fp::content_key(bd.as_deref().unwrap_or(""), amt, desc.as_deref()))).or_default().push((idx, key));
    }
    let mut plan = vec![];
    for ((uid, cur, ck), mut mem) in groups { mem.sort(); for (occ, (_, old)) in mem.into_iter().enumerate() { plan.push((old, fp::content_tx_key(&uid, Some(&cur), &ck, occ + 1))); } }
    let existing: HashSet<String> = con.prepare("SELECT tx_key FROM transactions")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let news: HashSet<_> = plan.iter().map(|p| &p.1).collect();
    if news.len() != plan.len() || plan.iter().any(|p| existing.contains(&p.1)) { bail!("re-keying would collide with existing keys; aborting the migration"); }
    for (old, new) in plan { fp::rekey_tables(con, &old, &new)?; con.execute("INSERT OR REPLACE INTO tx_key_remap VALUES (?,?,'position-based entry_reference',datetime('now'))", [&old, &new])?; }
    Ok(())
}
fn m0010(con: &Connection) -> Result<()> {
    if !has_column(con, "llm_batches", "purpose")? { con.execute_batch("ALTER TABLE llm_batches ADD COLUMN purpose TEXT")?; }
    if !has_column(con, "llm_usage", "batch_ref")? { con.execute_batch("ALTER TABLE llm_usage ADD COLUMN batch_ref TEXT")?; }
    con.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_batch_ref ON llm_usage(batch_ref) WHERE batch_ref IS NOT NULL;
        CREATE TABLE IF NOT EXISTS taxonomy_renames (token TEXT PRIMARY KEY, old_id TEXT, new_id TEXT, at TEXT)")?; Ok(())
}
/// Schema part of the Python 0020 (the one-off compliance pass over existing insights is not ported: new databases have none).
fn m0020(con: &Connection) -> Result<()> {
    if !has_column(con, "insights", "ai_generated")? { con.execute_batch("ALTER TABLE insights ADD COLUMN ai_generated INTEGER NOT NULL DEFAULT 1; UPDATE insights SET ai_generated=0 WHERE backend IN ('local','code')")?; }
    if !has_column(con, "insights", "compliance")? { con.execute_batch("ALTER TABLE insights ADD COLUMN compliance TEXT NOT NULL DEFAULT '[]'")?; }
    con.execute_batch("CREATE TABLE IF NOT EXISTS compliance_events (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, source TEXT NOT NULL, insight_id TEXT,
        codes TEXT NOT NULL DEFAULT '[]', flags TEXT NOT NULL DEFAULT '[]', lang TEXT NOT NULL DEFAULT 'en'); CREATE INDEX IF NOT EXISTS idx_compliance_ts ON compliance_events(ts)")?; Ok(())
}
/// Python truthiness of a JSON value (`m0023` mirrors `value_date or transaction_date`).
fn truthy(v: &serde_json::Value) -> bool { use serde_json::Value::*; match v { Null => false, Bool(b) => *b, Number(n) => n.as_f64() != Some(0.0), String(s) => !s.is_empty(), Array(a) => !a.is_empty(), Object(o) => !o.is_empty() } }
/// Dateless rows: take the raw `value_date or transaction_date` when booked (status absent, null, "" or "BOOK"), else move to pending.
fn m0023(con: &Connection) -> Result<()> {
    use serde_json::Value::Null;
    let mut st = con.prepare("SELECT tx_key, account_uid, amount, currency, counterparty, description, raw FROM transactions WHERE booking_date IS NULL OR booking_date = ''")?;
    type Row = (String, Option<String>, Option<f64>, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() { return Ok(()); }
    let mut refs = vec![];
    for t in user_tables(con)?.into_iter().filter(|t| t != "transactions" && t != "tx_key_remap") { for c in ["tx_key", "out_tx_key", "in_tx_key"] { if has_column(con, &t, c)? { refs.push((t.clone(), c)); } } }
    for (key, uid, amt, cur, party, desc, raw) in rows {
        let tx: serde_json::Value = raw.as_deref().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
        let (vd, td) = (tx.get("value_date").unwrap_or(&Null), tx.get("transaction_date").unwrap_or(&Null));
        let dv = if truthy(vd) { vd } else { td };
        let day = match dv { Null => None, serde_json::Value::String(s) => Some(s.clone()), v => Some(v.to_string()) };
        let booked = matches!(tx.get("status"), None | Some(Null)) || tx.get("status").and_then(|v| v.as_str()).is_some_and(|s| s.is_empty() || s == "BOOK");
        if let (true, true, Some(dd)) = (truthy(dv), booked, &day) { con.execute("UPDATE transactions SET booking_date=? WHERE tx_key=?", params![truncate_chars(dd, 10), key])?; continue; }
        con.execute("INSERT INTO pending_transactions VALUES (?,?,?,?,?,?,?)", params![uid, day, amt, cur, party, desc, raw])?;
        for (t, c) in &refs { con.execute(&format!("DELETE FROM {t} WHERE {c}=?"), [&key])?; }
        con.execute("DELETE FROM transactions WHERE tx_key=?", [&key])?;
    }
    Ok(())
}
fn m0025(con: &Connection) -> Result<()> {
    let mut st = con.prepare("SELECT tx_key, account_uid, entry_reference, booking_date, amount, description FROM transactions WHERE tx_key LIKE '%:cfp:%' AND entry_reference IS NOT NULL AND entry_reference != ''")?;
    let rows: Vec<(String, String, String, Option<String>, f64, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<f64>>(4)?.unwrap_or(0.0), r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    for (key, uid, rf, bd, amt, desc) in rows {
        if fp::is_position_ref(Some(&rf)) || !key.starts_with(&format!("{uid}:cfp:")) { continue; }
        let kept = format!("{uid}:ref:{rf}");
        let k: Option<(Option<String>, f64, Option<String>)> = con.query_row("SELECT booking_date, amount, description FROM transactions WHERE tx_key=?", [&kept], |r| Ok((r.get(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0), r.get(2)?))).optional()?;
        if let Some((kb, ka, kd)) = k { if fp::content_key(kb.as_deref().unwrap_or(""), ka, kd.as_deref()) == fp::content_key(bd.as_deref().unwrap_or(""), amt, desc.as_deref()) { fp::replace_tx_key(con, &key, &kept, "duplicate of the bank reference row")?; } }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn m0023_matches_python_truthiness_and_status() {
        let con = Connection::open_in_memory().unwrap(); apply_migrations(&con, false, None).unwrap();
        let rows = [("k1", r#"{"value_date":"","transaction_date":"2024-03-05"}"#), ("k2", r#"{"value_date":null,"transaction_date":"2024-03-06T10:00:00"}"#),
            ("k3", r#"{"value_date":"2024-03-07","status":null}"#), ("k4", r#"{"value_date":"2024-03-08","status":1}"#), ("k5", r#"{"value_date":"2024-03-09","status":"OTHR"}"#),
            ("k6", r#"{"value_date":"","transaction_date":""}"#), ("k7", "not json"), ("k8", r#"{"transaction_date":"2024-03-10","status":"BOOK"}"#), ("k9", r#"["2024-03-11"]"#)];
        for (k, raw) in rows { con.execute("INSERT INTO transactions(tx_key, account_uid, amount, raw) VALUES (?,'acc-1',-1.5,?)", params![k, raw]).unwrap(); }
        m0023(&con).unwrap();
        let mut st = con.prepare("SELECT tx_key, booking_date FROM transactions ORDER BY tx_key").unwrap();
        let kept: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(kept, [("k1", "2024-03-05"), ("k2", "2024-03-06"), ("k3", "2024-03-07"), ("k8", "2024-03-10")].map(|(a, b)| (a.to_string(), b.to_string())));
        let mut st = con.prepare("SELECT booking_date FROM pending_transactions").unwrap();
        let mut pend: Vec<Option<String>> = st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect(); pend.sort();
        assert_eq!(pend, [None, None, Some(String::new()), Some("2024-03-08".into()), Some("2024-03-09".into())]);
    }
}
