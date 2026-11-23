//! Non-secret settings from `config.toml` (same file and keys as the Python app). Precedence: env var > config.toml > default.
use anyhow::{bail, Result};
use std::{collections::BTreeMap, env, path::{Path, PathBuf}};
use toml::{Table, Value};

pub const CONFIG_NAME: &str = "config.toml";
pub const DEFAULT_API_URL: &str = "https://api.enablebanking.com";
pub const LLM_BACKENDS: [&str; 4] = ["claude-code", "anthropic-api", "ollama", "openai-compatible"];

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub root: PathBuf, pub config_path: Option<PathBuf>, pub data_dir: PathBuf, pub memory_dir: PathBuf, pub db_path: PathBuf,
    pub insecure_plaintext_db: bool, pub db_auto_migrate: bool,
    pub eb_app_id: Option<String>, pub eb_redirect_url: Option<String>, pub eb_private_key_path: Option<String>, pub eb_api_url: String,
    pub llm_backend: String, pub llm_model: String, pub llm_anthropic_model: String, pub llm_anthropic_base_url: Option<String>, pub llm_ollama_url: String, pub llm_ollama_model: String,
    pub llm_ollama_allow_remote: bool, pub llm_openai_base_url: String, pub llm_openai_model: String, pub llm_openrouter_deny_data_collection: bool, pub llm_allowlist: Vec<String>,
    pub knn_enabled: bool, pub knn_threshold: f64, pub knn_examples: i64, pub sync_daily_limit: i64, pub privacy_egress_journal: bool, pub coach_claude_env: Vec<String>, pub coach_claude_restricted: bool,
    pub coach_backend: String, pub import_profiles_dir: PathBuf, pub backup_dir: PathBuf, pub backup_retention: i64,
    pub transfer_window_days: i64, pub transfer_cross_bank_window_days: i64, pub transfer_min_confidence: f64, pub transfer_auto_link: bool, pub transfer_topup_merchants: Vec<String>, pub privacy_model_detail: String, pub privacy_local_only: bool, pub privacy_offline: bool,
    pub privacy_web_enrich: bool, pub ui_host: String, pub ui_port: i64, pub ui_session_hours: i64, pub ui_key_rotation_days: i64, pub ui_open_browser: bool, pub health_stale_days: i64, pub analytics: Table, pub alerts: Table,
    /// (key, value, source) for `finch config show`; never a secret.
    pub rows: Vec<(String, String, String)>,
}

impl Config {
    pub fn log_dir(&self) -> PathBuf { self.data_dir.join("logs") }
    /// Where the user's editable taxonomy.yaml / rules.yaml live: $COACH_CONFIG_DIR or <root>/config.
    pub fn user_config_dir(&self) -> PathBuf { env::var_os("COACH_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| self.root.join("config")) }
}

fn home() -> PathBuf { env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| ".".into()) }
fn expand(p: &str) -> PathBuf { p.strip_prefix("~/").map(|r| home().join(r)).unwrap_or_else(|| PathBuf::from(p)) }
pub fn expand_path(p: &str) -> PathBuf { expand(p) }
fn under(root: &Path, p: &str) -> PathBuf { let p = expand(p); if p.is_absolute() { p } else { root.join(p) } }

/// $COACH_HOME, else the nearest parent holding config.toml / config.example.toml, else ~/.ai-finance-coach.
pub fn find_root() -> PathBuf {
    if let Some(h) = env::var_os("COACH_HOME") { return expand(&h.to_string_lossy()); }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    cwd.ancestors().find(|d| d.join(CONFIG_NAME).exists() || d.join("config.example.toml").exists()).map(Path::to_path_buf).unwrap_or_else(|| home().join(".ai-finance-coach"))
}

struct Picker<'a> { raw: &'a Table, rows: Vec<(String, String, String)> }
impl Picker<'_> {
    fn get(&mut self, key: &str, envn: Option<&str>) -> Option<(Value, String)> {
        if let Some(v) = envn.and_then(|n| env::var(n).ok()).filter(|v| !v.is_empty()) { return Some((Value::String(v), format!("env {}", envn.unwrap()))); }
        let (sec, k) = key.rsplit_once('.').map_or((None, key), |(s, k)| (Some(s), k));
        let block = match sec { Some(s) => self.raw.get(s).and_then(Value::as_table)?, None => self.raw };
        block.get(k).filter(|v| v.as_str() != Some("")).map(|v| (v.clone(), CONFIG_NAME.into()))
    }
    fn s(&mut self, key: &str, def: &str, envn: Option<&str>) -> Result<String> {
        let (v, src) = match self.get(key, envn) { Some((Value::String(s), src)) => (s, src), Some((v, _)) => bail!("{key} must be a string, got {v}"), None => (def.into(), "default".into()) };
        self.rows.push((key.into(), v.clone(), src)); Ok(v)
    }
    fn os(&mut self, key: &str, envn: Option<&str>) -> Result<Option<String>> { let v = self.s(key, "", envn)?; Ok((!v.is_empty()).then_some(v)) }
    fn b(&mut self, key: &str, def: bool) -> Result<bool> {
        let (v, src) = match self.get(key, None) { Some((Value::Boolean(b), src)) => (b, src), Some((v, _)) => bail!("{key} must be true or false (unquoted), got {v}"), None => (def, "default".into()) };
        self.rows.push((key.into(), v.to_string(), src)); Ok(v)
    }
    fn f(&mut self, key: &str, def: f64) -> Result<f64> {
        let (v, src) = match self.get(key, None) { Some((Value::Float(f), src)) => (f, src), Some((Value::Integer(i), src)) => (i as f64, src), Some((v, _)) => bail!("{key} must be a number, got {v}"), None => (def, "default".into()) };
        self.rows.push((key.into(), v.to_string(), src)); Ok(v)
    }
    fn l(&mut self, key: &str, def: &[&str]) -> Result<Vec<String>> {
        let (v, src): (Vec<String>, String) = match self.get(key, None) { Some((Value::Array(a), src)) if a.iter().all(Value::is_str) => (a.iter().map(|x| x.as_str().unwrap().to_string()).collect(), src), Some((v, _)) => bail!("{key} must be a list of strings, got {v}"), None => (def.iter().map(|x| x.to_string()).collect(), "default".into()) };
        self.rows.push((key.into(), v.join(", "), src)); Ok(v)
    }
    fn i(&mut self, key: &str, def: i64) -> Result<i64> {
        let (v, src) = match self.get(key, None) { Some((Value::Integer(i), src)) => (i, src), Some((v, _)) => bail!("{key} must be an integer, got {v}"), None => (def, "default".into()) };
        self.rows.push((key.into(), v.to_string(), src)); Ok(v)
    }
}

