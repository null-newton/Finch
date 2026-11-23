//! `finch ui`: the local web app. Serves the built `web/` (static/) and its JSON API under /api/v1, with the Python app's
//! security model (see `security`): one-time login link, signed cookie, CSRF, Host/Origin checks, rate limits, strict headers,
//! and a CHILD login denied everything outside `CHILD_ALLOWED` (deny by default). Endpoints not ported yet answer 404 `not_ported`.
pub mod routes;
pub mod security;

use crate::{analytics::{recurring::{self, Recurring}, Dataset, Scope}, config::Config, db, net::Http};
use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use security::{Security, CSRF_HEADER};
use serde_json::{json, Value};
use std::{collections::HashMap, io::Read, path::{Path, PathBuf}, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex}, time::SystemTime};

pub const API: &str = "/api/v1";
pub const DEV_PORTS: [u16; 2] = [5173, 4173];
const PUBLIC: [(&str, &str); 2] = [("POST", "/api/v1/session/exchange"), ("GET", "/api/v1/session/methods")];
/// What a CHILD login may call; everything else is 403 whether or not it exists. Add a path here only on purpose, with a test.
pub const CHILD_ALLOWED: [(&str, &str); 7] = [("GET", "/api/v1/session"), ("POST", "/api/v1/session/logout"), ("GET", "/api/v1/meta/taxonomy"), ("GET", "/api/v1/me"),
    ("GET", "/api/v1/me/summary"), ("GET", "/api/v1/me/transactions"), ("GET", "/api/v1/me/preferences")];
const ASSET_EXT: [&str; 15] = ["js", "css", "png", "svg", "ico", "webmanifest", "json", "map", "txt", "woff", "woff2", "jpg", "jpeg", "webp", "xml"];

#[derive(Debug, Clone)]
pub struct User { pub id: String, pub role: String, pub member_id: Option<String>, pub created_at: Option<String>, pub disabled: bool, pub prefs: Value }
impl User {
    pub fn owner() -> Self { Self { id: "owner".into(), role: "adult".into(), member_id: None, created_at: None, disabled: false, prefs: json!({}) } }
    pub fn is_child(&self) -> bool { self.role == "child" }
    pub fn to_json(&self) -> Value { json!({"id": self.id, "role": self.role, "member_id": self.member_id, "created_at": self.created_at, "disabled": self.disabled, "prefs": self.prefs}) }
}
pub fn get_user(con: &Connection, id: &str) -> Option<User> {
    con.query_row("SELECT id, role, member_id, created_at, disabled_at, prefs FROM ui_users WHERE id=?", [id], |r| Ok(User { id: r.get(0)?, role: r.get(1)?, member_id: r.get(2)?, created_at: r.get(3)?,
        disabled: r.get::<_, Option<String>>(4)?.is_some(), prefs: r.get::<_, Option<String>>(5)?.and_then(|p| serde_json::from_str(&p).ok()).filter(Value::is_object).unwrap_or(json!({})) })).optional().ok().flatten()
}

pub struct Req { pub method: String, pub path: String, pub query: Vec<(String, String)>, pub headers: HashMap<String, String>, pub body: Vec<u8>, pub cookie: Option<String>, pub sid: Option<String>, pub user: Option<User> }
impl Req {
    pub fn new(method: &str, url: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        let (p, q) = url.split_once('?').unwrap_or((url, ""));
        Self { method: method.to_uppercase(), path: pdecode(p), query: url::form_urlencoded::parse(q.as_bytes()).into_owned().collect(), headers: headers.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect(), body, cookie: None, sid: None, user: None }
    }
    pub fn h(&self, k: &str) -> Option<&str> { self.headers.get(k).map(String::as_str) }
    pub fn q(&self, k: &str) -> Option<&str> { self.query.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty()) }
    pub fn qs(&self, k: &str) -> Vec<String> { self.query.iter().filter(|(a, v)| a == k && !v.is_empty()).map(|(_, v)| v.clone()).collect() }
    pub fn qb(&self, k: &str, def: bool) -> bool { self.q(k).map_or(def, |v| matches!(v, "true" | "1" | "yes")) }
    /// An integer query parameter within [lo, hi] (422 otherwise).
    pub fn qi(&self, k: &str, def: Option<i64>, lo: i64, hi: i64) -> Result<Option<i64>, ApiErr> {
        match self.q(k) { None => Ok(def), Some(v) => v.parse::<i64>().ok().filter(|n| (lo..=hi).contains(n)).map(Some).ok_or_else(|| ApiErr::new(422, "validation_error", &format!("the request is not valid: {k}: must be a whole number from {lo} to {hi}"))) }
    }
    pub fn json(&self) -> Result<Value, ApiErr> { if self.body.is_empty() { return Ok(json!({})); } serde_json::from_slice(&self.body).map_err(|_| ApiErr::new(422, "validation_error", "the request is not valid: the body is not JSON")) }
}
fn pdecode(s: &str) -> String {
    let b = s.as_bytes(); let mut out = Vec::with_capacity(b.len()); let mut i = 0;
    while i < b.len() { if b[i] == b'%' && i + 2 < b.len() { if let Some(x) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) { out.push(x); i += 3; continue; } } out.push(b[i]); i += 1; }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct Resp { pub status: u16, pub body: Vec<u8>, pub ctype: String, pub headers: Vec<(String, String)> }
