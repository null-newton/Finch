//! The only places that talk to the outside: an HTTP transport and a process runner, both behind traits so tests inject fakes
//! (no test may reach the network). Callers gate every use with [`crate::egress::allow`] first.
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

pub struct Response { pub status: u16, pub body: String }
impl Response { pub fn json(&self) -> Result<Value> { if self.body.trim().is_empty() { return Ok(Value::Object(Default::default())); } serde_json::from_str(&self.body).context("the answer is not JSON") } }

pub trait Http { fn send(&self, method: &str, url: &str, headers: &[(&str, String)], body: Option<&Value>) -> Result<Response>; }
pub struct UreqHttp { agent: ureq::Agent }
impl UreqHttp { pub fn new(timeout_s: u64) -> Self { Self { agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(timeout_s)).redirects(0).build() } } }
impl Http for UreqHttp {
    fn send(&self, method: &str, url: &str, headers: &[(&str, String)], body: Option<&Value>) -> Result<Response> {
        let mut r = self.agent.request(method, url); for (k, v) in headers { r = r.set(k, v); }
        let res = match body { Some(b) => r.send_json(b.clone()), None => r.call() };
        match res {
            Ok(x) => { let s = x.status(); Ok(Response { status: s, body: x.into_string()? }) }
            Err(ureq::Error::Status(s, x)) => Ok(Response { status: s, body: x.into_string().unwrap_or_default() }),
            Err(e) => Err(anyhow::anyhow!("network error: {}", e.kind())),
        }
    }
}

/// Run a program with stdin, returning (exit code, stdout, stderr).
pub trait Runner { fn run(&self, argv: &[String], stdin: &str, env: &[(String, String)], timeout_s: u64) -> Result<(i32, String, String)>; }
pub struct ProcessRunner;
impl Runner for ProcessRunner {
    /// Run with a hard time limit: the process is killed when it runs longer than `timeout_s`.
    fn run(&self, argv: &[String], stdin: &str, env: &[(String, String)], timeout_s: u64) -> Result<(i32, String, String)> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("finch-classify-{}", std::process::id())); std::fs::create_dir_all(&dir)?;
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..]).env_clear().envs(env.iter().cloned()).current_dir(&dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("cannot start {:?} (is it installed and on PATH?)", argv[0]))?;
        child.stdin.take().unwrap().write_all(stdin.as_bytes())?;
        let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let ro = std::thread::spawn(move || { let mut b = vec![]; let _ = std::io::Read::read_to_end(&mut so, &mut b); b }); let re = std::thread::spawn(move || { let mut b = vec![]; let _ = std::io::Read::read_to_end(&mut se, &mut b); b });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s.max(1));
        let status = loop { if let Some(st) = child.try_wait()? { break st; } if std::time::Instant::now() > end { let _ = child.kill(); let _ = child.wait(); let _ = std::fs::remove_dir_all(&dir); anyhow::bail!("{} did not answer within {timeout_s} s and was stopped", argv[0]); } std::thread::sleep(std::time::Duration::from_millis(100)); };
        let out = std::process::Output { status, stdout: ro.join().unwrap_or_default(), stderr: re.join().unwrap_or_default() }; let _ = std::fs::remove_dir_all(&dir);
        Ok((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into()))
    }
}