pub fn load_config(path: Option<&Path>) -> Result<Config> {
    let explicit = path.map(Path::to_path_buf).or_else(|| env::var_os("COACH_CONFIG").map(PathBuf::from));
    let (root, cfg_path) = match explicit {
        Some(p) => { let p = std::fs::canonicalize(expand(&p.to_string_lossy())).map_err(|_| anyhow::anyhow!("Config file not found: {}", p.display()))?; (p.parent().unwrap().to_path_buf(), Some(p)) }
        None => { let r = find_root(); let p = r.join(CONFIG_NAME); (r, p.exists().then_some(p)) }
    };
    let raw: Table = match &cfg_path { Some(p) => std::fs::read_to_string(p)?.parse().map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?, None => Table::new() };
    let mut p = Picker { raw: &raw, rows: vec![] };
    let data_dir = under(&root, &p.s("data_dir", "data", Some("COACH_DATA_DIR"))?);
    let memory_dir = under(&root, &p.s("memory_dir", "memory", Some("COACH_MEMORY_DIR"))?);
    let db_path = p.os("db_path", Some("COACH_DB"))?.map_or_else(|| data_dir.join("finance.db"), |s| under(&root, &s));
    let mut c = Config {
        insecure_plaintext_db: p.b("insecure_plaintext_db", false)?, db_auto_migrate: p.b("db.auto_migrate", true)?,
        eb_app_id: p.os("enable_banking.app_id", Some("EB_APP_ID"))?, eb_redirect_url: p.os("enable_banking.redirect_url", Some("EB_REDIRECT_URL"))?,
        eb_private_key_path: p.os("enable_banking.private_key_path", Some("EB_PRIVATE_KEY_PATH"))?, eb_api_url: p.s("enable_banking.api_url", DEFAULT_API_URL, Some("EB_API_URL"))?,
        llm_backend: p.s("llm.backend", "claude-code", None)?, llm_model: p.s("llm.model", "sonnet", None)?, llm_anthropic_model: p.s("llm.anthropic_model", "claude-haiku-4-5", None)?,
        llm_anthropic_base_url: p.os("llm.anthropic_base_url", None)?, llm_ollama_url: p.s("llm.ollama_url", "http://localhost:11434", None)?, llm_ollama_model: p.s("llm.ollama_model", "llama3.1", None)?,
        llm_ollama_allow_remote: p.b("llm.ollama_allow_remote", false)?, llm_openai_base_url: p.s("llm.openai_base_url", "https://openrouter.ai/api/v1", None)?, llm_openai_model: p.s("llm.openai_model", "", None)?,
        llm_openrouter_deny_data_collection: p.b("llm.openrouter_deny_data_collection", true)?, llm_allowlist: p.l("classify.llm_allowlist", &[])?,
        knn_enabled: p.b("classify.knn_enabled", true)?, knn_threshold: p.f("classify.knn_threshold", 0.92)?, knn_examples: p.i("classify.knn_examples", 5)?, sync_daily_limit: p.i("sync.daily_limit", 4)?,
        privacy_egress_journal: p.b("privacy.egress_journal", true)?, coach_claude_env: p.l("coach.claude_env", &[])?, coach_claude_restricted: p.b("coach.claude_restricted", false)?, coach_backend: p.s("coach.backend", "claude-code", None)?,
        import_profiles_dir: under(&root, &p.s("import.profiles_dir", "config/import_profiles", None)?),
        backup_dir: under(&root, &p.s("backup.dir", "backups", None)?), backup_retention: p.i("backup.retention", 14)?,
        transfer_window_days: p.i("transfers.window_days", 3)?, transfer_cross_bank_window_days: p.i("transfers.cross_bank_window_days", 5)?, transfer_min_confidence: p.f("transfers.min_confidence", 0.85)?,
        transfer_auto_link: p.b("transfers.auto_link", false)?, transfer_topup_merchants: p.l("transfers.topup_merchants", &["REVOLUT", "PAYPAL", "WISE"])?.into_iter().map(|x| x.trim().to_uppercase()).collect(), privacy_model_detail: p.s("privacy.model_detail", "coarse", None)?,
        privacy_local_only: p.b("privacy.local_only", false)?, privacy_offline: p.b("privacy.offline", false)?, privacy_web_enrich: p.b("privacy.web_enrich", false)?,
        ui_host: p.s("ui.host", "127.0.0.1", None)?, ui_port: p.i("ui.port", 8765)?, ui_session_hours: p.i("ui.session_hours", 12)?, ui_key_rotation_days: p.i("ui.key_rotation_days", 30)?, ui_open_browser: p.b("ui.open_browser", true)?, health_stale_days: p.i("health.stale_days", 2)?,
        analytics: raw.get("analytics").and_then(Value::as_table).cloned().unwrap_or_default(), alerts: raw.get("alerts").and_then(Value::as_table).cloned().unwrap_or_default(),
        root: root.clone(), config_path: cfg_path.clone(), data_dir: data_dir.clone(), memory_dir: memory_dir.clone(), db_path: db_path.clone(), rows: vec![],
    };
    if !(0.0 < c.knn_threshold && c.knn_threshold <= 1.0) { bail!("classify.knn_threshold must be a number in (0, 1]"); }
    if !(1..=720).contains(&c.ui_session_hours) || c.ui_key_rotation_days < 1 { bail!("ui.session_hours must be 1..720 and ui.key_rotation_days >= 1"); }
    if c.sync_daily_limit < 1 { bail!("sync.daily_limit must be >= 1"); }
    for r in &c.llm_allowlist { if fancy_regex::Regex::new(r).is_err() { bail!("classify.llm_allowlist: bad regex {r:?}"); } }
    if c.coach_claude_env.iter().any(|n| !n.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')) { bail!("coach.claude_env must be a list of environment variable names"); }
    crate::classify::llm::check_ollama_url(&c.llm_ollama_url, c.llm_ollama_allow_remote)?;
    if c.llm_backend == "openai-compatible" || c.coach_backend == "openai-compatible" { crate::classify::llm::check_openai_url(&c.llm_openai_base_url)?; if c.llm_backend == "openai-compatible" && c.llm_openai_model.is_empty() { bail!("the openai-compatible backend needs a model: set [llm] openai_model"); } }
    if !LLM_BACKENDS.contains(&c.llm_backend.as_str()) { bail!("llm.backend {:?} is not supported; allowed: {}", c.llm_backend, LLM_BACKENDS.join(", ")); }
    if !LLM_BACKENDS.contains(&c.coach_backend.as_str()) { bail!("coach.backend {:?} is not supported; allowed: {}", c.coach_backend, LLM_BACKENDS.join(", ")); }
    if !["coarse", "standard"].contains(&c.privacy_model_detail.as_str()) { bail!("privacy.model_detail must be 'coarse' or 'standard', got {:?}", c.privacy_model_detail); }
    if !(1..=65535).contains(&c.ui_port) { bail!("ui.port must be between 1 and 65535, got {}", c.ui_port); }
    if !(0.0..=1.0).contains(&c.transfer_min_confidence) { bail!("transfers.min_confidence must be a number between 0 and 1"); }
    if c.transfer_window_days < 0 || c.transfer_cross_bank_window_days < 0 { bail!("transfers.window_days and transfers.cross_bank_window_days must be >= 0"); }
    if c.backup_retention < 1 { bail!("backup.retention must be >= 1"); }
    crate::analytics::Settings::from_table(&c.analytics)?;
    let mut rows = vec![("config file".into(), cfg_path.map_or("(none, defaults only)".into(), |p| p.display().to_string()), String::new()), ("root".into(), root.display().to_string(), String::new())];
    rows.extend(p.rows.into_iter().map(|(k, v, s)| { let v = match k.as_str() { "data_dir" => data_dir.display().to_string(), "memory_dir" => memory_dir.display().to_string(), "db_path" => db_path.display().to_string(), _ if v.is_empty() => "(unset)".into(), _ => v }; (k, v, s) }));
    c.rows = rows; Ok(c)
}

/// Every effective setting as "key = value  (source)" lines.
pub fn show(c: &Config) -> String { c.rows.iter().map(|(k, v, s)| format!("{k:<34} {v}{}", if s.is_empty() { String::new() } else { format!("  ({s})") })).collect::<Vec<_>>().join("\n") }

pub fn analytics_overrides(c: &Config) -> BTreeMap<String, String> { c.analytics.iter().map(|(k, v)| (k.clone(), v.to_string())).collect() }
