//! Phase 5b on synthetic data: comment-preserving, validated and recorded memory edits from the web app, the open questions and
//! their generated view, sealed proposals (MCP -> listed -> accepted only by a human), and no name in what a model sees.
use finch::{classify, config, db, ingest, mcp, memstore::{EditOpts, Store}, proposals, questions, web::{App, Req}};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

const HH: &str = "# who we are\nmembers:\n  - id: zorro # the owner\n    name: Zorro Testmans\n    role: adult\n  - id: kiki\n    name: Kiki Testmans\n    role: child\n";
fn setup(dir: &Path) -> config::Config {
    std::fs::create_dir_all(dir.join("memory")).unwrap(); std::fs::write(dir.join("config.toml"), "insecure_plaintext_db = true\n").unwrap();
    std::fs::write(dir.join("memory/household.yaml"), HH).unwrap(); std::fs::write(dir.join("memory/budgets.yaml"), "# envelopes\nbudgets:\n  - id: fuel\n    category: transport.fuel\n    monthly: 80 # car\n").unwrap();
    let cfg = config::load_config(Some(&dir.join("config.toml"))).unwrap(); let con = db::connect(&cfg, db::Opts { insecure: true, create: true, ..Default::default() }).unwrap();
    let f = dir.join("a.csv"); std::fs::write(&f, "Datum;Omschrijving;Bedrag\n03/03/2025;BETALING MET DEBETKAART NUMMER 4871 04XX XXXX 1234 KRUIDENIER DEMO;-20,00\n").unwrap();
    let prof = ingest::profiles::from_toml(&include_str!("../assets/import_profiles/generic-csv-be-nl.toml").parse().unwrap(), "x").unwrap();
    ingest::import_file(&con, &f, ingest::ImportOpts { account: "new:Zicht", profile: Some(&prof), fmt: None, dry_run: false, bank: Some("KBC"), currency: None, iban: None, force: false }).unwrap();
    classify::normalize_all(&con, Some(&cfg.memory_dir)).unwrap(); cfg
}
fn git_ok() -> bool { std::process::Command::new("git").arg("--version").output().is_ok() }
struct W { a: App, c: String, t: String }
impl W {
    fn new(cfg: config::Config, dir: &Path) -> Self {
        let st = dir.join("static"); std::fs::create_dir_all(&st).unwrap(); let mut a = App::new(cfg, true, 8765, false, st, Arc::new(|| panic!("no network"))).unwrap(); a.today = chrono::NaiveDate::from_ymd_opt(2025, 4, 10);
        let h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Content-Type".into(), "application/json".into())];
        let r = a.handle(Req::new("POST", "/api/v1/session/exchange", h, json!({"token": a.sec.tokens.issue(None).unwrap()}).to_string().into_bytes()));
        let c = r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
        let mut w = W { a, c, t: String::new() }; w.t = w.call("GET", "/api/v1/session", None).1["csrf_token"].as_str().unwrap().to_string(); w
    }
    fn call(&self, m: &str, p: &str, body: Option<Value>) -> (u16, Value) {
        let mut h = vec![("Host".to_string(), "127.0.0.1:8765".to_string()), ("Cookie".into(), format!("{}={}", self.a.sec.cookie_name, self.c)), ("X-CSRF-Token".into(), self.t.clone())];
        if body.is_some() { h.push(("Content-Type".into(), "application/json".into())); }
        let r = self.a.handle(Req::new(m, p, h, body.map(|b| b.to_string().into_bytes()).unwrap_or_default())); (r.status, serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }
}

#[test]
fn web_edits_are_validated_previewed_recorded_and_keep_comments() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let mem = cfg.memory_dir.clone(); let w = W::new(cfg.clone(), tmp.path());
    let before = std::fs::read_to_string(mem.join("budgets.yaml")).unwrap();
    let (s, v) = w.call("POST", "/api/v1/budgets?dry_run=true", Some(json!({"target": "food.groceries", "monthly": "350"}))); assert_eq!(s, 200, "{v}");
    assert!(v["diff"].as_str().unwrap().contains("+  - id: food-groceries")); assert_eq!(std::fs::read_to_string(mem.join("budgets.yaml")).unwrap(), before, "a preview writes nothing");
    let (s, v) = w.call("POST", "/api/v1/budgets", Some(json!({"target": "food.groceries", "monthly": "350,50"}))); assert_eq!(s, 200, "{v}");
    let now = std::fs::read_to_string(mem.join("budgets.yaml")).unwrap();
    assert_eq!(now, "# envelopes\nbudgets:\n  - id: fuel\n    category: transport.fuel\n    monthly: 80 # car\n\n  - id: food-groceries\n    category: food.groceries\n    monthly: 350.5\n", "comments and the other items stay byte for byte");
    assert_eq!(w.call("GET", "/api/v1/budgets", None).1["budgets"].as_array().unwrap().len(), 2, "the dataset sees the new budget at once");
    assert_eq!(w.call("POST", "/api/v1/budgets", Some(json!({"target": "made.up", "monthly": "10"}))).0, 422);
    let (s, v) = w.call("PUT", "/api/v1/household/attribution/rules/ghost-card", Some(json!({"member": "ghost", "match": {"card_last4": "1234"}}))); assert_eq!((s, v["error"]["code"].as_str()), (422, Some("memory_invalid")), "{v}");
    let (s, v) = w.call("PUT", "/api/v1/household/attribution/rules/kiki-card", Some(json!({"member": "kiki", "match": {"card_last4": "1234"}}))); assert_eq!(s, 200, "{v}");
    let hh = std::fs::read_to_string(mem.join("household.yaml")).unwrap(); assert!(hh.starts_with(HH) && hh.ends_with("attribution:\n  - id: kiki-card\n    member: kiki\n    match:\n      card_last4: '1234'\n"), "{hh}");
    let t = w.call("GET", "/api/v1/transactions", None).1; assert_eq!(t["items"][0]["person"], "kiki", "the new rule attributes at once");
    let (s, v) = w.call("PUT", "/api/v1/household/members/kiki/pocket-money", Some(json!({"amount": 5, "period": "weekly"}))); assert_eq!(s, 200, "{v}");
    let key = t["items"][0]["tx_key"].as_str().unwrap().to_string();
    let (s, v) = w.call("POST", "/api/v1/transactions/category", Some(json!({"tx_key": key, "category": "food.groceries", "scope": "memory", "match": "transaction"}))); assert_eq!(s, 200, "{v}");
    assert_eq!(w.call("GET", "/api/v1/transactions", None).1["items"][0]["category"], "food.groceries", "a memory annotation decides the category");
    assert_eq!(w.call("GET", "/api/v1/annotations", None).1["annotations"][0]["matched"], 1);
    assert_eq!(w.call("POST", "/api/v1/budgets/fuel/delete", Some(json!({}))).0, 200);
    if git_ok() {
        let h = w.call("GET", "/api/v1/memory/history?limit=20", None).1; let subj: Vec<String> = h["changes"].as_array().unwrap().iter().map(|c| c["subject"].as_str().unwrap().to_string()).collect();
        assert!(subj.iter().any(|x| x == "coach: set-budget budgets.yaml (food-groceries)"), "{subj:?}"); assert!(!subj.iter().any(|x| x.contains("external")), "finch's own lock is never recorded: {subj:?}"); assert!(subj.iter().any(|x| x.starts_with("coach: initial snapshot")));
        assert_eq!(h["changes"][0]["source"], "ui"); let id = h["changes"][0]["id"].as_str().unwrap();
        assert!(w.call("GET", &format!("/api/v1/memory/history/{id}/diff"), None).1["diff"].as_str().unwrap().contains("-  - id: fuel"));
    }
    assert_eq!(w.call("GET", "/api/v1/memory/check", None).1["summary"]["errors"], 0);
}

