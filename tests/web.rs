//! The web API (phase 4) on synthetic data: the security model (login link, cookie, CSRF, Host / Origin, child scope) and the
//! read / write endpoints the web app calls. No test reaches the network.
use finch::{classify, config, db, ingest, web::{self, App, Req, Resp}};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

fn home(dir: &Path) -> config::Config {
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::write(dir.join("config.toml"), "insecure_plaintext_db = true\n").unwrap();
    std::fs::write(dir.join("memory/household.yaml"), "members:\n  - id: zorro\n    name: Zorro Testmans\n    role: adult\n  - id: kiki\n    name: Kiki Testmans\n    role: child\n").unwrap();
    std::fs::write(dir.join("memory/budgets.yaml"), "budgets:\n  - id: groceries\n    category: food.groceries\n    monthly: 100\n").unwrap();
    config::load_config(Some(&dir.join("config.toml"))).unwrap()
}
fn app(dir: &Path) -> App {
    let cfg = home(dir); let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap();
    let mut csv = String::from("Datum;Omschrijving;Bedrag\n");
    for m in 1..=4 { csv += &format!("01/{m:02}/2025;OVERSCHRIJVING VAN QUUXCORP NV LOON;2500,00\n03/{m:02}/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 COLRUYT GENT;-120,50\n10/{m:02}/2025;EUROPESE DOMICILIERING SCHULDEISER : PROXIMUS NV MANDAAT NUMMER : AB12345;-60,00\n"); }
    csv += "12/04/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 =CMD SHOP;-15,00\n";
    let f = dir.join("kbc.csv"); std::fs::write(&f, &csv).unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    ingest::import_file(&con, &f, ingest::ImportOpts { account: "new:Zichtrekening", profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap();
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap();
    let st = dir.join("static"); std::fs::create_dir_all(st.join("assets")).unwrap(); std::fs::write(st.join("index.html"), "<!doctype html><title>finch</title>").unwrap(); std::fs::write(st.join("assets/app.js"), "console.log(1)").unwrap();
    let mut a = App::new(cfg, true, 8765, false, st, Arc::new(|| panic!("no network in tests"))).unwrap();
    a.today = chrono::NaiveDate::from_ymd_opt(2025, 5, 10); a.inline_jobs = true; a
}

struct C<'a> { a: &'a App, cookie: Option<String>, csrf: Option<String> }
impl C<'_> {
    fn raw(&self, method: &str, path: &str, body: Option<Value>, extra: &[(&str, &str)]) -> Resp {
        let mut h: Vec<(String, String)> = vec![("Host".into(), "127.0.0.1:8765".into())];
        if let Some(c) = &self.cookie { h.push(("Cookie".into(), format!("other=1; {}={c}", self.a.sec.cookie_name))); }
        if let Some(t) = &self.csrf { h.push(("X-CSRF-Token".into(), t.clone())); }
        if body.is_some() { h.push(("Content-Type".into(), "application/json".into())); }
        for (k, v) in extra { h.retain(|(x, _)| !x.eq_ignore_ascii_case(k)); h.push((k.to_string(), v.to_string())); }
        self.a.handle(Req::new(method, path, h, body.map(|b| b.to_string().into_bytes()).unwrap_or_default()))
    }
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) { let r = self.raw(method, path, body, &[]); (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null)) }
    fn get(&self, path: &str) -> Value { let (s, v) = self.call("GET", path, None); assert_eq!(s, 200, "{path}: {v}"); v }
    fn login<'a>(a: &'a App, user: Option<&str>) -> C<'a> {
        let mut c = C { a, cookie: None, csrf: None };
        let r = c.raw("POST", "/api/v1/session/exchange", Some(json!({"token": a.sec.tokens.issue(user).unwrap()})), &[]); assert_eq!(r.status, 200);
        let sc = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.clone(); assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Strict"));
        c.cookie = Some(sc.split(';').next().unwrap().split_once('=').unwrap().1.to_string()); c.csrf = Some(c.get("/api/v1/session")["csrf_token"].as_str().unwrap().to_string()); c
    }
}
fn code(r: &Resp) -> String { serde_json::from_slice::<Value>(&r.body).unwrap()["error"]["code"].as_str().unwrap_or("").to_string() }