impl Resp {
    pub fn json(v: Value) -> Self { Self::with(200, v.to_string().into_bytes(), "application/json") }
    pub fn with(status: u16, body: Vec<u8>, ctype: &str) -> Self { Self { status, body, ctype: ctype.into(), headers: vec![] } }
    pub fn header(mut self, k: &str, v: &str) -> Self { self.headers.push((k.into(), v.into())); self }
}
#[derive(Debug)]
pub struct ApiErr { pub status: u16, pub code: String, pub message: String, pub details: Value }
impl ApiErr {
    pub fn new(status: u16, code: &str, message: &str) -> Self { Self { status, code: code.into(), message: message.into(), details: Value::Null } }
    pub fn rejected(e: impl std::fmt::Display) -> Self { Self::new(400, "rejected", &e.to_string()) }
    pub fn not_found(what: &str) -> Self { Self::new(404, "not_found", what) }
    pub fn resp(&self) -> Resp { Resp::with(self.status, json!({"error": {"code": self.code, "message": self.message, "details": self.details}}).to_string().into_bytes(), "application/json") }
}
/// Unexpected errors never echo internals (a database error may quote SQL, a client error a URL).
impl From<anyhow::Error> for ApiErr { fn from(e: anyhow::Error) -> Self { eprintln!("finch ui: unhandled error: {}", e.root_cause().to_string().chars().take(120).collect::<String>()); Self::new(500, "internal_error", "unexpected error; see the terminal running `finch ui`") } }
impl From<rusqlite::Error> for ApiErr { fn from(e: rusqlite::Error) -> Self { anyhow::Error::from(e).into() } }
pub type R = Result<Resp, ApiErr>;

#[derive(Clone, Debug, Default)]
pub struct Job { pub kind: String, pub state: String, pub started_at: Option<String>, pub finished_at: Option<String>, pub message: Option<String>, pub results: Vec<Value>, pub log: Vec<String>, pub url: Option<String>, pub auth_state: Option<String> }
impl Job {
    pub fn idle(kind: &str) -> Self { Self { kind: kind.into(), state: "idle".into(), ..Default::default() } }
    pub fn to_json(&self) -> Value { json!({"kind": self.kind, "state": self.state, "started_at": self.started_at, "finished_at": self.finished_at, "message": self.message, "message_msg": null, "results": self.results, "log": &self.log[self.log.len().saturating_sub(30)..], "url": self.url}) }
}

/// The analytics dataset of the moment, with its recurring series (computed once per data change).
pub struct Snap { pub ds: Dataset, pub rec: Recurring }
pub type HttpFactory = Arc<dyn Fn() -> Box<dyn Http> + Send + Sync>;

pub struct App { pub cfg: Config, pub insecure: bool, pub sec: Security, pub static_dir: PathBuf, pub today: Option<NaiveDate>, pub http: HttpFactory,
    pub sync_job: Mutex<Job>, pub auth_job: Mutex<Job>, gen: AtomicU64, snap: Mutex<Option<(String, Arc<Snap>)>>, pub inline_jobs: bool, pub me: std::sync::OnceLock<std::sync::Weak<App>> }
