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
    fn run(&self, argv: &[String], stdin: &str, env: &[(String, String)], _timeout_s: u64) -> Result<(i32, String, String)> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("finch-classify-{}", std::process::id())); std::fs::create_dir_all(&dir)?;
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..]).env_clear().envs(env.iter().cloned()).current_dir(&dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("cannot start {:?} (is it installed and on PATH?)", argv[0]))?;
        child.stdin.take().unwrap().write_all(stdin.as_bytes())?;
        let out = child.wait_with_output()?; let _ = std::fs::remove_dir_all(&dir);
        Ok((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into()))
    }
}