#[test]
fn security_model() {
    let tmp = tempfile::tempdir().unwrap(); let a = app(tmp.path()); let anon = C { a: &a, cookie: None, csrf: None };
    assert_eq!(anon.raw("GET", "/api/v1/session", None, &[("Host", "evil.example:8765")]).status, 421, "DNS rebinding");
    let r = anon.raw("GET", "/api/v1/session", None, &[]); assert_eq!((r.status, code(&r)), (401, "unauthorized".into()));
    assert!(r.headers.iter().any(|(k, v)| k == "Content-Security-Policy" && v.contains("frame-ancestors 'none'")) && r.headers.iter().any(|(k, v)| k == "Cache-Control" && v == "no-store"), "headers on errors too");
    assert_eq!(anon.call("POST", "/api/v1/session/exchange", Some(json!({"token": "nope"}))).0, 401);
    let t = a.sec.tokens.issue(None).unwrap();
    assert_eq!(anon.call("POST", "/api/v1/session/exchange", Some(json!({"token": t}))).0, 200);
    assert_eq!(anon.call("POST", "/api/v1/session/exchange", Some(json!({"token": t}))).0, 401, "a login token is single-use");
    let c = C::login(&a, None);
    let s = c.get("/api/v1/session"); assert_eq!(s["user"]["role"], "adult"); assert_eq!(s["today"], "2025-05-10");
    let no_csrf = C { a: &a, cookie: c.cookie.clone(), csrf: None };
    assert_eq!(code(&no_csrf.raw("POST", "/api/v1/transfers/unlink", Some(json!({"ref": "x"})), &[])), "csrf");
    assert_eq!(code(&c.raw("POST", "/api/v1/transfers/unlink", Some(json!({"ref": "x"})), &[("Origin", "https://evil.example")])), "forbidden");
    assert_eq!(c.raw("GET", "/api/v1/session", None, &[("Sec-Fetch-Site", "cross-site")]).status, 403);
    assert_eq!(c.raw("POST", "/api/v1/transfers/unlink", Some(json!({"ref": "x"})), &[("Content-Type", "text/plain")]).status, 415);
    let forged = C { a: &a, cookie: Some(format!("{}x", c.cookie.as_ref().unwrap())), csrf: c.csrf.clone() }; assert_eq!(forged.call("GET", "/api/v1/session", None).0, 401, "a tampered cookie is refused");
    // a child login sees nothing but its session, whatever the endpoint (deny by default)
    db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().execute("INSERT INTO ui_users(id, member_id, role, created_at) VALUES ('kiki-login', 'kiki', 'child', '2025-01-01')", []).unwrap();
    let kid = C::login(&a, Some("kiki-login")); assert_eq!(kid.get("/api/v1/session")["user"]["role"], "child");
    for p in ["/api/v1/transactions", "/api/v1/accounts/balances", "/api/v1/connections", "/api/v1/does-not-exist"] { let r = kid.raw("GET", p, None, &[]); assert_eq!((r.status, code(&r)), (403, "forbidden_scope".into()), "{p}"); }
    // logout revokes the cookie server-side; the shell and static files
    assert_eq!(kid.call("POST", "/api/v1/session/logout", Some(json!({}))).0, 200); assert_eq!(kid.call("GET", "/api/v1/session", None).0, 401);
    assert_eq!(anon.raw("GET", "/budgets", None, &[]).status, 200, "an app route serves the shell");
    assert_eq!(String::from_utf8_lossy(&anon.raw("GET", "/assets/app.js", None, &[]).body), "console.log(1)");
    for p in ["/assets/missing.js", "/../config.toml", "/%2e%2e/config.toml"] { assert_eq!(anon.raw("GET", p, None, &[]).status, 404, "{p}"); }
    let r = c.raw("GET", "/api/v1/loans", None, &[]); assert_eq!((r.status, code(&r)), (404, "not_ported".into()));
}

