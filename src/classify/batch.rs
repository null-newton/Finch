//! Anthropic Message Batches for the merchant labels (half the price, results within 24 hours): `submit` sends the redacted jobs of
//! a run as one batch and records which merchants each request asked about; `collect` stores the results of the batches that ended.
//! Resumable: the same jobs submitted again return the batch in flight instead of paying twice.
use super::{llm::{self, Job}, rules::Rules};
use crate::{config::Config, egress, net::Http, util::*};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn base(cfg: &Config) -> String { cfg.llm_anthropic_base_url.clone().unwrap_or_else(|| "https://api.anthropic.com".into()).trim_end_matches('/').to_string() }
fn headers() -> Result<Vec<(&'static str, String)>> { Ok(vec![("x-api-key", crate::secrets::get_secret("anthropic_api_key", true)?.unwrap_or_default()), ("anthropic-version", "2023-06-01".into()), ("content-type", "application/json".into())]) }
/// Submits the jobs (or returns the pending batch of the same jobs): (batch id, new?).
pub fn submit(con: &Connection, cfg: &Config, http: &dyn Http, rules: &Rules, jobs: &[Job], model: &str) -> Result<(String, bool)> {
    if jobs.is_empty() { bail!("nothing to label"); } let mid = llm::model_id(model); let schema = llm::label_schema(rules);
    let fp = sha256_hex(format!("{mid}|{}", jobs.iter().map(|j| format!("{}{}", j.stat, j.dynamic)).collect::<String>()).as_bytes())[..24].to_string();
    if let Some(id) = con.query_row("SELECT batch_id FROM llm_batches WHERE fingerprint=? AND status='pending'", [&fp], |r| r.get::<_, String>(0)).optional()? { return Ok((id, false)); }
    let reqs: Vec<Value> = jobs.iter().enumerate().map(|(i, j)| json!({"custom_id": format!("j{i}"), "params": llm::anthropic_body(&j.stat, &j.dynamic, &schema, &mid)})).collect(); let body = json!({"requests": reqs});
    egress::allow(Some(con), cfg, "llm.anthropic-api", &egress::host_of(&base(cfg)), body.to_string().len(), "classify.batch")?;
    let r = http.send("POST", &format!("{}/v1/messages/batches", base(cfg)), &headers()?.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>(), Some(&body))?; if r.status >= 400 { bail!("api.anthropic.com answered HTTP {} to the batch", r.status); }
    let id = r.json()?["id"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("no batch id in the answer"))?;
    con.execute("INSERT OR REPLACE INTO llm_batches(fingerprint, batch_id, backend, created_at, status, jobs) VALUES (?,?,?,?,'pending',?)", params![fp, id, "anthropic-api", now_iso(), jobs.len() as i64])?;
    for (i, j) in jobs.iter().enumerate() { con.execute("INSERT OR REPLACE INTO llm_batch_jobs(batch_id, custom_id, keys_json, model, purpose, items, status) VALUES (?,?,?,?,?,?,'pending')", params![id, format!("j{i}"), json!(j.keys).to_string(), mid, "label", j.keys.len() as i64])?; }
    Ok((id, true))
}
/// Checks every pending batch; an ended one has its labels stored (one llm_usage row per request, batch price). Returns one line per batch.
pub fn collect(con: &Connection, cfg: &Config, http: &dyn Http, rules: &Rules) -> Result<Vec<Value>> {
    let pend: Vec<String> = con.prepare("SELECT batch_id FROM llm_batches WHERE status='pending' ORDER BY created_at")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?; let h: Vec<(&str, String)> = headers()?.iter().map(|(k, v)| (*k, v.clone())).collect(); let mut out = vec![];
    for id in pend {
        egress::allow(Some(con), cfg, "llm.anthropic-api", &egress::host_of(&base(cfg)), 0, "classify.batch_status")?;
        let st = http.send("GET", &format!("{}/v1/messages/batches/{id}", base(cfg)), &h, None)?; if st.status >= 400 { out.push(json!({"batch": id, "status": "error", "http": st.status})); continue; } let b = st.json()?;
        if b["processing_status"] != "ended" { out.push(json!({"batch": id, "status": b["processing_status"], "counts": b["request_counts"]})); continue; }
        let url = b["results_url"].as_str().map(String::from).unwrap_or_else(|| format!("{}/v1/messages/batches/{id}/results", base(cfg))); if egress::host_of(&url) != egress::host_of(&base(cfg)) { bail!("the results address is not on the API host: refused"); }
        egress::allow(Some(con), cfg, "llm.anthropic-api", &egress::host_of(&url), 0, "classify.batch_fetch")?; let res = http.send("GET", &url, &h, None)?; if res.status >= 400 { out.push(json!({"batch": id, "status": "error", "http": res.status})); continue; }
        let (mut stored, mut failed, mut labelled) = (0, 0, 0);
        for line in res.body.lines().filter(|l| !l.trim().is_empty()) { let Ok(x) = serde_json::from_str::<Value>(line) else { continue }; let cid = x["custom_id"].as_str().unwrap_or("");
            let Some((keys, model)) = con.query_row("SELECT keys_json, model FROM llm_batch_jobs WHERE batch_id=? AND custom_id=? AND status='pending'", params![id, cid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))).optional()? else { continue };
            let keys: Vec<String> = serde_json::from_str(&keys).unwrap_or_default(); let model = model.unwrap_or_default();
            if x["result"]["type"] != "succeeded" { con.execute("UPDATE llm_batch_jobs SET status='failed', error=? WHERE batch_id=? AND custom_id=?", params![x["result"]["type"].as_str().unwrap_or("error"), id, cid])?; failed += 1; continue; }
            let m = &x["result"]["message"]; let u = &m["usage"]; let g = |k: &str| u[k].as_i64().unwrap_or(0);
            llm::record_usage(con, &llm::Usage { backend: "anthropic-api".into(), model: model.clone(), purpose: "label".into(), items: keys.len() as i64, tokens_in: g("input_tokens"), tokens_out: g("output_tokens"), cache_read: g("cache_read_input_tokens"), cache_write: g("cache_creation_input_tokens"),
                cost_usd: llm::estimate_cost(&model, g("input_tokens"), g("output_tokens"), g("cache_read_input_tokens"), g("cache_creation_input_tokens")).map(|c| c / 2.0), estimate: true, duration_s: 0.0 })?;
            let text = m["content"].as_array().into_iter().flatten().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str()).unwrap_or("");
            match serde_json::from_str::<Value>(text.trim()) { Ok(d) => { for (i, name, cat, conf, hint) in llm::validate_results(&d["results"], keys.len(), rules) { llm::upsert_label(con, &keys[i], &name, &cat, Some(conf), Some(hint), "llm", &model)?; labelled += 1; }
                    con.execute("UPDATE llm_batch_jobs SET status='stored' WHERE batch_id=? AND custom_id=?", params![id, cid])?; stored += 1; }
                Err(_) => { con.execute("UPDATE llm_batch_jobs SET status='failed', error='the answer is not JSON' WHERE batch_id=? AND custom_id=?", params![id, cid])?; failed += 1; } } }
        con.execute("UPDATE llm_batches SET status='done' WHERE batch_id=?", [&id])?; out.push(json!({"batch": id, "status": "ended", "stored": stored, "failed": failed, "labelled": labelled}));
    }
    Ok(out)
}
