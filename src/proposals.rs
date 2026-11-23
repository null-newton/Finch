//! Proposals (E6-5): the ONLY way the coach / an LLM changes memory, in the Python app's format (`memory/.proposals/<id>.json`,
//! the same seal and resolution registry), so either app can list, accept or reject them. Creating one never touches the target
//! file; accepting is a human action at a terminal (`finch memory accept`), validated again and recorded in the history.
use crate::memstore::{atomic_write, EditOpts, EditResult, Store};
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

pub const DIRNAME: &str = ".proposals";
const REGISTRY: &str = ".resolved";
const RESOLVED: &str = "resolved";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal { pub id: String, pub status: String, pub created: String, pub file: String, pub ops: Vec<Value>, pub reason: String, pub source: String, #[serde(default)] pub diff: String,
    #[serde(default)] pub changes: Vec<Value>, #[serde(default)] pub evidence: Vec<Value>, pub base_sha256: Option<String>, pub resolved: Option<String>, pub change_id: Option<String>, pub note: Option<String>, pub seal: Option<String>, pub seal_kind: Option<String> }

/// `json.dumps(v, sort_keys=True, ensure_ascii=False)` of Python, byte for byte: what the seal and the registry MACs are computed on.
pub fn py_dumps(v: &Value) -> String {
    match v {
        Value::Null => "null".into(), Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) { Some(f) if f.fract() == 0.0 && f.abs() < 1e16 => format!("{f:.1}"), Some(f) => { let s = format!("{f:e}"); let r = format!("{f}"); if r.len() <= 17 { r } else { s.replace('e', "e+").replace("e+-", "e-") } } None => n.to_string() },
        Value::String(s) => { let mut o = String::from("\""); for c in s.chars() { match c { '"' => o += "\\\"", '\\' => o += "\\\\", '\n' => o += "\\n", '\r' => o += "\\r", '\t' => o += "\\t", '\u{08}' => o += "\\b", '\u{0c}' => o += "\\f", c if (c as u32) < 0x20 => o += &format!("\\u{:04x}", c as u32), c => o.push(c) } } o + "\"" }
        Value::Array(a) => format!("[{}]", a.iter().map(py_dumps).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => { let mut k: Vec<&String> = m.keys().collect(); k.sort(); format!("{{{}}}", k.iter().map(|k| format!("{}: {}", py_dumps(&Value::String((*k).clone())), py_dumps(&m[*k]))).collect::<Vec<_>>().join(", ")) }
    }
}
fn mac(payload: &str) -> (String, &'static str) {
    match crate::secrets::get_secret("proposal_key", false).ok().flatten() {
        Some(k) => { let mut m = Hmac::<sha2::Sha256>::new_from_slice(k.as_bytes()).expect("key"); m.update(payload.as_bytes()); (hex::encode(m.finalize().into_bytes()), "hmac-sha256") }
        None => (crate::util::sha256_hex(payload.as_bytes()), "sha256"),
    }
}
/// Digest of everything that decides or documents what is applied (the status is not part of it).
fn seal(p: &Proposal) -> (String, &'static str) { mac(&py_dumps(&json!({"id": p.id, "created": p.created, "base_sha256": p.base_sha256, "file": p.file, "ops": p.ops, "reason": p.reason, "source": p.source, "evidence": p.evidence}))) }
pub fn is_sealed(p: &Proposal) -> bool { let (d, k) = seal(p); p.seal.as_deref() == Some(d.as_str()) && p.seal_kind.as_deref() == Some(k) }

fn dir(s: &Store) -> Result<PathBuf> { let d = s.root.join(DIRNAME); std::fs::create_dir_all(&d)?; Ok(d) }
fn check_id(pid: &str) -> Result<()> { if !crate::rs!(r"^p-\d{8}-[0-9a-f]{6}$").is_match(pid) { bail!("{pid:?} is not a proposal id (see `finch memory proposals`)"); } Ok(()) }
fn registry(s: &Store) -> Vec<Value> {
    let Ok(t) = std::fs::read_to_string(s.root.join(DIRNAME).join(REGISTRY)) else { return vec![] }; let mut prev = String::new(); let mut out = vec![];
    for l in t.lines() { let Ok(e) = serde_json::from_str::<Value>(l) else { continue };
        let body: Map<String, Value> = ["id", "status", "at", "change_id", "prev"].iter().map(|k| (k.to_string(), e[*k].clone())).collect(); let (d, k) = mac(&py_dumps(&Value::Object(body)));
        if e["prev"].as_str() != Some(prev.as_str()) || e["kind"].as_str() != Some(k) || e["mac"].as_str() != Some(d.as_str()) { eprintln!("WARNING: ignoring a proposal registry entry that does not verify"); continue; }
        prev = d; out.push(e); }
    out
}
fn registry_add(s: &Store, pid: &str, status: &str, change_id: Option<&str>) -> Result<()> {
    let prev = registry(s).last().and_then(|e| e["mac"].as_str().map(String::from)).unwrap_or_default();
    let mut e = json!({"id": pid, "status": status, "at": now_utc(), "change_id": change_id, "prev": prev}); let (m, k) = mac(&py_dumps(&e)); e["mac"] = m.into(); e["kind"] = k.into();
    use std::io::Write; let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir(s)?.join(REGISTRY))?; writeln!(f, "{}", py_dumps_compact(&e))?; Ok(())
}
fn py_dumps_compact(v: &Value) -> String { let m = v.as_object().unwrap(); format!("{{{}}}", m.iter().map(|(k, x)| format!("{}: {}", py_dumps(&Value::String(k.clone())), py_dumps(x))).collect::<Vec<_>>().join(", ")) }
fn now_utc() -> String { chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string() }
/// pending | accepted | rejected | resolved: from the history, the registry and the resolved/ folder (the JSON status is display only).
pub fn effective_status(s: &Store, p: &Proposal) -> String {
    if let Some(v) = s.repo().verdict(&p.id) { return v.into(); }
    if let Some(e) = registry(s).iter().rev().find(|e| e["id"] == p.id.as_str()) { return e["status"].as_str().unwrap_or("resolved").into(); }
    if s.root.join(DIRNAME).join(RESOLVED).join(format!("{}.json", p.id)).exists() { return if ["accepted", "rejected"].contains(&p.status.as_str()) { p.status.clone() } else { "resolved".into() }; }
    "pending".into()
}
fn load(s: &Store, pid: &str) -> Result<Proposal> {
    check_id(pid)?; let d = s.root.join(DIRNAME); let p = [d.join(format!("{pid}.json")), d.join(RESOLVED).join(format!("{pid}.json"))].into_iter().find(|p| p.exists()).ok_or_else(|| anyhow!("no proposal {pid:?}"))?;
    let pr: Proposal = serde_json::from_str(&std::fs::read_to_string(&p)?)?; if pr.id != pid { bail!("proposal file {} carries another id: refusing it", p.display()); } Ok(pr)
}
fn save(s: &Store, p: &Proposal, resolved: bool) -> Result<()> {
    let d = if resolved { dir(s)?.join(RESOLVED) } else { dir(s)? }; atomic_write(&d.join(format!("{}.json", p.id)), &(serde_json::to_string_pretty(p)? + "\n"))?;
    if resolved { let _ = std::fs::remove_file(dir(s)?.join(format!("{}.json", p.id))); } Ok(())
}
/// JSON has no dates: ISO date strings stay strings (the store writes them unquoted, as dates).
fn check_ops(ops: &[Value]) -> Result<Vec<Value>> {
    if ops.is_empty() { bail!("a proposal needs at least one operation"); }
    for o in ops { if !["set", "unset", "append", "remove", "create", "append_text", "replace_text"].contains(&o["op"].as_str().unwrap_or("")) { bail!("bad operation {o}: op must be one of append, append_text, create, remove, replace_text, set, unset"); } }
    Ok(ops.to_vec())
}
/// Validate a change against the current memory (dry run) and queue it. Fails if it would be invalid or change nothing.
pub fn create(s: &Store, file: &str, ops: &[Value], reason: &str, source: &str, evidence: Vec<Value>) -> Result<Proposal> {
    if reason.trim().is_empty() { bail!("a proposal needs a reason (what the user said / what the document says)"); }
    let ops = check_ops(ops)?; let res = s.edit(file, &ops, &EditOpts { action: "propose", reason: Some(reason), source: Some(source), dry_run: true, ..Default::default() })?;
    if !res.changed() { bail!("this proposal changes nothing"); }
    let now = chrono::Utc::now(); let id = format!("p-{}-{:06x}", now.format("%Y%m%d"), rand::random::<u32>() & 0xff_ffff);
    let mut p = Proposal { id, status: "pending".into(), created: now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(), file: file.into(), ops, reason: reason.trim().into(), source: source.into(), diff: String::new(), changes: vec![], evidence,
        base_sha256: s.exists(file).then(|| crate::util::sha256_hex(s.read_text(file).as_bytes())), resolved: None, change_id: None, note: None, seal: None, seal_kind: None };
    let (d, k) = seal(&p); p.seal = Some(d); p.seal_kind = Some(k.into()); save(s, &p, false)?; Ok(p)
}
pub fn get(s: &Store, pid: &str) -> Result<Proposal> { let mut p = load(s, pid)?; p.status = effective_status(s, &p); Ok(p) }
/// Proposals oldest first, by EFFECTIVE status; an unreadable file is skipped.
pub fn listing(s: &Store, status: Option<&str>) -> Vec<Proposal> {
    let d = s.root.join(DIRNAME); let mut files: Vec<PathBuf> = std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).collect();
    if status.is_none_or(|x| x != "pending") { files.extend(std::fs::read_dir(d.join(RESOLVED)).into_iter().flatten().flatten().map(|e| e.path())); }
    files.sort(); files.into_iter().filter_map(|f| f.file_stem().and_then(|x| x.to_str()).map(String::from)).filter(|n| check_id(n).is_ok()).filter_map(|n| get(s, &n).ok()).filter(|p| status.is_none_or(|x| x == "all" || x == p.status)).collect()
}
/// What the proposal means RIGHT NOW: recomputed from its operations (the stored diff / changes are never shown).
pub fn public(s: &Store, p: &Proposal) -> Value {
    let doc = s.load(&p.file).ok().map(|d| crate::memstore::jsonify(&d)); let lk = Store::list_key(&p.file).unwrap_or("");
    let old_of = |path: &str| -> Value { let Some(doc) = &doc else { return Value::Null }; let path = if !path.contains('[') && !path.starts_with(lk) { path.split_once('.').map_or(path.to_string(), |(a, b)| format!("{lk}[{a}].{b}")) } else { path.to_string() };
        let mut cur = doc.clone(); for seg in path.split('.') { let name = seg.split('[').next().unwrap_or(""); if !name.is_empty() { cur = cur[name].clone(); }
            for sel in crate::rs!(r"\[([^\]]+)\]").captures_iter(seg) { let k = &sel[1]; cur = match k.parse::<usize>() { Ok(i) => cur[i].clone(), Err(_) => cur.as_array().and_then(|a| a.iter().find(|x| x["id"] == k).cloned()).unwrap_or(Value::Null) }; } } cur };
    let flags: Map<String, Value> = p.evidence.iter().filter_map(|e| Some((e["path"].as_str()?.to_string(), e.clone()))).collect();
    let changes: Vec<Value> = p.ops.iter().map(|o| { let path = o["path"].as_str(); let f = path.and_then(|x| flags.get(x)).cloned().unwrap_or(json!({}));
        json!({"op": o["op"], "path": path, "old": if matches!(o["op"].as_str(), Some("set" | "unset" | "remove")) { path.map_or(Value::Null, old_of) } else { Value::Null }, "new": o.get("value").or(o.get("old")).cloned().unwrap_or(Value::Null),
            "snippet": f.get("snippet").cloned().unwrap_or(Value::Null), "suspicious": f["suspicious"].as_bool().unwrap_or(false)}) }).collect();
    let (diff, applicable, error) = match s.edit(&p.file, &p.ops, &EditOpts { action: "preview", dry_run: true, ..Default::default() }) { Ok(r) => (r.diff, true, Value::Null), Err(e) => (String::new(), false, json!(e.to_string())) };
    json!({"id": p.id, "status": effective_status(s, p), "created": p.created, "file": p.file, "ops": p.ops, "reason": p.reason, "source": p.source, "evidence": p.evidence, "base_sha256": p.base_sha256, "change_id": p.change_id, "note": p.note, "resolved": p.resolved, "seal_kind": p.seal_kind,
        "sealed": is_sealed(p), "changes": changes, "diff": diff, "applicable": applicable, "error": error, "accept_command": format!("finch memory accept {}", p.id), "suspicious_paths": suspicious(p)})
}
pub fn suspicious(p: &Proposal) -> Vec<String> { let mut v: Vec<String> = p.evidence.iter().filter(|e| e["suspicious"] == true).filter_map(|e| e["path"].as_str().map(String::from)).collect(); v.sort(); v.dedup(); v }
/// A fresh dry run of a pending proposal: (proposal, result, sealed, stale).
pub fn preview(s: &Store, pid: &str) -> Result<(Proposal, EditResult, bool, bool)> {
    let p = get(s, pid)?; if p.status != "pending" { bail!("proposal {pid} is already {}", p.status); }
    let stale = p.base_sha256.as_ref().is_some_and(|b| s.exists(&p.file) && *b != crate::util::sha256_hex(s.read_text(&p.file).as_bytes()));
    let res = s.edit(&p.file, &p.ops, &EditOpts { action: "preview", dry_run: true, ..Default::default() }).map_err(|e| anyhow!("cannot apply {pid} to {} as it is now: {e}", p.file))?;
    let sealed = is_sealed(&p); Ok((p, res, sealed, stale))
}
/// Apply a pending proposal: a HUMAN action (the CLI asks at a terminal). Refused when resolved, unsealed, a suspicious field is not
/// confirmed, or the file changed since (unless `force`).
pub fn accept(s: &Store, pid: &str, confirmed: bool, force: bool, by: &str, confirm_fields: &[String]) -> Result<(Proposal, EditResult)> {
    if !confirmed { bail!("accepting a proposal needs explicit confirmation of its diff"); }
    let (mut p, _, sealed, stale) = preview(s, pid)?;
    if !sealed { bail!("proposal {pid} was modified after it was created (or has no seal): refusing to apply it; reject it and ask for a new one"); }
    let missing: Vec<String> = suspicious(&p).into_iter().filter(|x| !confirm_fields.contains(x)).collect();
    if !missing.is_empty() { bail!("proposal {pid} comes from a document that contains instruction-like text (possible prompt injection): each of these fields needs an explicit --confirm-field: {}", missing.join(", ")); }
    if stale && !force { bail!("{} changed since proposal {pid} was made, so it may overwrite a newer value: re-propose, or accept with --force after reading the fresh diff", p.file); }
    let reason = format!("{} (proposed by {}, accepted by {by})", p.reason, p.source); let action = format!("accept {pid}");
    let extra = if p.file == "open-questions.yaml" { crate::questions::companion_after(s, &p.ops)? } else { vec![] };
    let res = s.edit_with(&p.file, &p.ops, &EditOpts { action: &action, reason: Some(&reason), source: Some(by), ..Default::default() }, &extra.iter().map(|(f, t)| (*f, t.clone())).collect::<Vec<_>>())?;
    registry_add(s, pid, "accepted", res.change_id.as_deref())?;
    p.status = "accepted".into(); p.resolved = Some(now_utc()); p.change_id = res.change_id.clone(); save(s, &p, true)?; Ok((p, res))
}
pub fn reject(s: &Store, pid: &str, note: Option<&str>) -> Result<Proposal> {
    let mut p = get(s, pid)?; if p.status != "pending" { bail!("proposal {pid} is already {}", p.status); }
    if s.history { s.repo().note(&format!("coach: reject {pid}"), &s.source, Some(note.unwrap_or("proposal rejected")))?; }
    registry_add(s, pid, "rejected", None)?; p.status = "rejected".into(); p.note = note.map(String::from); p.resolved = Some(now_utc()); save(s, &p, true)?; Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn python_json() {
        assert_eq!(py_dumps(&json!({"b": [1, 2.0, "é\n"], "a": null, "c": {"z": true, "y": 0.5}})), r#"{"a": null, "b": [1, 2.0, "é\n"], "c": {"y": 0.5, "z": true}}"#);
    }
}