#[test]
fn questions_and_their_view() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path()); let st = Store::new(&cfg, "cli").unwrap(); let d = chrono::NaiveDate::from_ymd_opt(2025, 4, 1).unwrap();
    let (id, _) = questions::add(&st, "What is KRUIDENIER DEMO?", "Merchants", d, false).unwrap(); assert_eq!(id, "q-001");
    let md = std::fs::read_to_string(cfg.memory_dir.join("open-questions.md")).unwrap(); assert!(md.contains("- [ ] `q-001` What is KRUIDENIER DEMO?\n"), "{md}");
    let w = W::new(cfg.clone(), tmp.path());
    assert_eq!(w.call("GET", "/api/v1/questions", None).1["questions"][0]["id"], "q-001");
    let (s, v) = w.call("POST", "/api/v1/questions/q-001/answer", Some(json!({"text": "the corner shop"}))); assert_eq!(s, 200, "{v}");
    assert!(std::fs::read_to_string(cfg.memory_dir.join("open-questions.md")).unwrap().contains("- [x] `q-001` What is KRUIDENIER DEMO? → the corner shop (answered 2025-04-10)"));
    assert_eq!(w.call("POST", "/api/v1/questions/q-001/answer", Some(json!({"text": "again"}))).0, 422, "reopen first");
    assert_eq!(w.call("POST", "/api/v1/questions/q-001/reopen", Some(json!({}))).0, 200);
    std::fs::write(cfg.memory_dir.join("open-questions.md"), "hand edit\n").unwrap();
    assert_eq!(w.call("POST", "/api/v1/questions/q-001/dismiss", Some(json!({"reason": "x"}))).0, 422, "a hand-edited view is never overwritten");
}

