//! Phase 5a on synthetic data: attribution (owner, card rule, manual), per-person views, the children's money, kid budgets, who pays
//! what, the redacted MCP tools and a child's /me pages.
use finch::{analytics::{cashflow::cashflow, Dataset, Scope}, classify, config, db, household, ingest, mcp, web::{App, Req}};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

const HH: &str = "members:
  - id: zorro
    name: Zorro Testmans
    role: adult
  - id: yara
    name: Yara Quixley
    role: adult
  - id: kiki
    name: Kiki Testmans
    role: child
    pocket_money: {amount: 10, period: weekly}
attribution:
  - id: yara-card
    member: yara
    match: {card_last4: '5678'}
kid_budgets:
  - id: kiki-snacks
    member: kiki
    period: monthly
    limit: 20
allocations:
  - id: groceries
    title: Groceries
    match: {group: food}
    method: equal
";
fn csv(dir: &Path, name: &str, rows: &[String]) -> std::path::PathBuf { let f = dir.join(name); std::fs::write(&f, format!("Datum;Omschrijving;Bedrag\n{}\n", rows.join("\n"))).unwrap(); f }
fn setup(dir: &Path) -> (config::Config, rusqlite::Connection) {
    std::fs::create_dir_all(dir.join("memory")).unwrap(); std::fs::write(dir.join("config.toml"), "insecure_plaintext_db = true\n").unwrap(); std::fs::write(dir.join("memory/household.yaml"), HH).unwrap();
    let cfg = config::load_config(Some(&dir.join("config.toml"))).unwrap(); let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    let imp = |f: &Path, acc: &str| { ingest::import_file(&con, f, ingest::ImportOpts { account: acc, profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap(); };
    let mut joint = vec![]; let mut own = vec![]; let mut kid = vec![];
    for m in 1..=4 {
        joint.push(format!("01/{m:02}/2025;OVERSCHRIJVING VAN QUUXCORP NV LOON;2400,00"));
        joint.push(format!("05/{m:02}/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 COLRUYT GENT;-100,00"));
        joint.push(format!("12/{m:02}/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 5678 DELHAIZE GENT;-40,00"));
        own.push(format!("20/{m:02}/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 9999 ALDI GENT;-30,00"));
        for d in [3, 10, 17, 24] { kid.push(format!("{d:02}/{m:02}/2025;OVERSCHRIJVING VAN TESTMANS ZORRO ZAKGELD;10,00")); }
        kid.push(format!("15/{m:02}/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 4321 SNOEPWINKEL DEMO;-6,00"));
    }
    kid.push("22/03/2025;OVERSCHRIJVING VAN QUIXLEY YARA VERJAARDAG;25,00".into());
    imp(&csv(dir, "joint.csv", &joint), "new:Gezamenlijk"); imp(&csv(dir, "own.csv", &own), "new:Zorro privé"); imp(&csv(dir, "kid.csv", &kid), "new:Kiki zakgeld");
    for (uid, owner) in [("imp-gezamenlijk", "joint"), ("imp-zorro-prive", "ZORRO TESTMANS"), ("imp-kiki-zakgeld", "kiki")] { con.execute("UPDATE accounts SET owner=?, purpose='main' WHERE uid=?", [owner, uid]).unwrap(); }
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap();
    (cfg, con)
}
fn today() -> Option<chrono::NaiveDate> { chrono::NaiveDate::from_ymd_opt(2025, 5, 10) }

#[test]
fn attribution_views_kids_and_split() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path());
    let uids: Vec<String> = con.prepare("SELECT uid FROM accounts ORDER BY uid").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect(); assert_eq!(uids.len(), 3, "{uids:?}");
    let ds = Dataset::load(&con, &cfg, today()).unwrap();
    let who = |like: &str| ds.txs.iter().find(|t| t.desc.contains(like)).unwrap().person.clone();
    assert_eq!(who("COLRUYT").as_deref(), Some("joint"), "the joint account's owner");
    assert_eq!(who("DELHAIZE").as_deref(), Some("yara"), "the card rule wins over the owner");
    assert_eq!(who("ALDI").as_deref(), Some("zorro"), "an owner given by name resolves to the member");
    // a manual reassignment wins, is logged and can be undone
    let key = ds.txs.iter().find(|t| t.desc.contains("COLRUYT")).unwrap().key.clone();
    assert_eq!(household::assign(&con, &ds.people, &key, "zorro", "test", None).unwrap()["changed"], true);
    assert!(household::assign(&con, &ds.people, &key, "nobody", "test", None).is_err());
    let ds2 = Dataset::load(&con, &cfg, today()).unwrap(); assert_eq!(ds2.txs.iter().find(|t| t.key == key).unwrap().person.as_deref(), Some("zorro"));
    let ex = household::explain(&con, &ds2.people, &key).unwrap(); assert_eq!((ex["source"].as_str(), ex["rules"][0]["matched"].as_bool()), (Some("manual"), Some(false)));
    let id = household::log(&con, Some(&key), 5).unwrap()[0]["id"].as_i64().unwrap(); household::revert(&con, &ds2.people, id, "test").unwrap();
    assert_eq!(Dataset::load(&con, &cfg, today()).unwrap().txs.iter().find(|t| t.key == key).unwrap().person.as_deref(), Some("joint"), "undone");
    // a person's view: only their transactions, only the balances of the accounts they own
    let v = ds.member_view("zorro"); assert!(v.txs.iter().all(|t| t.person.as_deref() == Some("zorro"))); assert_eq!(v.owned.as_ref().unwrap().len(), 1);
    let cf = cashflow(&v, &Scope::default(), 6, None, false, false); let mar = cf["household"]["months"].as_array().unwrap().iter().find(|m| m["month"] == "2025-03").unwrap(); assert_eq!(mar["spending"], "30.00");
    // the children's money: 4 weekly credits a month from a parent are pocket money, the birthday gift an extra top-up from the other parent
    let k = household::kid_report(&ds, &con, "kiki", 3).unwrap();
    assert_eq!(k["pocket_money"]["total"], "120.00"); assert_eq!(k["pocket_money"]["series"][0]["source"], "zorro"); assert_eq!(k["pocket_money"]["series"][0]["cadence"], "weekly");
    assert_eq!(k["extra_topups"]["total"], "25.00"); assert_eq!(k["extra_topups"]["items"][0]["source"], "yara"); assert_eq!(k["spending"]["total"], "18.00");
    let april = Dataset::load(&con, &cfg, chrono::NaiveDate::from_ymd_opt(2025, 4, 20)).unwrap(); let kb = &household::kid_budget_status(&april, Some("kiki"))[0];
    assert_eq!((kb["spent"].as_str(), kb["status"].as_str(), kb["period_start"].as_str()), (Some("6.00"), Some("ok"), Some("2025-04-01")));
    // who pays: food split equally between the adults; the joint payments settle nothing
    let wp = household::who_pays(&ds, 3, None); let r = &wp["rules"][0];
    assert_eq!((r["total"].as_str(), r["joint_paid"].as_str(), r["personal_paid"].as_str()), (Some("510.00"), Some("300.00"), Some("210.00")));
    let net = |m: &str| r["members"].as_array().unwrap().iter().find(|x| x["member"] == m).unwrap()["net"].as_str().unwrap().to_string();
    assert_eq!((net("zorro"), net("yara")), ("-15.00".into(), "15.00".into()), "the nets add up to zero");
    assert_eq!(r["unattributed"], "0.00", "every food cost belongs to the joint pot or an adult");
}

