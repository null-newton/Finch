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

#[test]
fn insight_states() {
    let tmp = tempfile::tempdir().unwrap(); let mut a = app(tmp.path()); a.today = chrono::NaiveDate::from_ymd_opt(2025, 4, 20); let c = C::login(&a, None);
    let feed = c.get("/api/v1/insights"); let cards = feed["cards"].as_array().unwrap().clone();
    let b = cards.iter().find(|x| x["kind"] == "budget").unwrap_or_else(|| panic!("a budget card: {feed}"))["id"].as_str().unwrap().to_string(); assert!(feed["cards"][0]["snoozed_until"].is_null());
    let kinds: std::collections::BTreeSet<&str> = cards.iter().filter_map(|x| x["kind"].as_str()).collect(); assert!(kinds.contains("budget") && kinds.contains("forecast"), "{kinds:?}");
    let ids = |v: &Value| v["cards"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    // a budget card (ui-state.json): dismiss hides it, restore brings it back
    let (s, v) = c.call("POST", &format!("/api/v1/insights/{b}/dismiss"), Some(json!({}))); assert_eq!((s, v["dismissed"].as_bool()), (200, Some(true)), "{v}");
    let f = c.get("/api/v1/insights"); assert!(!ids(&f).contains(&b)); assert_eq!(f["hidden"].as_u64().unwrap(), feed["hidden"].as_u64().unwrap() + 1);
    let st: Value = serde_json::from_str(&std::fs::read_to_string(tmp.path().join("data/ui-state.json")).unwrap()).unwrap(); assert_eq!(st["dismissed"], json!([b]), "Python-compatible ui-state.json");
    assert_eq!(c.call("POST", &format!("/api/v1/insights/{b}/restore"), Some(json!({}))).1["restored"], true); assert!(ids(&c.get("/api/v1/insights")).contains(&b));
    // snooze: hidden until the date, shown with include_snoozed and its date
    let (s, v) = c.call("POST", &format!("/api/v1/insights/{b}/snooze"), Some(json!({"days": 3}))); assert_eq!((s, v["snoozed_until"].as_str()), (200, Some("2025-04-23")), "{v}");
    assert!(!ids(&c.get("/api/v1/insights")).contains(&b)); let inc = c.get("/api/v1/insights?include_snoozed=true"); assert_eq!(inc["cards"].as_array().unwrap().iter().find(|x| x["id"] == b.as_str()).unwrap()["snoozed_until"], "2025-04-23");
    assert_eq!(c.call("POST", &format!("/api/v1/insights/{b}/snooze"), Some(json!({"days": 999}))).0, 422);
    c.call("POST", &format!("/api/v1/insights/{b}/restore"), Some(json!({})));
    // every other card kind (anomaly, price change, forecast) can be dismissed and restored too
    for k in cards.iter().filter(|x| x["id"] != b.as_str()) { let id = k["id"].as_str().unwrap();
        let (s, v) = c.call("POST", &format!("/api/v1/insights/{id}/done"), Some(json!({}))); assert_eq!(s, 200, "{} {v}", k["kind"]); assert!(!ids(&c.get("/api/v1/insights")).contains(&id.to_string()), "{}", k["kind"]);
        c.call("POST", &format!("/api/v1/insights/{id}/restore"), Some(json!({}))); assert!(ids(&c.get("/api/v1/insights")).contains(&id.to_string()), "{} restored", k["kind"]); }
    assert_eq!(c.call("POST", "/api/v1/insights/ins_0000000000/dismiss", Some(json!({}))).0, 404); assert_eq!(c.call("POST", &format!("/api/v1/insights/{b}/read"), Some(json!({}))).0, 404);
    // the coach's own insights: status in the database
    db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().execute("INSERT INTO insights(id, created, kind, title, body, status) VALUES ('cin_1', '2025-04-19', 'answer', 't', 'b', 'new')", []).unwrap();
    assert_eq!(c.call("POST", "/api/v1/insights/cin_1/snooze", Some(json!({"days": 2}))).1["snoozed_until"], "2025-04-22"); assert!(c.get("/api/v1/insights")["coach"]["items"].as_array().unwrap().is_empty());
    assert_eq!(c.get("/api/v1/insights?include_snoozed=true")["coach"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(c.call("POST", "/api/v1/insights/cin_1/restore", Some(json!({}))).1["status"], "new"); assert_eq!(c.get("/api/v1/insights")["coach"]["items"].as_array().unwrap().len(), 1);
}

#[test]
fn coach_resolve() {
    let tmp = tempfile::tempdir().unwrap(); let a = app(tmp.path()); let c = C::login(&a, None);
    let t = c.get("/api/v1/transactions?q=colruyt&limit=1")["items"][0].clone(); let h = finch::privacy::stable_hash(t["tx_key"].as_str().unwrap());
    let rec = c.get("/api/v1/subscriptions?status=all")["series"][0]["id"].as_str().unwrap().to_string();
    db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().execute("INSERT INTO anomalies(id, type, severity, payload, first_seen, last_seen) VALUES ('anm_abc123', 'new_merchant', 'low', '{}', 'x', 'x')", []).unwrap();
    let (s, v) = c.call("POST", "/api/v1/coach/resolve", Some(json!({"refs": [h, rec, "anm_abc123", "anm_ffffff", "chg_ffffff", "h_0000000000", "zz", h]}))); assert_eq!(s, 200, "{v}"); let m = &v["refs"];
    assert_eq!(m[&h], json!({"kind": "transaction", "tx_key": t["tx_key"], "date": t["date"], "amount": t["amount"], "category": t["category"], "merchant": t["entity"]}));
    assert_eq!(m[&rec]["link"], "/subscriptions"); assert_eq!(m["anm_abc123"]["kind"], "anomaly");
    for x in ["anm_ffffff", "chg_ffffff", "h_0000000000", "zz"] { assert!(m.get(x).is_some_and(Value::is_null), "an invented ref resolves to nothing: {x}"); }
    assert_eq!(c.call("POST", "/api/v1/coach/resolve", Some(json!({"refs": []}))).0, 422);
}

#[test]
fn rental_writes_and_indicators() {
    let tmp = tempfile::tempdir().unwrap(); let a = app(tmp.path()); let c = C::login(&a, None); let m = tmp.path().join("memory");
    std::fs::write(m.join("assets.yaml"), "assets:\n  - id: flat\n    kind: real_estate_rental\n    account: Zichtrekening\n    value: 200000\n    as_of: 2025-04-01\n    rent_monthly: 700\n").unwrap();
    std::fs::create_dir_all(m.join("liabilities")).unwrap(); std::fs::write(m.join("liabilities/hyp.yaml"), "id: hyp\nkind: mortgage\nasset: flat\noutstanding: 150000\noutstanding_as_of: 2025-04-01\nrate:\n  type: fixed\n  nominal: 3.5\n").unwrap();
    let assets = || std::fs::read_to_string(m.join("assets.yaml")).unwrap();
    // indicators: no market rate recorded (nothing is looked up), the equity from the declared value and outstanding
    let i = c.get("/api/v1/rental/flat/indicators"); assert_eq!(i["market_rate"]["status"], "missing"); assert_eq!(i["loan_rate"]["missing"], json!(["a market rate you entered"]));
    assert_eq!((i["equity"]["status"].as_str(), i["equity"]["net_equity"].as_str(), i["equity"]["outstanding"].as_str()), (Some("computed"), Some("50000.00"), Some("150000.00"))); assert_eq!(i["disclaimer_key"], "loan");
    assert_eq!(c.call("GET", "/api/v1/rental/nope/indicators", None).0, 404); assert_eq!(c.call("GET", "/api/v1/rental/flat/indicators?market_rate=99", None).0, 422);
    let g = c.get("/api/v1/rental/flat/indicators?market_rate=3.4&market_date=2025-05-01"); assert_eq!((g["loan_rate"]["status"].as_str(), g["market_rate"]["basis"].as_str()), (Some("close_to_market"), Some("given for this call")));
    // a market rate the owner typed in: previewed, then recorded through the memory store
    let before = assets(); let (s, v) = c.call("POST", "/api/v1/rental/flat/market-rate?dry_run=true", Some(json!({"rate_pct": 2.5, "as_of": "2025-03-01", "source": "own quote"}))); assert_eq!((s, v["changed"].as_bool()), (200, Some(true)), "{v}"); assert_eq!(assets(), before, "a dry run writes nothing");
    assert_eq!(c.call("POST", "/api/v1/rental/flat/market-rate", Some(json!({"rate_pct": 30}))).0, 422);
    assert_eq!(c.call("POST", "/api/v1/rental/flat/market-rate", Some(json!({"rate_pct": 2.5, "as_of": "2025-03-01", "source": "own quote"}))).0, 200);
    let i = c.get("/api/v1/rental/flat/indicators"); assert_eq!((i["market_rate"]["basis"].as_str(), i["market_rate"]["age_days"].as_i64(), i["loan_rate"]["status"].as_str(), i["loan_rate"]["gap_pts"].as_f64()), (Some("recorded on the property"), Some(70), Some("above_market"), Some(1.0)));
    assert!(i["market_rate"]["warning"].as_str().unwrap().contains("70 days old")); let sig: Vec<&str> = i["signals"].as_array().unwrap().iter().filter_map(|x| x["id"].as_str()).collect(); assert_eq!(sig, ["rate_above_market", "equity_positive"]);
    assert!(i["loan_rate"]["reading"].as_str().unwrap().ends_with("This is general information, not financial advice."));
    // a vacancy period
    assert_eq!(c.call("POST", "/api/v1/rental/flat/vacancy", Some(json!({"start": "2025-03-01", "end": "2025-02-01"}))).0, 422);
    assert_eq!(c.call("POST", "/api/v1/rental/flat/vacancy", Some(json!({"start": "2025-03-01", "end": "2025-03-31", "note": "works"}))).0, 200);
    let rd = c.get("/api/v1/rental/flat"); assert_eq!(rd["asset"]["vacancies"], json!([{"start": "2025-03-01", "end": "2025-03-31", "note": "works"}])); assert_eq!(rd["asset"]["market_rate"]["rate_pct"], 2.5);
    // the commitment the owner recorded shows, and the extension decision is recorded on it
    assert_eq!(rd["scheme"]["declared"], false); assert_eq!(c.call("POST", "/api/v1/rental/flat/extension", Some(json!({"decision": "extend"}))).0, 422, "an extension needs its years");
    assert_eq!(c.call("PUT", "/api/v1/memory/assets/flat", Some(json!({"fields": {"commitment": {"start_date": "2016-06-01", "years": 9, "rent_cap_monthly": 650}}}))).0, 200);
    let sc = c.get("/api/v1/rental/flat")["scheme"].clone(); assert_eq!((sc["state"].as_str(), sc["end_date"].as_str(), sc["decision_needed"].as_bool()), (Some("active"), Some("2025-05-31"), Some(true)), "{sc}");
    assert_eq!((sc["rent_cap"]["status"].as_str(), sc["rent_cap"]["gap"].as_str()), (Some("above_cap"), Some("50.00")));
    let (s, v) = c.call("POST", "/api/v1/rental/flat/extension", Some(json!({"decision": "extend", "years": 3, "note": "x"}))); assert_eq!(s, 200, "{v}");
    let rd = c.get("/api/v1/rental/flat"); assert_eq!((rd["scheme"]["effective_end_date"].as_str(), rd["scheme"]["extension"]["decision"].as_str(), rd["scheme"]["decision_needed"].as_bool()), (Some("2028-05-31"), Some("extend"), Some(false)));
    assert_eq!(rd["asset"]["commitment"]["extension"]["decided_on"], "2025-05-10"); assert_eq!(c.get("/api/v1/rental/properties")["properties"][0]["scheme"]["state"], "active");
    assert_eq!(c.get("/api/v1/rental/flat/indicators")["commitment"]["effective_end_date"], "2028-05-31");
    assert_eq!(c.call("POST", "/api/v1/rental/nope/vacancy", Some(json!({"start": "2025-03-01"}))).0, 404);
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

/// A fake HTTP transport answering like Ollama: it records every body (no test reaches the network).
#[derive(Clone, Default)]
struct FakeLlm(Arc<std::sync::Mutex<Vec<String>>>);
impl finch::net::Http for FakeLlm {
    fn send(&self, _m: &str, _u: &str, _h: &[(&str, String)], body: Option<&Value>) -> anyhow::Result<finch::net::Response> {
        self.0.lock().unwrap().push(body.map(|b| b.to_string()).unwrap_or_default());
        let content = json!({"results": [{"id": 0, "merchant": "Zephyrbak", "category": "food.groceries", "confidence": 0.912, "recurring_hint": false}]}).to_string();
        Ok(finch::net::Response { status: 200, body: json!({"message": {"content": content}, "prompt_eval_count": 120, "eval_count": 30}).to_string() })
    }
}
/// The Review page's AI suggestion app: a low-confidence LLM label on a card merchant whose key holds a family name and a town.
fn suggest_app(dir: &Path, enable: bool, offline: bool) -> (App, FakeLlm, String) {
    let mut a = app(dir); let con = db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap(); let key = "ZEPHYRBAK TESTMANS GENT".to_string();
    con.execute("UPDATE tx_enriched SET merchant_key=?1, merchant_raw=?1 WHERE merchant_raw LIKE '%COLRUYT%'", [&key]).unwrap();
    classify::llm::upsert_label(&con, &key, "Zephyrbak", "other.uncategorized", Some(0.4), None, "llm", "fake").unwrap();
    std::fs::write(dir.join("memory/household.yaml"), "members:\n  - id: zorro\n    name: Zorro Testmans\n    role: adult\n  - id: kiki\n    name: Kiki Testmans\n    role: child\nplaces:\n  - Gent\n").unwrap();
    let fake = FakeLlm::default(); let f = fake.clone(); a.http = Arc::new(move || Box::new(f.clone()));
    (a.cfg.review_suggest, a.cfg.llm_backend, a.cfg.privacy_offline) = (enable, "ollama".into(), offline); (a, fake, key)
}

#[test]
fn review_ai_suggestion() {
    // off by default: the shape the page reads, web search never, nothing sent
    let tmp = tempfile::tempdir().unwrap(); let (a, fake, key) = suggest_app(tmp.path(), false, false); let c = C::login(&a, None);
    let ai = c.get("/api/v1/review/ai"); assert_eq!((ai["web"]["allowed"].as_bool(), ai["web"]["code"].as_str()), (Some(false), Some("not_ported")), "{ai}");
    assert!(ai["web"]["why"].as_str().unwrap().contains("not ported") && ai["label"]["allowed"] == false && ai["label"]["code"] == "disabled", "{ai}");
    let (s, v) = c.call("POST", "/api/v1/review/suggest", Some(json!({"key": key}))); assert_eq!((s, v["error"]["code"].as_str()), (409, Some("disabled")), "{v}");
    db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap().execute("INSERT INTO ui_users(id, member_id, role, created_at) VALUES ('kiki-login', 'kiki', 'child', '2025-01-01')", []).unwrap();
    let kid = C::login(&a, Some("kiki-login"));
    for (m, p) in [("POST", "/api/v1/review/suggest"), ("GET", "/api/v1/review/ai"), ("GET", "/api/v1/review/suggest/s_x")] { let r = kid.raw(m, p, (m == "POST").then(|| json!({"key": key})), &[]); assert_eq!((r.status, code(&r)), (403, "forbidden_scope".into()), "{p}"); }
    assert!(fake.0.lock().unwrap().is_empty());
    // [privacy] offline refuses the flow (even towards a loopback Ollama)
    let tmp = tempfile::tempdir().unwrap(); let (a, fake, key) = suggest_app(tmp.path(), true, true); let c = C::login(&a, None);
    let ai = c.get("/api/v1/review/ai"); assert_eq!((ai["label"]["allowed"].as_bool(), ai["label"]["code"].as_str()), (Some(false), Some("egress_refused")), "{ai}");
    let (s, v) = c.call("POST", "/api/v1/review/suggest", Some(json!({"key": key}))); assert_eq!((s, v["error"]["code"].as_str()), (409, Some("egress_refused")), "{v}");
    assert!(fake.0.lock().unwrap().is_empty(), "nothing sent offline");
    // enabled: a job, the redacted label only, a proposal that changes nothing
    let tmp = tempfile::tempdir().unwrap(); let (a, fake, key) = suggest_app(tmp.path(), true, false); let c = C::login(&a, None);
    assert_eq!(c.get("/api/v1/review/ai")["label"]["allowed"], true);
    let (s, v) = c.call("POST", "/api/v1/review/suggest", Some(json!({"key": "NOT IN THE QUEUE"}))); assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_in_queue")));
    let (s, j) = c.call("POST", "/api/v1/review/suggest", Some(json!({"key": key, "max_conf": 0.7}))); assert_eq!(s, 200, "{j}");
    let id = j["id"].as_str().unwrap().to_string(); assert!(id.starts_with("s_") && ["running", "done"].contains(&j["state"].as_str().unwrap()));
    let j = c.get(&format!("/api/v1/review/suggest/{id}")); assert_eq!(j["state"], "done", "{j}");
    assert_eq!((j["suggestion"]["category"].as_str(), j["suggestion"]["confidence"].as_f64(), j["suggestion"]["mode"].as_str(), j["mode"].as_str()), (Some("food.groceries"), Some(0.91), Some("label"), Some("label")));
    assert!(j["ai_generated"] == true && j["ai_label_short"] == "AI-generated" && j["suggestion"]["evidence"].is_null());
    let sent = fake.0.lock().unwrap().clone(); assert_eq!(sent.len(), 1); let b = &sent[0];
    assert!(b.contains("ZEPHYRBAK [NAME]") && b.contains("food.groceries") && b.contains(r#"\"avg_amount\":100"#), "{b}");
    for bad in ["TESTMANS", "Testmans", "Zorro", "GENT", "Gent", "4871", "1234", "120.5", "120,5", "DEBETKAART", "BETALING", "PROXIMUS", "QUUXCORP", "Zichtrekening", "KBC"] { assert!(!b.contains(bad), "{bad} leaked: {b}"); }
    let con = db::connect(&a.cfg, db::Opts { insecure: true, ..Default::default() }).unwrap();
    let m: (String, f64, String) = con.query_row("SELECT category, confidence, source FROM merchants WHERE merchant_key=?", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(m, ("other.uncategorized".into(), 0.4, "llm".into()), "a suggestion is not stored");
    assert_eq!(con.query_row("SELECT COUNT(*) FROM llm_usage WHERE purpose='suggest' AND backend='ollama'", [], |r| r.get::<_, i64>(0)).unwrap(), 1, "usage logged");
    let kinds: Vec<String> = con.prepare("SELECT kind FROM egress_journal WHERE outcome='allowed' ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect(); assert_eq!(kinds, ["classify.suggest", "llm.ollama"]);
    assert_eq!(c.call("GET", "/api/v1/review/suggest/s_nope", None).0, 404);
    // applying it stays the user's click on the existing category fix
    assert_eq!(c.call("POST", "/api/v1/review/correct", Some(json!({"key": key, "category": "food.groceries"}))).0, 200);
}
