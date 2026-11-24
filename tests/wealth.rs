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

#[test]
fn web_edits_assets_loans_contracts() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let mem = cfg.memory_dir.clone();
    let st = tmp.path().join("static"); std::fs::create_dir_all(&st).unwrap(); let mut a = App::new(cfg, true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = today();
    let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
    let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(None).unwrap()}).to_string().into_bytes()));
    let ck = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let hh = |x: Vec<(String, String)>| { let mut v = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Cookie".into(), format!("{}={ck}", a.sec.cookie_name))]; v.extend(x); v };
    let csrf: String = serde_json::from_slice::<Value>(&a.handle(Req::new("GET", "/api/v1/session", hh(vec![]), vec![])).body).unwrap()["csrf_token"].as_str().unwrap().into();
    let call = |m: &str, p: &str, b: Value| -> (u16, Value) { let r = a.handle(Req::new(m, p, hh(vec![("Content-Type".into(), "application/json".into()), ("X-CSRF-Token".into(), csrf.clone())]), b.to_string().into_bytes())); (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null)) };
    let (s, v) = call("PUT", "/api/v1/memory/assets/auto", json!({"fields": {"value": 9000, "as_of": "2025-05-01"}})); assert_eq!(s, 200, "{v}");
    assert!(std::fs::read_to_string(mem.join("assets.yaml")).unwrap().contains("  - id: auto\n    kind: vehicle\n    value: 9000\n    as_of: 2025-05-01\n"));
    assert_eq!(call("PUT", "/api/v1/memory/assets/boat", json!({"fields": {"kind": "yacht"}})).0, 422, "an unknown asset kind is refused");
    assert_eq!(call("PUT", "/api/v1/memory/liabilities/lease", json!({"fields": {"lender": "x"}})).0, 422, "a new liability needs a kind");
    let (s, v) = call("PUT", "/api/v1/memory/liabilities/lease", json!({"fields": {"kind": "loa", "monthly_payment": 299, "end_date": "2027-06-30", "rate": {"nominal": 4.5}}})); assert_eq!(s, 200, "{v}");
    let lf = std::fs::read_to_string(mem.join("liabilities/lease.yaml")).unwrap(); assert!(lf.starts_with("id: lease\nkind: loa\n") && lf.contains("rate:\n  type:\n  nominal: 4.5\n"), "{lf}");
    assert_eq!(call("POST", "/api/v1/loans/lease/odometer", json!({"date": "2025-05-01", "km": 12000})).0, 200);
    assert_eq!(call("POST", "/api/v1/loans/lease/odometer", json!({"date": "2025-05-09", "km": 11000})).0, 422, "an odometer never goes down");
    let (s, v) = call("PUT", "/api/v1/subs/contracts/gsm/usage", json!({"frequency": "daily"})); assert_eq!(s, 200, "{v}");
    assert!(std::fs::read_to_string(mem.join("contracts/gsm.yaml")).unwrap().contains("usage:\n  frequency: daily\n"));
    std::fs::write(mem.join("liabilities/hypotheek.yaml"), format!("{}asset: huis-kerkstraat\n", std::fs::read_to_string(mem.join("liabilities/hypotheek.yaml")).unwrap())).unwrap();
    assert_eq!(call("POST", "/api/v1/memory/assets/huis-kerkstraat/delete", json!({})).0, 409, "an asset a loan points to stays");
    assert_eq!(call("POST", "/api/v1/memory/assets/auto/delete", json!({})).0, 200);
}