#[test]
fn mcp_household_tools_never_name_anyone() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path()); let mut s = mcp::Session::with(cfg, con, today());
    let leaks = ["Zorro", "ZORRO", "zorro", "Yara", "YARA", "yara", "Kiki", "KIKI", "kiki", "Testmans", "TESTMANS", "Quixley", "QUIXLEY", "5678", "Gezamenlijk", "imp-"];
    let ov = s.call("household_overview", &json!({})); assert!(ov.ok, "{}", ov.text);
    let kid = ov.data["members"].as_array().unwrap().iter().find(|m| m["role"] == "child").unwrap()["member"].as_str().unwrap().to_string(); assert!(kid.starts_with("kid-"), "{kid}");
    let k = s.call("kids_money", &json!({"member": kid, "months": 3})); assert!(k.ok, "{}", k.text); assert_eq!(k.data["children"][0]["pocket_money"]["total"], "120.00");
    assert!(k.data["children"][0]["pocket_money"]["series"][0]["source"].as_str().unwrap().starts_with("adult-"));
    let w = s.call("who_pays", &json!({"months": 3})); assert!(w.ok, "{}", w.text);
    let c = s.call("cashflow", &json!({"member": "adult-1", "months": 3})); assert!(c.ok, "{}", c.text);
    let t = s.call("transactions_search", &json!({"member": kid})); assert!(t.ok, "{}", t.text); assert_eq!(t.data["total"]["count"].as_i64().or(t.data["count"].as_i64()).unwrap_or(-1) >= 0, true);
    for o in [&ov, &k, &w, &c, &t] { for l in leaks { assert!(!o.text.contains(l), "{l} leaked: {}", o.text); } }
    assert!(!s.call("cashflow", &json!({"member": "zorro"})).ok, "a real member id is refused");
}