/// One e-mail: (host, port, tls = starttls | ssl, user, from, to, subject, body); the password is passed apart.
pub struct Mail<'a> { pub host: &'a str, pub port: u16, pub tls: &'a str, pub user: &'a str, pub from: &'a str, pub to: &'a str, pub subject: &'a str, pub body: &'a str }
pub trait Smtp { fn send(&self, m: &Mail, password: &str) -> Result<()>; }
/// The real transports, or fakes in tests: HTTP, SMTP, and the process runner (macOS notification).
pub struct Transports<'a> { pub http: &'a dyn Http, pub smtp: &'a dyn Smtp, pub run: &'a dyn Runner }
pub struct TlsSmtp { pub timeout_s: u64 }
fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    std::sync::Arc::new(rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().expect("tls versions").with_root_certificates(roots).with_no_client_auth())
}
impl Smtp for TlsSmtp {
    fn send(&self, m: &Mail, password: &str) -> Result<()> {
        use std::net::ToSocketAddrs; let to = Duration::from_secs(self.timeout_s.max(1));
        let addr = (m.host, m.port).to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("the SMTP host does not resolve"))?; let tcp = std::net::TcpStream::connect_timeout(&addr, to)?; tcp.set_read_timeout(Some(to))?; tcp.set_write_timeout(Some(to))?;
        let name = rustls::pki_types::ServerName::try_from(m.host.to_string()).map_err(|_| anyhow::anyhow!("bad SMTP host name"))?;
        let wrap = |t: std::net::TcpStream| -> Result<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>> { Ok(rustls::StreamOwned::new(rustls::ClientConnection::new(tls_config(), name.clone())?, t)) };
        if m.tls == "ssl" { let mut s = wrap(tcp)?; reply(&mut s, 220)?; smtp_session(&mut s, m, password) }
        else { let mut t = tcp; reply(&mut t, 220)?; cmd(&mut t, "EHLO localhost", 250)?; cmd(&mut t, "STARTTLS", 220)?; let mut s = wrap(t)?; smtp_session(&mut s, m, password) }
    }
}
/// Reads one (possibly multi-line) SMTP reply and checks its code.
pub fn reply<S: std::io::Read>(s: &mut S, want: u16) -> Result<String> {
    let mut all = String::new();
    loop { let mut line = vec![]; let mut b = [0u8; 1]; while !line.ends_with(b"\r\n") { if s.read(&mut b)? == 0 { anyhow::bail!("the SMTP server closed the connection"); } line.push(b[0]); if line.len() > 4096 { anyhow::bail!("SMTP reply too long"); } }
        let l = String::from_utf8_lossy(&line).to_string(); all.push_str(&l); if l.len() < 4 || l.as_bytes()[3] != b'-' { let code: u16 = l.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0); if code != want && !(want == 250 && code == 251) { anyhow::bail!("SMTP answered {code}"); } return Ok(all); } }
}
fn cmd<S: std::io::Read + std::io::Write>(s: &mut S, c: &str, want: u16) -> Result<String> { s.write_all(format!("{c}\r\n").as_bytes())?; s.flush()?; reply(s, want) }
fn hdr(v: &str) -> String { if v.is_ascii() { v.into() } else { use base64::Engine; format!("=?UTF-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(v)) } }
/// EHLO, AUTH PLAIN, MAIL / RCPT / DATA, QUIT on an open (TLS) stream after the greeting.
pub fn smtp_session<S: std::io::Read + std::io::Write>(s: &mut S, m: &Mail, password: &str) -> Result<()> {
    use base64::Engine; if [m.from, m.to, m.subject, m.user].iter().any(|x| x.contains(['\r', '\n'])) { anyhow::bail!("a line break in an e-mail header is refused"); }
    cmd(s, "EHLO localhost", 250)?; cmd(s, &format!("AUTH PLAIN {}", base64::engine::general_purpose::STANDARD.encode(format!("\0{}\0{password}", m.user))), 235)?;
    cmd(s, &format!("MAIL FROM:<{}>", m.from), 250)?; cmd(s, &format!("RCPT TO:<{}>", m.to), 250)?; cmd(s, "DATA", 354)?;
    let body = base64::engine::general_purpose::STANDARD.encode(m.body.as_bytes()); let body: Vec<&str> = body.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let msg = format!("From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{:016x}@finch.invalid>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n.", m.from, m.to, hdr(m.subject), chrono::Local::now().to_rfc2822(), rand::random::<u64>(), body.join("\r\n"));
    cmd(s, &msg, 250)?; let _ = cmd(s, "QUIT", 221); Ok(())
}
