//! Secret lookup: environment variable -> secret store (OS keychain, or a folder of files) -> helpful error. Never printed.
use anyhow::{bail, Result};
use std::{env, path::PathBuf};

pub const SERVICE: &str = "ai-finance-coach";
pub const SECRETS: [(&str, &str); 10] = [("db_key", "COACH_DB_KEY"), ("backup_key", "COACH_BACKUP_KEY"), ("anthropic_api_key", "ANTHROPIC_API_KEY"),
    ("openai_api_key", "COACH_OPENAI_API_KEY"), ("claude_code_oauth_token", "CLAUDE_CODE_OAUTH_TOKEN"), ("proposal_key", "COACH_PROPOSAL_KEY"),
    ("ntfy_token", "COACH_NTFY_TOKEN"), ("smtp_password", "COACH_SMTP_PASSWORD"), ("telegram_bot_token", "COACH_TELEGRAM_BOT_TOKEN"), ("sso_client_secret", "COACH_SSO_CLIENT_SECRET")];
const OPTIONAL: [&str; 8] = ["anthropic_api_key", "openai_api_key", "claude_code_oauth_token", "proposal_key", "ntfy_token", "smtp_password", "telegram_bot_token", "sso_client_secret"];

fn var_of(name: &str) -> Result<&'static str> { SECRETS.iter().find(|s| s.0 == name).map(|s| s.1).ok_or_else(|| anyhow::anyhow!("Unknown secret {name:?}")) }
pub fn backend() -> Result<String> { let b = env::var("COACH_SECRETS_BACKEND").unwrap_or_else(|_| "keychain".into()).trim().to_lowercase(); if b != "keychain" && b != "file" { bail!("COACH_SECRETS_BACKEND must be one of keychain, file, got {b:?}"); } Ok(b) }
pub fn secrets_dir() -> PathBuf { env::var_os("COACH_SECRETS_DIR").map(PathBuf::from).unwrap_or_else(|| "/run/secrets".into()) }

#[cfg(unix)]
fn check_mode(p: &std::path::Path, dir: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let m = std::fs::metadata(p)?; let mode = m.permissions().mode() & 0o7777;
    if m.uid() != unsafe_uid() && m.uid() != 0 { bail!("{} is owned by another user (not you, not root): refused.", p.display()); }
    if dir { if mode & 0o022 != 0 && mode & 0o1000 == 0 { bail!("The secrets folder {} is writable by others (mode {mode:04o}).", p.display()); } }
    else { let bad = if env::var("COACH_SECRETS_ALLOW_READABLE").as_deref() == Ok("1") { 0o022 } else { 0o077 }; if mode & bad != 0 { bail!("{} is accessible to other users (mode {mode:04o}): run `chmod 600 {}`", p.display(), p.display()); } }
    Ok(())
}
#[cfg(unix)]
fn unsafe_uid() -> u32 { unsafe { libc::getuid() } }
#[cfg(not(unix))]
fn check_mode(_: &std::path::Path, _: bool) -> Result<()> { Ok(()) }

fn read_file(name: &str) -> Result<Option<String>> {
    let d = secrets_dir(); if d.exists() { check_mode(&d, true)?; }
    let p = d.join(name); if !p.exists() { return Ok(None); }
    if !p.is_file() { bail!("{} is not a regular file.", p.display()); }
    check_mode(&p, false)?; let v = std::fs::read_to_string(&p)?.trim().to_string(); Ok((!v.is_empty()).then_some(v))
}
/// The keychain service: `COACH_KEYCHAIN_SERVICE` keeps one home's secrets apart (the desktop app uses its own).
pub fn service() -> String { env::var("COACH_KEYCHAIN_SERVICE").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| SERVICE.into()) }
/// Keeps a secret in the OS keychain (the keychain build only).
pub fn store(name: &str, value: &str) -> Result<()> { var_of(name)?; store_kc(name, value) }
#[cfg(feature = "keychain")]
fn store_kc(name: &str, value: &str) -> Result<()> { keyring::Entry::new(&service(), name)?.set_password(value).map_err(|e| anyhow::anyhow!("Cannot store secret '{name}' in the keychain ({e}).")) }
#[cfg(not(feature = "keychain"))]
fn store_kc(name: &str, _: &str) -> Result<()> { bail!("this build has no keychain support: provide '{name}' as its environment variable") }
#[cfg(feature = "keychain")]
fn keychain(name: &str) -> Result<Option<String>> { match keyring::Entry::new(&service(), name)?.get_password() { Ok(v) => Ok(Some(v)), Err(keyring::Error::NoEntry) => Ok(None), Err(e) => bail!("Cannot read secret '{name}' from the keychain ({e}).") } }
#[cfg(not(feature = "keychain"))]
fn keychain(_: &str) -> Result<Option<String>> { Ok(None) }

/// (value, origin) with origin env | file | keychain.
pub fn lookup(name: &str) -> Result<Option<(String, &'static str)>> {
    let var = var_of(name)?;
    if let Ok(v) = env::var(var) { if !v.is_empty() { return Ok(Some((v, "env"))); } }
    Ok(if backend()? == "file" { read_file(name)?.map(|v| (v, "file")) } else { keychain(name)?.map(|v| (v, "keychain")) })
}
pub fn get_secret(name: &str, required: bool) -> Result<Option<String>> {
    if let Some((v, _)) = lookup(name)? { return Ok(Some(v)); }
    if !required { return Ok(None); }
    let var = var_of(name)?;
    if backend()? == "file" { bail!("Secret '{name}' not found. Provide it as the environment variable {var} or as the file {} (mode 0600, the value only).", secrets_dir().join(name).display()); }
    bail!("Secret '{name}' not found. Provide it as the environment variable {var}{}.", if cfg!(feature = "keychain") { format!(" or in the OS keychain (service '{}')", service()) } else { " (this build has no keychain support: build with --features keychain, or use COACH_SECRETS_BACKEND=file)".into() })
}
/// (name, masked status) per known secret: never the value.
pub fn describe() -> Vec<(String, String)> {
    SECRETS.iter().map(|(n, _)| (n.to_string(), match lookup(n) { Ok(Some((_, o))) => format!("set ({o})  ********"), Ok(None) => format!("not set{}", if OPTIONAL.contains(n) { " (optional)" } else { "" }), Err(_) => "UNKNOWN: secret store unavailable".into() })).collect()
}
