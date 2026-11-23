//! Encrypted, dated backups of the database + `memory/`, and `finch export`, in the Python app's formats (either app restores
//! the other's archives). File: MAGIC(6) | salt(16) | nonce(12) | AES-256-GCM(tar or zip, aad = the header); key = scrypt(N=2^15,
//! r=8, p=1) of the `backup_key` secret. Backups (AFCBK1, a tar of `finance.db` + `memory/`) are verified after writing and pruned to
//! `backup.retention`; exports (AFCEX1, a zip of every table as JSON, a transactions CSV and the memory files) are encrypted unless the
//! user types the plain-export phrase at a terminal. Nothing here makes a network call.
use crate::{config::Config, db};
use aes_gcm::{aead::{Aead, KeyInit, Payload}, Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::{io::{Read, Write}, path::{Path, PathBuf}};

pub const MAGIC: &[u8; 6] = b"AFCBK1";
pub const EXPORT_MAGIC: &[u8; 6] = b"AFCEX1";
const PREFIX: &str = "coach-backup-";
const SUFFIX: &str = ".tar.enc";
pub const PLAIN_PHRASE: &str = "EXPORT UNENCRYPTED";
const EXCLUDED: [&str; 7] = ["sessions", "pending_auth", "schema_migrations", "llm_batches", "llm_batch_jobs", "sqlite_sequence", "consent_alerts"];
const SKIP_DIRS: [&str; 3] = [".history.git", ".proposals", ".backups"];

fn derive(key: &str, salt: &[u8]) -> Result<[u8; 32]> { let mut out = [0u8; 32]; scrypt::scrypt(key.as_bytes(), salt, &scrypt::Params::new(15, 8, 1, 32).map_err(|e| anyhow!("{e}"))?, &mut out).map_err(|e| anyhow!("{e}"))?; Ok(out) }
pub fn encrypt(data: &[u8], key: &str, magic: &[u8; 6]) -> Result<Vec<u8>> {
    let (mut salt, mut nonce) = ([0u8; 16], [0u8; 12]); rand::thread_rng().fill_bytes(&mut salt); rand::thread_rng().fill_bytes(&mut nonce);
    let header = [magic.as_slice(), &salt, &nonce].concat(); let c = Aes256Gcm::new_from_slice(&derive(key, &salt)?).unwrap();
    let ct = c.encrypt(Nonce::from_slice(&nonce), Payload { msg: data, aad: &header }).map_err(|_| anyhow!("encryption failed"))?; Ok([header, ct].concat())
}
pub fn decrypt(blob: &[u8], key: &str, magic: &[u8; 6]) -> Result<Vec<u8>> {
    if blob.len() < 34 + 16 || &blob[..6] != magic { bail!("Not a coach {} file (bad header)", if magic == MAGIC { "backup" } else { "export" }); }
    let (header, ct) = blob.split_at(34); let c = Aes256Gcm::new_from_slice(&derive(key, &header[6..22])?).unwrap();
    c.decrypt(Nonce::from_slice(&header[22..34]), Payload { msg: ct, aad: header }).map_err(|_| anyhow!("Decryption failed: wrong backup_key or corrupted file"))
}
fn key(k: Option<&str>) -> Result<String> { k.map(String::from).map(Ok).unwrap_or_else(|| crate::secrets::get_secret("backup_key", true)?.ok_or_else(|| anyhow!("the secret 'backup_key' is not set"))) }
/// The memory files to archive (no history, proposals, backups, locks, or links leading outside).
fn memory_files(mem: &Path) -> Vec<(PathBuf, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let mut es: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect(); es.sort_by_key(|e| e.file_name());
        for e in es { let p = e.path(); let n = e.file_name().to_string_lossy().to_string(); let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.is_dir() { if !SKIP_DIRS.contains(&n.as_str()) { walk(base, &p, out); } } else if m.is_file() && ![".lock", ".lock.finch", ".DS_Store"].contains(&n.as_str()) { out.push((p.clone(), format!("memory/{}", p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/")))); } }
    }
    let mut out = vec![]; if mem.is_dir() { walk(mem, mem, &mut out); } out
}
pub fn list(cfg: &Config) -> Vec<PathBuf> {
    let rx = regex::Regex::new(&format!(r"^{}(\d{{8}}-\d{{6}})(?:-(\d+))?{}$", PREFIX, regex::escape(SUFFIX))).unwrap();
    let mut v: Vec<(String, i64, PathBuf)> = std::fs::read_dir(&cfg.backup_dir).into_iter().flatten().flatten().filter_map(|e| { let n = e.file_name().to_string_lossy().to_string(); let c = rx.captures(&n)?; Some((c[1].to_string(), c.get(2).map_or(0, |x| x.as_str().parse().unwrap_or(0)), e.path())) }).collect();
    v.sort(); v.into_iter().map(|x| x.2).collect()
}
/// Decrypt and check an archive: every entry readable, a safe path, and the database present. Returns the entry names.
pub fn verify(path: &Path, k: Option<&str>) -> Result<Vec<String>> {
    let data = decrypt(&std::fs::read(path)?, &key(k)?, MAGIC)?; let mut ar = tar::Archive::new(&data[..]); let mut names = vec![];
    for e in ar.entries()? { let mut e = e?; let n = e.path()?.to_string_lossy().replace('\\', "/"); safe(&n)?; if !(e.header().entry_type().is_file() || e.header().entry_type().is_dir()) { bail!("Unsupported entry type in archive: {n}"); } std::io::copy(&mut e, &mut std::io::sink())?; names.push(n); }
    if !names.iter().any(|n| n == "finance.db") { bail!("Archive does not contain the database"); } Ok(names)
}
fn safe(n: &str) -> Result<()> { if n.starts_with('/') || n.split('/').any(|s| s == ".." || s.contains(':')) || n.contains('\\') { bail!("Unsafe path in archive: {n}"); } Ok(()) }
/// A dated (UTC) encrypted archive, verified after writing; then the oldest are pruned to the retention. Returns (path, pruned).
pub fn create(cfg: &Config, con: &Connection, k: Option<&str>) -> Result<(PathBuf, Vec<PathBuf>)> {
    let k = key(k)?; std::fs::create_dir_all(&cfg.backup_dir)?;
    let tmpdir = tempdir_in(&cfg.backup_dir)?; let snap = tmpdir.join("snapshot.db"); db::snapshot(con, &snap, None)?;
    let mut tb = tar::Builder::new(vec![]); tb.mode(tar::HeaderMode::Deterministic);
    let _lock = if cfg.memory_dir.is_dir() { Some(crate::memstore::Lock::take(&cfg.memory_dir)?) } else { None };
    tb.append_path_with_name(&snap, "finance.db")?; for (p, n) in memory_files(&cfg.memory_dir) { tb.append_path_with_name(&p, &n)?; }
    let blob = encrypt(&tb.into_inner()?, &k, MAGIC)?; drop(_lock); let _ = std::fs::remove_dir_all(&tmpdir);
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string(); let mut dest = cfg.backup_dir.join(format!("{PREFIX}{stamp}{SUFFIX}")); let mut n = 1;
    while dest.exists() { dest = cfg.backup_dir.join(format!("{PREFIX}{stamp}-{n}{SUFFIX}")); n += 1; }
    crate::memstore::atomic_write_bytes(&dest, &blob)?;
    if let Err(e) = verify(&dest, Some(&k)) { let _ = std::fs::remove_file(&dest); return Err(e); }
    let all = list(cfg); let keep = cfg.backup_retention.max(1) as usize; let pruned: Vec<PathBuf> = if all.len() > keep { all[..all.len() - keep].to_vec() } else { vec![] };
    for p in &pruned { std::fs::remove_file(p)?; } Ok((dest, pruned))
}
fn tempdir_in(d: &Path) -> Result<PathBuf> { let p = d.join(format!("{PREFIX}tmp-{:08x}", rand::random::<u32>())); std::fs::create_dir_all(&p)?; Ok(p) }
/// Extract into `to` (never overwriting unless `force`); files are written owner-only.
pub fn restore(archive: &Path, to: &Path, force: bool, k: Option<&str>) -> Result<Vec<PathBuf>> {
    let data = decrypt(&std::fs::read(archive)?, &key(k)?, MAGIC)?; let mut entries = vec![];
    for e in tar::Archive::new(&data[..]).entries()? { let mut e = e?; let n = e.path()?.to_string_lossy().replace('\\', "/"); safe(&n)?; let dir = e.header().entry_type().is_dir();
        if !(dir || e.header().entry_type().is_file()) { bail!("Unsupported entry type in archive: {n}"); } let mut b = vec![]; e.read_to_end(&mut b)?; entries.push((n, dir, b)); }
    let clash: Vec<&String> = entries.iter().filter(|(n, d, _)| !d && to.join(n).exists()).map(|x| &x.0).collect();
    if !clash.is_empty() && !force { bail!("Refusing to overwrite {} existing file(s), e.g. {} (restore into an empty directory, or pass --force)", clash.len(), to.join(clash[0]).display()); }
    let mut out = vec![]; for (n, d, b) in entries { let p = to.join(&n); if d { std::fs::create_dir_all(&p)?; } else { crate::memstore::atomic_write_bytes(&p, &b)?; out.push(p); } } Ok(out)
}

// ---------------------------------------------------------------- export
fn cell(v: &str) -> String { if v.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{v}") } else { v.into() } }
fn sql_json(v: rusqlite::types::ValueRef) -> Value { use rusqlite::types::ValueRef::*; match v { Null => Value::Null, Integer(i) => json!(i), Real(f) => json!(f), Text(t) => json!(String::from_utf8_lossy(t)), Blob(b) => json!(hex::encode(b)) } }
pub fn build_zip(cfg: &Config, con: &Connection) -> Result<Vec<u8>> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![])); let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut counts = Map::new();
    for t in db::user_tables(con)? { if EXCLUDED.contains(&t.as_str()) { continue; }
        let mut st = con.prepare(&format!("SELECT * FROM \"{t}\""))?; let cols: Vec<String> = st.column_names().into_iter().map(String::from).collect();
        let rows: Vec<Value> = st.query_map([], |r| Ok(Value::Object(cols.iter().enumerate().map(|(i, c)| (c.clone(), sql_json(r.get_ref(i).unwrap()))).collect())))?.collect::<rusqlite::Result<_>>()?;
        counts.insert(t.clone(), json!(rows.len())); z.start_file(format!("data/{t}.json"), o)?; z.write_all(serde_json::to_string_pretty(&rows)?.as_bytes())?; }
    let mut w = csv::Writer::from_writer(vec![]); w.write_record(["date", "amount", "currency", "account", "bank", "counterparty", "description", "merchant", "category", "tx_type", "tx_key"])?;
    let mut st = con.prepare("SELECT t.booking_date, t.amount, t.currency, COALESCE(a.label, a.name, a.uid), a.bank, t.counterparty, t.description, COALESCE(m.merchant_name, e.merchant_key, ''), COALESCE(o.category, m.category, ''), COALESCE(e.tx_type, ''), t.tx_key
        FROM transactions t LEFT JOIN accounts a ON a.uid=t.account_uid LEFT JOIN tx_enriched e ON e.tx_key=t.tx_key LEFT JOIN merchants m ON m.merchant_key=e.merchant_key LEFT JOIN tx_overrides o ON o.tx_key=t.tx_key ORDER BY t.booking_date, t.tx_key")?;
    let mut rows = st.query([])?; while let Some(r) = rows.next()? { let rec: Vec<String> = (0..11).map(|i| match sql_json(r.get_ref(i).unwrap()) { Value::Null => String::new(), Value::String(s) => cell(&s), x => x.to_string() }).collect(); w.write_record(&rec)?; }
    z.start_file("transactions.csv", o)?; z.write_all(&w.into_inner().map_err(|e| anyhow!("{e}"))?)?;
    let cats = crate::classify::rules::Rules::load(cfg)?.categories; z.start_file("categories.json", o)?; z.write_all(serde_json::to_string_pretty(&cats)?.as_bytes())?;
    let mf = memory_files(&cfg.memory_dir); for (p, n) in &mf { z.start_file(n.clone(), o)?; z.write_all(&std::fs::read(p)?)?; }
    let migs: Vec<i64> = con.prepare("SELECT version FROM schema_migrations ORDER BY version")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut not: Vec<String> = EXCLUDED.iter().map(|x| x.to_string()).collect(); not.sort(); not.extend(["memory/.history.git", "memory/.proposals", "secrets (Keychain)", "backups"].map(String::from));
    let now = chrono::Utc::now();
    z.start_file("manifest.json", o)?; z.write_all(serde_json::to_string_pretty(&json!({"created": now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(), "format": "zip", "tables": counts, "memory_files": mf.len(), "not_included": not, "schema_migrations": migs}))?.as_bytes())?;
    z.start_file("README.txt", o)?; z.write_all(format!("AI finance coach: export of {}\n\nmanifest.json      row counts and what is not included\ntransactions.csv   your transactions (date, amount, account, counterparty, merchant, category)\ndata/<table>.json  every table of the database (list of {{column: value}} objects), except bank session credentials\nmemory/            your household memory files (YAML / Markdown / documents), without the change history and proposals\ncategories.json    the category taxonomy\n\nThis archive holds your whole financial life. Keep it encrypted; delete it when you no longer need it.\n", now.format("%Y-%m-%d %H:%M UTC")).as_bytes())?;
    Ok(z.finish()?.into_inner())
}
/// The export archive: encrypted (verified by decrypting it again) unless `plain`. Never overwrites a file.
pub fn export(cfg: &Config, con: &Connection, out: Option<PathBuf>, plain: bool, k: Option<&str>) -> Result<PathBuf> {
    let data = build_zip(cfg, con)?; let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let out = out.unwrap_or_else(|| cfg.data_dir.join("exports").join(format!("coach-export-{stamp}{}", if plain { ".zip" } else { ".zip.enc" })));
    if out.exists() { bail!("{} already exists: choose another --out (nothing is overwritten)", out.display()); }
    if plain { crate::memstore::atomic_write_bytes(&out, &data)?; return Ok(out); }
    let k = key(k)?; crate::memstore::atomic_write_bytes(&out, &encrypt(&data, &k, EXPORT_MAGIC)?)?;
    let back = decrypt(&std::fs::read(&out)?, &k, EXPORT_MAGIC).and_then(|b| zip::ZipArchive::new(std::io::Cursor::new(b)).context("not a zip").map(|z| z.file_names().any(|n| n == "manifest.json")));
    if !back.unwrap_or(false) { let _ = std::fs::remove_file(&out); bail!("the written archive failed its check"); } Ok(out)
}
pub fn decrypt_export(archive: &Path, to: &Path, k: Option<&str>) -> Result<PathBuf> {
    let data = decrypt(&std::fs::read(archive)?, &key(k)?, EXPORT_MAGIC)?; let dest = to.join(archive.file_name().unwrap().to_string_lossy().trim_end_matches(".enc").to_string());
    if dest.exists() { bail!("{} already exists", dest.display()); } crate::memstore::atomic_write_bytes(&dest, &data)?; Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn roundtrip() { let b = encrypt(b"hello", "k1", MAGIC).unwrap(); assert_eq!(&b[..6], b"AFCBK1"); assert_eq!(decrypt(&b, "k1", MAGIC).unwrap(), b"hello"); assert!(decrypt(&b, "k2", MAGIC).is_err()); assert!(decrypt(&b, "k1", EXPORT_MAGIC).is_err()); }
}
