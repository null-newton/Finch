//! Phase 3 on synthetic data with FAKE transports (no test reaches the network): Enable Banking connect / sync / reconnect, and the
//! LLM classifier (redaction, person guard, kNN, usage and egress journals, privacy modes).
use finch::{classify::{self, llm, rules::Rules}, config, db, egress, ingest::{self, bank}, net::{Http, Response, Runner}};
use serde_json::{json, Value};
use std::cell::RefCell;

#[derive(Default)]
struct FakeHttp { routes: RefCell<Vec<(String, String, Vec<Response>, Option<(u16, String)>)>>, log: RefCell<Vec<(String, String, String, Option<Value>)>> }
impl FakeHttp {
    /// Queue an answer: answers are served in order, the last one again when the queue is empty.
    fn on(&self, method: &str, path: &str, status: u16, body: Value) { let mut r = self.routes.borrow_mut(); match r.iter_mut().find(|x| x.0 == method && x.1 == path) { Some(x) => x.2.push(Response { status, body: body.to_string() }), None => r.push((method.into(), path.into(), vec![Response { status, body: body.to_string() }], None)) } }
    fn sent(&self, needle: &str) -> bool { self.log.borrow().iter().any(|(_, u, h, b)| u.contains(needle) || h.contains(needle) || b.as_ref().is_some_and(|b| b.to_string().contains(needle))) }
}
impl Http for FakeHttp {
    fn send(&self, method: &str, url: &str, headers: &[(&str, String)], body: Option<&Value>) -> anyhow::Result<Response> {
        self.log.borrow_mut().push((method.into(), url.into(), format!("{headers:?}"), body.cloned()));
        let path = url::Url::parse(url)?.path().to_string();
        let mut r = self.routes.borrow_mut(); let x = r.iter_mut().find(|x| x.0 == method && x.1 == path).unwrap_or_else(|| panic!("unexpected call {method} {path}"));
        if !x.2.is_empty() { let a = x.2.remove(0); x.3 = Some((a.status, a.body.clone())); return Ok(a); }
        let (status, body) = x.3.clone().unwrap(); Ok(Response { status, body })
    }
}
struct FakeClaude { answer: Value, seen: RefCell<Vec<(Vec<String>, String, Vec<(String, String)>)>> }
impl Runner for FakeClaude {
    fn run(&self, argv: &[String], stdin: &str, env: &[(String, String)], _t: u64) -> anyhow::Result<(i32, String, String)> { self.seen.borrow_mut().push((argv.to_vec(), stdin.into(), env.to_vec())); Ok((0, json!({"structured_output": self.answer, "usage": {"input_tokens": 1200, "output_tokens": 80}, "total_cost_usd": 0.004}).to_string(), String::new())) }
}

fn home(extra: &str) -> (tempfile::TempDir, config::Config, rusqlite::Connection) {
    let tmp = tempfile::tempdir().unwrap(); std::fs::create_dir_all(tmp.path().join("memory")).unwrap();
    std::fs::write(tmp.path().join("config.toml"), format!("insecure_plaintext_db = true\n[enable_banking]\napp_id = \"test-app\"\nredirect_url = \"https://localhost:8443/callback\"\nprivate_key_path = \"unused.pem\"\n{extra}")).unwrap();
    std::fs::write(tmp.path().join("memory/household.yaml"), "members:\n  - id: zorro\n    name: Zorro Testmans\n    role: adult\n").unwrap();
    let cfg = config::load_config(Some(&tmp.path().join("config.toml"))).unwrap();
    let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap(); (tmp, cfg, con)
}
fn client<'a>(cfg: &'a config::Config, con: &'a rusqlite::Connection, http: &'a FakeHttp) -> bank::Client<'a> { bank::Client { app_id: "test-app".into(), key_pem: vec![], api_url: "https://api.enablebanking.com".into(), http, cfg, con, token_override: Some("test-token".into()) } }
fn tx(rf: &str, date: &str, amt: f64, dbit: bool, text: &str, status: &str) -> Value { json!({"entry_reference": rf, "booking_date": date, "value_date": date, "transaction_amount": {"amount": amt.to_string(), "currency": "EUR"}, "credit_debit_indicator": if dbit { "DBIT" } else { "CRDT" }, "remittance_information": [text], "status": status}) }

