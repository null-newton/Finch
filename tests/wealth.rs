//! Phase 5c on synthetic data: net worth, a Belgian mortgage schedule and its early-repayment cap, the subscriptions inventory with
//! Belgian cancellation rules, a rental property with rent indexation, the web endpoints, and no lender / description / contract
//! number in what a model sees.
use finch::{classify, config, db, ingest, mcp, web::{App, Req}};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

fn setup(dir: &Path) -> config::Config {
    let m = dir.join("memory"); std::fs::create_dir_all(m.join("liabilities")).unwrap(); std::fs::create_dir_all(m.join("contracts")).unwrap();
    std::fs::write(dir.join("config.toml"), "insecure_plaintext_db = true\n").unwrap();
    std::fs::write(m.join("assets.yaml"), "assets:\n  - id: huis-kerkstraat\n    kind: real_estate\n    description: Huis Kerkstraat 12\n    value: 350000\n    as_of: 2025-03-01\n  - id: studio-zeedijk\n    kind: real_estate_rental\n    description: Studio Zeedijk\n    value: 180000\n    as_of: 2025-03-01\n    account: Verhuur\n    lease: {start_date: 2023-09-01, base_rent: 650, base_index: 125.0, current_index: 131.25}\n  - id: auto\n    kind: vehicle\n").unwrap();
    std::fs::write(m.join("liabilities/hypotheek.yaml"), "id: hypotheek\nkind: mortgage\nlender: Testbank Hypotheek NV\nprincipal: 200000\nrate: {type: fixed, nominal: 3}\nstart_date: 2024-01-10\nterm_months: 240\nmonthly_payment: 1105.15\npayment_match: TESTBANK\n").unwrap();
    std::fs::write(m.join("contracts/gsm.yaml"), "id: gsm\nprovider: Telcodemo\nkind: telecom\ncontract_number: ABC-123-SECRET\nmerchant_match: TELCODEMO\nstart_date: 2025-02-15\n").unwrap();
    let cfg = config::load_config(Some(&dir.join("config.toml"))).unwrap(); let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    let imp = |name: &str, rows: Vec<String>, acc: &str| { let f = dir.join(name); std::fs::write(&f, format!("Datum;Omschrijving;Bedrag\n{}\n", rows.join("\n"))).unwrap(); ingest::import_file(&con, &f, ingest::ImportOpts { account: acc, profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap(); };
    let mut main = vec![]; let mut rent = vec![];
    for mo in 1..=4 { main.push(format!("10/{mo:02}/2025;EUROPESE DOMICILIERING SCHULDEISER : TESTBANK HYPOTHEEK NV MANDAAT NUMMER : HY1;-1105,15")); main.push(format!("05/{mo:02}/2025;EUROPESE DOMICILIERING SCHULDEISER : TELCODEMO NV MANDAAT NUMMER : TC1;-35,00"));
        if mo != 3 { rent.push(format!("02/{mo:02}/2025;OVERSCHRIJVING VAN HUURDER DEMO HUUR STUDIO;682,50")); } rent.push(format!("20/{mo:02}/2025;EUROPESE DOMICILIERING SCHULDEISER : SYNDIC DEMO MANDAAT NUMMER : SY1;-60,00")); }
    imp("main.csv", main, "new:Zicht"); imp("rent.csv", rent, "new:Verhuur");
    con.execute("UPDATE accounts SET purpose='rental' WHERE uid='imp-verhuur'", []).unwrap();
    con.execute("INSERT INTO balances(account_uid, fetched_at, balance_type, amount, currency) VALUES ('imp-zicht', '2025-05-01', 'CLBD', 5000, 'EUR')", []).unwrap();
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap(); cfg
}
fn today() -> Option<chrono::NaiveDate> { chrono::NaiveDate::from_ymd_opt(2025, 5, 10) }

#[test]
fn net_worth_loans_subs_rental_and_redaction() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path());
    let st = tmp.path().join("static"); std::fs::create_dir_all(&st).unwrap(); let mut a = App::new(cfg.clone(), true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = today();
    let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
    let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(None).unwrap()}).to_string().into_bytes()));
    let ck = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let get = |p: &str| -> Value { let r = a.handle(Req::new("GET", p, vec![("Host".into(), "127.0.0.1:8765".into()), ("Cookie".into(), format!("{}={ck}", a.sec.cookie_name))], vec![])); assert_eq!(r.status, 200, "{p}: {}", String::from_utf8_lossy(&r.body)); serde_json::from_slice(&r.body).unwrap() };
    // the mortgage: 16 instalments made by 2025-05-10, capital from the schedule; the declared instalment matches the Belgian convention
    let l = get("/api/v1/loans/hypotheek"); assert_eq!(l["schedule"]["payments_made"], 16); assert_eq!(l["schedule"]["payment"], "1105.15"); assert_eq!(l["payments"]["count"], 4);
    let lv = get("/api/v1/liabilities"); assert_eq!(lv["liabilities"][0]["schedule"]["payment_check"]["status"], "matches");
    let rem = l["schedule"]["remaining_capital"].as_str().unwrap().to_string();
    let e = get("/api/v1/loans/hypotheek/early-repayment?amount=10000"); assert_eq!(e["max_indemnity"], "75.00");
    // net worth: bank 5000 + house 350000 + studio 180000 - the mortgage; the car has no value: unknown, not zero
    let nw = get("/api/v1/net-worth"); assert_eq!(nw["complete"], false); assert_eq!(nw["liabilities"]["total"], rem.as_str());
    let expect = 535_000.0 - rem.parse::<f64>().unwrap() + nw["bank"]["accounts"].as_array().unwrap().iter().filter(|x| x["uid"] == "imp-verhuur").filter_map(|x| x["balance"].as_str()).map(|s| s.parse::<f64>().unwrap()).sum::<f64>();
    assert_eq!(nw["net_worth"].as_str().unwrap().parse::<f64>().unwrap(), (expect * 100.0).round() / 100.0); assert!(nw["unknown"].as_array().unwrap().iter().any(|u| u["id"] == "auto"));
    // subscriptions: the telecom series carries its contract and the Belgian 6-month rule
    let inv = get("/api/v1/subs/inventory"); let tel = inv["rows"].as_array().unwrap().iter().find(|r| r["contract_id"] == "gsm").expect("telecom row linked to its contract");
    assert_eq!(tel["cancellation"]["family"], "telecom"); assert_eq!(tel["cancellation"]["can_cancel_now"], true); assert_eq!(tel["cancellation"]["early_termination_cost"]["free_exit_date"], "2025-08-15");
    assert!(!inv["rows"].as_array().unwrap().iter().any(|r| r["name"].as_str().unwrap_or("").contains("TESTBANK")), "a loan instalment is not a subscription");
    // rental: March rent missing, the indexed rent 682.50 is the expected one
    let rl = get("/api/v1/rental/properties"); assert_eq!(rl["properties"][0]["account_link"], "linked"); assert_eq!(rl["properties"][0]["expected_rent"], "682.50");
    let rd = get("/api/v1/rental/studio-zeedijk"); assert_eq!(rd["indexation"]["indexed_rent"], "682.50"); let mar = rd["cashflow"]["months"].as_array().unwrap().iter().find(|m| m["month"] == "2025-03").unwrap();
    assert_eq!((mar["rent_status"].as_str(), mar["charges"].as_str()), (Some("missing"), Some("60.00"))); assert_eq!(rd["cashflow"]["vacancy"]["missing_months"], json!(["2025-03"]));
    assert_eq!(get("/api/v1/rental/studio-zeedijk/tax")["status"], "not_applicable");
    // MCP: kinds and refs only
    let con = db::connect(&cfg, db::Opts { insecure: true, ..Default::default() }).unwrap(); let mut s = mcp::Session::with(cfg, con, today());
    let outs = [s.call("net_worth", &json!({})), s.call("loans_overview", &json!({"prepayment": 10000})), s.call("rental_overview", &json!({})), s.call("subscriptions_inventory", &json!({})), s.call("cancellability", &json!({"ref": "gsm"}))];
    for o in &outs { assert!(o.ok, "{}", o.text); for leak in ["Testbank Hypotheek", "Kerkstraat", "Zeedijk", "kerkstraat", "zeedijk", "ABC-123", "hypotheek.yaml", "HUURDER"] { assert!(!o.text.contains(leak), "{leak} leaked: {}", o.text); } }
    assert_eq!(outs[1].data["loans"][0]["ref"], "loan-1"); assert_eq!(outs[1].data["loans"][0]["early_repayment"]["max_indemnity"], "75.00");
    assert!(!s.call("loans_overview", &json!({"loan": "hypotheek"})).ok, "a real id is refused");
}
