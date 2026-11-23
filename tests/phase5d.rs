//! Phase 5d on synthetic data: encrypted backups and exports, the alert engine (dedupe, escalation, baseline, guarded minimal
//! messages, the egress policy), LLM usage, the year review, and the coach run through a fake `claude` (isolation check, events,
//! unverified amounts, the SSE stream of the web app). No test reaches the network or starts a real process.
use anyhow::Result;
use finch::{alerts, analytics::{recurring, Dataset, Scope}, backup, classify, coach, config, db, ingest, net::{Http, Response}, web::{App, Req}};
use serde_json::{json, Value};
use std::{path::Path, sync::{atomic::AtomicBool, Arc, Mutex}};

fn setup(dir: &Path, extra: &str) -> (config::Config, rusqlite::Connection) {
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::write(dir.join("config.toml"), format!("insecure_plaintext_db = true\n[backup]\nretention = 2\n{extra}")).unwrap();
    std::fs::write(dir.join("memory/household.yaml"), "members:\n  - id: zorro\n    name: Zorro Testmans\n    role: adult\n").unwrap();
    std::fs::write(dir.join("memory/budgets.yaml"), "budgets:\n  - id: groceries\n    category: food.groceries\n    monthly: 100\n").unwrap();
    let cfg = config::load_config(Some(&dir.join("config.toml"))).unwrap(); let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap();
    let mut rows = vec![]; for (y, m) in [(2024, 3), (2024, 4), (2024, 5), (2024, 6), (2025, 3), (2025, 4), (2025, 5), (2025, 6)] { rows.push(format!("03/{m:02}/{y};BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 COLRUYT GENT;-{},00", if y == 2025 { 150 } else { 100 })); rows.push(format!("01/{m:02}/{y};OVERSCHRIJVING VAN QUUXCORP NV LOON;2000,00")); }
    rows.push("02/07/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 COLRUYT GENT;-180,00".into()); rows.push("01/07/2025;OVERSCHRIJVING VAN QUUXCORP NV LOON;2000,00".into());
    let f = dir.join("a.csv"); std::fs::write(&f, format!("Datum;Omschrijving;Bedrag\n{}\n", rows.join("\n"))).unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    ingest::import_file(&con, &f, ingest::ImportOpts { account: "new:Zicht", profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap();
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap(); (cfg, con)
}
fn today() -> Option<chrono::NaiveDate> { chrono::NaiveDate::from_ymd_opt(2025, 7, 20) }

#[test]
fn backups_and_exports() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path(), "");
    let (p, _) = backup::create(&cfg, &con, Some("s3cret")).unwrap(); assert!(backup::verify(&p, Some("s3cret")).unwrap().contains(&"memory/household.yaml".to_string()));
    assert!(backup::verify(&p, Some("wrong")).is_err());
    let to = tmp.path().join("restore"); let files = backup::restore(&p, &to, false, Some("s3cret")).unwrap(); assert!(files.iter().any(|f| f.ends_with("finance.db")));
    assert!(std::fs::read_to_string(to.join("memory/household.yaml")).unwrap().contains("Zorro"));
    assert!(backup::restore(&p, &to, false, Some("s3cret")).is_err(), "never overwrites without --force");
    let r = db::open_raw(&to.join("finance.db"), None).unwrap(); let n: i64 = r.query_row("SELECT COUNT(*) FROM transactions", [], |x| x.get(0)).unwrap(); assert_eq!(n, 18);
    for _ in 0..2 { std::thread::sleep(std::time::Duration::from_millis(1100)); backup::create(&cfg, &con, Some("s3cret")).unwrap(); }
    assert_eq!(backup::list(&cfg).len(), 2, "pruned to the retention");
    let e = backup::export(&cfg, &con, Some(tmp.path().join("x.zip.enc")), false, Some("s3cret")).unwrap(); assert!(std::fs::read(&e).unwrap().starts_with(b"AFCEX1"));
    let z = backup::decrypt_export(&e, tmp.path(), Some("s3cret")).unwrap(); let mut ar = zip::ZipArchive::new(std::fs::File::open(z).unwrap()).unwrap();
    let names: Vec<String> = ar.file_names().map(String::from).collect(); for n in ["manifest.json", "transactions.csv", "data/transactions.json", "memory/budgets.yaml", "README.txt"] { assert!(names.contains(&n.to_string()), "{n}"); }
    assert!(!names.iter().any(|n| n.contains("sessions.json") || n.contains(".history.git")));
    let mut csv = String::new(); std::io::Read::read_to_string(&mut ar.by_name("transactions.csv").unwrap(), &mut csv).unwrap(); assert!(csv.contains("-180.0") || csv.contains("-180"));
    assert!(backup::export(&cfg, &con, Some(tmp.path().join("x.zip.enc")), false, Some("s3cret")).is_err(), "never overwrites");
}