#[test]
fn bank_connect_sync_reconnect() {
    let (_t, cfg, con) = home(""); let http = FakeHttp::default(); let c = client(&cfg, &con, &http);
    http.on("POST", "/auth", 200, json!({"url": "https://bank.example/login?x=1"}));
    let (state, url) = bank::start_auth(&c, "KBC", "BE", 180, None, false).unwrap(); assert!(url.starts_with("https://bank.example"));
    assert!(bank::parse_finish_arg(&con, "https://localhost:8443/callback?code=abc&state=nope").is_err(), "a state this tool did not start is refused");
    let (code, st) = bank::parse_finish_arg(&con, &format!("  'https://localhost:8443/callback\\?code=abc\\&state={state}'  ")).unwrap(); assert_eq!((code.as_str(), st.as_deref()), ("abc", Some(state.as_str())));
    let acct = json!({"uid": "u1", "account_id": {"iban": "BE68539007547034"}, "name": "Zichtrekening", "currency": "EUR", "cash_account_type": "CACC"});
    http.on("POST", "/sessions", 200, json!({"session_id": "s1", "aspsp": {"name": "KBC", "country": "BE"}, "access": {"valid_until": "2099-01-01T00:00:00+00:00"}, "accounts": [acct]}));
    let info = bank::complete_auth(&c, &code, st.as_deref()).unwrap(); assert_eq!(info["accounts"][0]["new"], true);
    assert!(bank::complete_auth(&c, &code, st.as_deref()).is_err(), "a code is completed once");
    assert!(bank::start_auth(&c, "KBC", "BE", 180, None, false).unwrap_err().to_string().contains("already has an active consent"));

    http.on("GET", "/sessions/s1", 200, json!({"status": "AUTHORIZED"}));
    http.on("GET", "/accounts/u1/transactions", 200, json!({"transactions": [tx("R1", "2025-03-01", 25.5, true, "BETALING MET DEBETKAART COLRUYT GENT", "BOOK"), tx("R2", "2025-03-02", 2500.0, false, "OVERSCHRIJVING VAN ACME NV LOON", "BOOK")], "continuation_key": "p2"}));
    http.on("GET", "/accounts/u1/transactions", 200, json!({"transactions": [tx("R3", "2025-03-03", 9.0, true, "BETALING VIA PAYCONIQ FRITUUR", "BOOK"), tx("P1", "2025-03-04", 40.0, true, "PENDING SHOP", "PDNG"), tx("R1", "2025-03-01", 25.5, true, "BETALING MET DEBETKAART COLRUYT GENT", "BOOK")]}));
    http.on("GET", "/accounts/u1/balances", 200, json!({"balances": [{"balance_type": "CLBD", "balance_amount": {"amount": "1234.56", "currency": "EUR"}, "reference_date": "2025-03-04"}]}));
    let r = bank::sync_all(&c, None, false, false).unwrap(); let ok = r.iter().find(|x| x["status"] == "ok").unwrap();
    assert_eq!((ok["pages"].as_i64(), ok["new"].as_i64(), ok["pending"].as_i64()), (Some(2), Some(3), Some(1)), "{r:?}");
    assert!(http.sent("continuation_key=p2") && http.sent("strategy=longest"));
    let n = |sql: &str| con.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!((n("SELECT COUNT(*) FROM transactions"), n("SELECT COUNT(*) FROM pending_transactions"), n("SELECT COUNT(*) FROM balances")), (3, 1, 1));
    // a second sync: same data plus an enriched description of R2 -> nothing new, the text is refreshed
    http.on("GET", "/accounts/u1/transactions", 200, json!({"transactions": [tx("R2", "2025-03-02", 2500.0, false, "OVERSCHRIJVING VAN ACME NV LOON MAART", "BOOK")]}));
    let r = bank::sync_account(&c, "u1", false, true).unwrap(); assert_eq!((r["new"].as_i64(), r["refreshed"].as_i64()), (Some(0), Some(1)), "{r}");
    assert!(http.sent("date_from=2025-02"), "an incremental sync overlaps a week");
    http.on("GET", "/accounts/u1/transactions", 429, json!({"error": "ASPSP_RATE_LIMIT_EXCEEDED"}));
    assert_eq!(bank::sync_account(&c, "u1", false, true).unwrap()["status"], "failed");
    assert_eq!(n("SELECT COUNT(*) FROM sync_log WHERE ok=0"), 1); assert_eq!(n("SELECT COUNT(*) FROM transactions"), 3, "a failed sync writes nothing");
    // the egress journal: host and purpose only, never a path, a query or an id
    let j = egress::journal(&con, 50).unwrap(); assert!(j.iter().all(|x| x.1 == "enable_banking" && x.2 == "api.enablebanking.com" && !x.4.contains('/')));
    assert!(!format!("{j:?}").contains("u1") && !format!("{j:?}").contains("s1"));

    // reconnect: the bank returns a NEW uid for the same IBAN -> the stable record continues, the old session is retired
    http.on("POST", "/auth", 200, json!({"url": "https://bank.example/login?x=2"}));
    let (st2, _) = bank::start_auth(&c, "KBC", "BE", 180, Some("s1"), true).unwrap();
    http.on("POST", "/sessions", 200, json!({"session_id": "s2", "aspsp": {"name": "KBC", "country": "BE"}, "access": {"valid_until": "2099-06-01T00:00:00+00:00"}, "accounts": [{"uid": "u9", "account_id": {"iban": "BE68 5390 0754 7034"}, "name": "Zichtrekening", "currency": "EUR", "cash_account_type": "CACC"}]}));
    let info = bank::complete_auth(&c, "def", Some(&st2)).unwrap();
    assert_eq!((info["accounts"][0]["uid"].as_str(), info["accounts"][0]["remapped"].as_bool()), (Some("u1"), Some(true)), "{info}"); assert_eq!(info["retired"], json!(["s1"]));
    let cs = bank::list_consents(&con, false).unwrap(); assert_eq!(cs.len(), 1); assert_eq!(cs[0].session_id, "s2");

    // offline: nothing outbound at all
    let mut off = cfg.clone(); off.privacy_offline = true; let c2 = client(&off, &con, &http);
    assert!(bank::start_auth(&c2, "Belfius", "BE", 90, None, false).unwrap_err().to_string().contains("egress refused"));
}