#[test]
fn web_member_views_and_child_me() {
    let tmp = tempfile::tempdir().unwrap(); let (cfg, con) = setup(tmp.path());
    con.execute("INSERT INTO ui_users(id, member_id, role, created_at) VALUES ('kiki-login', 'kiki', 'child', '2025-01-01')", []).unwrap(); drop(con);
    let st = tmp.path().join("static"); std::fs::create_dir_all(&st).unwrap();
    let mut a = App::new(cfg, true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = today(); a.inline_jobs = true;
    let login = |user: Option<&str>| -> (String, String) {
        let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
        let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(user).unwrap()}).to_string().into_bytes()));
        let c = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
        let s: Value = serde_json::from_slice(&a.handle(Req::new("GET", "/api/v1/session", vec![("Host".into(), "127.0.0.1:8765".into()), ("Cookie".into(), format!("{}={c}", a.sec.cookie_name))], vec![])).body).unwrap();
        (c, s["csrf_token"].as_str().unwrap().to_string())
    };
    let call = |ck: &(String, String), m: &str, p: &str, body: Option<Value>| -> (u16, Value) {
        let mut h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Cookie".into(), format!("{}={}", a.sec.cookie_name, ck.0)), ("X-CSRF-Token".into(), ck.1.clone())];
        if body.is_some() { h.push(("Content-Type".into(), "application/json".into())); }
        let r = a.handle(Req::new(m, p, h, body.map(|b| b.to_string().into_bytes()).unwrap_or_default())); (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    };
    let owner = login(None);
    let (s, v) = call(&owner, "GET", "/api/v1/transactions?member=zorro", None); assert_eq!(s, 200, "{v}"); assert_eq!(v["total"], 4); assert!(v["items"].as_array().unwrap().iter().all(|i| i["person"] == "zorro"));
    assert_eq!(call(&owner, "GET", "/api/v1/accounts/balances?member=zorro", None).1["n_accounts"], 1, "a person's view shows only the accounts they own");
    assert_eq!(call(&owner, "GET", "/api/v1/analytics/cashflow?member=nobody", None).0, 404);
    assert_eq!(call(&owner, "GET", "/api/v1/household/overview", None).1["members"].as_array().unwrap().len(), 3);
    assert_eq!(call(&owner, "GET", "/api/v1/household/kids", None).1["children"][0]["pocket_money"]["total"], "160.00", "6 months window by default: the 4 months of data");
    assert!(call(&owner, "GET", "/api/v1/household/allocation?months=3", None).1["rules"].is_array());
    let key = v["items"][0]["tx_key"].as_str().unwrap().to_string();
    let (s, r) = call(&owner, "POST", "/api/v1/transactions/person", Some(json!({"tx_key": key, "member": "yara"}))); assert_eq!(s, 200, "{r}");
    assert_eq!(call(&owner, "GET", &format!("/api/v1/transactions/attribution?tx_key={key}"), None).1["source"], "manual");
    assert_eq!(call(&owner, "GET", "/api/v1/transactions?member=zorro", None).1["total"], 3, "the reassignment shows at once");
    assert_eq!(call(&owner, "POST", "/api/v1/transactions/person/clear", Some(json!({"tx_key": key}))).0, 200);
    // the child: its own money only, sources never named, everything else denied whatever the parameters
    let kid = login(Some("kiki-login"));
    let (s, me) = call(&kid, "GET", "/api/v1/me/summary", None); assert_eq!(s, 200, "{me}");
    let txt = me.to_string(); for l in ["zorro", "Zorro", "ZORRO", "yara", "Yara", "YARA", "Quixley", "QUIXLEY"] { assert!(!txt.contains(l), "{l} shown to the child: {txt}"); }
    assert!(txt.contains("a parent"));
    let (_, mt) = call(&kid, "GET", "/api/v1/me/transactions", None); assert!(!mt.to_string().contains("TESTMANS"), "{mt}");
    assert_eq!(call(&kid, "GET", "/api/v1/transactions?member=zorro", None).0, 403);
    assert_eq!(call(&kid, "PUT", "/api/v1/me/preferences", Some(json!({"prefs": {"theme": "dark"}}))).1["prefs"]["theme"], "dark");
    assert_eq!(call(&kid, "PUT", "/api/v1/me/preferences", Some(json!({"prefs": {"default_member": "zorro"}}))).0, 400, "a child's default view is its own");
}
