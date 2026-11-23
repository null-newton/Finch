//! LLM usage (E12): calls, tokens, cost per job / backend / model and per day, from `llm_usage`, with the outbound destinations of
//! the egress journal. claude-code costs are NOTIONAL (API prices of a subscription's tokens); a model without a price has no cost.
use crate::config::Config;
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn job_of(p: Option<&str>) -> String { match p.unwrap_or("") { "" => "other".into(), p if p.starts_with("classify") => "classification".into(), p if p.starts_with("coach") => "coach".into(), p if p.starts_with("eval") => "evaluation".into(), p => p.into() } }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }
/// The month's cost against `[usage] monthly_threshold_usd` (no threshold: no level).
pub fn month_status(con: &Connection, cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> Value {
    let month = now.format("%Y-%m").to_string(); let u = cfg.alerts.get("usage").and_then(|v| v.as_table()).cloned().unwrap_or_default();
    let thr = u.get("monthly_threshold_usd").and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64))); let notional = u.get("include_notional").and_then(|v| v.as_bool()).unwrap_or(true);
    let cost: f64 = con.query_row(&format!("SELECT COALESCE(SUM(cost_usd),0) FROM llm_usage WHERE substr(ts,1,7)=?{}", if notional { "" } else { " AND backend<>'claude-code'" }), [&month], |r| r.get(0)).unwrap_or(0.0);
    let unknown: i64 = con.query_row("SELECT COUNT(*) FROM llm_usage WHERE substr(ts,1,7)=? AND cost_usd IS NULL", [&month], |r| r.get(0)).unwrap_or(0);
    let ratio = thr.filter(|t| *t > 0.0).map(|t| r4(cost / t)); let level = ratio.and_then(|r| if r >= 2.0 { Some("high") } else if r >= 1.0 { Some("medium") } else { None });
    json!({"month": month, "cost_usd": r4(cost), "threshold_usd": thr, "ratio": ratio, "level": level, "unknown_cost_calls": unknown, "include_notional": notional})
}
pub fn summary(con: &Connection, cfg: &Config, days: i64, now: chrono::DateTime<chrono::Utc>) -> Result<Value> {
    let since = (now - chrono::Duration::days(days)).format("%Y-%m-%dT%H:%M:%S").to_string();
    let lines: Vec<Value> = con.prepare("SELECT purpose, backend, model, COUNT(*), COALESCE(SUM(items),0), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_write_tokens),0), SUM(cost_usd), SUM(cost_usd IS NULL), COALESCE(SUM(duration_s),0), MAX(ts)
        FROM llm_usage WHERE ts >= ? GROUP BY purpose, backend, model ORDER BY SUM(COALESCE(cost_usd,0)) DESC, 4 DESC")?.query_map([&since], |r| { let (n, dur): (i64, f64) = (r.get(3)?, r.get(11)?); let b: String = r.get(1)?;
            Ok(json!({"job": job_of(r.get::<_, Option<String>>(0)?.as_deref()), "purpose": r.get::<_, Option<String>>(0)?, "backend": b, "model": r.get::<_, Option<String>>(2)?, "calls": n, "items": r.get::<_, i64>(4)?, "tokens_in": r.get::<_, i64>(5)?, "tokens_out": r.get::<_, i64>(6)?,
                "cache_read_tokens": r.get::<_, i64>(7)?, "cache_write_tokens": r.get::<_, i64>(8)?, "cost_usd": r.get::<_, Option<f64>>(9)?.map(r4), "cost_unknown_calls": r.get::<_, i64>(10)?, "notional": b == "claude-code", "duration_s": (dur * 10.0).round() / 10.0, "avg_duration_s": if n > 0 { ((dur / n as f64) * 100.0).round() / 100.0 } else { 0.0 }, "last": r.get::<_, Option<String>>(12)?})) })?.collect::<rusqlite::Result<_>>()?;
    let mut by_job: std::collections::BTreeMap<String, Value> = Default::default();
    for l in &lines { let j = by_job.entry(l["job"].as_str().unwrap().to_string()).or_insert(json!({"job": l["job"], "calls": 0, "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "duration_s": 0.0}));
        for k in ["calls", "tokens_in", "tokens_out"] { j[k] = json!(j[k].as_i64().unwrap() + l[k].as_i64().unwrap_or(0)); } j["cost_usd"] = json!(r4(j["cost_usd"].as_f64().unwrap() + l["cost_usd"].as_f64().unwrap_or(0.0))); j["duration_s"] = json!(j["duration_s"].as_f64().unwrap() + l["duration_s"].as_f64().unwrap_or(0.0)); }
    let mut days_map: std::collections::BTreeMap<String, Value> = con.prepare("SELECT SUBSTR(ts,1,10), COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COALESCE(SUM(cost_usd),0) FROM llm_usage WHERE ts >= ? GROUP BY 1")?.query_map([&since], |r| Ok((r.get::<_, String>(0)?, json!({"date": r.get::<_, String>(0)?, "calls": r.get::<_, i64>(1)?, "tokens_in": r.get::<_, i64>(2)?, "tokens_out": r.get::<_, i64>(3)?, "cost_usd": r4(r.get::<_, f64>(4)?)}))))?.collect::<rusqlite::Result<_>>()?;
    let by_day: Vec<Value> = (0..days).rev().map(|i| { let d = (now - chrono::Duration::days(i)).date_naive().to_string(); days_map.remove(&d).unwrap_or(json!({"date": d, "calls": 0, "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0})) }).collect();
    let dest: Vec<Value> = con.prepare("SELECT kind, host, purpose, COUNT(*), COALESCE(SUM(bytes),0), SUM(outcome<>'allowed'), MAX(ts) FROM egress_journal WHERE ts >= ? GROUP BY kind, host, purpose ORDER BY 4 DESC").and_then(|mut st| st.query_map(params![since], |r| Ok(json!({"kind": r.get::<_, String>(0)?, "host": r.get::<_, String>(1)?, "purpose": r.get::<_, Option<String>>(2)?.unwrap_or_default(), "calls": r.get::<_, i64>(3)?, "bytes": r.get::<_, i64>(4)?, "denied": r.get::<_, i64>(5)?, "web": false, "last": r.get::<_, String>(6)?}))).map(|i| i.flatten().collect())).unwrap_or_default();
    let tot = |k: &str| lines.iter().map(|l| l[k].as_i64().unwrap_or(0)).sum::<i64>(); let cost: f64 = lines.iter().map(|l| l["cost_usd"].as_f64().unwrap_or(0.0)).sum(); let notional: f64 = lines.iter().filter(|l| l["notional"] == true).map(|l| l["cost_usd"].as_f64().unwrap_or(0.0)).sum();
    Ok(json!({"days": days, "since": since, "lines": lines, "by_job": by_job.into_values().collect::<Vec<_>>(), "by_day": by_day, "destinations": dest, "journal_available": cfg.privacy_egress_journal,
        "totals": {"calls": tot("calls"), "tokens_in": tot("tokens_in"), "tokens_out": tot("tokens_out"), "cache_read_tokens": tot("cache_read_tokens"), "cache_write_tokens": tot("cache_write_tokens"), "cost_usd": r4(cost), "notional_cost_usd": r4(notional), "estimated_cost_usd": r4(cost - notional), "unknown_cost_calls": tot("cost_unknown_calls"), "duration_s": lines.iter().map(|l| l["duration_s"].as_f64().unwrap_or(0.0)).sum::<f64>()},
        "month": month_status(con, cfg, now), "note": "claude-code costs are NOTIONAL (API prices for the tokens a subscription call used: nothing is billed for them); anthropic-api costs are estimates from the price table; ollama is local and free; a model without a known price has no cost (unknown calls are counted apart)."}))
}