#[test]
fn generated_and_proposed_questions() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let con = db::connect(&cfg, db::Opts { insecure: true, ..Default::default() }).unwrap();
    let ds = finch::analytics::Dataset::load(&con, &cfg, today()).unwrap(); let rec = finch::analytics::recurring::detect(&ds, &finch::analytics::Scope::default(), true); let st = finch::memstore::Store::new(&cfg, "cli").unwrap();
    let qs = finch::qgen::generate(&st, &con, &cfg, &ds, &rec, 300.0, 2000.0).unwrap(); let keys: Vec<&str> = qs.iter().filter_map(|q| q["key"].as_str()).collect();
    assert!(keys.contains(&"fill:asset:auto"), "the car has no value: {keys:?}"); assert!(keys.iter().any(|k| k.starts_with("acct:")), "{keys:?}");
    assert!(!keys.iter().any(|k| k.contains("TESTBANK")), "a loan instalment is covered by its loan file: {keys:?}");
    finch::qgen::add(&st, &qs, false).unwrap(); assert!(finch::qgen::generate(&st, &con, &cfg, &ds, &rec, 300.0, 2000.0).unwrap().is_empty(), "never asked twice");
    assert!(std::fs::read_to_string(cfg.memory_dir.join("open-questions.md")).unwrap().contains("Asset auto (vehicle) has no value"));
    let mut s = mcp::Session::with(cfg.clone(), con, today()); let sid = s.call("subscriptions_inventory", &json!({})).data["rows"].as_array().unwrap().iter().find(|r| r["ref"].as_str().unwrap_or("").starts_with("rec_")).unwrap()["ref"].as_str().unwrap().to_string();
    let p = s.call("questions_propose", &json!({"series": [sid], "liabilities": ["loan-1"], "reason": "the user wants to review"})); assert!(p.ok, "{}", p.text); assert!(p.data["questions"].as_u64().unwrap() >= 1);
    assert!(!s.call("questions_propose", &json!({"liabilities": ["hypotheek"]})).ok, "a real id is refused");
}

#[test]
fn loan_scenarios_history_and_inference() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let con = db::connect(&cfg, db::Opts { insecure: true, ..Default::default() }).unwrap();
    let ds = finch::analytics::Dataset::load(&con, &cfg, today()).unwrap(); let (ls, _) = finch::loans::liabilities(&cfg.memory_dir); let l = &ls[0]; let s = finch::loans::compute(l, today().unwrap());
    let p = finch::loans_extra::prepay(l, &s, today().unwrap(), 10000.0, None, None).unwrap(); assert_eq!(p["status"], "computed"); assert_eq!(p["penalty"], "75.00", "3 months of interest at 3 % on 10 000");
    let (k1, k2) = (&p["options"][0], &p["options"][1]); assert!(k1["months_saved"].as_u64().unwrap() > 0 && k1["new_instalment"] == "1105.15"); assert!(k2["new_instalment"].as_str().unwrap().parse::<f64>().unwrap() < 1105.15);
    assert!(k1["interest_saved"].as_str().unwrap().parse::<f64>().unwrap() > k2["interest_saved"].as_str().unwrap().parse::<f64>().unwrap(), "finishing earlier saves more interest");
    let r = finch::loans_extra::renegotiate(l, &s, today().unwrap(), 2.0, Some("internal"), 500.0, 0.0, 0.0, None).unwrap(); assert!(r["monthly_saving"].as_str().unwrap().parse::<f64>().unwrap() > 0.0); assert_eq!(r["penalty"], "0.00");
    assert_eq!(finch::loans_extra::insurance(&s, 20.0, 0.0).unwrap()["status"], "needs_fields", "no insurance recorded: nothing is guessed");
    finch::loans_extra::snapshot(&con, &ds, 3).unwrap(); let h = finch::loans_extra::history(&con, 12).unwrap(); assert_eq!(h.len(), 1); assert_eq!(h[0]["source"], "snapshot");
    let inf = finch::loans_extra::infer(&ds, l); assert_eq!(inf["status"], "inferred"); assert!(inf["fields"].as_array().unwrap().iter().any(|f| f["field"] == "payment_day" && f["value"] == 10));
}