impl App {
    pub fn new(cfg: Config, insecure: bool, port: u16, dev: bool, static_dir: PathBuf, http: HttpFactory) -> Result<Self> {
        let secret = security::load_secret(&cfg.data_dir, cfg.ui_key_rotation_days)?;
        let sec = Security::new(secret, port, if dev { &DEV_PORTS } else { &[] }, cfg.ui_session_hours, &cfg.data_dir);
        Ok(Self { cfg, insecure, sec, static_dir, today: None, http, sync_job: Mutex::new(Job::idle("sync")), auth_job: Mutex::new(Job::idle("connect")), gen: AtomicU64::new(0), snap: Mutex::new(None), inline_jobs: false, me: Default::default() })
    }
    pub fn con(&self) -> Result<Connection> { db::connect(&self.cfg, db::Opts { insecure: self.insecure, ..Default::default() }) }
    /// Every write calls this: the next read recomputes the dataset.
    pub fn touch(&self) { self.gen.fetch_add(1, Ordering::SeqCst); }
    fn data_key(&self) -> String {
        let mt = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        let wal = PathBuf::from(format!("{}-wal", self.cfg.db_path.display()));
        let mem: u128 = std::fs::read_dir(&self.cfg.memory_dir).map(|r| r.flatten().map(|e| mt(&e.path())).max().unwrap_or(0)).unwrap_or(0);
        format!("{}:{}:{}:{}:{}", self.gen.load(Ordering::SeqCst), mt(&self.cfg.db_path), mt(&wal), mem, self.today.map_or_else(|| chrono::Local::now().date_naive(), |d| d))
    }
    pub fn snap(&self) -> Result<Arc<Snap>> {
        let key = self.data_key();
        if let Some((k, s)) = self.snap.lock().unwrap().as_ref() { if *k == key { return Ok(s.clone()); } }
        let ds = Dataset::load(&self.con()?, &self.cfg, self.today)?; let rec = recurring::detect(&ds, &Scope::default(), true);
        let s = Arc::new(Snap { ds, rec }); *self.snap.lock().unwrap() = Some((key, s.clone())); Ok(s)
    }
    pub fn today(&self) -> NaiveDate { self.today.unwrap_or_else(|| chrono::Local::now().date_naive()) }
    pub fn eb_configured(&self) -> bool { self.cfg.eb_app_id.is_some() && self.cfg.eb_private_key_path.is_some() && self.cfg.eb_redirect_url.is_some() }

