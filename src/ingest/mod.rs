//! Ingest: file imports (CSV / OFX / CAMT.053 / CODA) with the same dedup guarantees as the API sync, accounts, fingerprints.
//! Dedup = multiset on the content fingerprint per account: re-importing a file adds 0, overlapping files add only the
//! difference, and genuine same-day same-amount same-label purchases are kept.
pub mod accounts;
pub mod fingerprint;
pub mod parsers;
pub mod profiles;

use crate::{rs, util::*};
use accounts::{ensure_hashes, ident_hash, normalize_iban, resolve_account};
use anyhow::{bail, Result};
use fingerprint::{content_key, import_tx_key, Key};
use parsers::{Parsed, Row};
use profiles::Profile;
use rusqlite::{params, Connection, OptionalExtension};
use std::{collections::HashMap, path::Path};

#[derive(Debug, Default)]
pub struct ImportReport { pub file: String, pub format: String, pub account_uid: String, pub account_created: bool, pub dry_run: bool, pub rows_total: usize, pub rows_new: usize, pub rows_duplicate: usize, pub rows_skipped: usize, pub date_min: String, pub date_max: String, pub already_imported: Option<String>, pub new_rows: Vec<Row>, pub warnings: Vec<String>, pub import_id: Option<i64> }

pub fn slug(label: &str) -> Result<String> {
    let t = strip_accents(label).to_lowercase(); let t = rs!(r"[^a-z0-9]+").replace_all(&t, "-").trim_matches('-').to_string();
    if t.is_empty() { bail!("cannot derive an account id from label {label:?}"); } Ok(truncate_chars(&t, 48))
}

