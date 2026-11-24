//! The /api/v1 endpoints that phases 1-4 can serve, with the Python app's shapes (money as two-decimal strings). Everything else
//! under /api answers 404 `not_ported`. Reads come from the cached [`Snap`]; writes go through the existing functions and `touch()`.
use super::{ApiErr, App, Job, Req, Resp, R};
use crate::{analytics::{self, anomalies, averages::{category_averages, spending_txs}, cashflow::cashflow, planning, recurring, Dataset, Scope, Tx, is_income, is_transfer},
    classify::{self, llm, rules::Rules}, household, ingest::{accounts, bank}, memory, transfers, util::*};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};

const NOT_PORTED: &str = "this part of the app is not ported to finch yet (it still works in the Python app)";
const BALANCE_TYPE: [(&str, &str); 8] = [("CLBD", "booked"), ("ITBD", "booked (interim)"), ("XPCD", "expected"), ("CLAV", "available (closing)"), ("ITAV", "available (interim)"), ("OPBD", "booked (opening)"), ("OPAV", "available (opening)"), ("FWAV", "forward available")];
const KNOWN_TAGS: [&str; 7] = ["one_off", "exclude_from_averages", "capital", "savings", "investment", "reimbursable", "business"];

pub fn route(a: &App, r: &Req) -> R {
    let p = r.path.strip_prefix(super::API).unwrap_or(&r.path).trim_end_matches('/').to_string();
    let seg: Vec<&str> = p.trim_start_matches('/').split('/').collect();
    match (r.method.as_str(), seg.as_slice()) {
        ("GET", ["session"]) => session(a, r),
        ("POST", ["session", "exchange"]) => exchange(a, r),
        ("POST", ["session", "logout"]) => { a.sec.revoke(r.cookie.as_deref()); Ok(Resp::json(json!({"ok": true})).header("Set-Cookie", &a.sec.clear_cookie())) }
        ("GET", ["session", "methods"]) => ok(json!({"passkeys": false, "sso": false, "sso_sign_out": null})),
        ("GET", ["meta", "taxonomy"]) => ok(taxonomy(&rules(a)?)),
        ("GET", ["meta", "disclaimers"]) => ok(disclaimers(r.q("lang").unwrap_or("en"))),
        ("GET", ["meta", "filters"]) => filters(a),
        ("GET", ["health"]) => ok(health(a, &a.con()?)?),
        ("GET", ["analytics", "coverage"]) => { let s = a.view(r)?; ok(analytics::coverage(&s.ds)) }
        ("GET", ["accounts", "balances"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(balances(&s.ds, &sc)) }
        ("GET", ["analytics", "cashflow"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; let by = r.q("by");
            let mut d = cashflow(&s.ds, &sc, r.qi("months", Some(6), 1, 36)?.unwrap() as usize, None, r.qb("include_current", true), by.is_some());
            if by.is_none() { d["by_purpose"] = json!({}); d["by_owner"] = json!({}); } ok(d) }
        ("GET", ["analytics", "averages"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(category_averages(&s.ds, &sc, r.qi("window", None, 1, 36)?.map(|x| x as usize))) }
        ("GET", ["analytics", "month-categories"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(month_categories(&s.ds, &sc, month_param(r)?)) }
        ("GET", ["analytics", "forecast"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(planning::forecast(&s.ds, &s.rec, &sc, r.qi("days", Some(90), 7, 365)?.unwrap(), r.qb("points", true))) }
        ("GET", ["categories"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(category_overview(&s.ds, &sc, &rules(a)?)) }
        ("GET", ["categories", id]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(category_detail(&s.ds, &sc, &rules(a)?, id, r.qi("months", Some(24), 3, 60)?.unwrap())?) }
        ("GET", ["calendar"]) => calendar(a, r),
        ("GET", ["calendar.ics"]) => { let s = a.view(r)?; let c = planning::calendar(&s.ds, &a.con()?, &s.rec, Some(r.qi("days", Some(90), 1, 400)?.unwrap() as usize), false);
            Ok(Resp::with(200, ics(&c).into_bytes(), "text/calendar; charset=utf-8").header("Content-Disposition", "attachment; filename=\"finance-coach.ics\"")) }
        ("GET", ["budgets"]) => budgets(a, r),
        ("GET", ["budgets", "suggestions"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(planning::suggest(&s.ds, &sc, r.qi("months", None, 2, 24)?.map(|x| x as usize), r.qi("limit", Some(20), 1, 100)?.map(|x| x as usize))) }
        ("GET", ["goals"]) => { let s = a.snap()?; ok(planning::goals(&s.ds)) }
        ("GET", ["subscriptions"]) => subscriptions(a, r),
        ("GET", ["insights"]) => insights(a, r),
        ("POST", ["insights", id, act @ ("read" | "done" | "dismiss" | "snooze")]) if id.starts_with("cin_") => { let d = r.json()?["days"].as_i64().unwrap_or(7); if !crate::insights::coach_set(&a.con()?, id, act, d, a.today())? { return Err(ApiErr::not_found("no such insight")); } a.touch(); ok(json!({"id": id, "status": act})) }
        ("POST", ["insights", id, "dismiss"]) | ("POST", ["price-changes", id, "dismiss"]) => dismiss(a, id),
        ("GET", ["transactions"]) => tx_list(a, r),
        ("GET", ["transactions", "export.csv"]) => tx_csv(a, r),
        ("GET", ["transactions", "detail"]) => tx_detail(a, r),
        ("POST", ["transactions", "category"]) => tx_category(a, r),
        ("POST", ["transactions", "override", "clear"]) => { let k = body_str(r, "tx_key")?; let n = a.con()?.execute("DELETE FROM tx_overrides WHERE tx_key=?", [&k]).map_err(anyhow::Error::from)?; a.touch(); ok(json!({"tx_key": k, "cleared": n > 0})) }
        ("GET", ["transfers"]) => transfers_list(a),
        ("POST", ["transfers", "link"]) => { let b = r.json()?; let id = transfers::link(&a.con()?, b["out_tx"].as_str().unwrap_or(""), b["in_tx"].as_str().unwrap_or(""), b["allow_amount_mismatch"].as_bool().unwrap_or(false)).map_err(ApiErr::rejected)?; a.touch(); ok(json!({"id": id})) }
        ("POST", ["transfers", "unlink"]) => { let (id, o, i) = transfers::unlink(&a.con()?, &body_str(r, "ref")?).map_err(ApiErr::rejected)?; a.touch(); ok(json!({"id": id, "out_tx_key": o, "in_tx_key": i})) }
        ("GET", ["connections"]) => connections(a),
        ("PATCH", ["accounts", uid @ ..]) => patch_account(a, r, &uid.join("/")),
        ("POST", ["sync"]) => start_sync(a, r),
        ("GET", ["sync", "status"]) => ok(a.sync_job.lock().unwrap().to_json()),
        ("POST", ["connections", "connect"]) => { let b = r.json()?; start_connect(a, b["bank"].as_str().unwrap_or("").trim(), &b["country"].as_str().unwrap_or("").to_uppercase(), b["days"].as_i64().unwrap_or(180), None, b["replace"].as_bool().unwrap_or(false)) }
        ("POST", ["connections", "reconnect"]) => { let b = r.json()?; let c = bank::find_session(&a.con()?, b["session_id"].as_str().unwrap_or("")).map_err(ApiErr::rejected)?; start_connect(a, &c.bank, &c.country, b["days"].as_i64().unwrap_or(180), Some(c.session_id), true) }
        ("GET", ["connections", "auth", "status"]) => ok(a.auth_job.lock().unwrap().to_json()),
        ("POST", ["connections", "auth", "finish"]) => finish_connect(a, r),
        ("GET", ["banks"]) => banks(a, r),
        ("GET", ["review"]) => review(a, r),
        ("POST", ["review", "correct"]) => { let b = r.json()?; let (k, c) = (b["key"].as_str().unwrap_or(""), b["category"].as_str().unwrap_or("")); llm::correct(&a.con()?, &rules(a)?, k, c, None).map_err(ApiErr::rejected)?; a.touch(); ok(json!({"key": k, "category": c, "merchants": [k]})) }
        ("POST", ["review", "confirm"]) => review_confirm(a, r),
        ("GET", ["household", "overview"]) => { let s = a.snap()?; ok(household::overview(&s.ds, &a.con()?)?) }
        ("GET", ["household", "kids"]) => { let s = a.snap()?; ok(json!({"as_of": s.ds.today.to_string(), "children": household::children_overview(&s.ds, &a.con()?, r.qi("months", Some(6), 1, 24)?.unwrap() as usize)?})) }
        ("GET", ["household", "kids", m]) => { let s = a.snap()?; if s.ds.people.get(m).is_none() { return Err(ApiErr::new(404, "unknown_member", "no such household member")); } ok(household::kid_report(&s.ds, &a.con()?, m, r.qi("months", Some(6), 1, 24)?.unwrap() as usize)?) }
        ("GET", ["household", "kid-budgets"]) => { let s = a.snap()?; ok(json!({"as_of": s.ds.today.to_string(), "budgets": household::kid_budget_status(&s.ds, None)})) }
        ("GET", ["household", "allocation"]) => { let s = a.snap()?; ok(household::who_pays(&s.ds, r.qi("months", Some(12), 1, 36)?.unwrap() as usize, r.q("rule"))) }
        ("GET", ["household", "attribution", "log"]) => ok(json!({"entries": household::log(&a.con()?, r.q("tx_key"), r.qi("limit", Some(50), 1, 500)?.unwrap())?})),
        ("POST", ["household", "attribution", "log", id, "undo"]) => { let id: i64 = id.parse().map_err(|_| ApiErr::not_found("no such log line"))?; let v = household::revert(&a.con()?, &a.snap()?.ds.people, id, &actor(r)).map_err(ApiErr::rejected)?; a.touch(); ok(v) }
        ("GET", ["transactions", "attribution"]) => { let k = r.q("tx_key").ok_or_else(|| ApiErr::new(422, "validation_error", "the request is not valid: tx_key: field required"))?; ok(household::explain(&a.con()?, &a.snap()?.ds.people, k).map_err(|e| ApiErr::not_found(&e.to_string()))?) }
        ("POST", ["transactions", "person"]) => { let b = r.json()?; let v = household::assign(&a.con()?, &a.snap()?.ds.people, b["tx_key"].as_str().unwrap_or(""), b["member"].as_str().unwrap_or(""), &actor(r), b["note"].as_str()).map_err(ApiErr::rejected)?; a.touch(); ok(v) }
        ("POST", ["transactions", "person", "clear"]) => { let v = household::clear(&a.con()?, &body_str(r, "tx_key")?, &actor(r)).map_err(ApiErr::rejected)?; a.touch(); ok(v) }
        ("GET", ["household", "users"]) => ok(json!({"users": household::overview(&a.snap()?.ds, &a.con()?)?["users"], "manage": "logins are managed in a terminal (ui_users)"})),
        ("GET", ["household", "audit"]) => { let lim = r.qi("limit", Some(100), 1, 500)?.unwrap(); let con = a.con()?;
            let rows: Vec<Value> = con.prepare("SELECT at, actor, method, path, status FROM audit_log WHERE (?1 IS NULL OR actor=?1) ORDER BY id DESC LIMIT ?2")?.query_map(params![r.q("actor"), lim], |x| Ok(json!({"at": x.get::<_, String>(0)?, "actor": x.get::<_, String>(1)?, "method": x.get::<_, String>(2)?, "path": x.get::<_, String>(3)?, "status": x.get::<_, i64>(4)?})))?.collect::<rusqlite::Result<_>>()?;
            let mem: Vec<Value> = crate::memstore::Store::new(&a.cfg, "ui").ok().and_then(|st| st.repo().log(None, lim as usize).ok()).unwrap_or_default().into_iter().map(|c| json!({"id": c.id, "date": c.date, "source": c.source.unwrap_or_else(|| "unknown".into()), "subject": c.subject, "files": c.files})).collect();
            ok(json!({"requests": rows, "memory": mem})) }
        ("POST", ["transactions", "split", "clear"]) => { let k = body_str(r, "tx_key")?; let n = a.con()?.execute("DELETE FROM tx_splits WHERE tx_key=?", [&k]).map_err(anyhow::Error::from)?; a.touch(); ok(json!({"tx_key": k, "removed": n})) }
        ("GET", ["me"]) => { let u = r.user.clone().unwrap_or_else(super::User::owner); let s = a.snap()?; let m = u.member_id.as_deref().and_then(|m| s.ds.people.get(m)); ok(json!({"user": u.to_json(), "member": m.map(|m| json!({"id": m.id, "name": m.name, "role": m.role}))})) }
        ("GET", ["me", "summary"]) => me_summary(a, r),
        ("GET", ["me", "transactions"]) => me_transactions(a, r),
        ("GET", ["me", "preferences"]) => { let u = r.user.clone().unwrap_or_else(super::User::owner); ok(json!({"prefs": u.prefs, "allowed": {"locale": ["fr-FR", "en-GB", "it-IT"], "theme": ["system", "light", "dark"], "default_member": null, "landing": ["dashboard", "transactions", "kids"]}, "stored": u.id != "owner"})) }
        ("PUT", ["me", "preferences"]) => put_prefs(a, r),
        ("GET", ["net-worth"]) => { let s = a.snap()?; let mut v = crate::loans::net_worth(&s.ds, a.today(), s.ds.settings.asset_stale_months as i32); if r.qb("history", false) { v["history"] = json!(crate::loans_extra::history(&a.con()?, r.qi("months", Some(36), 1, 240)?.unwrap())?); } ok(v) }
        ("GET", ["liabilities"]) => { let s = a.snap()?; ok(crate::loans::liabilities_view(&s.ds, a.today())) }
        ("GET", ["loans", id]) => { let s = a.snap()?; crate::loans::loan_detail(&s.ds, id, a.today()).map(Resp::json).ok_or_else(|| ApiErr::not_found("no such loan")) }
        ("GET", ["loans", id, "early-repayment"]) => { let s = a.snap()?; let (ls, _) = crate::loans::liabilities(&s.ds.memory_dir); let l = ls.iter().find(|l| l.id == *id).ok_or_else(|| ApiErr::not_found("no such loan"))?;
            let amt = r.q("amount").map(|x| x.replace(',', ".").parse::<f64>().map(crate::loans::cents).map_err(|_| ApiErr::new(422, "bad_amount", "amount: a number"))).transpose()?; ok(crate::loans::early_repayment(l, &crate::loans::compute(l, a.today()), amt, a.today())) }
        ("POST", ["loans", id, "scenario"]) => { let s = a.snap()?; let (ls, _) = crate::loans::liabilities(&s.ds.memory_dir); let l = ls.iter().find(|l| l.id == *id).ok_or_else(|| ApiErr::not_found("no such loan"))?; let b = r.json()?; let sc = crate::loans::compute(l, a.today());
            let f = |k: &str| b[k].as_f64(); let ty = b["type"].as_str().unwrap_or("prepay");
            let res = match ty { "prepay" => crate::loans_extra::prepay(l, &sc, a.today(), f("amount").ok_or_else(|| ApiErr::new(422, "missing_field", "amount is required for a prepayment"))?, b["on"].as_str().and_then(d), f("penalty")),
                "renegotiate" => crate::loans_extra::renegotiate(l, &sc, a.today(), f("new_rate").ok_or_else(|| ApiErr::new(422, "missing_field", "new_rate (the offered rate, %) is required"))?, b["variant"].as_str(), f("bank_fees").unwrap_or(0.0), f("guarantee_fees").unwrap_or(0.0), f("other_fees").unwrap_or(0.0), f("penalty")),
                _ => crate::loans_extra::insurance(&sc, f("alternative").ok_or_else(|| ApiErr::new(422, "missing_field", "alternative (the other policy's monthly premium) is required"))?, f("fees").unwrap_or(0.0)) }.map_err(|e| ApiErr::new(422, "bad_scenario", &e.to_string()))?;
            let saved = if b["save"] == true { if res["status"] != "computed" { return Err(ApiErr::new(422, "not_computed", "only a computed scenario can be saved")); } let id = crate::loans_extra::save_insight(&a.con()?, ty, &l.kind, &res)?; a.touch(); Some(id) } else { None };
            ok(json!({"scenario": ty, "result": res, "saved_insight": saved})) }
        ("POST", ["loans", id, "infer", "propose"]) => { let s = a.snap()?; let (ls, _) = crate::loans::liabilities(&s.ds.memory_dir); let l = ls.iter().find(|l| l.id == *id).ok_or_else(|| ApiErr::not_found("no such loan"))?; let inf = crate::loans_extra::infer(&s.ds, l);
            let ops: Vec<Value> = inf["fields"].as_array().into_iter().flatten().filter(|f| f["confidence"] != "low").map(|f| json!({"op": "set", "path": f["field"], "value": f["value"]})).collect(); if ops.is_empty() { return Err(ApiErr::new(409, "nothing_inferred", "nothing could be inferred with enough confidence")); }
            let st = crate::memstore::Store::new(&a.cfg, "ui").map_err(ApiErr::from)?; let p = crate::proposals::create(&st, &l.file, &ops, "loan facts inferred from the bank payments", "infer", vec![]).map_err(|e| ApiErr::new(409, "proposal_error", &e.to_string()))?; a.touch();
            ok(json!({"id": p.id, "accept_command": format!("finch memory accept {}", p.id)})) }
        ("GET", ["net-worth", "history"]) => ok(json!({"history": crate::loans_extra::history(&a.con()?, r.qi("months", Some(36), 1, 240)?.unwrap())?})),
        ("POST", ["net-worth", "snapshot"]) => { let s = a.snap()?; let v = crate::loans_extra::snapshot(&a.con()?, &s.ds, s.ds.settings.asset_stale_months as i32)?; a.touch(); ok(v) }
        ("GET", ["gold"]) => { let con = a.con()?; let rules = rules(a)?; let (anns, _) = crate::memory::load_annotations(&a.cfg.memory_dir); let st = r.q("strategy").filter(|x| ["money", "stratified"].contains(x)).unwrap_or("money"); let seed = r.qi("seed", Some(1), 0, i64::MAX)?.unwrap() as u64;
            let runs = crate::quality::runs(&con, None, 10)?; let latest = runs.iter().find(|x| x["kind"] == "classify").cloned();
            ok(json!({"counts": crate::quality::counts(&con)?, "latest": latest, "runs": runs, "sample": crate::quality::sample(&con, &rules, &anns, r.qi("n", Some(8), 1, 50)?.unwrap() as usize, st, seed)?, "strategy": st, "seed": seed})) }
        ("POST", ["gold", "bootstrap"]) => { let con = a.con()?; let (anns, _) = crate::memory::load_annotations(&a.cfg.memory_dir); let v = crate::quality::bootstrap(&con, &rules(a)?, &anns, false)?; a.touch(); ok(v) }
        ("POST", ["gold", "label"]) => { let b = r.json()?; crate::quality::set_gold(&a.con()?, &rules(a)?, b["tx_key"].as_str().unwrap_or(""), b["category"].as_str().unwrap_or(""), "user", "manual", b["note"].as_str()).map_err(|e| ApiErr::new(422, "invalid_label", &e.to_string()))?; a.touch(); ok(json!({"tx_key": b["tx_key"], "category": b["category"]})) }
        ("POST", ["eval", "classify"]) => { let v = crate::quality::evaluate(&a.con()?, &a.cfg, &rules(a)?, true).map_err(|e| ApiErr::new(409, "eval_unavailable", &e.to_string()))?; ok(json!({"headline": v["headline"], "regressions": v["regressions"], "pipeline": v["pipeline"]})) }
        ("GET", ["subs", "inventory"]) => { let s = a.view(r)?; let mut v = crate::subs::inventory(&s.ds, &s.rec, r.qb("include_ended", false), a.today()); crate::subs_extra::enrich(&mut v, &a.con()?, &s.rec, a.today())?; ok(v) }
        ("GET", ["subs", "savings"]) => { let s = a.snap()?; ok(crate::subs_extra::savings(&a.con()?, &s.rec, a.today())?) }
        ("GET", ["rental", "properties"]) => { let s = a.snap()?; ok(crate::rental::list(&s.ds)) }
        ("GET", ["rental", _, "tax"]) => ok(crate::rental::tax(r.qi("year", Some(chrono::Datelike::year(&a.today()) as i64 - 1), 2000, 2100)?.unwrap() as i32)),
        ("GET", ["rental", id]) => { let s = a.snap()?; crate::rental::detail(&s.ds, id, r.qi("year", None, 2000, 2100)?.map(|y| y as i32)).map(Resp::json).ok_or_else(|| ApiErr::not_found("no such rental property")) }
        ("GET", ["alerts"]) => { let s = crate::alerts::Settings::load(&a.cfg).map_err(ApiErr::rejected)?; ok(crate::alerts::alerts_view(&a.con()?, &s, r.q("status"), r.q("kind"), r.q("severity"), r.qb("include_resolved", false))?) }
        ("GET", ["alerts", "summary"]) => ok(crate::alerts::summary(&a.con()?)?),
        ("GET", ["alerts", "channels"]) => { let s = crate::alerts::Settings::load(&a.cfg).map_err(ApiErr::rejected)?; ok(crate::alerts::channels_view(&a.con()?, &a.cfg, &s)?) }
        ("GET", ["alerts", "digest"]) => ok(crate::alerts::digest(&a.con()?, a.today())?),
        ("POST", ["alerts", "check"]) => { let s = crate::alerts::Settings::load(&a.cfg).map_err(ApiErr::rejected)?; let snap = a.snap()?; let v = crate::alerts::evaluate(&a.con()?, &snap.ds, &snap.rec, &s, chrono::Utc::now(), false)?; a.touch(); ok(v) }
        ("POST", ["alerts", "channels", name, "test"]) => { let s = crate::alerts::Settings::load(&a.cfg).map_err(ApiErr::rejected)?; if !crate::alerts::CHANNELS.contains(name) { return Err(ApiErr::not_found("no such channel")); }
            let sample = vec![json!({"kind": "unusual_charge", "severity": "high", "payload": {"amount_c": 12345}}), json!({"kind": "consent", "severity": "medium", "payload": {}})]; let con = a.con()?; let g = crate::privacy::Guard::new(&a.cfg, Some(&con));
            let (title, body, note) = crate::alerts::compose(&sample, &s, &g).unwrap_or(("Coach alerts".into(), String::new(), Some("refused by the privacy filter"))); let pr = if s.enabled(name) { crate::alerts::problems(&a.cfg, &s, name) } else { vec!["the channel is not enabled".into()] };
            ok(json!({"dry_run": true, "sent": false, "channel": name, "enabled": s.enabled(name), "ready": s.enabled(name) && pr.is_empty(), "problems": pr, "detail": s.external_detail, "note": note, "message": {"title": title, "body": body}, "text": format!("{title}\n{body}"), "sample": "two sample alerts (one high)"})) }
        ("POST", ["alerts", "kinds", k, what]) => { let b = r.json()?; crate::alerts::kind_act(&a.con()?, k, what, b["days"].as_i64(), a.today()).map_err(ApiErr::rejected)?; a.touch(); ok(json!({"kind": k, "done": what})) }
        ("POST", ["alerts", id, what]) => { let b = r.json()?; if !crate::alerts::act(&a.con()?, id, what, b["days"].as_i64(), a.today()).map_err(ApiErr::rejected)? { return Err(ApiErr::not_found("no such alert")); } a.touch(); ok(json!({"id": id, "done": what})) }
        ("GET", ["usage"]) => ok(crate::usage::summary(&a.con()?, &a.cfg, r.qi("days", Some(30), 1, 365)?.unwrap(), chrono::Utc::now())?),
        ("GET", ["analytics", "year-review"]) => { let s = a.view(r)?; let sc = scope(r, &s.ds)?; ok(crate::analytics::review::year_review(&s.ds, r.qi("year", None, 2000, 2100)?.map(|y| y as i32), &sc, 10)) }
        ("GET", ["coach", "status"]) => ok(crate::coach::status(&a.cfg, a.coach.as_ref(), a.coach_job.lock().unwrap().as_ref().map(|j| j.0.clone()))),
        ("GET", ["coach", "prompts"]) => ok(json!({"prompts": crate::coach::quick_prompts()})),
        ("POST", ["coach", "stream"]) => coach_stream(a, r),
        ("POST", ["coach", "jobs", id, "cancel"]) => { match a.coach_job.lock().unwrap().as_ref() { Some((j, c)) if j == id => { c.store(true, std::sync::atomic::Ordering::SeqCst); ok(json!({"id": id, "cancelled": true})) } _ => Err(ApiErr::not_found("no such running job")) } }
        ("GET", ["review", "ai"]) => ok(json!({"available": false, "web_search": false, "backend": a.cfg.llm_backend, "message": NOT_PORTED})),
        // the shell asks for these on every page: empty answers until their features are ported
        _ => super::memory_routes::route(a, r, &seg).unwrap_or_else(|| Err(ApiErr::new(404, "not_ported", NOT_PORTED))),
    }
}
fn ok(v: Value) -> R { Ok(Resp::json(v)) }
fn rules(a: &App) -> Result<Rules, ApiErr> { Ok(Rules::load(&a.cfg)?) }
fn body_str(r: &Req, k: &str) -> Result<String, ApiErr> { r.json()?[k].as_str().filter(|s| !s.is_empty()).map(String::from).ok_or_else(|| ApiErr::new(422, "validation_error", &format!("the request is not valid: {k}: field required"))) }
fn month_param(r: &Req) -> Result<Option<String>, ApiErr> {
    match r.q("month") { None => Ok(None), Some(m) if rx_month().is_match(m) => Ok(Some(m.into())), Some(_) => Err(ApiErr::new(422, "validation_error", "the request is not valid: month: YYYY-MM")) }
}
fn rx_month() -> regex::Regex { regex::Regex::new(r"^\d{4}-(0[1-9]|1[0-2])$").unwrap() }

/// The analytics scope of a request (owner / purpose / account); `member` is handled by `App::view`.
fn scope(r: &Req, ds: &Dataset) -> Result<Scope, ApiErr> {
    let one = |x: Option<&str>| x.map(|v| [v.to_string()].into());
    let accounts = r.q("account").map(|x| [ds.accounts.values().find(|a| a.uid == x || a.label.eq_ignore_ascii_case(x)).map_or(x.to_string(), |a| a.uid.clone())].into());
    Ok(Scope { owners: one(r.q("owner")), purposes: one(r.q("purpose")), accounts })
}

// ---------------------------------------------------------------- session
fn session(a: &App, r: &Req) -> R {
    let u = r.user.clone().unwrap_or_else(super::User::owner); let csrf = a.sec.csrf(r.cookie.as_deref().unwrap_or(""));
    let auth = json!({"passkeys": false, "sso": false, "sso_sign_out": null}); let today = a.today().to_string();
    if u.is_child() { return ok(json!({"csrf_token": csrf, "version": env!("CARGO_PKG_VERSION"), "user": u.to_json(), "today": today, "locale": "en-GB", "allow_remote": false, "coach": {"configured": false, "backend": "", "message": ""}, "sync_daily_limit": 0, "enable_banking_configured": false, "memory_history": false, "auth": auth})); }
    ok(json!({"csrf_token": csrf, "version": env!("CARGO_PKG_VERSION"), "user": u.to_json(), "today": today, "locale": "en-GB", "allow_remote": false, "coach": coach_brief(a),
        "sync_daily_limit": a.cfg.sync_daily_limit, "enable_banking_configured": a.eb_configured(), "memory_history": a.cfg.memory_history, "auth": auth}))
}
fn exchange(a: &App, r: &Req) -> R {
    if !a.sec.exchange.take("login") { return Err(ApiErr::new(429, "rate_limited", "too many login attempts: wait a few minutes")); }
    let bad = || ApiErr::new(401, "invalid_login_token", "this login link is invalid, already used or expired: run `finch ui --login-link` in your terminal for a fresh one");
    let t = r.json()?["token"].as_str().unwrap_or("").to_string(); let (valid, login) = a.sec.tokens.consume(&t);
    if !valid { return Err(bad()); }
    if let Some(l) = &login { if super::get_user(&a.con()?, l).is_none_or(|u| u.disabled) { return Err(bad()); } }
    Ok(Resp::json(json!({"ok": true})).header("Set-Cookie", &a.sec.set_cookie(&a.sec.new_cookie(login.as_deref()))))
}

// ---------------------------------------------------------------- meta
fn taxonomy(rules: &Rules) -> Value {
    let mut groups: Vec<(String, Vec<Value>)> = vec![];
    for (id, desc) in &rules.categories { let g = id.split('.').next().unwrap_or(id).to_string(); if groups.last().is_none_or(|x| x.0 != g) { groups.push((g.clone(), vec![])); } groups.last_mut().unwrap().1.push(json!({"id": id, "description": desc})); }
    json!({"groups": groups.into_iter().map(|(g, c)| json!({"id": g, "categories": c})).collect::<Vec<_>>()})
}
/// The legal labels the web shows itself (its locale files never copy them). Belgian sources.
fn disclaimers(lang: &str) -> Value {
    let code = match lang.get(..2).unwrap_or("en").to_lowercase().as_str() { "fr" => "fr", "it" => "it", _ => "en" };
    json!({"lang": code, "texts": {"ai_label": "AI-generated content: it can contain mistakes. Check the figures against your accounts.", "ai_label_short": "AI-generated",
        "contract": "General summary of consumer-law rules, not legal advice: the contract's own terms and your situation decide. Verify with your contract and the official source (FPS Economy, your provider or insurer) before sending anything.",
        "contract_verify": "Verify with your contract and the official source before acting: this is a general summary, not legal advice.", "general_advice": "This is general information, not financial advice.",
        "loan": "Estimate from the figures you gave, not an offer: your bank or a broker gives the binding numbers. General information, not financial advice; no product is recommended.",
        "tax": "General information, not tax advice: rules, rates and ceilings change every year and depend on your situation. Verify on the official site (FPS Finance / MyMinfin) or with an adviser before declaring anything. Nothing is filed by finch.",
        "tax_short": "General information, not tax advice."}})
}
fn filters(a: &App) -> R {
    let s = a.snap()?; let ds = &s.ds; let members = memory::members(&a.cfg.memory_dir);
    let mut accs: Vec<_> = ds.accounts.values().collect(); accs.sort_by(|x, y| (x.bank.clone().unwrap_or_default(), &x.label).cmp(&(y.bank.clone().unwrap_or_default(), &y.label)));
    let owners: BTreeSet<String> = ds.accounts.values().filter_map(|a| a.owner.clone()).chain(members.iter().map(|m| m.id.clone())).chain(["joint".to_string()]).collect();
    let tags: BTreeSet<String> = ds.txs.iter().flat_map(|t| t.tags.iter().cloned()).chain(KNOWN_TAGS.iter().map(|t| t.to_string())).collect();
    let sources: BTreeSet<String> = ds.txs.iter().map(|t| t.source.clone()).chain(["override", "split", "memory", "user", "rule"].map(String::from)).collect();
    let groups: BTreeSet<String> = rules(a)?.categories.keys().map(|c| c.split('.').next().unwrap_or(c).to_string()).collect();
    ok(json!({"accounts": accs.iter().map(|a| json!({"uid": a.uid, "label": a.label, "bank": a.bank, "owner": a.owner, "purpose": a.purpose})).collect::<Vec<_>>(), "owners": owners,
        "members": members.iter().map(|m| json!({"id": m.id, "name": m.name, "role": m.role})).collect::<Vec<_>>(), "purposes": ds.accounts.values().filter_map(|a| a.purpose.clone()).collect::<BTreeSet<_>>(),
        "tags": tags, "sources": sources, "events": crate::memstore::Store::new(&a.cfg, "ui").map(|st| super::memory_routes::events_list(&st)).unwrap_or_default(), "groups": groups, "today": ds.today.to_string()}))
}

// ---------------------------------------------------------------- health / connections
fn health(a: &App, con: &Connection) -> anyhow::Result<Value> {
    let now = chrono::Utc::now(); let consents = bank::list_consents(con, false)?; let lim = a.cfg.sync_daily_limit; let today = a.today();
    let q1 = |sql: &str, uid: &str| con.query_row(sql, [uid], |r| r.get::<_, Option<String>>(0)).ok().flatten();
    let mut banks: BTreeMap<(String, Option<String>), (Value, Vec<Value>)> = BTreeMap::new();
    for ac in accounts::list_accounts(con)? {
        let sid: Option<String> = con.query_row("SELECT session_id FROM accounts WHERE uid=?", [&ac.uid], |r| r.get(0)).ok().flatten();
        let c = sid.as_ref().and_then(|s| consents.iter().find(|c| &c.session_id == s));
        let api = ac.source == "api"; let last_ok = q1("SELECT MAX(ran_at) FROM sync_log WHERE ok=1 AND account_uid=?", &ac.uid); let last_import = q1("SELECT MAX(imported_at) FROM imports WHERE account_uid=?", &ac.uid);
        let n_today = if api { bank::syncs_today(con, &ac.uid)? } else { 0 };
        let stale = api && last_ok.as_deref().and_then(d).is_none_or(|x| (today - x).num_days() > a.cfg.health_stale_days);
        let mut problems: Vec<String> = vec![]; let mut level = "green";
        if let Some(c) = c { if c.status == "expired" || c.status == "revoked" { problems.push(format!("consent {}: reconnect the bank", c.status)); level = "red"; } else if c.days_left.is_some_and(|x| x <= 7) { problems.push(format!("consent ends in {} day(s): reconnect soon", c.days_left.unwrap())); level = "amber"; } }
        if stale && !ac.exclude { problems.push("no successful sync recently".into()); if level == "green" { level = "amber"; } }
        if ac.needs_review { problems.push("needs review: merge it with the old account or resolve it".into()); if level == "green" { level = "amber"; } }
        let bank_name = ac.bank.clone().or(c.map(|c| c.bank.clone())).unwrap_or_else(|| "(file imports)".into());
        let e = banks.entry((bank_name.clone(), sid.clone())).or_insert_with(|| (json!({"bank": bank_name, "country": c.map_or("BE".to_string(), |c| c.country.clone()), "session_id": sid, "consent_status": c.map(|c| c.status), "consent_days_left": c.and_then(|c| c.days_left), "valid_until": c.and_then(|c| c.valid_until.clone())}), vec![]));
        e.1.push(json!({"uid": ac.uid, "label": ac.label.clone().or(ac.name.clone()).unwrap_or(ac.uid.clone()), "bank": bank_name, "source": ac.source, "level": level, "problems": problems, "last_ok_sync": last_ok, "syncs_today": n_today, "daily_limit": lim,
            "syncs_left_today": (lim - n_today).max(0), "consent_days_left": c.and_then(|c| c.days_left), "tx_count": ac.tx_count, "stale": stale, "excluded": ac.exclude, "last_import": last_import}));
    }
    let worst = |ls: &mut dyn Iterator<Item = &str>| ls.fold("green", |w, l| if l == "red" || w == "red" { "red" } else if l == "amber" || w == "amber" { "amber" } else { "green" });
    let banks: Vec<Value> = banks.into_values().map(|(mut b, accs)| { let lv = worst(&mut accs.iter().filter(|x| x["excluded"] == false).filter_map(|x| x["level"].as_str())); b["level"] = lv.into(); b["accounts"] = accs.into(); b }).collect();
    let level = worst(&mut banks.iter().filter_map(|b| b["level"].as_str()));
    Ok(json!({"generated_at": now.to_rfc3339(), "banks": banks, "ok": level == "green", "level": level, "memory": crate::memstore::Store::new(&a.cfg, "ui").map(|st| super::memory_routes::summary(&super::memory_routes::check(&st))).unwrap_or_else(|_| json!({"errors": 0, "warnings": 0, "info": 0, "by_code": {}, "failed": "error"})), "last_run": super::memory_routes::last_run(&a.cfg.log_dir())}))
}
fn conn_account(ac: &accounts::AccountRow) -> Value {
    json!({"uid": ac.uid, "bank": ac.bank, "label": ac.label, "name": ac.name, "iban_last4": accounts::mask_iban(ac.iban.as_deref()), "currency": ac.currency, "owner": ac.owner, "purpose": ac.purpose, "excluded": ac.exclude, "source": ac.source, "session_status": ac.session_status, "needs_review": ac.needs_review, "tx_count": ac.tx_count, "syncs_left_today": null})
}
fn connections(a: &App) -> R {
    let con = a.con()?; let h = health(a, &con)?;
    let left: BTreeMap<String, Value> = h["banks"].as_array().into_iter().flatten().flat_map(|b| b["accounts"].as_array().cloned().unwrap_or_default()).filter(|x| x["source"] == "api").map(|x| (x["uid"].as_str().unwrap_or("").to_string(), x["syncs_left_today"].clone())).collect();
    let accs: Vec<Value> = accounts::list_accounts(&con)?.iter().map(|ac| { let mut v = conn_account(ac); if let Some(l) = left.get(&ac.uid) { v["syncs_left_today"] = l.clone(); } v }).collect();
    let consents: Vec<Value> = bank::list_consents(&con, false)?.iter().map(|c| json!({"session_id": c.session_id, "bank": c.bank, "country": c.country, "valid_until": c.valid_until, "days_left": c.days_left, "status": c.status, "live_status": c.live_status, "live_checked_at": null, "accounts": c.accounts})).collect();
    let mut sync = a.sync_job.lock().unwrap().to_json(); sync["daily_limit"] = a.cfg.sync_daily_limit.into();
    ok(json!({"accounts": accs, "consents": consents, "health": h, "purposes": accounts::PURPOSES, "sync": sync, "enable_banking_configured": a.eb_configured(), "connect": a.auth_job.lock().unwrap().to_json()}))
}
fn patch_account(a: &App, r: &Req, uid: &str) -> R {
    let b = r.json()?; let s = |k: &str| b[k].as_str().map(String::from);
    let members: Vec<String> = memory::members(&a.cfg.memory_dir).into_iter().map(|m| m.id).collect(); let con = a.con()?;
    let real = accounts::set_account(&con, uid, accounts::AccountSet { label: s("label"), owner: s("owner"), purpose: s("purpose"), exclude: b["exclude"].as_bool(), resolve: b["resolve"].as_bool().unwrap_or(false) }, &members).map_err(ApiErr::rejected)?;
    a.touch(); ok(accounts::list_accounts(&con)?.iter().find(|x| x.uid == real).map(conn_account).unwrap_or(Value::Null))
}

// ---------------------------------------------------------------- background jobs (sync, connect)
fn begin(j: &std::sync::Mutex<Job>, kind: &str) -> Result<(), ApiErr> {
    let mut j = j.lock().unwrap(); if j.state == "running" { return Err(ApiErr::new(409, "busy", &format!("a {kind} is already running"))); }
    *j = Job { state: "running".into(), started_at: Some(now_iso()), ..Job::idle(kind) }; Ok(())
}
/// Run a job in its own thread (on the shared app), or inline in tests and when the app is not shared.
fn run_job(a: &App, f: impl FnOnce(&App) + Send + 'static) { match a.me.get().and_then(|w| w.upgrade()).filter(|_| !a.inline_jobs) { Some(app) => { std::thread::spawn(move || f(&app)); } None => f(a) } }
fn start_sync(a: &App, r: &Req) -> R {
    if a.cfg.eb_app_id.is_none() || a.cfg.eb_private_key_path.is_none() { return Err(ApiErr::new(409, "not_configured", "Enable Banking is not configured: see [enable_banking] in config.toml")); }
    let account = r.json()?["account"].as_str().map(String::from); begin(&a.sync_job, "sync")?;
    run_job(a, move |app| {
        let res = (|| -> anyhow::Result<(String, Vec<Value>, String)> {
            let con = app.con()?; let http = (app.http)(); let cl = bank::Client::from_config(&app.cfg, &con, http.as_ref())?;
            let res = bank::sync_all(&cl, account.as_deref(), false, false)?; let n_new: i64 = res.iter().map(|x| x["new"].as_i64().unwrap_or(0)).sum();
            if n_new > 0 { classify::normalize_all(&con, Some(&app.cfg.memory_dir))?; let _ = transfers::match_transfers(&con, &Rules::load(&app.cfg)?, &memory::load_annotations(&app.cfg.memory_dir).0, &app.cfg.memory_dir, &transfer_opts(app), app.cfg.transfer_auto_link); }
            let (f, sk) = (res.iter().filter(|x| x["status"] == "failed").count(), res.iter().filter(|x| x["status"] == "skipped").count());
            Ok((if f > 0 && f == res.len() { "failed" } else { "done" }.into(), res, format!("{n_new} new transaction(s); {sk} account(s) skipped (daily limit); {f} failed")))
        })();
        let mut j = app.sync_job.lock().unwrap();
        match res { Ok((st, res, msg)) => { j.state = st; j.results = res.iter().map(|x| { let mut o = Map::new(); for k in ["uid", "bank", "status", "new", "pending", "note"] { if !x[k].is_null() { o.insert(k.into(), x[k].clone()); } } Value::Object(o) }).collect(); j.message = Some(msg); }
            Err(e) => { j.state = "failed".into(); j.message = Some(e.root_cause().to_string().chars().take(200).collect()); } }
        j.finished_at = Some(now_iso()); drop(j); app.touch();
    });
    ok(a.sync_job.lock().unwrap().to_json())
}
fn transfer_opts(a: &App) -> transfers::Opts { transfers::Opts { window_days: a.cfg.transfer_window_days, cross_window_days: a.cfg.transfer_cross_bank_window_days, min_confidence: a.cfg.transfer_min_confidence, topups: a.cfg.transfer_topup_merchants.clone() } }
/// Start a bank authorisation: the job carries the bank's login page; the page then sends back the address the bank redirected to.
fn start_connect(a: &App, bank_name: &str, country: &str, days: i64, replaces: Option<String>, replace: bool) -> R {
    if bank_name.is_empty() || country.len() != 2 || !(1..=730).contains(&days) { return Err(ApiErr::new(422, "validation_error", "the request is not valid: bank, a 2-letter country and 1-730 days are required")); }
    if !a.eb_configured() { return Err(ApiErr::new(409, "not_configured", "Enable Banking is not configured: see [enable_banking] in config.toml")); }
    begin(&a.auth_job, "connect")?;
    let res = (|| -> anyhow::Result<(String, String)> { let con = a.con()?; let http = (a.http)(); let cl = bank::Client::from_config(&a.cfg, &con, http.as_ref())?; bank::start_auth(&cl, bank_name, country, days, replaces.as_deref(), replace) })();
    let mut j = a.auth_job.lock().unwrap();
    match res { Ok((state, url)) => { j.url = Some(url); j.auth_state = Some(state); j.message = Some("Log in at your bank; then paste here the address your browser lands on.".into()); j.log.push("waiting for the bank login".into()); }
        Err(e) => { j.state = "failed".into(); j.finished_at = Some(now_iso()); j.message = Some(e.root_cause().to_string().chars().take(200).collect()); } }
    ok(j.to_json())
}
fn finish_connect(a: &App, r: &Req) -> R {
    let url = body_str(r, "url")?; let con = a.con()?;
    let expected = { let j = a.auth_job.lock().unwrap(); if j.state != "running" { return Err(ApiErr::new(409, "no_authorisation", "no bank authorisation is waiting: start one first")); } j.auth_state.clone() };
    let (code, st) = bank::parse_finish_arg(&con, &url).map_err(ApiErr::rejected)?;
    if st.is_some() && expected.is_some() && st != expected { return Err(ApiErr::rejected("that address belongs to another authorisation")); }
    let http = (a.http)(); let res = bank::Client::from_config(&a.cfg, &con, http.as_ref()).and_then(|cl| bank::complete_auth(&cl, &code, st.as_deref().or(expected.as_deref())));
    let mut j = a.auth_job.lock().unwrap(); j.finished_at = Some(now_iso()); j.url = None;
    match res { Ok(info) => { j.state = "done".into(); j.message = Some("connected".into()); j.results = info["accounts"].as_array().cloned().unwrap_or_default().iter().map(|x| json!({"uid": x["uid"], "status": if x["needs_review"] == true { "needs_review" } else { "ok" }})).collect(); }
        Err(e) => { j.state = "failed".into(); j.message = Some(format!("the authorisation did not complete: {}", e.root_cause().to_string().chars().take(200).collect::<String>())); } }
    let out = j.to_json(); drop(j); a.touch(); ok(out)
}
fn banks(a: &App, r: &Req) -> R {
    let country = r.q("country").filter(|c| c.len() == 2).ok_or_else(|| ApiErr::new(422, "validation_error", "the request is not valid: country: 2 letters"))?.to_uppercase();
    let con = a.con()?; let http = (a.http)(); let cl = bank::Client::from_config(&a.cfg, &con, http.as_ref()).map_err(|e| ApiErr::new(409, "not_configured", &e.root_cause().to_string()))?;
    let res = cl.call("GET", "/aspsps", &[("country", country), ("psu_type", "personal".into())], None).map_err(|_| ApiErr::new(502, "bank_api_error", "Enable Banking answered with an error"))?;
    let q = r.q("q").map(str::to_lowercase); let items = res.get("aspsps").cloned().unwrap_or(res);
    let out: Vec<Value> = items.as_array().into_iter().flatten().filter(|b| q.as_ref().is_none_or(|q| b["name"].as_str().unwrap_or("").to_lowercase().contains(q.as_str())))
        .map(|b| json!({"name": b["name"], "country": b["country"], "max_consent_days": b["maximum_consent_validity"].as_i64().unwrap_or(0) / 86400, "beta": b["beta"].as_bool().unwrap_or(false)})).take(200).collect();
    ok(json!({"banks": out}))
}

// ---------------------------------------------------------------- review queue
fn review(a: &App, r: &Req) -> R {
    let limit = r.qi("limit", Some(40), 1, 200)?.unwrap() as usize; let con = a.con()?; let v = llm::review(&con, &a.cfg, &rules(a)?)?;
    let names: BTreeMap<String, Option<String>> = con.prepare("SELECT merchant_key, merchant_name FROM merchants")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>().map_err(anyhow::Error::from)?;
    let mut items: Vec<Value> = v["low_confidence"].as_array().cloned().unwrap_or_default().into_iter().map(|x| { let k = x["key"].as_str().unwrap_or("").to_string(); let t = to_cents(x["total"].as_f64().unwrap_or(0.0));
        json!({"key": k, "name": names.get(&k).cloned().flatten().unwrap_or(k.clone()), "category": x["category"], "confidence": x["confidence"], "source": x["source"], "reason": if x["category"] == "other.uncategorized" { "uncategorized" } else { "low_confidence" }, "n": null, "total": money_str(t), "at_stake": money_str(t), "banks": [], "accounts": []}) }).collect();
    items.extend(v["held_back_person_like"].as_array().cloned().unwrap_or_default().into_iter().map(|x| { let t = money_str(to_cents(x["total"].as_f64().unwrap_or(0.0))); json!({"key": x["key"], "name": x["key"], "category": "other.uncategorized", "confidence": null, "source": "held_back", "reason": "not_labelled", "n": x["n"], "total": t, "at_stake": t, "banks": [], "accounts": []}) }));
    ok(json!({"total": items.len(), "items": items.into_iter().take(limit).collect::<Vec<_>>()}))
}
fn review_confirm(a: &App, r: &Req) -> R {
    let k = body_str(r, "key")?; let con = a.con()?;
    let row: Option<(String, String)> = con.query_row("SELECT category, source FROM merchants WHERE merchant_key=?", [&k], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(anyhow::Error::from)?;
    let (cat, src) = row.ok_or_else(|| ApiErr::rejected("this merchant has no label to confirm yet: choose a category instead"))?;
    if src == "user" { return Err(ApiErr::rejected("this label is already yours")); }
    if src == "knn" || cat == "other.uncategorized" { return Err(ApiErr::rejected("a similarity or uncategorized label cannot be confirmed: choose a category instead")); }
    con.execute("UPDATE merchants SET source='user', confidence=1.0, updated_at=? WHERE merchant_key=?", params![now_iso(), k]).map_err(anyhow::Error::from)?; a.touch(); ok(json!({"key": k, "category": cat}))
}

// ---------------------------------------------------------------- balances / categories
fn balances(ds: &Dataset, sc: &Scope) -> Value {
    let stale = ds.settings.forecast_balance_stale_days as i64; let mut total = 0;
    let mut rows: Vec<Value> = ds.accounts_in(sc).into_iter().filter(|a| ds.owned.as_ref().is_none_or(|o| o.contains(&a.uid))).map(|a| { let b = ds.balances.get(&a.uid); if let Some(b) = b { total += b.amount_c; }
        json!({"uid": a.uid, "label": a.label, "bank": a.bank, "owner": a.owner, "purpose": a.purpose, "balance": b.map(|b| money_str(b.amount_c)), "balance_type": b.map(|b| b.ty.clone()),
            "balance_type_label": b.map(|b| BALANCE_TYPE.iter().find(|x| x.0 == b.ty).map_or(b.ty.clone(), |x| x.1.to_string())), "booked": b.is_some_and(|b| b.ty == "CLBD" || b.ty == "ITBD"),
            "as_of": b.map(|b| b.as_of.to_string()), "age_days": b.map(|b| (ds.today - b.as_of).num_days()), "stale": b.is_some_and(|b| (ds.today - b.as_of).num_days() > stale)}) }).collect();
    let cents = |v: &Value| v["balance"].as_str().map_or(i64::MIN, |s| to_cents(s.parse().unwrap_or(0.0)));
    rows.sort_by_key(|r| std::cmp::Reverse(cents(r)));
    let non_booked: Vec<String> = rows.iter().filter(|r| !r["balance"].is_null() && r["booked"] == false).filter_map(|r| r["label"].as_str().map(String::from)).collect();
    let mut note = "One balance per account, the most booked type the bank provides.".to_string();
    if !non_booked.is_empty() { note += &format!(" {} account(s) only give a non-booked balance ({}): the total mixes types.", non_booked.len(), non_booked.iter().take(4).cloned().collect::<Vec<_>>().join(", ")); }
    json!({"as_of": ds.today.to_string(), "household_total": money_str(total), "n_accounts": rows.len(), "n_without_balance": rows.iter().filter(|r| r["balance"].is_null()).count(), "mixed_types": !non_booked.is_empty(), "non_booked": non_booked, "accounts": rows, "note": note})
}
fn cents_of(v: &Value) -> i64 { v.as_str().and_then(|s| s.parse::<f64>().ok()).map_or(0, to_cents) }
fn kind_of(cat: &str) -> &'static str { if is_transfer(cat) { "transfer" } else if is_income(cat) { "income" } else { "spending" } }
fn month_categories(ds: &Dataset, sc: &Scope, month: Option<String>) -> Value {
    let month = month.unwrap_or_else(|| month_key(ds.today)); let av = category_averages(ds, sc, None);
    let avg: BTreeMap<String, &Value> = av["categories"].as_array().into_iter().flatten().map(|c| (c["category"].as_str().unwrap_or("").to_string(), c)).collect();
    let mut per: BTreeMap<String, (i64, i64, usize)> = BTreeMap::new();
    for t in spending_txs(ds, sc).into_iter().filter(|t| t.month() == month) { let e = per.entry(t.category.clone()).or_default(); e.0 -= t.amount_c; if t.is_one_off() { e.1 -= t.amount_c; } e.2 += 1; }
    let mut rows: Vec<(i64, Value)> = per.iter().map(|(c, (sp, one, n))| { let a = avg.get(c); (*sp, json!({"category": c, "group": c.split('.').next(), "spent": money_str(*sp), "one_off": money_str(*one), "n_tx": n, "monthly_avg": a.map(|a| a["monthly_avg"].clone()), "avg_months": a.map_or(0, |a| a["n_months"].as_i64().unwrap_or(0)), "low_confidence": a.is_some_and(|a| a["low_confidence"] == true)})) }).collect();
    rows.sort_by(|x, y| y.0.cmp(&x.0)); let total: i64 = rows.iter().map(|r| r.0).sum();
    let mut groups: BTreeMap<String, i64> = BTreeMap::new(); for (c, (sp, ..)) in &per { *groups.entry(c.split('.').next().unwrap_or(c).into()).or_default() += sp; }
    let mut g: Vec<_> = groups.into_iter().collect(); g.sort_by(|x, y| y.1.cmp(&x.1));
    json!({"month": month, "partial": month == month_key(ds.today), "as_of": ds.today.to_string(), "total_spent": money_str(total), "household_monthly_avg": av["household_monthly_avg"], "household_avg_months": av["household_months"].as_array().map_or(0, |x| x.len()),
        "categories": rows.into_iter().map(|r| r.1).collect::<Vec<_>>(), "groups": g.into_iter().map(|(g, v)| json!({"group": g, "spent": money_str(v)})).collect::<Vec<_>>(), "coverage": av["coverage"]})
}
fn leaves(rules: &Rules, group: &str) -> Vec<String> { rules.categories.keys().filter(|c| c.split('.').next() == Some(group)).cloned().collect() }
fn carriers_of(ds: &Dataset, sc: &Scope, ls: &[String]) -> Vec<String> { ls.iter().flat_map(|l| ds.carriers_split(l, sc).0).collect::<BTreeSet<_>>().into_iter().collect() }
fn group_average(ds: &Dataset, sc: &Scope, rules: &Rules, group: &str) -> Option<Value> {
    let ls = leaves(rules, group); if ls.first().is_none_or(|l| kind_of(l) != "spending") { return None; }
    let carriers = carriers_of(ds, sc, &ls); let ms = analytics::last_n(&ds.coverage.common_months(&carriers), ds.settings.average_window_months); if ms.is_empty() { return None; }
    let set: HashSet<&String> = ms.iter().collect(); let txs: Vec<&Tx> = ds.txs_in(sc).into_iter().filter(|t| ls.contains(&t.category) && !t.is_saved() && set.contains(&t.month())).collect();
    let (run, all) = (-txs.iter().filter(|t| !t.is_one_off()).map(|t| t.amount_c).sum::<i64>(), -txs.iter().map(|t| t.amount_c).sum::<i64>()); let n = ms.len() as i64;
    Some(json!({"monthly": money_str(div_cents(run, n)), "monthly_with_one_offs": money_str(div_cents(all, n)), "n_months": n, "months": ms, "accounts": carriers.iter().map(|u| ds.label(u)).collect::<Vec<_>>(), "ignored_accounts": [], "lumpy": false, "low_confidence": (n as usize) < ds.settings.average_min_months}))
}
fn category_overview(ds: &Dataset, sc: &Scope, rules: &Rules) -> Value {
    let av = category_averages(ds, sc, None); let (cur, prev) = (month_key(ds.today), last_closed_month(ds.today));
    let (mut this_m, mut last_m): (BTreeMap<String, i64>, BTreeMap<String, i64>) = Default::default();
    for t in spending_txs(ds, sc) { if t.month() == cur { *this_m.entry(t.category.clone()).or_default() -= t.amount_c; } else if t.month() == prev { *last_m.entry(t.category.clone()).or_default() -= t.amount_c; } }
    let mut seen = BTreeSet::new();
    let mut rows: Vec<Value> = av["categories"].as_array().into_iter().flatten().map(|c| { let cat = c["category"].as_str().unwrap_or("").to_string(); seen.insert(cat.clone());
        json!({"category": cat, "group": cat.split('.').next(), "monthly_avg": c["monthly_avg"], "monthly_avg_with_one_offs": c["monthly_avg_with_one_offs"], "n_months": c["n_months"], "low_confidence": c["low_confidence"], "lumpy": c["lumpy"],
            "this_month": money_str(*this_m.get(&cat).unwrap_or(&0)), "last_month": money_str(*last_m.get(&cat).unwrap_or(&0)), "accounts": c["accounts"]}) }).collect();
    for cat in this_m.keys().chain(last_m.keys()).collect::<BTreeSet<_>>() { if !seen.contains(cat) { rows.push(json!({"category": cat, "group": cat.split('.').next(), "monthly_avg": null, "monthly_avg_with_one_offs": null, "n_months": 0, "low_confidence": true, "lumpy": false, "this_month": money_str(*this_m.get(cat).unwrap_or(&0)), "last_month": money_str(*last_m.get(cat).unwrap_or(&0)), "accounts": []})); } }
    let gsum = |m: &BTreeMap<String, i64>, g: &str| m.iter().filter(|(c, _)| c.split('.').next() == Some(g)).map(|(_, v)| v).sum::<i64>();
    let mut groups: Vec<Value> = rows.iter().filter_map(|r| r["group"].as_str().map(String::from)).collect::<BTreeSet<_>>().into_iter().map(|g| { let ga = group_average(ds, sc, rules, &g);
        json!({"group": g, "monthly_avg": ga.as_ref().map(|x| x["monthly"].clone()), "n_months": ga.as_ref().map_or(json!(0), |x| x["n_months"].clone()), "low_confidence": ga.as_ref().is_some_and(|x| x["low_confidence"] == true), "this_month": money_str(gsum(&this_m, &g)), "last_month": money_str(gsum(&last_m, &g))}) }).collect();
    groups.sort_by_key(|g| std::cmp::Reverse(cents_of(&g["monthly_avg"])));
    json!({"as_of": ds.today.to_string(), "categories": rows, "groups": groups, "unavailable": av["unavailable"], "household_monthly_avg": av["household_monthly_avg"], "household_months": av["household_months"], "coverage": av["coverage"]})
}
fn category_detail(ds: &Dataset, sc: &Scope, rules: &Rules, id: &str, months: i64) -> Result<Value, ApiErr> {
    let is_group = !id.contains('.') && rules.categories.keys().any(|c| c.split('.').next() == Some(id));
    if !is_group && !rules.categories.contains_key(id) { return Err(ApiErr::new(404, "unknown_category", &format!("no category or group {id:?}"))); }
    let ls = if is_group { leaves(rules, id) } else { vec![id.to_string()] }; let kind = kind_of(&ls[0]); let sign = if kind == "income" { 1 } else { -1 };
    let txs: Vec<&Tx> = ds.txs_in(sc).into_iter().filter(|t| ls.contains(&t.category) && (kind != "spending" || !t.is_saved())).collect();
    let cur = month_key(ds.today); let carriers = carriers_of(ds, sc, &ls); let covered: HashSet<String> = ds.coverage.common_months(&carriers).into_iter().collect();
    let mut series = vec![];
    if let Some(first) = txs.iter().map(|t| t.month()).min() {
        let mut m = first.max(add_months_key(&cur, -(months as i32 - 1)));
        while m <= cur { let it: Vec<&&Tx> = txs.iter().filter(|t| t.month() == m).collect(); let tot = sign * it.iter().map(|t| t.amount_c).sum::<i64>(); let one = sign * it.iter().filter(|t| t.is_one_off()).map(|t| t.amount_c).sum::<i64>();
            series.push(json!({"month": m, "total": money_str(tot), "run_rate": money_str(tot - one), "one_off": money_str(one), "n_tx": it.len(), "covered": covered.contains(&m), "partial": m == cur})); m = add_months_key(&m, 1); }
    }
    let avg = if kind != "spending" { None } else if is_group { group_average(ds, sc, rules, id) } else {
        category_averages(ds, sc, None)["categories"].as_array().into_iter().flatten().find(|c| c["category"] == id).map(|c| json!({"monthly": c["monthly_avg"], "monthly_with_one_offs": c["monthly_avg_with_one_offs"], "n_months": c["n_months"], "months": c["months"], "accounts": c["accounts"], "ignored_accounts": c["ignored_accounts"], "lumpy": c["lumpy"], "low_confidence": c["low_confidence"]})) };
    let cov: Vec<&Value> = series.iter().filter(|s| s["covered"] == true).collect();
    let trend = (cov.len() >= 6).then(|| { let (rec, pri) = (&cov[cov.len() - 3..], &cov[cov.len() - 6..cov.len() - 3]); let (r, p): (i64, i64) = (rec.iter().map(|s| cents_of(&s["run_rate"])).sum(), pri.iter().map(|s| cents_of(&s["run_rate"])).sum());
        json!({"recent_avg": money_str(div_cents(r, 3)), "prior_avg": money_str(div_cents(p, 3)), "delta": money_str(div_cents(r - p, 3)), "pct": if p != 0 { json!(round4((r - p) as f64 / p as f64)) } else { Value::Null }, "recent_months": rec.iter().map(|s| s["month"].clone()).collect::<Vec<_>>(), "prior_months": pri.iter().map(|s| s["month"].clone()).collect::<Vec<_>>()}) });
    let lo12 = add_months_key(&cur, -11); let mut by_e: BTreeMap<String, (i64, i64, usize, chrono::NaiveDate, BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for t in txs.iter().filter(|t| t.month() >= lo12) { let e = by_e.entry(t.entity.clone()).or_insert((0, 0, 0, t.date, Default::default(), Default::default())); e.0 += sign * t.amount_c; if t.is_one_off() { e.1 += sign * t.amount_c; } e.2 += 1; e.3 = e.3.max(t.date); e.4.insert(ds.label(&t.account)); e.5.insert(t.category.clone()); }
    let grand = by_e.values().map(|e| e.0).sum::<i64>(); let g = if grand == 0 { 1 } else { grand };
    let mut ents: Vec<(i64, Value)> = by_e.iter().map(|(n, e)| (e.0, json!({"entity": n, "total": money_str(e.0), "one_off": money_str(e.1), "n_tx": e.2, "last_date": e.3.to_string(), "share": round4(e.0 as f64 / g as f64), "accounts": e.4, "categories": e.5}))).collect();
    ents.sort_by(|x, y| y.0.cmp(&x.0));
    let mut one_offs: Vec<&&Tx> = txs.iter().filter(|t| t.is_one_off()).collect(); one_offs.sort_by(|x, y| y.date.cmp(&x.date));
    let mut notes = vec![];
    if let Some(a) = &avg { if a["low_confidence"] == true { notes.push(format!("low confidence: only {} fully covered month(s) back this figure", a["n_months"])); } if a["lumpy"] == true { notes.push("seasonal or lumpy category: its average needs at least 12 covered months".into()); } }
    let missing = series.iter().filter(|s| s["covered"] == false && s["partial"] == false && s["n_tx"].as_u64().unwrap_or(0) > 0).count();
    if missing > 0 { notes.push(format!("{missing} month(s) with transactions are not fully covered by the accounts carrying this category (shown lighter, left out of the average)")); }
    if !ds.foreign.is_empty() { notes.push(format!("{} non-EUR transaction(s) are left out of every figure", ds.foreign.len())); }
    let cov_months: Vec<String> = avg.as_ref().and_then(|a| a["months"].as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    Ok(json!({"id": id, "is_group": is_group, "kind": kind, "description": if is_group { Value::Null } else { json!(rules.categories.get(id)) }, "leaves": if is_group { ls.iter().map(|l| json!({"id": l, "description": rules.categories.get(l)})).collect::<Vec<_>>() } else { vec![] },
        "group": id.split('.').next(), "as_of": ds.today.to_string(), "series": series, "average": avg, "trend": trend, "entities": ents.into_iter().take(40).map(|e| e.1).collect::<Vec<_>>(),
        "one_offs": one_offs.iter().take(50).map(|t| json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "entity": t.entity, "category": t.category, "tags": t.tags, "event": t.event, "account": ds.label(&t.account)})).collect::<Vec<_>>(),
        "notes": notes.clone(), "coverage": ds.coverage.info(&carriers, &cov_months, "months fully covered by every account that carries the category", notes),
        "totals": {"last_12_months": money_str(grand), "n_tx": by_e.values().map(|e| e.2).sum::<usize>()}}))
}

// ---------------------------------------------------------------- calendar / budgets / subscriptions / insights
fn calendar(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let con = a.con()?;
    if let Some(m) = month_param(r)? {
        let n = ((month_end(&m) - s.ds.today).num_days() + 1).clamp(1, 400) as usize; let mut d = planning::calendar(&s.ds, &con, &s.rec, Some(n), false);
        let items: Vec<Value> = d["items"].as_array().cloned().unwrap_or_default().into_iter().filter(|i| i["date"].as_str().is_some_and(|x| x.starts_with(&m))).collect();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new(); for i in &items { *counts.entry(i["source"].as_str().unwrap_or("").into()).or_default() += 1; }
        d["items"] = items.into(); d["month"] = m.into(); d["counts"] = json!(counts); return ok(d);
    }
    ok(planning::calendar(&s.ds, &con, &s.rec, Some(r.qi("days", Some(14), 1, 400)?.unwrap() as usize), false))
}
fn ics(c: &Value) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n");
    let mut out = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//finch//calendar//EN\r\nCALSCALE:GREGORIAN\r\n");
    for i in c["items"].as_array().into_iter().flatten() {
        let date = i["date"].as_str().unwrap_or("").replace('-', ""); let title = format!("{}{}", i["title"].as_str().unwrap_or(""), i["amount"].as_str().map(|a| format!(" ({a} EUR)")).unwrap_or_default());
        out += &format!("BEGIN:VEVENT\r\nUID:{}-{date}@finch\r\nDTSTAMP:{date}T000000Z\r\nDTSTART;VALUE=DATE:{date}\r\nSUMMARY:{}\r\nEND:VEVENT\r\n", &sha1_hex(&format!("{}{}", i["ref"], i["title"]))[..12], esc(&title));
    }
    out + "END:VCALENDAR\r\n"
}
fn budgets(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let mut d = planning::status(&s.ds, &s.rec, None); let (list, problems) = planning::budgets(&s.ds);
    for row in d["budgets"].as_array_mut().into_iter().flatten() { if let Some(b) = list.iter().find(|b| row["id"] == b.id.as_str()) { row["category"] = json!(b.category); row["group"] = json!(b.group); row["note"] = json!(b.note); row["start"] = json!(b.start); } }
    d["problems"] = json!(problems); ok(d)
}
fn subscriptions(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let sc = scope(r, &s.ds)?; let (status, kind, cadence, contracts_only) = (r.q("status").unwrap_or("active"), r.q("kind").unwrap_or("expense"), r.q("cadence"), r.qb("contracts_only", false));
    let pcs = recurring::price_changes(&s.ds, &s.rec, None, false, &HashSet::new(), false);
    let rows: Vec<Value> = s.rec.json["series"].as_array().into_iter().flatten().filter(|x| (kind == "all" || x["kind"] == kind) && (status == "all" || x["status"] == status) && cadence.is_none_or(|c| x["cadence"] == c)
        && (!contracts_only || x["contract_candidate"] == true || x["links"].as_array().is_some_and(|l| !l.is_empty())) && (sc.is_all() || s.ds.accounts.get(x["account"].as_str().unwrap_or("")).is_some_and(|ac| sc.accepts(ac))))
        .map(|x| { let mut d = x.clone(); if let Some(o) = d["occurrences"].as_array_mut() { let n = o.len(); o.drain(..n.saturating_sub(12)); } d["monthly_cost"] = money_str(div_cents(cents_of(&x["yearly_cost"]), 12)).into();
            d["price_changes"] = pcs["changes"].as_array().into_iter().flatten().filter(|c| c["series_id"] == x["id"]).cloned().collect::<Vec<_>>().into(); d["linked"] = json!([]); d }).collect();
    let active: Vec<&Value> = rows.iter().filter(|r| r["status"] == "active" && r["direction"] == "out").collect();
    ok(json!({"as_of": s.ds.today.to_string(), "series": rows, "totals": {"active_monthly": money_str(active.iter().map(|r| cents_of(&r["monthly_cost"])).sum()), "active_yearly": money_str(active.iter().map(|r| cents_of(&r["yearly_cost"])).sum()), "n_active": active.len(), "n_missing_contract": active.iter().filter(|r| r["missing_contract"] == true).count()},
        "by_cadence": s.rec.json["by_cadence"], "coverage": s.rec.json["coverage"]}))
}
fn coach_brief(a: &App) -> Value { let st = crate::coach::status(&a.cfg, a.coach.as_ref(), None); json!({"configured": st["configured"], "backend": a.cfg.coach_backend, "message": st["message"]}) }
fn coach_feed(a: &App) -> Result<Value, ApiErr> { let st = crate::coach::status(&a.cfg, a.coach.as_ref(), None); let (items, hidden) = crate::insights::coach_items(&a.con()?, a.today(), false)?; Ok(json!({"configured": st["configured"], "items": items, "hidden": hidden, "message": st["message"]})) }
fn insights(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let sc = scope(r, &s.ds)?; let (cards, hidden) = crate::insights::cards(&s.ds, &s.rec, &a.con()?, &sc)?; let mut counts: BTreeMap<String, usize> = BTreeMap::new(); for c in &cards { *counts.entry(c["kind"].as_str().unwrap_or("").into()).or_default() += 1; }
    let al = crate::alerts::summary(&a.con()?).unwrap_or(json!({"open": 0, "high": 0}));
    ok(json!({"as_of": s.ds.today.to_string(), "cards": cards, "hidden": hidden, "alerts": {"open": al["open"], "high": al["high"]}, "counts": counts, "coach": coach_feed(a)?}))
}
fn dismiss(a: &App, id: &str) -> R {
    let con = a.con()?;
    let done = if id.starts_with("anm_") { anomalies::dismiss(&con, id, None)? } else if id.starts_with("chg_") { con.execute("INSERT OR REPLACE INTO price_change_dismissals(id, dismissed_at, note) VALUES (?,?,NULL)", params![id, now_iso()]).map_err(anyhow::Error::from)? > 0 }
        else { return Err(ApiErr::new(404, "not_ported", "only anomalies and price changes can be dismissed in finch yet")) };
    if !done { return Err(ApiErr::not_found("no such insight")); }
    a.touch(); ok(json!({"id": id, "dismissed": true}))
}

// ---------------------------------------------------------------- transactions
struct Flags { over: HashSet<String>, linked: HashSet<String>, split: HashSet<String> }
fn flags(con: &Connection) -> anyhow::Result<Flags> {
    let set = |sql: &str| -> anyhow::Result<HashSet<String>> { Ok(con.prepare(sql)?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?) };
    Ok(Flags { over: set("SELECT tx_key FROM tx_overrides")?, linked: set("SELECT out_tx_key FROM transfer_links UNION SELECT in_tx_key FROM transfer_links")?, split: set("SELECT DISTINCT tx_key FROM tx_splits")? })
}
fn select(ds: &Dataset, r: &Req) -> Result<Vec<Tx>, ApiErr> {
    let amt = |k: &str| r.q(k).map(|v| v.replace(',', ".").trim().parse::<f64>().map(|x| to_cents(x.abs())).map_err(|_| ApiErr::new(422, "bad_amount", &format!("{k}: {v:?} is not an amount")))).transpose();
    let date = |k: &str| r.q(k).map(|v| d(v).ok_or_else(|| ApiErr::new(422, "validation_error", &format!("the request is not valid: {k}: YYYY-MM-DD")))).transpose();
    let (lo, hi, from, to) = (amt("amount_min")?, amt("amount_max")?, date("date_from")?, date("date_to")?);
    let accs: Option<HashSet<String>> = { let v = r.qs("account"); (!v.is_empty()).then(|| v.iter().map(|x| ds.accounts.values().find(|a| &a.uid == x || a.label.eq_ignore_ascii_case(x)).map_or(x.clone(), |a| a.uid.clone())).collect()) };
    let (cats, groups, tags, sources) = (r.qs("category"), r.qs("group"), r.qs("tag"), r.qs("source"));
    let needles: Vec<String> = r.q("q").unwrap_or("").split_whitespace().map(fold).collect(); let merchant = r.q("merchant").map(fold); let dir = r.q("direction").unwrap_or("all"); let xt = r.qb("exclude_transfers", false);
    let mut out: Vec<Tx> = ds.whole().into_iter().filter(|t| { let acc = ds.accounts.get(&t.account);
        accs.as_ref().is_none_or(|s| s.contains(&t.account)) && r.q("owner").is_none_or(|o| acc.is_some_and(|a| a.owner.as_deref().unwrap_or("") == o)) && r.q("purpose").is_none_or(|p| acc.is_some_and(|a| a.purpose.as_deref().unwrap_or("") == p))
        && from.is_none_or(|f| t.date >= f) && to.is_none_or(|x| t.date <= x) && ((cats.is_empty() && groups.is_empty()) || cats.contains(&t.category) || groups.iter().any(|g| t.category.split('.').next() == Some(g)))
        && !(xt && is_transfer(&t.category)) && !(dir == "in" && t.amount_c <= 0 || dir == "out" && t.amount_c >= 0) && lo.is_none_or(|l| t.amount_c.abs() >= l) && hi.is_none_or(|h| t.amount_c.abs() <= h)
        && tags.iter().all(|x| t.tags.contains(x)) && (sources.is_empty() || sources.contains(&t.source)) && r.q("event").is_none_or(|e| t.event.as_deref() == Some(e)) && r.q("entity").is_none_or(|e| t.entity == e)
        && merchant.as_ref().is_none_or(|m| fold(&format!("{} {} {}", t.entity, t.merchant.as_deref().unwrap_or(""), t.mkey)).contains(m.as_str()))
        && (needles.is_empty() || { let mag = money_str(t.amount_c.abs()); let hay = fold(&format!("{} {} {} {} {} {} {} {mag} {}", t.desc, t.entity, t.merchant.as_deref().unwrap_or(""), t.mkey, t.category, t.event.as_deref().unwrap_or(""), t.tags.iter().cloned().collect::<Vec<_>>().join(" "), mag.replace('.', ","))); needles.iter().all(|n| hay.contains(n.as_str())) })
    }).collect();
    match r.q("sort").unwrap_or("date_desc") { "date_asc" => out.sort_by(|x, y| (x.date, &x.key).cmp(&(y.date, &y.key))), "amount_desc" => out.sort_by(|x, y| (y.amount_c.abs(), y.date).cmp(&(x.amount_c.abs(), x.date))), "amount_asc" => out.sort_by_key(|x| (x.amount_c.abs(), x.date)),
        "date_desc" => out.sort_by(|x, y| (y.date, &y.key).cmp(&(x.date, &x.key))), _ => return Err(ApiErr::new(422, "validation_error", "the request is not valid: sort")) }
    Ok(out)
}
fn item(ds: &Dataset, t: &Tx, f: &Flags) -> Value {
    let acc = ds.accounts.get(&t.account);
    json!({"tx_key": t.key, "date": t.date.to_string(), "amount": money_str(t.amount_c), "category": t.category, "source": t.source, "tags": t.tags, "event": t.event, "entity": t.entity, "merchant": t.merchant, "description": t.desc, "account": t.account,
        "account_label": acc.map_or(t.account.clone(), |a| a.label.clone()), "owner": acc.and_then(|a| a.owner.clone()), "purpose": acc.and_then(|a| a.purpose.clone()), "type": t.ty, "person": t.person,
        "split": f.split.contains(&t.key) || t.source == "split", "overridden": f.over.contains(&t.key), "transfer_linked": f.linked.contains(&t.key)})
}
fn tx_list(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let (limit, offset) = (r.qi("limit", Some(50), 1, 500)?.unwrap() as usize, r.qi("offset", Some(0), 0, i64::MAX)?.unwrap() as usize);
    let rows = select(&s.ds, r)?; let f = flags(&a.con()?)?; let (inc, out): (i64, i64) = (rows.iter().filter(|t| t.amount_c > 0).map(|t| t.amount_c).sum(), rows.iter().filter(|t| t.amount_c < 0).map(|t| t.amount_c).sum());
    ok(json!({"items": rows.iter().skip(offset).take(limit).map(|t| item(&s.ds, t, &f)).collect::<Vec<_>>(), "totals": {"count": rows.len(), "sum": money_str(inc + out), "income": money_str(inc), "outflow": money_str(out)},
        "limit": limit, "offset": offset, "total": rows.len(), "next_offset": if offset + limit < rows.len() { json!(offset + limit) } else { Value::Null }}))
}
fn tx_csv(a: &App, r: &Req) -> R {
    let s = a.view(r)?; let rows = select(&s.ds, r)?; let f = flags(&a.con()?)?; let eu = matches!(r.q("locale"), Some("fr-FR") | Some("it-IT") | Some("nl-BE"));
    let cell = |v: &Value| { let s = match v { Value::Null => String::new(), Value::String(s) => s.clone(), x => x.to_string() }; if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s } };
    let mut w = csv::WriterBuilder::new().delimiter(if eu { b';' } else { b',' }).from_writer(if eu { "\u{feff}".as_bytes().to_vec() } else { vec![] });
    w.write_record(["date", "account", "owner", "purpose", "amount_eur", "category", "source", "merchant", "description", "tags", "event"]).map_err(anyhow::Error::from)?;
    for t in &rows { let it = item(&s.ds, t, &f); let amount = it["amount"].as_str().unwrap_or("").to_string();
        w.write_record([it["date"].as_str().unwrap_or("").to_string(), cell(&it["account_label"]), cell(&it["owner"]), cell(&it["purpose"]), if eu { amount.replace('.', ",") } else { amount }, cell(&it["category"]), cell(&it["source"]), cell(&it["entity"]), cell(&it["description"]), cell(&json!(t.tags.iter().cloned().collect::<Vec<_>>().join(" "))), cell(&it["event"])]).map_err(anyhow::Error::from)?; }
    let body = w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))?;
    Ok(Resp::with(200, body, "text/csv; charset=utf-8").header("Content-Disposition", &format!("attachment; filename=\"transactions-{}.csv\"", s.ds.today)))
}
fn tx_detail(a: &App, r: &Req) -> R {
    let k = r.q("tx_key").ok_or_else(|| ApiErr::new(422, "validation_error", "the request is not valid: tx_key: field required"))?; let s = a.snap()?; let con = a.con()?;
    let row: Option<(String, f64, Option<String>, Option<String>, Option<String>)> = con.query_row("SELECT COALESCE(t.booking_date,''), t.amount, t.description, COALESCE(a.bank, ''), a.purpose FROM transactions t LEFT JOIN accounts a ON a.uid=t.account_uid WHERE t.tx_key=?", [k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(anyhow::Error::from)?;
    let (date, amount, desc, bank_name, purpose) = row.ok_or_else(|| ApiErr::not_found("no such transaction"))?;
    let parsed: Value = con.query_row("SELECT e.tx_type, e.op_date, e.merchant_raw, e.merchant_key, p.parser, p.counterparty, p.mandate_ref, p.creditor_id, p.reference FROM tx_enriched e LEFT JOIN tx_parse_meta p USING(tx_key) WHERE e.tx_key=?", [k],
        |r| Ok(json!({"type": r.get::<_, Option<String>>(0)?, "op_date": r.get::<_, Option<String>>(1)?, "merchant_raw": r.get::<_, Option<String>>(2)?, "merchant_key": r.get::<_, Option<String>>(3)?, "parser": r.get::<_, Option<String>>(4)?, "counterparty": r.get::<_, Option<String>>(5)?, "mandate_ref": r.get::<_, Option<String>>(6)?, "creditor_id": r.get::<_, Option<String>>(7)?, "reference": r.get::<_, Option<String>>(8)?}))).optional().map_err(anyhow::Error::from)?.unwrap_or(json!({}));
    let ov: Option<(String, Option<String>)> = con.query_row("SELECT category, note FROM tx_overrides WHERE tx_key=?", [k], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(anyhow::Error::from)?;
    let link: Option<Value> = con.query_row("SELECT id, out_tx_key, in_tx_key, method, confidence FROM transfer_links WHERE out_tx_key=?1 OR in_tx_key=?1", [k], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "out": r.get::<_, String>(1)?, "in": r.get::<_, String>(2)?, "method": r.get::<_, String>(3)?, "confidence": r.get::<_, f64>(4)?}))).optional().map_err(anyhow::Error::from)?;
    let whole = s.ds.whole(); let t = whole.iter().find(|t| t.key == k); let f = flags(&con)?;
    let same = t.map_or(0, |t| whole.iter().filter(|x| !t.mkey.is_empty() && x.mkey == t.mkey).count());
    let fin = t.map(|t| json!({"category": t.category, "source": t.source, "tags": t.tags, "event": t.event, "before_memory": {"category": t.category, "source": t.source}}));
    ok(json!({"transaction": {"tx_key": k, "date": date, "amount": money_str(to_cents(amount)), "description": desc.unwrap_or_default(), "bank": bank_name.filter(|b| !b.is_empty()), "account": t.map(|t| s.ds.label(&t.account)), "account_purpose": purpose},
        "parsed": parsed, "steps": [], "split": null, "memory": {"annotations": [], "winner": null}, "final": fin.unwrap_or(json!({"category": "other.uncategorized", "source": "excluded", "tags": [], "event": null, "before_memory": {"category": "other.uncategorized", "source": "excluded"}})),
        "consistent": true, "override": ov.map(|(c, n)| json!({"category": c, "note": n})), "transfer_link": link, "same_merchant_count": same, "item": t.map(|t| item(&s.ds, t, &f))}))
}
fn tx_category(a: &App, r: &Req) -> R {
    let b = r.json()?; let (k, cat, sc) = (b["tx_key"].as_str().unwrap_or(""), b["category"].as_str().unwrap_or(""), b["scope"].as_str().unwrap_or("")); let rules = rules(a)?;
    if !rules.categories.contains_key(cat) { return Err(ApiErr::new(422, "unknown_category", &format!("unknown category {cat:?}"))); }
    let con = a.con()?; if con.query_row("SELECT 1 FROM transactions WHERE tx_key=?", [k], |_| Ok(())).optional().map_err(anyhow::Error::from)?.is_none() { return Err(ApiErr::not_found("no such transaction")); }
    let mkey: Option<String> = con.query_row("SELECT merchant_key FROM tx_enriched WHERE tx_key=?", [k], |r| r.get::<_, Option<String>>(0)).optional()?.flatten().filter(|m| !m.is_empty());
    if sc == "memory" { let by_tx = b["match"].as_str() == Some("transaction") || mkey.is_none(); let mut body = json!({"category": cat, "tags": b["tags"], "event": b["event"], "note": b["note"], "reason": b["note"].as_str().map(String::from).unwrap_or(format!("category fixed in the web app: {cat}"))});
        if by_tx { body["tx_keys"] = json!([k]); } else { body["merchant_key"] = json!(mkey); } let mut resp = super::memory_routes::annotate(a, r, &body)?; let mut v: Value = serde_json::from_slice(&resp.body).unwrap_or_default(); v["scope"] = sc.into(); v["category"] = cat.into(); resp.body = v.to_string().into_bytes(); return Ok(resp); }
    if r.qb("dry_run", false) { let n = if sc == "merchant" { let s = a.snap()?; mkey.as_ref().map_or(0, |m| s.ds.whole().iter().filter(|t| &t.mkey == m).count()) } else { 1 }; return ok(json!({"dry_run": true, "scope": sc, "category": cat, "changed": n > 0, "affected": {"count": n, "blocked": [], "merchant_key": mkey}, "diff": "", "warnings": []})); }
    let (count, mkey) = match sc {
        "transaction" => { con.execute("INSERT INTO tx_overrides(tx_key, category, note) VALUES (?,?,?) ON CONFLICT(tx_key) DO UPDATE SET category=excluded.category, note=excluded.note", params![k, cat, b["note"].as_str()]).map_err(anyhow::Error::from)?; (1, None) }
        "merchant" => { let m: String = con.query_row("SELECT merchant_key FROM tx_enriched WHERE tx_key=?", [k], |r| r.get::<_, Option<String>>(0)).optional().map_err(anyhow::Error::from)?.flatten().filter(|m| !m.is_empty()).ok_or_else(|| ApiErr::rejected("this transaction has no merchant key: fix this transaction only"))?;
            (llm::correct(&con, &rules, &m, cat, None).map_err(ApiErr::rejected)?, Some(m)) }
        _ => return Err(ApiErr::new(422, "validation_error", "the request is not valid: scope: transaction | merchant | memory")),
    };
    a.touch(); ok(json!({"dry_run": false, "scope": sc, "category": cat, "changed": count > 0, "affected": {"count": count, "blocked": [], "merchant_key": mkey}, "diff": "", "warnings": []}))
}
fn transfers_list(a: &App) -> R {
    let con = a.con()?; let rules = rules(a)?; let ann = memory::load_annotations(&a.cfg.memory_dir).0;
    let res = transfers::find_pairs(&con, &rules, &ann, &a.cfg.memory_dir, &transfer_opts(a))?;
    let leg = |l: &transfers::Leg| json!({"tx_key": l.tx_key, "date": l.date, "amount": money_str(to_cents(l.amount)), "account": l.account, "description": l.description});
    let links: Vec<Value> = transfers::list_links(&con)?.into_iter().map(|mut l| { l["amount"] = money_str(to_cents(l["amount"].as_f64().unwrap_or(0.0))).into(); l }).collect();
    ok(json!({"links": links, "proposals": res.linked.iter().chain(&res.proposals).map(|p| json!({"debit": leg(&p.debit), "credit": leg(&p.credit), "confidence": p.confidence, "days": p.days, "topup": p.topup, "strong": p.strong, "scope": if p.household { "household" } else { "own" }, "to_child": p.to_child})).collect::<Vec<_>>(), "ambiguous": res.ambiguous.len()}))
}

// ---------------------------------------------------------------- the logged-in person (/me): derived from the LOGIN, never from a parameter
fn actor(r: &Req) -> String { r.user.as_ref().map_or("ui".into(), |u| if u.id == "owner" { "ui".into() } else { format!("ui:{}", u.id) }) }
fn my_member(a: &App, r: &Req) -> Result<(std::sync::Arc<super::Snap>, String), ApiErr> {
    let s = a.snap()?; let m = r.user.as_ref().and_then(|u| u.member_id.clone()).filter(|m| s.ds.people.get(m).is_some()).ok_or_else(|| ApiErr::new(404, "no_member", "this login is not linked to a household member"))?; Ok((s, m))
}
/// What a child is told about where money came from: never a name.
fn kind_of_source(p: &household::People, src: &str) -> &'static str { if src == household::JOINT || p.role(src) == Some("adult") { "a parent" } else if p.role(src) == Some("child") { "family" } else { "someone else" } }
fn me_summary(a: &App, r: &Req) -> R {
    let (s, m) = my_member(a, r)?; let p = &s.ds.people; let mut rep = household::kid_report(&s.ds, &a.con()?, &m, 6)?;
    for it in rep["extra_topups"]["items"].as_array_mut().into_iter().flatten() { let k = kind_of_source(p, it["source"].as_str().unwrap_or("")); it["source"] = k.into(); it.as_object_mut().unwrap().remove("tx_key"); }
    for x in rep["pocket_money"]["series"].as_array_mut().into_iter().flatten() { let k = kind_of_source(p, x["source"].as_str().unwrap_or("")); x["source"] = k.into(); let o = x.as_object_mut().unwrap(); o.remove("evidence"); o.remove("id"); }
    let mut by: BTreeMap<&str, i64> = BTreeMap::new(); for (src, v) in rep["extra_topups"]["by_source"].as_object().cloned().unwrap_or_default() { *by.entry(kind_of_source(p, &src)).or_default() += cents_of(&v); }
    rep["extra_topups"]["by_source"] = json!(by.into_iter().map(|(k, v)| (k.to_string(), json!(money_str(v)))).collect::<Map<_, _>>()); rep.as_object_mut().unwrap().remove("accounts");
    let budgets: Vec<Value> = household::kid_budget_status(&s.ds, Some(&m)).into_iter().map(|mut b| { b.as_object_mut().unwrap().remove("member"); b }).collect();
    ok(json!({"member": m, "name": p.get(&m).map(|x| x.name.clone()), "report": rep, "budgets": budgets}))
}
fn me_transactions(a: &App, r: &Req) -> R {
    let (s, m) = my_member(a, r)?; let p = &s.ds.people; let (limit, offset) = (r.qi("limit", Some(30), 1, 200)?.unwrap() as usize, r.qi("offset", Some(0), 0, i64::MAX)?.unwrap() as usize);
    let mut rows: Vec<Tx> = s.ds.whole().into_iter().filter(|t| t.person.as_deref() == Some(m.as_str())).collect(); rows.sort_by(|x, y| (y.date, &y.key).cmp(&(x.date, &x.key)));
    let who = |t: &Tx| -> String { let named: Vec<String> = p.mentioned(&format!("{} {} {}", t.entity, t.merchant.as_deref().unwrap_or(""), t.desc)).into_iter().filter(|x| *x != m).collect();
        if let Some(n) = named.first() { return kind_of_source(p, n).into(); }
        if t.category.starts_with("transfer.") || t.ty.as_deref().is_some_and(|x| ["person_transfer_in", "person_transfer_out", "internal_transfer", "wero_in", "wero_out"].contains(&x)) { return if t.category.contains("people") { "someone else" } else { "a transfer" }.into(); }
        t.entity.clone() };
    ok(json!({"total": rows.len(), "limit": limit, "offset": offset, "items": rows.iter().skip(offset).take(limit).map(|t| json!({"date": t.date.to_string(), "amount": money_str(t.amount_c), "category": t.category, "merchant": who(t)})).collect::<Vec<_>>()}))
}
fn put_prefs(a: &App, r: &Req) -> R {
    let u = r.user.clone().unwrap_or_else(super::User::owner); if u.id == "owner" { return Err(ApiErr::new(409, "no_stored_login", "the owner login has no stored preferences: create a per-person login (ui_users)")); }
    let b = r.json()?; let prefs = b["prefs"].as_object().ok_or_else(|| ApiErr::new(422, "validation_error", "the request is not valid: prefs: an object"))?; let s = a.snap()?;
    let mut merged = u.prefs.as_object().cloned().unwrap_or_default();
    for (k, v) in prefs { let v = v.as_str().unwrap_or("").to_string(); let ok = match k.as_str() { "locale" => ["fr-FR", "en-GB", "it-IT"].contains(&v.as_str()), "theme" => ["system", "light", "dark"].contains(&v.as_str()), "landing" => ["dashboard", "transactions", "kids"].contains(&v.as_str()),
            "default_member" => !u.is_child() && (v.is_empty() || v == household::JOINT || s.ds.people.get(&v).is_some()), _ => false };
        if !ok { return Err(ApiErr::rejected(format!("preference {k}: {v:?} is not allowed"))); } merged.insert(k.clone(), v.into()); }
    a.con()?.execute("UPDATE ui_users SET prefs=? WHERE id=?", params![Value::Object(merged.clone()).to_string(), u.id])?; ok(json!({"prefs": merged}))
}

/// Ask the coach: the answer streams as server-sent events from a worker thread (one question at a time).
fn coach_stream(a: &App, r: &Req) -> R {
    let b = r.json()?; let skill = b["skill"].as_str().unwrap_or("ask").to_string(); if crate::coach::spec(&skill).is_none() { return Err(ApiErr::new(422, "validation_error", "the request is not valid: skill: unknown")); }
    let q = b["question"].as_str().map(str::trim).filter(|q| (skill != "ask" || !q.is_empty()) && q.chars().count() <= 4000).map(String::from).ok_or_else(|| ApiErr::new(422, "validation_error", "the request is not valid: question: 1 to 4000 characters"))?;
    let st = crate::coach::status(&a.cfg, a.coach.as_ref(), None); if st["configured"] != true { return Err(ApiErr::new(409, "coach_unavailable", st["message"].as_str().unwrap_or(""))); }
    let id = format!("job_{:08x}", rand::random::<u32>()); let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    { let mut j = a.coach_job.lock().unwrap(); if let Some((cur, _)) = j.as_ref() { return Err(ApiErr::new(409, "coach_busy", &format!("the coach is answering another question ({cur})"))); } *j = Some((id.clone(), cancel.clone())); }
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>(); let sse = |ev: &str, d: &Value| format!("event: {ev}\ndata: {d}\n\n").into_bytes();
    let _ = tx.send(sse("meta", &json!({"job_id": id})));
    let (cfg, insecure, sp) = (a.cfg.clone(), a.insecure, a.coach.clone()); let app = a.me.get().and_then(|w| w.upgrade());
    let work = move |a: &App| { let con = a.con().ok(); let mut emit = |ev: &str, d: Value| { let _ = tx.send(sse(ev, &d)); };
        if let Err(e) = crate::coach::run_skill(&cfg, insecure, &skill, &q, sp.as_ref(), cancel, con.as_ref(), &mut emit) { emit("error", json!({"code": "failed", "message": e.root_cause().to_string().chars().take(300).collect::<String>()})); emit("done", json!({})); }
        *a.coach_job.lock().unwrap() = None; };
    match app { Some(app) => { std::thread::spawn(move || work(&app)); } None => work(a) }
    let mut resp = Resp::with(200, vec![], "text/event-stream"); resp.stream = Some(rx); Ok(resp.header("X-Accel-Buffering", "no"))
}
