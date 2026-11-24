//! Local HTTPS receiver for the Enable Banking redirect (opt-in `[callback] enabled`): binds 127.0.0.1 only, on the port of
//! `[enable_banking] redirect_url` (https://localhost:PORT/PATH), with a self-signed certificate generated once into
//! `<data_dir>/tls/` (key owner-only), answers only `GET <path>`, hands `state` + `code` to the caller (the same completion as
//! `finch bank finish`), and stops after one finished request or the timeout. Nothing it receives is logged; it makes no outbound call.
use anyhow::{bail, Context, Result};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{collections::HashMap, io::{ErrorKind, Read, Write}, net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream}, path::Path, sync::Arc, time::{Duration, Instant}};

pub const CERT: &str = "localhost.crt";
pub const KEY: &str = "localhost.key";
const CERT_DAYS: i64 = 825;
const RENEW_DAYS: i64 = 30;
const CONN_SECS: u64 = 10;
const MAX_HEAD: usize = 8192;

/// What one redirect did: `Wait` keeps the server listening (shown to the browser), `Done` ends it.
pub enum Outcome<T> { Wait(String), Done(Result<T, String>) }

/// (port, path) of the redirect address when it is an https loopback address the callback can serve.
pub fn parse_redirect(url: &str) -> Result<(u16, String)> {
    let u = url::Url::parse(url.trim()).context("[enable_banking] redirect_url is not a valid address")?;
    if u.scheme() != "https" { bail!("[enable_banking] redirect_url must start with https:// for the local callback"); }
    if !matches!(u.host_str(), Some("localhost" | "127.0.0.1")) { bail!("[enable_banking] redirect_url is not a loopback address (https://localhost:PORT/callback or https://127.0.0.1:PORT/callback)"); }
    Ok((u.port_or_known_default().unwrap_or(443), u.path().into()))
}

/// Generate a self-signed certificate for localhost / 127.0.0.1 into `dir` (the key file readable by the owner only).
pub fn generate_cert(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?; }
    let ymd = |d: chrono::NaiveDate| { use chrono::Datelike; rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8) };
    let today = chrono::Utc::now().date_naive();
    let mut p = rcgen::CertificateParams::new(vec!["localhost".into()])?;
    p.subject_alt_names.push(rcgen::SanType::IpAddress(Ipv4Addr::LOCALHOST.into()));
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, "localhost"); p.distinguished_name.push(rcgen::DnType::OrganizationName, "finch (local, self-signed)");
    (p.not_before, p.not_after) = (ymd(today - chrono::Duration::days(1)), ymd(today + chrono::Duration::days(CERT_DAYS)));
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let kp = rcgen::KeyPair::generate()?; let cert = p.self_signed(&kp)?;
    crate::web::security::write_private(&dir.join(KEY), kp.serialize_pem().as_bytes())?;
    std::fs::write(dir.join(CERT), cert.pem())?; Ok(())
}

/// The TLS server configuration from the certificate in `dir`, generated when missing, unreadable or close to its expiry.
pub fn tls_config(dir: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let (c, k) = (dir.join(CERT), dir.join(KEY));
    let fresh = c.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() < ((CERT_DAYS - RENEW_DAYS) * 86400) as u64);
    let load = || -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> { Ok((CertificateDer::from_pem_file(&c)?, PrivateKeyDer::from_pem_file(&k)?)) };
    let (cert, key) = match fresh.then(load).and_then(Result::ok) { Some(x) => x, None => { generate_cert(dir)?; load()? } };
    Ok(Arc::new(rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(vec![cert], key)?))
}

/// Listen on 127.0.0.1:port (0 = any free port), refusing a port another program already answers on.
pub fn bind(port: u16) -> Result<TcpListener> {
    let a = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if port != 0 && TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok() { bail!("127.0.0.1:{port} is already served by another program: stop it, or use --no-server and paste the address"); }
    let l = TcpListener::bind(a).with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?; l.set_nonblocking(true)?; Ok(l)
}

fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }
fn page(status: u16, title: &str, msg: &str) -> Vec<u8> {
    let body = format!("<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{t}</title><style>body{{font:16px system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem}}</style><h1>{t}</h1><p>{m}</p>", t = esc(title), m = esc(msg));
    let reason = match status { 200 => "OK", 404 => "Not Found", 405 => "Method Not Allowed", _ => "Bad Request" };
    format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

/// Wait for the redirect until `timeout`: Ok(Some) = finished, Ok(None) = timed out (nothing consumed), Err = the redirect failed.
pub fn serve<T>(l: &TcpListener, tls: Arc<rustls::ServerConfig>, path: &str, timeout: Duration, mut handle: impl FnMut(&HashMap<String, String>) -> Outcome<T>) -> Result<Option<T>> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        match l.accept() {
            Ok((s, _)) => { let dl = (Instant::now() + Duration::from_secs(CONN_SECS)).min(end); if let Some(r) = one(s, &tls, path, dl, &mut handle) { return r.map(Some).map_err(anyhow::Error::msg); } }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {}
        }
    }
    Ok(None)
}

// One connection under a TOTAL deadline (a client dribbling bytes cannot hold the server); TLS failures (the certificate warning) are routine.
fn one<T>(s: TcpStream, tls: &Arc<rustls::ServerConfig>, path: &str, dl: Instant, handle: &mut impl FnMut(&HashMap<String, String>) -> Outcome<T>) -> Option<Result<T, String>> {
    s.set_nonblocking(false).ok()?;
    let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(tls.clone()).ok()?, s);
    let left = |t: &rustls::StreamOwned<rustls::ServerConnection, TcpStream>| -> Option<()> { let d = dl.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?; t.sock.set_read_timeout(Some(d)).ok()?; t.sock.set_write_timeout(Some(d)).ok() };
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 1024]);
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") { left(&t)?; let n = t.read(&mut chunk).ok()?; if n == 0 || buf.len() > MAX_HEAD { return None; } buf.extend_from_slice(&chunk[..n]); }
    let head = String::from_utf8_lossy(&buf); let mut rl = head.lines().next().unwrap_or("").split(' ');
    let (method, target) = (rl.next().unwrap_or(""), rl.next().unwrap_or(""));
    let u = target.starts_with('/').then(|| url::Url::parse(&format!("https://localhost{target}")).ok()).flatten();
    let (resp, out) = match u {
        _ if method != "GET" => (page(405, "Not allowed", "This address only receives the bank redirect."), None),
        Some(u) if u.path() == path => {
            let mut q = HashMap::new(); for (k, v) in u.query_pairs() { q.entry(k.into_owned()).or_insert_with(|| v.into_owned()); }
            match handle(&q) {
                Outcome::Wait(m) => (page(400, "Not this login", &m), None),
                Outcome::Done(Ok(v)) => (page(200, "Bank connected", "You can close this tab and go back to the terminal."), Some(Ok(v))),
                Outcome::Done(Err(m)) => (page(400, "Connection failed", "Nothing was connected: the terminal says why. You can close this tab."), Some(Err(m))),
            }
        }
        _ => (page(404, "Not found", "This address is not the bank redirect."), None),
    };
    if left(&t).is_some() { let _ = t.write_all(&resp); t.conn.send_close_notify(); let _ = t.flush(); }
    out
}