#[test]
fn proposals_are_sealed_and_only_a_human_accepts_them() {
    let tmp = tempfile::tempdir().unwrap(); let cfg = setup(tmp.path());
    let con = db::connect(&cfg, db::Opts { insecure: true, ..Default::default() }).unwrap(); let mut s = mcp::Session::with(cfg.clone(), con, chrono::NaiveDate::from_ymd_opt(2025, 4, 10));
    let kid = s.call("household_overview", &json!({})).data["members"].as_array().unwrap().iter().find(|m| m["role"] == "child").unwrap()["member"].as_str().unwrap().to_string();
    let p = s.call("memory_propose", &json!({"file": "household.yaml", "ops": [{"op": "set", "path": format!("members[{kid}].pocket_money"), "value": {"amount": 7, "period": "weekly"}}], "reason": "the user said the child gets 7 a week"}));
    assert!(p.ok, "{}", p.text); let pid = p.data["proposal_id"].as_str().unwrap().to_string();
    let bad = s.call("memory_propose", &json!({"file": "household.yaml", "ops": [{"op": "set", "path": "members[nobody].role", "value": "adult"}], "reason": "test"})); assert!(!bad.ok);
    let q = s.call("questions_propose", &json!({"custom": [{"question": "Is Zorro Testmans paying the KRUIDENIER DEMO bills?", "topic": "Merchants"}]})); assert!(q.ok, "{}", q.text);
    let oq = s.call("open_questions", &json!({})); assert!(oq.ok);
    for o in [&p, &bad, &q, &oq] { for l in ["Zorro", "ZORRO", "Testmans", "kiki", "Kiki"] { assert!(!o.text.contains(l), "{l} leaked: {}", o.text); } }
    let st = Store::new(&cfg, "cli").unwrap(); let hh = std::fs::read_to_string(cfg.memory_dir.join("household.yaml")).unwrap(); assert_eq!(hh, HH, "a proposal changes nothing");
    let pr = proposals::get(&st, &pid).unwrap(); assert!(proposals::is_sealed(&pr)); assert_eq!(pr.ops[0]["path"], "members[kiki].pocket_money", "pseudonyms are turned back into the real ids");
    let w = W::new(cfg.clone(), tmp.path()); let l = w.call("GET", "/api/v1/proposals", None).1; assert_eq!(l["proposals"].as_array().unwrap().len(), 2); assert!(l["proposals"].as_array().unwrap().iter().find(|p| p["id"] == pid.as_str()).unwrap()["diff"].as_str().unwrap().contains("+    pocket_money:"));
    assert_eq!(w.call("POST", &format!("/api/v1/proposals/{pid}/accept"), Some(json!({}))).0, 403, "never from the web app");
    // a tampered proposal is refused; the genuine one is accepted by the human path
    let f = cfg.memory_dir.join(".proposals").join(format!("{pid}.json")); let orig = std::fs::read_to_string(&f).unwrap(); std::fs::write(&f, orig.replace("\"amount\": 7", "\"amount\": 700")).unwrap();
    assert!(proposals::accept(&st, &pid, true, false, "cli", &[]).unwrap_err().to_string().contains("modified")); std::fs::write(&f, orig).unwrap();
    assert!(proposals::accept(&st, &pid, false, false, "cli", &[]).is_err(), "needs the explicit confirmation");
    let (acc, res) = proposals::accept(&st, &pid, true, false, "cli", &[]).unwrap(); assert_eq!(acc.status, "accepted"); assert!(res.changed());
    assert!(std::fs::read_to_string(cfg.memory_dir.join("household.yaml")).unwrap().contains("    pocket_money:\n      amount: 7\n      period: weekly\n"));
    assert_eq!(proposals::get(&st, &pid).unwrap().status, "accepted"); assert!(proposals::accept(&st, &pid, true, false, "cli", &[]).is_err(), "never twice");
    let qid = proposals::listing(&st, Some("pending"))[0].id.clone(); let (s2, _) = w.call("POST", &format!("/api/v1/proposals/{qid}/reject"), Some(json!({"note": "no"}))); assert_eq!(s2, 200);
    assert_eq!(proposals::get(&st, &qid).unwrap().status, "rejected");
    if git_ok() { let subj: Vec<String> = st.repo().log(None, 20).unwrap().into_iter().map(|c| c.subject).collect(); assert!(subj.iter().any(|x| x.starts_with(&format!("coach: accept {pid} household.yaml")))); assert!(subj.iter().any(|x| *x == format!("coach: reject {qid}"))); }
    let _ = st.edit("budgets.yaml", &[], &EditOpts::default());
}