/// (stable uid, created). `new:Label` makes (or reuses) the manual account `imp-<slug>`.
pub fn ensure_account(con: &Connection, spec: &str, bank: Option<&str>, currency: Option<&str>, iban: Option<&str>, create: bool, force: bool) -> Result<(String, bool)> {
    let Some(label) = spec.strip_prefix("new:") else { return Ok((resolve_account(con, spec)?, false)) };
    ensure_hashes(con)?;
    let label = label.trim(); if label.is_empty() { bail!("--account new:LABEL needs a label"); }
    let uid = format!("imp-{}", slug(label)?);
    if let Some(src) = con.query_row("SELECT source FROM accounts WHERE uid=?", [&uid], |r| r.get::<_, String>(0)).optional()? { if src != "import" { bail!("{uid} exists and is not a manual account"); } return Ok((uid, false)); }
    if let (Some(i), false) = (iban, force) {
        if let Some((u, l)) = con.query_row("SELECT uid, COALESCE(label, name, uid) FROM accounts WHERE identification_hash=? AND uid<>?", params![ident_hash(Some(i), None), uid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? {
            let n = normalize_iban(Some(i)).unwrap_or_default(); bail!("the file's IBAN (…{}) already belongs to account {u} ({l}): import there with --account {u}, or pass --force to create a separate manual account anyway", &n[n.len().saturating_sub(4)..]);
        }
    }
    if create {
        if con.query_row("SELECT 1 FROM accounts WHERE LOWER(label)=LOWER(?)", [label], |_| Ok(())).optional()?.is_some() { bail!("an account is already labelled {label:?}: use --account <uid|label> for it"); }
        let iban = normalize_iban(iban);
        // name stays NULL on purpose: account names feed the household-member detection of the parser
        con.execute("INSERT INTO accounts(uid, session_id, name, iban, currency, cash_account_type, raw, bank, label, source, identification_hash) VALUES (?,NULL,NULL,?,?,NULL,'{}',?,?,'import',?)",
            params![uid, iban, currency, bank.unwrap_or(label), label, ident_hash(iban.as_deref(), None)])?;
    }
    Ok((uid, true))
}

fn check_account(con: &Connection, uid: &str, p: &Parsed, rows: &[Row]) -> Result<()> {
    let (ai, ac): (Option<String>, Option<String>) = con.query_row("SELECT iban, currency FROM accounts WHERE uid=?", [uid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or((None, None));
    if let (Some(fi), Some(a)) = (&p.iban, &ai) { if normalize_iban(Some(fi)) != normalize_iban(Some(a)) { bail!("the file is for IBAN …{} but account {uid} has …{}: wrong account?", &fi[fi.len().saturating_sub(4)..], &a[a.len().saturating_sub(4)..]); } }
    let mut curs: Vec<_> = rows.iter().map(|r| r.currency.clone()).collect(); curs.sort(); curs.dedup();
    if let Some(a) = ac.filter(|a| a != "XXX") { if curs.iter().any(|c| *c != a) { bail!("the file has {} rows but account {uid} is {a}; use --currency to import one currency only", curs.join(", ")); } }
    if curs.len() > 1 { bail!("the file mixes currencies ({}): import one at a time with --currency, into one account each", curs.join(", ")); }
    Ok(())
}

pub struct ImportOpts<'a> { pub account: &'a str, pub profile: Option<&'a Profile>, pub fmt: Option<&'a str>, pub dry_run: bool, pub bank: Option<&'a str>, pub currency: Option<&'a str>, pub iban: Option<&'a str>, pub force: bool }

pub fn import_file(con: &Connection, path: &Path, o: ImportOpts) -> Result<ImportReport> {
    if !path.is_file() { bail!("file not found: {}", path.display()); }
    let data = std::fs::read(path)?; let sha = sha256_hex(&data);
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let fmt = match o.fmt { Some(f) => f, None => parsers::detect_format(&name, &data)? };
    let parsed = match fmt { "ofx" => parsers::parse_ofx(&data)?, "camt053" => parsers::parse_camt053(&data)?, "coda" => parsers::parse_coda(&data)?,
        "csv" => parsers::parse_csv(&data, o.profile.ok_or_else(|| anyhow::anyhow!("a CSV import needs --profile NAME or an inline mapping (--date-col/--desc-col/--amount-col ...); see --list-profiles"))?)?, f => bail!("unknown format {f:?}") };
    let mut rows = parsed.rows.clone(); let mut skipped = 0;
    if let Some(c) = o.currency { let c = c.to_uppercase(); let n = rows.len(); rows.retain(|r| r.currency == c); skipped = n - rows.len(); if rows.is_empty() { bail!("no {c} rows in the file"); } }
    let (uid, created) = ensure_account(con, o.account, o.bank, Some(&rows[0].currency), o.iban.or(parsed.iban.as_deref()), !o.dry_run, o.force)?;
    let mut rep = ImportReport { file: name.clone(), format: parsed.format.into(), account_uid: uid.clone(), account_created: created, dry_run: o.dry_run, rows_total: rows.len(), rows_skipped: skipped, warnings: parsed.warnings.clone(), ..Default::default() };
    rep.date_min = rows.iter().map(|r| r.date.clone()).min().unwrap(); rep.date_max = rows.iter().map(|r| r.date.clone()).max().unwrap();
    check_account(con, &uid, &parsed, &rows)?;
    rep.already_imported = con.query_row("SELECT imported_at FROM imports WHERE account_uid=? AND file_sha256=?", [&uid, &sha], |r| r.get(0)).optional()?;
    let mut existing: HashMap<Key, usize> = HashMap::new();
    if !(created && o.dry_run) {
        let mut st = con.prepare("SELECT booking_date, amount, description FROM transactions WHERE account_uid=? AND booking_date BETWEEN ? AND ?")?;
        for r in st.query_map([&uid, &rep.date_min, &rep.date_max], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<f64>>(1)?.unwrap_or(0.0), r.get::<_, Option<String>>(2)?)))? { let (d, a, de) = r?; *existing.entry(content_key(&d, a, de.as_deref())).or_default() += 1; }
    }
    let mut seen: HashMap<Key, usize> = HashMap::new(); let mut todo = vec![];
    for r in &rows { let k = content_key(&r.date, r.amount, Some(&r.description)); let n = seen.entry(k.clone()).or_default(); *n += 1; if *n > existing.get(&k).copied().unwrap_or(0) { todo.push((r.clone(), import_tx_key(&uid, &k, *n))); } }
    rep.rows_duplicate = rows.len() - todo.len(); rep.rows_new = todo.len(); rep.new_rows = todo.iter().map(|t| t.0.clone()).collect();
    if o.dry_run { return Ok(rep); }
    let ts = now_iso();
    con.execute_batch("BEGIN")?;
    let res = (|| -> Result<usize> {
        con.execute("INSERT INTO imports(account_uid, file_name, file_sha256, format, profile, imported_at, rows_total, rows_new) VALUES (?,?,?,?,?,?,?,0)", params![uid, name, sha, parsed.format, o.profile.filter(|_| fmt == "csv").map(|p| p.name.clone()), ts, rows.len() as i64])?;
        let id = con.last_insert_rowid(); rep.import_id = Some(id); let mut ins = 0;
        for (r, key) in &todo {
            let mut raw = serde_json::Map::from_iter([("source".to_string(), "import".into()), ("file".into(), name.clone().into()), ("line".into(), r.line.into())]); raw.extend(r.raw.clone());
            let n = con.execute("INSERT OR IGNORE INTO transactions(tx_key, account_uid, entry_reference, booking_date, value_date, amount, currency, counterparty, description, mcc, bank_tx_code, raw, first_seen, last_seen) VALUES (?,?,?,?,?,?,?,?,?,NULL,NULL,?,?,?)",
                params![key, uid, r.reference, r.date, r.value_date, r.amount, r.currency, r.counterparty, r.description, serde_json::Value::Object(raw).to_string(), ts, ts])?;
            if n > 0 { con.execute("INSERT INTO import_rows VALUES (?,?)", params![key, id])?; ins += 1; }
        }
        con.execute("UPDATE imports SET rows_new=? WHERE id=?", params![ins as i64, id])?;
        // a statement's closing balance (CODA record 8) is the account's balance on that day, unless a later one is stored
        if let Some((dd, v)) = &parsed.balance { if con.query_row("SELECT 1 FROM balances WHERE account_uid=? AND fetched_at>=?", params![uid, dd], |_| Ok(())).optional()?.is_none() {
            con.execute("INSERT INTO balances(account_uid, fetched_at, balance_type, amount, currency, reference_date) VALUES (?,?, 'CLBD', ?,?,?)", params![uid, dd, v, rows[0].currency, dd])?; rep.warnings.push(format!("closing balance {v:.2} on {dd} stored")); } }
        Ok(ins)
    })();
    match res { Ok(n) => { con.execute_batch("COMMIT")?; rep.rows_new = n; rep.rows_duplicate = rows.len() - n; Ok(rep) } Err(e) => { let _ = con.execute_batch("ROLLBACK"); Err(e) } }
}

pub fn format_report(r: &ImportReport, sample: usize) -> String {
    let mut l = vec![format!("{}{} ({}) -> account {}{}", if r.dry_run { "DRY RUN - nothing written. " } else { "" }, r.file, r.format, r.account_uid, if r.account_created { " (new manual account)" } else { "" }),
        format!("  rows read: {}  new: {}  already present: {}{}", r.rows_total, r.rows_new, r.rows_duplicate, if r.rows_skipped > 0 { format!("  other currency skipped: {}", r.rows_skipped) } else { String::new() }), format!("  dates: {} -> {}", r.date_min, r.date_max)];
    if let Some(a) = &r.already_imported { l.push(format!("  note: this exact file was already imported on {a}")); }
    l.extend(r.warnings.iter().map(|w| format!("  warning: {w}")));
    if r.dry_run && !r.new_rows.is_empty() { l.push(format!("  would insert ({} of {}):", sample.min(r.new_rows.len()), r.new_rows.len())); l.extend(r.new_rows.iter().take(sample).map(|x| format!("    {} {:>10.2} {}  {}", x.date, x.amount, x.currency, truncate_chars(&x.description, 60)))); }
    if !r.dry_run && r.rows_new > 0 { l.push("  next: `finch normalize` to parse the new rows (categories are then resolved on read)".into()); }
    l.join("\n")
}