#[test]
fn subscription_alternatives_decisions_letters_drafts() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let mem = cfg.memory_dir.clone();
    let st = tmp.path().join("static"); std::fs::create_dir_all(&st).unwrap(); let mut a = App::new(cfg.clone(), true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = today();
    let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
    let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(None).unwrap()}).to_string().into_bytes()));
    let ck = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let hh = |x: Vec<(String, String)>| { let mut v = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Cookie".into(), format!("{}={ck}", a.sec.cookie_name))]; v.extend(x); v };
    let csrf: String = serde_json::from_slice::<Value>(&a.handle(Req::new("GET", "/api/v1/session", hh(vec![]), vec![])).body).unwrap()["csrf_token"].as_str().unwrap().into();
    let call = |m: &str, p: &str, b: Value| -> (u16, Value) { let r = a.handle(Req::new(m, p, hh(vec![("Content-Type".into(), "application/json".into()), ("X-CSRF-Token".into(), csrf.clone())]), if b.is_null() { vec![] } else { b.to_string().into_bytes() })); (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null)) };
    let inv = || call("GET", "/api/v1/subs/inventory", Value::Null).1; let v = inv(); let rows = v["rows"].as_array().unwrap();
    let tel = rows.iter().find(|r| r["contract_id"] == "gsm").unwrap()["ref"].as_str().unwrap().to_string(); assert!(tel.starts_with("rec_"), "{tel}");
    // alternatives: https only, finch computes the savings; an offer seen more than 30 days ago is outdated
    assert_eq!(call("POST", "/api/v1/subs/alternatives", json!({"ref": tel, "provider": "Demotel", "offer_name": "Basis", "monthly_price": 20, "source_url": "http://x.test/a", "retrieved_at": "2025-05-01", "method": "manual"})).0, 422);
    let (s, x) = call("POST", "/api/v1/subs/alternatives", json!({"ref": tel, "provider": "Demotel", "offer_name": "Basis", "monthly_price": "20,00", "source_url": "https://x.test/a", "retrieved_at": "2025-05-01", "method": "manual"})); assert_eq!(s, 200, "{x}");
    assert_eq!(call("POST", "/api/v1/subs/alternatives", json!({"ref": tel, "provider": "Oldtel", "offer_name": "Mini", "monthly_price": 10, "retrieved_at": "2025-03-01"})).0, 200);
    let row = inv()["rows"].as_array().unwrap().iter().find(|r| r["ref"] == tel.as_str()).cloned().unwrap(); let al = &row["alternatives"];
    assert_eq!((al["count"].as_u64(), al["outdated"].as_u64()), (Some(2), Some(1)), "{al}"); assert_eq!(al["best"]["savings"]["monthly"], "15.00"); assert_eq!(al["best"]["savings"]["net_12m"], "180.00");
    let old = al["items"].as_array().unwrap().iter().find(|i| i["provider"] == "Oldtel").unwrap()["id"].as_str().unwrap().to_string(); assert_eq!(call("DELETE", &format!("/api/v1/subs/alternatives/{old}"), Value::Null).0, 200);
    // a decision: dry run, then a cancellation the bank data contradicts (still charged after the effective date)
    let (_, d) = call("POST", "/api/v1/subs/decisions?dry_run=true", json!({"ref": tel, "decision": "cancelled", "before_monthly": 35, "after_monthly": 0})); assert_eq!((d["valid"].as_bool(), d["monthly_saving"].as_f64()), (Some(true), Some(35.0)), "{d}");
    assert_eq!(call("POST", "/api/v1/subs/decisions", json!({"ref": tel, "decision": "sold", "before_monthly": 35})).0, 422);
    let (s, d) = call("POST", "/api/v1/subs/decisions", json!({"ref": tel, "decision": "cancelled", "before_monthly": 35, "after_monthly": 0, "decided_on": "2025-01-20", "effective_on": "2025-02-01"})); assert_eq!(s, 200, "{d}");
    let sv = call("GET", "/api/v1/subs/savings", Value::Null).1; assert_eq!(sv["contradicted"], 1, "{sv}"); assert_eq!(sv["realised_monthly"], "0.00"); assert_eq!(sv["reminders"].as_array().unwrap().len(), 1);
    assert_eq!(inv()["totals"]["reminders"], 1);
    // the coach only proposes: a decision waits for the user, a contract draft is a sealed proposal (nothing written)
    let con = db::connect(&cfg, db::Opts { insecure: true, ..Default::default() }).unwrap(); let mut s = mcp::Session::with(cfg.clone(), con, today());
    let p = s.call("decision_propose", &json!({"ref": tel, "decision": "downgraded", "after_monthly": 25})); assert!(p.ok, "{}", p.text); let did = p.data["decision_id"].as_str().unwrap().to_string();
    let t = s.call("savings_tracker", &json!({})); assert!(t.ok); assert_eq!(t.data["proposed"].as_array().unwrap().len(), 1);
    let ar = s.call("alternatives_record", &json!({"ref": tel, "provider": "Webtel", "offer_name": "Flex", "monthly_price": 30, "source_url": "https://x.test/b", "retrieved_at": "2025-05-09"})); assert!(ar.ok, "{}", ar.text); assert_eq!(ar.data["savings"]["monthly"], "5.00");
    assert!(!s.call("alternatives_record", &json!({"ref": tel, "provider": "Webtel", "offer_name": "Flex", "monthly_price": 30, "source_url": "https://x.test/b", "retrieved_at": "2099-01-01"})).ok, "a price seen in the future is refused");
    let free = inv()["rows"].as_array().unwrap().iter().find(|r| r["contract"]["status"] == "missing" && r["series_id"].is_string()).cloned().expect("a series without contract"); let fid = free["ref"].as_str().unwrap().to_string();
    let dp = s.call("contracts_draft", &json!({"ref": fid})); assert!(dp.ok, "{}", dp.text); assert!(dp.data["proposal_id"].is_string());
    assert!(!s.call("contracts_draft", &json!({"ref": tel})).ok, "the telecom service already has a contract");
    assert_eq!(std::fs::read_dir(mem.join("contracts")).unwrap().count(), 1, "a proposal writes no contract file");
    for o in [&p.text, &t.text, &ar.text, &dp.text] { assert!(!o.contains("ABC-123-SECRET") && !o.contains("Kerkstraat"), "{o}"); }
    assert_eq!(call("POST", &format!("/api/v1/subs/decisions/{did}/reject"), json!({})).0, 200); assert_eq!(call("POST", &format!("/api/v1/subs/decisions/{did}/confirm"), json!({})).0, 409, "a rejected decision stays rejected");
    // a draft from the web: dry run shows the file, the real one writes it
    let (s1, dv) = call("POST", "/api/v1/subs/contracts/draft?dry_run=true", json!({"series": fid})); assert_eq!(s1, 200, "{dv}"); assert!(dv["diff"].as_str().unwrap().contains("merchant_match"));
    assert_eq!(std::fs::read_dir(mem.join("contracts")).unwrap().count(), 1); let (s2, dv) = call("POST", "/api/v1/subs/contracts/draft", json!({"series": fid})); assert_eq!(s2, 200, "{dv}");
    assert!(mem.join(dv["contract"]["file"].as_str().unwrap()).is_file());
    // the letter: filled locally, placeholders until the contact block is set; contact details cannot be proposed by the coach
    let l = call("GET", "/api/v1/subs/letter?contract=gsm&lang=nl", Value::Null).1; assert!(l["text"].as_str().unwrap().contains("ABC-123-SECRET"), "{l}"); assert!(l["placeholders"].as_array().unwrap().iter().any(|p| p == "your address")); assert_eq!(l["sent"], false);
    assert_eq!(call("PUT", "/api/v1/subs/contact", json!({"email": "not-an-email"})).0, 422);
    let (s3, c) = call("PUT", "/api/v1/subs/contact", json!({"address": "Demostraat 1, 1000 Brussel", "email": "demo@example.test"})); assert_eq!(s3, 200, "{c}");
    assert_eq!(call("GET", "/api/v1/subs/contact", Value::Null).1["set"], true);
    let l = call("GET", "/api/v1/subs/letter?contract=gsm&lang=fr", Value::Null).1; assert!(l["text"].as_str().unwrap().contains("Demostraat 1") && l["subject"].as_str().unwrap().starts_with("Résiliation"), "{l}");
    assert!(!s.call("memory_propose", &json!({"file": "household.yaml", "ops": [{"op": "set", "path": "contact.email", "value": "x@y.z"}], "reason": "letters"})).ok);
}
