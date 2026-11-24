//! The review queue's AI category suggestion (B13), LABEL-ONLY: one merchant's REDACTED label (household names removed, towns cut,
//! amount an order of magnitude) and the taxonomy go to the `[llm]` backend through the `classify.suggest` egress flow (opt-in
//! `[classify] review_suggest`, refused under `[privacy] offline` / `local_only`). No web search (deliberately not ported), no example
//! labels, a person-like merchant is never sent. Nothing is stored but the usage: the user applies a suggestion with the queue's "Set".
use super::{llm::{self, Backend}, parsers::common::household, rules::Rules};
use crate::{config::Config, egress, util::now_iso};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const FLOW: &str = "classify.suggest";
/// A refusal the page shows: `code` is stable (i18n key), the message English.
#[derive(Debug, Clone)]
pub struct Refused { pub code: &'static str, pub msg: String }
fn no(code: &'static str, msg: impl Into<String>) -> Refused { Refused { code, msg: msg.into() } }

/// Web search from the review queue: never (not ported to finch).
pub fn web_status() -> (bool, &'static str, String) { (false, "not_ported", "web search is deliberately not ported to finch: a suggestion uses the redacted merchant label only".into()) }
/// (allowed, code, why) of a label-only suggestion with the configured `[llm]` backend, without calling anything.
pub fn label_status(cfg: &Config) -> (bool, &'static str, String) {
    if !cfg.review_suggest { return (false, "disabled", "opt-in: set [classify] review_suggest = true in config.toml to let the [llm] backend suggest a category (it receives the redacted merchant label only)".into()); }
    if let Some(w) = egress::evaluate(cfg, FLOW, "") { return (false, "egress_refused", w); }
    match backend_problem(cfg) { Some(w) => (false, "backend_unavailable", w), None => (true, "ok", String::new()) }
}
fn backend_problem(cfg: &Config) -> Option<String> {
    let sec = |n: &str| crate::secrets::get_secret(n, false).ok().flatten().is_none();
    match cfg.llm_backend.as_str() {
        "claude-code" => egress::evaluate(cfg, "llm.claude-code", ""),
        "anthropic-api" => egress::evaluate(cfg, "llm.anthropic-api", "").or_else(|| sec("anthropic_api_key").then(|| "no Anthropic API key: set the secret anthropic_api_key ([llm] backend anthropic-api)".into())),
        "ollama" => llm::check_ollama_url(&cfg.llm_ollama_url, cfg.llm_ollama_allow_remote).err().map(|e| e.to_string()).or_else(|| egress::evaluate(cfg, "llm.ollama", &egress::host_of(&cfg.llm_ollama_url))),
        "openai-compatible" => llm::check_openai_url(&cfg.llm_openai_base_url).err().map(|e| e.to_string()).or_else(|| egress::evaluate(cfg, "llm.openai-compatible", &egress::host_of(&cfg.llm_openai_base_url)))
            .or_else(|| sec("openai_api_key").then(|| "no API key: set the secret openai_api_key ([llm] backend openai-compatible)".into())).or_else(|| cfg.llm_openai_model.is_empty().then(|| "no model: set [llm] openai_model".into())),
        b => Some(format!("unknown [llm] backend {b:?}")),
    }
}

/// The exact redacted request for one merchant.
pub struct Plan { pub key: String, pub stat: String, pub dynamic: String }
/// The redacted request for `key`, or why it is not sent (opt-in, privacy, a rule decides it, a possible person).
pub fn plan(con: &Connection, cfg: &Config, rules: &Rules, key: &str) -> Result<Plan, Refused> {
    let (ok, code, why) = label_status(cfg); if !ok { return Err(no(code, why)); }
    if rules.rule_category(key, None).is_some() { return Err(no("decided_by_rule", "a classification rule decides this merchant")); }
    let e = |e: rusqlite::Error| no("error", e.to_string());
    let (n, avg, raw, ty, fx): (i64, Option<f64>, Option<String>, Option<String>, Option<String>) = con.query_row("SELECT COUNT(*), AVG(t.amount), MAX(e.merchant_raw), GROUP_CONCAT(DISTINCT e.tx_type), GROUP_CONCAT(DISTINCT e.fx_currency) FROM tx_enriched e JOIN transactions t USING(tx_key) WHERE e.merchant_key=?", [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).map_err(e)?;
    if n == 0 { return Err(no("unknown_merchant", "no transaction has this merchant key")); }
    let types: std::collections::HashSet<String> = ty.unwrap_or_default().split(',').filter(|x| !x.is_empty()).map(String::from).collect();
    let (h, places, known) = (household(con, Some(&cfg.memory_dir)), llm::known_places(con, &cfg.memory_dir, 3), llm::known_merchants(con).map_err(|x| no("error", x.to_string()))?);
    if llm::hold_back(key, raw.as_deref(), &types, &h.family, &h.first_names, &known, &cfg.llm_allowlist, false, &places) { return Err(no("person_like", "this merchant may be a person: it is never sent to an AI; choose its category yourself")); }
    let names: Vec<String> = h.family.iter().filter(|x| x.chars().count() >= 3).cloned().collect();
    let bare = llm::strip_places(key, &places); let label = llm::redact_names(if bare.is_empty() { key } else { &bare }, &names);
    let mut it = json!({"id": 0, "key": label, "n": n, "direction": if avg.unwrap_or(0.0) > 0.0 { "in" } else { "out" }, "avg_amount": llm::coarse_amount(avg.unwrap_or(0.0)), "types": types.iter().collect::<BTreeSet<_>>().into_iter().cloned().collect::<Vec<_>>().join(",")});
    if let Some(f) = fx { it["foreign_currency"] = f.into(); }
    Ok(Plan { key: key.into(), stat: llm::label_static(rules), dynamic: format!("\nItems (direction=out means money spent; avg_amount in EUR):\n{}", Value::Array(vec![it])) })
}
/// The call (through the flow's gate, then the backend's) and its usage, recorded. -> the suggestion, or None when the answer is unusable.
pub fn ask(con: &Connection, cfg: &Config, rules: &Rules, be: &Backend, p: &Plan) -> anyhow::Result<Option<Value>> {
    egress::allow(Some(con), cfg, FLOW, "", p.stat.len() + p.dynamic.len(), FLOW)?;
    let model = be.default_model(); let (data, u) = be.complete(&p.stat, &p.dynamic, &llm::label_schema(rules), &model, "suggest", 1)?; llm::record_usage(con, &u)?;
    Ok(llm::validate_results(&data["results"], 1, rules).into_iter().next().map(|(_, m, c, conf, _)| json!({"key": p.key, "mode": "label", "model": u.model, "merchant": m, "category": c, "confidence": (conf * 100.0).round() / 100.0, "evidence": null})))
}

/// One background suggestion, polled by the page.
#[derive(Debug, Clone)]
pub struct Job { pub id: String, pub key: String, pub state: String, pub started_at: String, pub finished_at: Option<String>, pub suggestion: Option<Value>, pub error: Option<Value> }
impl Job {
    pub fn new(key: &str) -> Self { Self { id: format!("s_{:010x}", rand::random::<u64>() & 0xff_ffff_ffff), key: key.into(), state: "running".into(), started_at: now_iso(), finished_at: None, suggestion: None, error: None } }
    pub fn fail(&mut self, code: &str, msg: &str) { self.state = "failed".into(); self.error = Some(json!({"code": code, "message": msg.chars().take(200).collect::<String>()})); self.finished_at = Some(now_iso()); }
    pub fn to_json(&self) -> Value {
        let mut v = json!({"id": self.id, "key": self.key, "state": self.state, "mode": "label", "started_at": self.started_at, "finished_at": self.finished_at, "suggestion": self.suggestion, "error": self.error});
        if self.suggestion.is_some() { v["ai_generated"] = true.into(); v["ai_label"] = crate::disclaimers::AI_LABEL.into(); v["ai_label_short"] = crate::disclaimers::AI_LABEL_SHORT.into(); } v
    }
}
pub const MAX_RUNNING: usize = 3;
pub const MAX_KEPT: usize = 40;
/// Register a job for `key`: the running one for the same key, or a new one (None = too many running).
pub fn register(jobs: &mut Vec<Job>, key: &str) -> Option<(Job, bool)> {
    if let Some(j) = jobs.iter().find(|j| j.state == "running" && j.key == key) { return Some((j.clone(), false)); }
    if jobs.iter().filter(|j| j.state == "running").count() >= MAX_RUNNING { return None; }
    let j = Job::new(key); jobs.push(j.clone());
    while jobs.len() > MAX_KEPT { match jobs.iter().position(|j| j.state != "running") { Some(i) => { jobs.remove(i); } None => break } }
    Some((j, true))
}