struct FakeHttp { sent: Mutex<Vec<(String, Value)>> }
impl Http for FakeHttp { fn send(&self, _m: &str, url: &str, _h: &[(&str, String)], body: Option<&Value>) -> Result<Response> { self.sent.lock().unwrap().push((url.into(), body.cloned().unwrap_or(Value::Null))); Ok(Response { status: 200, body: "{}".into() }) } }

#[test]
fn alert_engine() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path(), "[alerts]\nquiet_hours = \"\"\nexternal_min_severity = \"low\"\n[alerts.ntfy]\nenabled = true\nurl = \"https://ntfy.example/finch-test\"\n");
    let s = alerts::Settings::load(&cfg).unwrap(); let ds = Dataset::load(&con, &cfg, today()).unwrap(); let rec = recurring::detect(&ds, &Scope::default(), true); let now = chrono::Utc::now();
    let v = alerts::evaluate(&con, &ds, &rec, &s, now, false).unwrap(); assert!(!v["new"].as_array().unwrap().is_empty(), "{v}");
    let ev = alerts::listing(&con, Some(false)).unwrap(); let b = ev.iter().find(|e| e["kind"] == "budget").expect("the groceries budget is over"); assert_eq!(b["severity"], "high");
    let v2 = alerts::evaluate(&con, &ds, &rec, &s, now, false).unwrap(); assert!(v2["new"].as_array().unwrap().is_empty(), "the same situation is the same event");
    let http = FakeHttp { sent: Mutex::new(vec![]) };
    let r = alerts::dispatch(&con, &cfg, &s, now, false, &http).unwrap(); assert_eq!(r[0]["status"], "nothing"); assert!(r[0]["baselined"].as_u64().unwrap() >= 1, "the first time, existing alerts are history"); assert!(http.sent.lock().unwrap().is_empty());
    con.execute("INSERT INTO alert_events(id, kind, severity, created, updated, last_seen, title, body, payload, status) VALUES ('alr_test', 'consent', 'high', ?1, ?1, ?1, 'KBC Testmans: consent EXPIRED', 'reconnect', '{}', 'new')", [chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()]).unwrap();
    let r = alerts::dispatch(&con, &cfg, &s, now, true, &http).unwrap(); assert_eq!(r[0]["status"], "would_send"); assert!(http.sent.lock().unwrap().is_empty(), "a dry run sends nothing");
    let r = alerts::dispatch(&con, &cfg, &s, now, false, &http).unwrap(); assert_eq!(r[0]["status"], "sent", "{r:?}");
    let sent = http.sent.lock().unwrap().clone(); assert_eq!(sent.len(), 1); assert_eq!(sent[0].1, json!("Coach: 1 new alert (1 high). Open the app.")); assert!(!sent[0].1.to_string().contains("Testmans") && !sent[0].1.to_string().contains("KBC"));
    assert_eq!(alerts::dispatch(&con, &cfg, &s, now, false, &http).unwrap()[0]["status"], "nothing", "sent once");
    let mut off = cfg.clone(); off.privacy_local_only = true; assert_eq!(alerts::dispatch(&con, &off, &s, now, false, &http).unwrap()[0]["status"], "not_ready", "local_only refuses external channels");
    assert!(alerts::act(&con, "alr_test", "ack", None, today().unwrap()).unwrap()); assert_eq!(alerts::summary(&con).unwrap()["high"].as_i64().unwrap() >= 1, true);
    alerts::kind_act(&con, "budget", "mute", None, today().unwrap()).unwrap(); assert!(alerts::listing(&con, Some(false)).unwrap().iter().filter(|e| e["kind"] == "budget").all(|e| e["status"] == "suppressed"));
    let rv = finch::analytics::review::year_review(&ds, Some(2025), &Scope::default(), 10); assert_eq!(rv["year"], 2025);
    let g = rv["biggest_increases"].as_array().unwrap(); assert!(g.is_empty() || g[0]["category"].is_string());
}