#[test]
fn endpoints() {
    let tmp = tempfile::tempdir().unwrap(); let a = app(tmp.path()); let c = C::login(&a, None);
    let tax = c.get("/api/v1/meta/taxonomy"); assert!(tax["groups"].as_array().unwrap().iter().any(|g| g["id"] == "food"));
    let f = c.get("/api/v1/meta/filters"); assert_eq!(f["accounts"][0]["label"], "Zichtrekening"); assert!(f["owners"].as_array().unwrap().contains(&json!("joint")));
    assert!(c.get("/api/v1/meta/disclaimers?lang=fr")["texts"]["general_advice"].as_str().unwrap().contains("not financial advice"));
    let cf = c.get("/api/v1/analytics/cashflow?months=6");
    let mar = cf["household"]["months"].as_array().unwrap().iter().find(|m| m["month"] == "2025-03").unwrap(); assert_eq!(mar["income"], "2500.00"); assert_eq!(mar["spending"], "180.50");
    assert_eq!(cf["by_owner"], json!({}), "per-owner blocks only on request");
    let cats = c.get("/api/v1/categories"); let g = cats["categories"].as_array().unwrap().iter().find(|x| x["category"] == "food.groceries").unwrap(); assert_eq!(g["monthly_avg"], "120.50");
    let det = c.get("/api/v1/categories/food.groceries"); assert_eq!(det["average"]["monthly"], "120.50"); assert_eq!(det["totals"]["n_tx"], 4);
    assert!(c.get("/api/v1/categories/food")["is_group"].as_bool().unwrap());
    assert_eq!(c.call("GET", "/api/v1/categories/nope.nope", None).0, 404);
    let mc = c.get("/api/v1/analytics/month-categories?month=2025-04"); assert_eq!(mc["month"], "2025-04"); assert!(mc["categories"].as_array().unwrap().iter().any(|x| x["category"] == "food.groceries" && x["spent"] == "120.50"));
    assert_eq!(c.call("GET", "/api/v1/analytics/month-categories?month=2025-13", None).0, 422);
    assert_eq!(c.call("GET", "/api/v1/analytics/cashflow?months=99", None).0, 422);
    assert_eq!(c.call("GET", "/api/v1/analytics/cashflow?member=zorro", None).0, 200, "a person's view");
    assert_eq!(c.call("GET", "/api/v1/analytics/cashflow?member=ghost", None).0, 404, "an unknown member");
    assert!(c.get("/api/v1/analytics/forecast?days=30")["household"].is_object());
    assert!(c.get("/api/v1/calendar?days=40")["items"].is_array());
    let ics = c.raw("GET", "/api/v1/calendar.ics", None, &[]); assert!(String::from_utf8_lossy(&ics.body).starts_with("BEGIN:VCALENDAR"));
    let b = c.get("/api/v1/budgets"); assert_eq!(b["budgets"][0]["id"], "groceries"); assert_eq!(b["budgets"][0]["category"], "food.groceries");
    c.get("/api/v1/budgets/suggestions"); c.get("/api/v1/goals"); c.get("/api/v1/insights"); c.get("/api/v1/analytics/coverage"); c.get("/api/v1/accounts/balances");
    let subs = c.get("/api/v1/subscriptions?status=all"); assert!(subs["series"].as_array().unwrap().iter().any(|s| s["monthly_cost"].is_string()));
    // transactions: filters, totals over the whole filtered set, CSV with formula cells neutralised
    let t = c.get("/api/v1/transactions?q=colruyt&limit=2"); assert_eq!(t["total"], 4); assert_eq!(t["totals"]["outflow"], "-482.00"); assert_eq!(t["items"].as_array().unwrap().len(), 2); assert_eq!(t["next_offset"], 2);
    let csv = String::from_utf8_lossy(&c.raw("GET", "/api/v1/transactions/export.csv?q=shop", None, &[]).body).to_string(); assert!(csv.starts_with("date,account") && !csv.contains(",=CMD"), "{csv}");
    let key = t["items"][0]["tx_key"].as_str().unwrap().to_string();
    let d = c.get(&format!("/api/v1/transactions/detail?tx_key={key}")); assert_eq!(d["item"]["tx_key"], key.as_str());
    let (s, v) = c.call("POST", "/api/v1/transactions/category", Some(json!({"tx_key": key, "category": "shopping.clothing", "scope": "transaction"}))); assert_eq!(s, 200, "{v}");
    let it = c.get(&format!("/api/v1/transactions/detail?tx_key={key}"))["item"].clone(); assert_eq!((it["category"].as_str(), it["overridden"].as_bool()), (Some("shopping.clothing"), Some(true)), "a write refreshes the cached dataset");
    assert_eq!(c.call("POST", "/api/v1/transactions/category", Some(json!({"tx_key": key, "category": "made.up", "scope": "transaction"}))).0, 422);
    assert_eq!(c.call("POST", "/api/v1/transactions/override/clear", Some(json!({"tx_key": key}))).0, 200);
    // accounts, connections, health, review, transfers
    let uid = f["accounts"][0]["uid"].as_str().unwrap().to_string();
    let (s, v) = c.call("PATCH", &format!("/api/v1/accounts/{uid}"), Some(json!({"label": "Daily", "purpose": "main"}))); assert_eq!((s, v["label"].as_str()), (200, Some("Daily")), "{v}");
    assert_eq!(c.call("PATCH", &format!("/api/v1/accounts/{uid}"), Some(json!({"purpose": "nonsense"}))).0, 400);
    let con = c.get("/api/v1/connections"); assert_eq!(con["accounts"][0]["label"], "Daily"); assert_eq!(con["enable_banking_configured"], false); assert!(con["health"]["banks"].is_array());
    let (s, v) = c.call("POST", "/api/v1/sync", Some(json!({}))); assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_configured")));
    c.get("/api/v1/review"); c.get("/api/v1/transfers"); c.get("/api/v1/health"); c.get("/api/v1/alerts/summary");
    let n: i64 = db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().query_row("SELECT COUNT(*) FROM audit_log WHERE actor='owner' AND method='PATCH'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "who changed what is audited (never a payload)");
    let _ = web::API;
}

fn git_ok() -> bool { std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) }
#[test]
fn memory_lists_and_health() {
    let tmp = tempfile::tempdir().unwrap(); let a = app(tmp.path()); let c = C::login(&a, None); let m = tmp.path().join("memory");
    std::fs::write(m.join("household.yaml"), "members:\n  - id: zorro\n    name: Zorro Testmans\n    role: adult\n    birth_year: 1980\n    aliases: [ZT]\n  - id: kiki\n    name: Kiki Testmans\n    role: child\n").unwrap();
    std::fs::write(m.join("events.yaml"), "events:\n  - id: trip-x\n    title: Trip X\n    start: 2025-03-01\n    budget: 500\n").unwrap();
    std::fs::write(m.join("events.md"), "# Events\n\n## works-y\nsome works\n\n## trip-x\n").unwrap();
    std::fs::create_dir_all(m.join("contracts")).unwrap(); std::fs::write(m.join("contracts/net.yaml"), "id: net\nprovider: Quuxnet\nkind: telecom\nbilling:\n  amount: 45.5\n  period: monthly\nnotice_period_days: 30\nusage:\n  frequency: daily\n").unwrap();
    let h = c.get("/api/v1/household"); assert_eq!(h["members"][0]["birth_year"], 1980); assert_eq!(h["members"][0]["aliases"], json!(["ZT"])); assert_eq!(h["members"][1]["role"], "child");
    let ev = c.get("/api/v1/events")["events"].clone(); assert_eq!(ev, json!([{"id": "trip-x", "title": "Trip X", "start": "2025-03-01", "end": null, "budget": "500.00", "status": null, "note": null, "source": "events.yaml"}, {"id": "works-y", "title": null, "start": null, "end": null, "budget": null, "status": null, "note": null, "source": "events.md"}]));
    assert_eq!(c.get("/api/v1/meta/filters")["events"].as_array().unwrap().len(), 2);
    let ct = c.get("/api/v1/contracts"); assert_eq!(ct["contracts"][0]["file"], "contracts/net.yaml"); assert_eq!(ct["contracts"][0]["billing"]["amount"], "45.50"); assert_eq!(ct["contracts"][0]["usage"]["frequency"], "daily");
    let o = c.get("/api/v1/memory/overview")["counts"].clone(); assert_eq!((o["contracts"].as_i64(), o["events"].as_i64(), o["members"].as_i64(), o["liabilities"].as_i64()), (Some(1), Some(2), Some(2), Some(0)));
    assert_eq!(c.get("/api/v1/session")["memory_history"], true);
    // health: the memory check and the last scheduled run (step names, statuses and counts only)
    std::fs::write(m.join("budgets.yaml"), "budgets: [oops\n").unwrap();
    let logs = a.cfg.log_dir(); std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("runs.jsonl.1"), "{\"run\": \"r1\", \"event\": \"run_start\", \"ts\": \"2025-05-08T06:00:00\"}\n").unwrap();
    std::fs::write(logs.join("runs.jsonl"), "{\"run\": \"r2\", \"event\": \"run_start\", \"ts\": \"2025-05-09T06:00:00\"}\nnot json\n{\"run\": \"r2\", \"event\": \"step\", \"step\": \"sync\", \"status\": \"error\", \"ms\": 12, \"counts\": {\"new\": 0}}\n{\"run\": \"r2\", \"event\": \"run_end\", \"ts\": \"2025-05-09T06:00:02\", \"outcome\": \"failed\", \"exit\": 1, \"ms\": 2345}\n").unwrap();
    let hl = c.get("/api/v1/health"); assert_eq!(hl["memory"]["errors"], 1, "{hl}"); assert_eq!(hl["memory"]["by_code"]["invalid"], 1);
    assert_eq!((hl["last_run"]["run"].as_str(), hl["last_run"]["outcome"].as_str(), hl["last_run"]["duration_s"].as_f64()), (Some("r2"), Some("failed"), Some(2.3))); assert_eq!(hl["last_run"]["failed"], json!(["sync"]));
    std::fs::write(m.join("budgets.yaml"), "budgets: []\n").unwrap();
    // removing a split; the audit lists the memory history with its source
    let key = c.get("/api/v1/transactions?q=colruyt&limit=1")["items"][0]["tx_key"].as_str().unwrap().to_string();
    db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().execute("INSERT INTO tx_splits(tx_key, amount, category) VALUES (?1, -100, 'food.groceries'), (?1, -20.5, 'shopping.other')", [&key]).unwrap();
    let (s, v) = c.call("POST", "/api/v1/transactions/split/clear", Some(json!({"tx_key": key}))); assert_eq!((s, v["removed"].as_i64()), (200, Some(2)), "{v}");
    assert_eq!(c.call("POST", "/api/v1/transactions/split/clear", Some(json!({}))).0, 422);
    if git_ok() { assert_eq!(c.call("POST", "/api/v1/goals", Some(json!({"id": "buffer", "target": "1000"}))).0, 200);
        let au = c.get("/api/v1/household/audit"); assert!(au["memory"].as_array().unwrap().iter().any(|x| x["source"] == "ui" && x["files"] == json!(["goals.yaml"])), "{au}"); }
}
