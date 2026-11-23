//! Security of the local web app, as the Python app: a per-install secret (0600 file, rotated when old), one-time login tokens
//! (hash + expiry in a file shared with `finch ui --login-link`), an HttpOnly SameSite=Strict cookie per port signed with the
//! secret, CSRF = HMAC of the cookie, Host / Origin checks against DNS rebinding, token-bucket rate limits and strict headers.
use crate::util::sha256_hex;
use anyhow::Result;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::{json, Map, Value};
use std::{collections::{HashMap, HashSet}, fs, path::{Path, PathBuf}, sync::Mutex, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

pub const KEY_FILE: &str = "ui-session.key";
pub const LOGIN_FILE: &str = "ui-login.json";
pub const REVOKED_FILE: &str = "ui-revoked.json";
pub const LOGIN_TTL: f64 = 120.0;
pub const CSRF_HEADER: &str = "x-csrf-token";
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; manifest-src 'self'; worker-src 'self'";
pub const HEADERS: [(&str, &str); 7] = [("X-Frame-Options", "DENY"), ("Referrer-Policy", "no-referrer"), ("X-Content-Type-Options", "nosniff"), ("Cross-Origin-Resource-Policy", "same-origin"),
    ("Cross-Origin-Opener-Policy", "same-origin"), ("Permissions-Policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()"), ("Content-Security-Policy", CSP)];

pub fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()) }
pub fn is_loopback(h: &str) -> bool { matches!(h.trim().to_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]") }
pub fn token(n: usize) -> String { let mut b = vec![0u8; n]; rand::thread_rng().fill_bytes(&mut b); base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b) }

/// Write a file readable by the owner only (atomic replace).
pub fn write_private(p: &Path, data: &[u8]) -> Result<()> {
    let tmp = p.with_extension("tmp"); fs::write(&tmp, data)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?; }
    fs::rename(&tmp, p)?; Ok(())
}
fn read_json(p: &Path) -> Map<String, Value> { fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default() }

pub fn rotate_secret(dir: &Path) -> Result<Vec<u8>> { fs::create_dir_all(dir)?; let raw = hex::encode({ let mut b = [0u8; 32]; rand::thread_rng().fill_bytes(&mut b); b }).into_bytes(); write_private(&dir.join(KEY_FILE), &raw)?; let _ = fs::remove_file(dir.join(REVOKED_FILE)); Ok(raw) }
/// The per-install secret: created on first use, replaced when older than `rotate_days`.
pub fn load_secret(dir: &Path, rotate_days: i64) -> Result<Vec<u8>> {
    fs::create_dir_all(dir)?; let p = dir.join(KEY_FILE);
    if let Ok(raw) = fs::read(&p) { let raw: Vec<u8> = String::from_utf8_lossy(&raw).trim().as_bytes().to_vec(); let age = fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).map_or(0, |d| d.as_secs());
        if raw.len() >= 32 && age <= rotate_days as u64 * 86400 { return Ok(raw); } }
    rotate_secret(dir)
}

/// Cross-process exclusive section over a lock file (the server and `finch ui --login-link` share the token file).
fn locked<T>(p: &Path, f: impl FnOnce() -> T) -> T {
    let lock = p.with_extension("lock"); let t0 = Instant::now();
    while fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
        if t0.elapsed() > Duration::from_secs(3) { let _ = fs::remove_file(&lock); continue; } std::thread::sleep(Duration::from_millis(10));
    }
    let r = f(); let _ = fs::remove_file(&lock); r
}
fn exp_of(v: &Value) -> f64 { v.as_f64().or_else(|| v["exp"].as_f64()).unwrap_or(0.0) }

/// One-time login tokens, stored as SHA-256 hashes with their expiry (and, for a per-person login, its id).
pub struct LoginTokens { pub path: PathBuf }
impl LoginTokens {
    pub fn new(dir: &Path) -> Self { Self { path: dir.join(LOGIN_FILE) } }
    pub fn issue(&self, user: Option<&str>) -> Result<String> {
        if let Some(d) = self.path.parent() { fs::create_dir_all(d)?; }
        let (t, n) = (token(24), now());
        locked(&self.path, || { let mut d: Map<String, Value> = read_json(&self.path).into_iter().filter(|(_, e)| exp_of(e) > n).collect();
            d.insert(sha256_hex(t.as_bytes()), user.map_or(json!(n + LOGIN_TTL), |u| json!({"exp": n + LOGIN_TTL, "user": u}))); write_private(&self.path, Value::Object(d).to_string().as_bytes()) })?;
        Ok(t)
    }
    /// (valid, login id): true exactly once for a valid, unexpired token.
    pub fn consume(&self, t: &str) -> (bool, Option<String>) {
        if t.is_empty() || t.len() > 200 { return (false, None); }
        let n = now();
        locked(&self.path, || { let mut d = read_json(&self.path); let v = d.remove(&sha256_hex(t.as_bytes())); let ok = v.as_ref().is_some_and(|v| exp_of(v) > n);
            let user = v.filter(|_| ok).and_then(|v| v["user"].as_str().map(String::from)); d.retain(|_, e| exp_of(e) > n); let _ = write_private(&self.path, Value::Object(d).to_string().as_bytes()); (ok, user) })
    }
}

