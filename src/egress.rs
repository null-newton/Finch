//! Egress policy: the registry of every way data can leave this machine, the ONE check each network call goes through
//! ([`allow`]), and the local journal (`egress_journal`: time, kind, destination HOST, bytes, purpose, redaction, outcome; never a
//! payload, path, query, name or token). `[privacy] offline` refuses everything outbound; `local_only` allows only Enable Banking
//! (the data source) and a loopback Ollama.
use crate::config::Config;
use anyhow::{bail, Result};
use rusqlite::{params, Connection};

/// (kind, external, local_only behaviour, offline behaviour, redaction mode, default host)
pub const FLOWS: [(&str, bool, &str, &str, &str, &str); 12] = [
    ("classify.suggest", true, "refused", "refused", "label-only", "the [llm] backend (one redacted merchant label from the review queue; opt-in [classify] review_suggest)"),
    ("update.github", true, "refused", "refused", "none", "github.com (the desktop app's signed update check: sends no data)"),
    ("alerts.email", true, "refused", "refused", "minimal-guarded", "the SMTP server of [alerts.email] host"),
    ("alerts.macos", false, "allowed", "allowed", "minimal-guarded", "this Mac (notification centre)"),
    ("alerts.ntfy", true, "refused", "refused", "minimal-guarded", "the ntfy server of [alerts.ntfy] url"),
    ("alerts.telegram", true, "refused", "refused", "minimal-guarded", "api.telegram.org"),
    ("enable_banking", true, "allowed", "refused", "none", "api.enablebanking.com"),
    ("llm.claude-code", true, "refused", "refused", "item-redact", "api.anthropic.com (via the claude CLI)"),
    ("llm.anthropic-api", true, "refused", "refused", "item-redact", "api.anthropic.com"),
    ("llm.ollama", false, "loopback", "loopback", "item-redact", "localhost"),
    ("llm.openai-compatible", true, "refused", "refused", "item-redact", "openrouter.ai"),
    ("mcp.finance", true, "refused", "refused", "analytics-pseudonym", "(MCP client)"),
];

pub fn host_of(u: &str) -> String { url::Url::parse(u).ok().and_then(|x| x.host_str().map(str::to_lowercase)).unwrap_or_default() }
pub fn is_loopback(host: &str) -> bool { let h = host.trim_matches(['[', ']']); h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) }
pub fn mode(cfg: &Config) -> &'static str { if cfg.privacy_offline { "offline" } else if cfg.privacy_local_only { "local_only" } else { "standard" } }

/// Why `kind` would be refused under the current [privacy] settings (None = allowed).
pub fn evaluate(cfg: &Config, kind: &str, host: &str) -> Option<String> {
    let Some(f) = FLOWS.iter().find(|f| f.0 == kind) else { return Some(format!("unknown egress kind {kind:?}: refused")) };
    let (local_only, offline) = (cfg.privacy_local_only || cfg.privacy_offline, cfg.privacy_offline);
    let rule = if offline { f.3 } else if local_only { f.2 } else { "allowed" };
    match rule { "allowed" => None, "loopback" if is_loopback(host) => None,
        "loopback" => Some(format!("[privacy] {} allows {kind} only on this machine (host {host} is not loopback)", mode(cfg))),
        _ => Some(format!("[privacy] {} = true refuses {kind}: switch it off in config.toml to allow it", mode(cfg))) }
}
/// THE check before any outbound call: refuses per the policy, and journals the attempt (allowed or denied).
pub fn allow(con: Option<&Connection>, cfg: &Config, kind: &str, host: &str, bytes: usize, purpose: &str) -> Result<()> {
    let why = evaluate(cfg, kind, host);
    let host = if host.is_empty() { FLOWS.iter().find(|f| f.0 == kind).map_or("", |f| f.5) } else { host };
    if let (Some(con), true) = (con, cfg.privacy_egress_journal) {
        let red = FLOWS.iter().find(|f| f.0 == kind).map_or("", |f| f.4);
        let _ = con.execute("INSERT INTO egress_journal(ts, kind, host, bytes, purpose, redaction, outcome, reason) VALUES (?,?,?,?,?,?,?,?)",
            params![crate::util::now_iso(), kind, host, bytes as i64, purpose, red, if why.is_some() { "denied" } else { "allowed" }, if why.is_some() { mode(cfg) } else { "" }]);
    }
    if let Some(w) = why { bail!("egress refused: {w}"); }
    Ok(())
}
/// The journal's last rows (newest first).
pub fn journal(con: &Connection, limit: usize) -> Result<Vec<(String, String, String, i64, String, String)>> {
    Ok(con.prepare("SELECT ts, kind, host, bytes, purpose, outcome FROM egress_journal ORDER BY id DESC LIMIT ?")?.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn policy() {
        let mut c = Config::default(); assert!(evaluate(&c, "llm.anthropic-api", "api.anthropic.com").is_none());
        c.privacy_local_only = true; assert!(evaluate(&c, "llm.anthropic-api", "").is_some()); assert!(evaluate(&c, "enable_banking", "").is_none()); assert!(evaluate(&c, "llm.ollama", "127.0.0.1").is_none()); assert!(evaluate(&c, "llm.ollama", "10.0.0.5").is_some());
        c.privacy_offline = true; assert!(evaluate(&c, "enable_banking", "").is_some()); assert!(evaluate(&c, "nope", "").is_some());
        assert_eq!(host_of("https://API.enablebanking.com/x?y"), "api.enablebanking.com");
    }
}