    /// The whole request pipeline: guard, route, headers, audit.
    pub fn handle(&self, mut r: Req) -> Resp {
        let is_api = r.path.starts_with("/api");
        let mut resp = match self.guard(&mut r, is_api) { Err(e) => e.resp(), Ok(()) => match if is_api { routes::route(self, &r) } else { self.static_file(&r) } { Ok(x) => x, Err(e) => e.resp() } };
        for (k, v) in security::HEADERS { resp.headers.push((k.into(), v.into())); }
        if is_api { resp.headers.push(("Cache-Control".into(), "no-store".into())); }
        if is_api && r.sid.is_some() && matches!(r.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
            if let (Some(u), Ok(con)) = (&r.user, self.con()) { let _ = con.execute("INSERT INTO audit_log(at, actor, method, path, status) VALUES (?,?,?,?,?)", rusqlite::params![crate::util::now_iso(), u.id, r.method, r.path.chars().take(200).collect::<String>(), resp.status]); }
        }
        resp
    }
    fn guard(&self, r: &mut Req, is_api: bool) -> Result<(), ApiErr> {
        if !self.sec.host_allowed(r.h("host")) { return Err(ApiErr::new(421, "bad_host", "this host name is not allowed (DNS rebinding protection)")); }
        let cookie = r.h("cookie").and_then(|c| c.split(';').filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == self.sec.cookie_name).map(|(_, v)| v.to_string()));
        if let Some((sid, login)) = self.sec.parse_session(cookie.as_deref()) {
            let user = match login { None => Some(User::owner()), Some(l) => self.con().ok().and_then(|c| get_user(&c, &l)).filter(|u| !u.disabled) };
            if user.is_some() { r.sid = Some(sid); r.user = user; r.cookie = cookie; }
        }
        if !is_api { return Ok(()); }
        if !matches!(r.h("sec-fetch-site"), None | Some("same-origin") | Some("none")) { return Err(ApiErr::new(403, "forbidden", "cross-site request refused")); }
        let ctype = r.h("content-type").unwrap_or("").to_lowercase();
        let has_body = !r.body.is_empty() || r.h("transfer-encoding").is_some();
        let bad_type = (!ctype.is_empty() && !ctype.starts_with("application/json")) || (has_body && ctype.is_empty());
        if PUBLIC.contains(&(r.method.as_str(), r.path.as_str())) {
            if !self.sec.origin_allowed(r.h("origin")) { return Err(ApiErr::new(403, "forbidden", "origin not allowed")); }
            if bad_type { return Err(ApiErr::new(415, "unsupported_media_type", "send application/json")); }
            return Ok(());
        }
        let Some(sid) = r.sid.clone() else { return Err(ApiErr::new(401, "unauthorized", "no valid session: open the one-time login link printed by `finch ui` (or run `finch ui --login-link`)")) };
        let p = r.path.trim_end_matches('/'); let p = if p.is_empty() { "/" } else { p };
        if r.user.as_ref().is_some_and(User::is_child) && !CHILD_ALLOWED.contains(&(r.method.as_str(), p)) { return Err(ApiErr::new(403, "forbidden_scope", "this login can only see its own data")); }
        if matches!(r.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
            if !self.sec.origin_allowed(r.h("origin")) { return Err(ApiErr::new(403, "forbidden", "origin not allowed")); }
            if !self.sec.csrf_valid(r.cookie.as_deref().unwrap_or(""), r.h(CSRF_HEADER)) { return Err(ApiErr::new(403, "csrf", "missing or wrong CSRF token (reload the page)")); }
            if bad_type { return Err(ApiErr::new(415, "unsupported_media_type", "send application/json")); }
            if !self.sec.write.take(&sid) { return Err(ApiErr::new(429, "rate_limited", "too many changes in a short time: wait a moment")); }
        }
        Ok(())
    }
    fn static_file(&self, r: &Req) -> R {
        if !matches!(r.method.as_str(), "GET" | "HEAD") { return Err(ApiErr::new(405, "method_not_allowed", "method not allowed")); }
        let rel = r.path.trim_start_matches('/');
        if !rel.is_empty() {
            if rel.split('/').any(|s| s == ".." || s.contains('\\') || s.contains(':')) { return Err(ApiErr::not_found("not found")); }
            let f = self.static_dir.join(rel);
            if f.is_file() && f.file_name().is_some_and(|n| n != "index.html") {
                let ext = rel.rsplit('.').next().unwrap_or("").to_lowercase();
                let mut resp = Resp::with(200, std::fs::read(&f).map_err(anyhow::Error::from)?, mime(&ext)).header("Cache-Control", if rel.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" });
                if rel == "sw.js" { resp = resp.header("Service-Worker-Allowed", "/"); }
                return Ok(resp);
            }
            if rel.rsplit('/').next().and_then(|n| n.rsplit_once('.')).is_some_and(|(_, e)| ASSET_EXT.contains(&e.to_lowercase().as_str())) { return Err(ApiErr::not_found("not found")); }
        }
        // the page shell carries no data and no cookie: the session starts only from the one-time login link
        match std::fs::read(self.static_dir.join("index.html")) {
            Ok(b) => Ok(Resp::with(200, b, "text/html; charset=utf-8").header("Cache-Control", "no-cache")),
            Err(_) => Ok(Resp::with(503, b"<h1>The web app is not built yet</h1><p>Run <code>pnpm install &amp;&amp; pnpm build</code> in <code>web/</code>.</p>".to_vec(), "text/html; charset=utf-8")),
        }
    }
}
fn mime(ext: &str) -> &'static str {
    match ext { "js" => "text/javascript", "css" => "text/css", "png" => "image/png", "svg" => "image/svg+xml", "ico" => "image/x-icon", "webmanifest" => "application/manifest+json", "json" | "map" => "application/json",
        "woff2" => "font/woff2", "woff" => "font/woff", "jpg" | "jpeg" => "image/jpeg", "webp" => "image/webp", "xml" => "application/xml", "txt" => "text/plain; charset=utf-8", "html" => "text/html; charset=utf-8", _ => "application/octet-stream" }
}

/// The one-time link: the token sits in the URL fragment, which a browser never sends to a server, a log or a Referer.
pub fn login_url(port: u16, dev: bool, token: &str) -> String { format!("{}/login#t={token}", if dev { "http://localhost:5173".to_string() } else { format!("http://127.0.0.1:{port}") }) }

/// Serve until the process is stopped, with a few worker threads.
pub fn serve(app: Arc<App>, host: &str, port: u16) -> Result<()> {
    let server = Arc::new(tiny_http::Server::http((host, port)).map_err(|e| anyhow::anyhow!("cannot listen on {host}:{port}: {e}"))?);
    let workers: Vec<_> = (0..6).map(|_| { let (s, a) = (server.clone(), app.clone()); std::thread::spawn(move || {
        while let Ok(mut rq) = s.recv() {
            let headers = rq.headers().iter().map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string())).collect();
            let mut body = vec![]; if rq.as_reader().take(4 << 20).read_to_end(&mut body).is_err() { continue; }
            let resp = a.handle(Req::new(&rq.method().to_string(), rq.url(), headers, body));
            let mut out = tiny_http::Response::from_data(resp.body).with_status_code(resp.status);
            for (k, v) in std::iter::once(("Content-Type".to_string(), resp.ctype)).chain(resp.headers) { if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) { out.add_header(h); } }
            let _ = rq.respond(out);
        }
    }) }).collect();
    for w in workers { let _ = w.join(); }
    Ok(())
}