fn classify_home(extra: &str) -> (tempfile::TempDir, config::Config, rusqlite::Connection) {
    let (t, cfg, con) = home(extra);
    let csv = "Datum;Omschrijving;Bedrag\n02/01/2025;BETALING MET DEBETKAART FRITUUR DE MOLEN;-9,00\n05/01/2025;BETALING MET DEBETKAART STREAMFLIX.BE;-12,99\n06/01/2025;OVERSCHRIJVING NAAR PEETERS LOTTE;-50,00\n07/01/2025;EUROPESE DOMICILIERING SCHULDEISER : TESTMANS ZORRO;-30,00\n08/01/2025;BETALING MET DEBETKAART COLRUYT GENT;-60,00\n09/01/2025;BETALING MET DEBETKAART STREAMFLIX.COM;-12,99\n";
    let f = t.path().join("x.csv"); std::fs::write(&f, csv).unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    ingest::import_file(&con, &f, ingest::ImportOpts { account: "new:Zicht", profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap();
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap(); (t, cfg, con)
}

#[test]
fn llm_classify_redacts_holds_back_and_stores() {
    let (_t, cfg, con) = classify_home("[llm]\nbackend = \"anthropic-api\"\n[classify]\nknn_threshold = 0.6\n");
    let rules = Rules::load(&cfg).unwrap();
    // user labels: the near-duplicate STREAMFLIX COM is then labelled by similarity, without the LLM
    llm::correct(&con, &rules, "COLRUYT GENT", "food.groceries", Some("Colruyt")).unwrap();
    llm::correct(&con, &rules, "STREAMFLIX BE", "subscriptions.video_streaming", Some("Streamflix")).unwrap();
    let (c, held) = llm::candidates(&con, &cfg, &rules, false).unwrap();
    let ck: Vec<&str> = c.iter().map(|x| x.key.as_str()).collect(); let hk: Vec<&str> = held.iter().map(|x| x.key.as_str()).collect();
    assert!(ck.contains(&"FRITUUR DE MOLEN") && ck.contains(&"STREAMFLIX COM"), "{ck:?}"); assert!(hk.contains(&"TESTMANS ZORRO"), "the household's own name is never sent: {hk:?} / {ck:?}");
    assert!(!ck.contains(&"PEETERS LOTTE"), "a person transfer is never a candidate");
    let dry = llm::run(&con, &cfg, &rules, None, &llm::RunOpts { dry_run: true, refresh: false, limit: None, batch: 40, model: None, knn: true }).unwrap();
    let p = dry.dry_payload.unwrap().to_string(); assert!(!p.contains("TESTMANS") && !p.contains("PEETERS") && !p.contains("Zorro"), "{p}");
    assert_eq!(con.query_row("SELECT COUNT(*) FROM merchants", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "a dry run writes nothing");

    let http = FakeHttp::default(); std::env::set_var("ANTHROPIC_API_KEY", "test-key-not-real");
    let answer = json!({"results": [{"id": 0, "merchant": "Frituur", "category": "food.fast_food", "confidence": 0.8, "recurring_hint": false}, {"id": 1, "merchant": "Streamflix", "category": "subscriptions.video_streaming", "confidence": 0.9, "recurring_hint": true}]});
    http.on("POST", "/v1/messages", 200, json!({"content": [{"type": "text", "text": answer.to_string()}], "stop_reason": "end_turn", "usage": {"input_tokens": 1500, "output_tokens": 90}}));
    let be = llm::Backend { name: "anthropic-api".into(), cfg: &cfg, con: &con, t: llm::Transport::Http(&http) };
    let r = llm::run(&con, &cfg, &rules, Some(&be), &llm::RunOpts { dry_run: false, refresh: false, limit: None, batch: 40, model: None, knn: true }).unwrap();
    assert_eq!(r.knn, 1, "STREAMFLIX COM by similarity"); assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert!(r.labelled >= 1 && r.cost > 0.0, "{r:?}");
    assert!(!http.sent("TESTMANS") && !http.sent("PEETERS") && http.sent("x-api-key"));
    let cat = |k: &str| con.query_row("SELECT category || '/' || source FROM merchants WHERE merchant_key=?", [k], |r| r.get::<_, String>(0)).unwrap();
    assert_eq!(cat("COLRUYT GENT"), "food.groceries/user"); assert_eq!(cat("STREAMFLIX COM"), "subscriptions.video_streaming/knn"); assert_eq!(cat("FRITUUR DE MOLEN"), "food.fast_food/llm");
    assert_eq!(con.query_row("SELECT COUNT(*) FROM llm_usage", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert!(egress::journal(&con, 10).unwrap().iter().any(|x| x.1 == "llm.anthropic-api" && x.4 == "classify.label" && x.5 == "allowed"));
    // the user's own label always wins over a later automatic one
    llm::upsert_label(&con, "COLRUYT GENT", "x", "shopping.clothing", Some(0.9), None, "llm", "m").unwrap(); assert_eq!(cat("COLRUYT GENT"), "food.groceries/user");

    // local_only: a cloud backend is refused before anything is sent (and the refusal is journalled)
    let mut lo = cfg.clone(); lo.privacy_local_only = true; let be2 = llm::Backend { name: "anthropic-api".into(), cfg: &lo, con: &con, t: llm::Transport::Http(&http) };
    let n0 = http.log.borrow().len();
    let r = llm::run(&con, &lo, &rules, Some(&be2), &llm::RunOpts { dry_run: false, refresh: true, limit: None, batch: 40, model: None, knn: false }).unwrap();
    assert!(!r.failed.is_empty() && r.failed[0].contains("egress refused")); assert_eq!(http.log.borrow().len(), n0, "nothing was sent");
}

#[test]
fn claude_code_backend_is_isolated() {
    let (_t, cfg, con) = classify_home(""); let rules = Rules::load(&cfg).unwrap();
    std::env::set_var("COACH_DB_KEY", "must-not-reach-claude");
    let fake = FakeClaude { answer: json!({"results": [{"id": 0, "merchant": "Frituur", "category": "food.fast_food", "confidence": 0.7, "recurring_hint": false}]}), seen: Default::default() };
    let be = llm::Backend { name: "claude-code".into(), cfg: &cfg, con: &con, t: llm::Transport::Run(&fake) };
    let r = llm::run(&con, &cfg, &rules, Some(&be), &llm::RunOpts { dry_run: false, refresh: false, limit: Some(1), batch: 40, model: None, knn: false }).unwrap();
    assert_eq!(r.labelled, 1, "{r:?}");
    let seen = fake.seen.borrow(); let (argv, stdin, env) = &seen[0];
    assert!(argv.windows(2).any(|w| w[0] == "--tools" && w[1].is_empty()) && argv.iter().any(|a| a.contains("WebFetch")) && argv.contains(&"--disable-slash-commands".to_string()));
    assert!(!env.iter().any(|(k, _)| k == "COACH_DB_KEY" || k == "ANTHROPIC_API_KEY"), "no secret in the claude environment");
    assert!(!stdin.contains("TESTMANS"));
}