/// Token bucket per key: `cap` actions at once, refilled at `rate` per second.
pub struct Bucket { cap: f64, rate: f64, st: Mutex<HashMap<String, (f64, Instant)>> }
impl Bucket {
    pub fn new(cap: f64, rate: f64) -> Self { Self { cap, rate, st: Mutex::new(HashMap::new()) } }
    pub fn take(&self, key: &str) -> bool {
        let mut st = self.st.lock().unwrap(); let n = Instant::now(); let (tok, last) = st.get(key).copied().unwrap_or((self.cap, n));
        let tok = (tok + n.duration_since(last).as_secs_f64() * self.rate).min(self.cap);
        if st.len() > 1000 { st.clear(); }
        if tok < 1.0 { st.insert(key.into(), (tok, n)); false } else { st.insert(key.into(), (tok - 1.0, n)); true }
    }
}

pub struct Security { secret: Vec<u8>, pub cookie_name: String, pub max_age: i64, hosts: HashSet<String>, dir: PathBuf, revoked: Mutex<HashMap<String, f64>>, pub tokens: LoginTokens,
    pub write: Bucket, pub preview: Bucket, pub exchange: Bucket }
impl Security {
    pub fn new(secret: Vec<u8>, port: u16, dev_ports: &[u16], session_hours: i64, dir: &Path) -> Self {
        let hosts = std::iter::once(port).chain(dev_ports.iter().copied()).flat_map(|p| [format!("localhost:{p}"), format!("127.0.0.1:{p}"), format!("[::1]:{p}")]).collect();
        let revoked = read_json(&dir.join(REVOKED_FILE)).into_iter().filter_map(|(k, v)| v.as_f64().map(|v| (k, v))).collect();
        Self { secret, cookie_name: format!("coach_session_{port}"), max_age: session_hours * 3600, hosts, dir: dir.into(), revoked: Mutex::new(revoked), tokens: LoginTokens::new(dir),
            write: Bucket::new(30.0, 2.0), preview: Bucket::new(40.0, 8.0), exchange: Bucket::new(8.0, 8.0 / 300.0) }
    }
    fn mac(&self, label: &str, v: &str) -> String { let mut m = Hmac::<sha2::Sha256>::new_from_slice(&self.secret).expect("hmac key"); m.update(format!("{label}:{v}").as_bytes()); hex::encode(m.finalize().into_bytes()) }
    /// sid.issued_at.signature (the owner) or sid.issued_at.login.signature (a per-person login).
    pub fn new_cookie(&self, user: Option<&str>) -> String { let body = format!("{}.{}{}", token(18), now() as i64, user.map(|u| format!(".{u}")).unwrap_or_default()); format!("{body}.{}", self.mac("sid", &body)) }
    /// (session id, login id) of a valid, unexpired, unrevoked cookie.
    pub fn parse_session(&self, v: Option<&str>) -> Option<(String, Option<String>)> {
        let v = v?; let parts: Vec<&str> = v.split('.').collect(); if !(3..=4).contains(&parts.len()) { return None; }
        let (sig, body) = (parts[parts.len() - 1], parts[..parts.len() - 1].join("."));
        if !ct_eq(&self.mac("sid", &body), sig) { return None; }
        let iat: i64 = parts[1].parse().ok().filter(|_| parts[1].bytes().all(|b| b.is_ascii_digit()))?; let n = now() as i64;
        if n - iat > self.max_age || iat > n + 60 || self.revoked.lock().unwrap().contains_key(parts[0]) { return None; }
        Some((parts[0].into(), parts.get(3).and(parts.get(2)).map(|u| u.to_string())))
    }
    pub fn revoke(&self, cookie: Option<&str>) {
        let Some((sid, _)) = self.parse_session(cookie) else { return }; let n = now(); let mut r = self.revoked.lock().unwrap();
        r.retain(|_, e| *e > n); r.insert(sid, n + self.max_age as f64 + 60.0); let _ = write_private(&self.dir.join(REVOKED_FILE), json!(*r).to_string().as_bytes());
    }
    pub fn csrf(&self, cookie: &str) -> String { self.mac("csrf", cookie) }
    pub fn csrf_valid(&self, cookie: &str, t: Option<&str>) -> bool { t.is_some_and(|t| !t.is_empty() && ct_eq(&self.csrf(cookie), t)) }
    pub fn host_allowed(&self, h: Option<&str>) -> bool { h.is_some_and(|h| self.hosts.contains(&h.trim().to_lowercase())) }
    /// No Origin (same-origin GET, curl) is fine; a present one must be http(s)://<allowed host>.
    pub fn origin_allowed(&self, o: Option<&str>) -> bool { o.is_none_or(|o| { let l = o.to_lowercase(); ["http://", "https://"].iter().any(|s| l.strip_prefix(s).is_some_and(|h| self.hosts.contains(h))) }) }
    pub fn set_cookie(&self, v: &str) -> String { format!("{}={v}; Max-Age={}; Path=/; HttpOnly; SameSite=Strict", self.cookie_name, self.max_age) }
    pub fn clear_cookie(&self) -> String { format!("{}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict", self.cookie_name) }
}
fn ct_eq(a: &str, b: &str) -> bool { a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 }