struct FakeClaude { lines: Vec<String>, argv: Mutex<Vec<String>> }
impl coach::Spawner for FakeClaude {
    fn which(&self) -> bool { true }
    fn start(&self, argv: &[String], _env: &[(String, String)], _stdin: &str, _cwd: &Path) -> Result<(Box<dyn Iterator<Item = String> + Send>, Box<dyn Fn() + Send + Sync>)> { *self.argv.lock().unwrap() = argv.to_vec(); Ok((Box::new(self.lines.clone().into_iter()), Box::new(|| {}))) }
}
fn stream(tools: &[&str]) -> Vec<String> {
    vec![json!({"type": "system", "subtype": "init", "model": "sonnet", "tools": tools, "mcp_servers": [{"name": "finance", "status": "connected"}]}).to_string(),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "mcp__finance__cashflow", "input": {"months": 3}}]}}).to_string(),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "{\"spending\": \"1234.50\", \"proposal_id\": \"p-20250720-abc123\"}"}]}]}}).to_string(),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "You spent 1.234,50 EUR; maybe 99.99 more."}}}).to_string(),
        json!({"type": "result", "subtype": "success", "result": "You spent 1.234,50 EUR; maybe 99.99 more.", "usage": {"input_tokens": 1000, "output_tokens": 50}, "total_cost_usd": 0.0123}).to_string()]
}

#[test]
fn coach_runs_isolated_and_streams() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path(), "");
    let fake = FakeClaude { lines: stream(&["mcp__finance__coverage", "mcp__finance__cashflow"]), argv: Mutex::new(vec![]) }; let mut evs = vec![];
    let ans = coach::run(&cfg, true, "How much did we spend?", &fake, Arc::new(AtomicBool::new(false)), Some(&con), &mut |e, d| evs.push((e.to_string(), d))).unwrap();
    assert_eq!(ans["unverified_numbers"], json!(["99.99"]), "only an amount no tool returned is flagged");
    let kinds: Vec<&str> = evs.iter().map(|e| e.0.as_str()).collect(); for k in ["init", "tool_call", "tool_result", "proposal", "delta", "usage", "answer", "done"] { assert!(kinds.contains(&k), "{k}: {kinds:?}"); }
    let argv = fake.argv.lock().unwrap().clone(); for f in ["--strict-mcp-config", "--disable-slash-commands", "--setting-sources", "--allowedTools"] { assert!(argv.contains(&f.to_string()), "{f}"); }
    assert_eq!(argv[argv.iter().position(|a| a == "--tools").unwrap() + 1], "", "no built-in tool");
    let n: i64 = con.query_row("SELECT COUNT(*) FROM llm_usage WHERE purpose='coach.ask'", [], |r| r.get(0)).unwrap(); assert_eq!(n, 1);
    assert_eq!(finch::usage::summary(&con, &cfg, 30, chrono::Utc::now()).unwrap()["totals"]["calls"], 1);
    // a CLI that starts with a built-in tool is stopped before anything counts
    let bad = FakeClaude { lines: stream(&["Bash", "mcp__finance__coverage"]), argv: Mutex::new(vec![]) }; let mut evs = vec![];
    assert!(coach::run(&cfg, true, "x", &bad, Arc::new(AtomicBool::new(false)), Some(&con), &mut |e, d| evs.push((e.to_string(), d))).is_err());
    assert!(evs.iter().any(|e| e.0 == "error" && e.1["code"] == "unexpected_tool")); assert!(!evs.iter().any(|e| e.0 == "delta"));
    // the web app's stream
    let st = tmp.path().join("static"); std::fs::create_dir_all(&st).unwrap(); let mut a = App::new(cfg, true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = today();
    a.coach = Arc::new(FakeClaude { lines: stream(&["mcp__finance__cashflow"]), argv: Mutex::new(vec![]) });
    let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
    let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(None).unwrap()}).to_string().into_bytes()));
    let ck = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let hh = |extra: Vec<(String, String)>| { let mut v = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Cookie".into(), format!("{}={ck}", a.sec.cookie_name))]; v.extend(extra); v };
    let s: Value = serde_json::from_slice(&a.handle(Req::new("GET", "/api/v1/session", hh(vec![]), vec![])).body).unwrap(); let csrf = s["csrf_token"].as_str().unwrap().to_string();
    let st: Value = serde_json::from_slice(&a.handle(Req::new("GET", "/api/v1/coach/status", hh(vec![]), vec![])).body).unwrap(); assert_eq!(st["configured"], true, "{st}");
    let r = a.handle(Req::new("POST", "/api/v1/coach/stream", hh(vec![("Content-Type".into(), "application/json".into()), ("X-CSRF-Token".into(), csrf)]), json!({"question": "How much?"}).to_string().into_bytes()));
    assert_eq!(r.status, 200); let body: String = r.stream.unwrap().iter().map(|b| String::from_utf8(b).unwrap()).collect();
    for e in ["event: meta", "event: delta", "event: tool_call", "event: answer", "event: done"] { assert!(body.contains(e), "{e}: {body}"); }
    assert_eq!(a.coach_job.lock().unwrap().is_none(), true, "the job slot is free again");
    let al: Value = serde_json::from_slice(&a.handle(Req::new("GET", "/api/v1/alerts", hh(vec![]), vec![])).body).unwrap(); assert_eq!(al["ready"], true);
}
