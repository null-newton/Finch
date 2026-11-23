//! The web app's memory endpoints (phase 5b): direct edits by the person at the keyboard (source `ui`, recorded in the history,
//! `dry_run=true` previews the diff), the open questions, the proposals (listed and rejected here; accepting stays at the terminal),
//! the history and the memory check.
use super::{ApiErr, App, Req, R};
use crate::{memstore::{op, EditOpts, EditResult, Store}, proposals, questions};
use serde_json::{json, Map, Value};

fn ok(v: Value) -> R { Ok(super::Resp::json(v)) }
fn store(a: &App, r: &Req) -> Result<Store, ApiErr> { Store::new(&a.cfg, &r.user.as_ref().map_or("ui".into(), |u| if u.id == "owner" { "ui".into() } else { format!("ui:{}", u.id) })).map_err(ApiErr::from) }
fn rej(e: anyhow::Error) -> ApiErr { let m = e.to_string(); if m.contains("git is not installed") || m.starts_with("git ") { ApiErr::new(409, "history_error", &m) } else { ApiErr::new(422, "memory_invalid", &m) } }
fn out(a: &App, res: &EditResult, dry: bool, extra: Value) -> R {
    if !dry && res.changed() { a.touch(); }
    let mut o = json!({"dry_run": dry, "changed": res.changed(), "diff": res.diff, "change_id": res.change_id, "warnings": res.warnings, "warnings_msg": res.warnings.iter().map(|_| Value::Null).collect::<Vec<_>>()});
    if let Some(m) = extra.as_object() { for (k, v) in m { o[k] = v.clone(); } } ok(o)
}
fn edit(a: &App, r: &Req, rel: &str, ops: Vec<Value>, action: &str, reason: Option<String>, detail: &str, extra: Value) -> R {
    let st = store(a, r)?; let dry = r.qb("dry_run", false);
    let mut ops = ops; if !st.exists(rel) && ops.first().is_none_or(|o| o["op"] != "create") { ops.insert(0, json!({"op": "create", "value": {Store::list_key(rel).unwrap_or("items"): []}})); }
    let res = st.edit(rel, &ops, &EditOpts { action, reason: reason.as_deref(), detail: Some(detail), dry_run: dry, ..Default::default() }).map_err(rej)?; out(a, &res, dry, extra)
}
fn num(v: f64) -> Value { if v.fract() == 0.0 && v.abs() < 1e15 { json!(v as i64) } else { json!(v) } }
fn money(b: &Value, k: &str) -> Result<Option<f64>, ApiErr> {
    match &b[k] { Value::Null => Ok(None), Value::Number(n) => Ok(n.as_f64()), Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => s.replace(['€', ' '], "").replace(',', ".").parse::<f64>().map(|x| Some((x * 100.0).round() / 100.0)).map_err(|_| ApiErr::new(422, "bad_amount", &format!("bad {k} {s:?}"))), _ => Err(ApiErr::new(422, "bad_amount", &format!("bad {k}"))) }
}
fn date(b: &Value, k: &str) -> Result<Option<String>, ApiErr> { match b[k].as_str().filter(|s| !s.is_empty()) { None => Ok(None), Some(d) => crate::util::d(d).filter(|x| x.to_string() == d).map(|_| Some(d.to_string())).ok_or_else(|| ApiErr::new(422, "bad_date", &format!("{k}: {d:?} is not a date (use YYYY-MM-DD)"))) } }
fn slug(s: &str) -> Result<(), ApiErr> { if crate::rs!(r"^[a-z0-9][a-z0-9_-]*$").is_match(s) { Ok(()) } else { Err(ApiErr::new(422, "bad_id", "an id is lowercase letters, digits, '-' or '_'")) } }
/// Ops that create or replace the item `id` of the list `key` of household.yaml.
fn upsert(st: &Store, key: &str, id: &str, value: Value) -> Vec<Value> {
    let doc = st.load("household.yaml").ok().map(|d| crate::memstore::jsonify(&d)).unwrap_or(Value::Null);
    match doc[key].as_array() { None => vec![op("set", key, json!([value]))], Some(l) if l.iter().any(|i| i["id"] == id) => vec![op("set", &format!("{key}[{id}]"), value)], Some(_) => vec![op("append", key, value)] }
}
fn exists_in(st: &Store, rel: &str, key: &str, id: &str) -> bool { st.load(rel).ok().map(|d| crate::memstore::jsonify(&d)).is_some_and(|d| d[key].as_array().is_some_and(|l| l.iter().any(|i| i["id"] == id))) }

pub fn route(a: &App, r: &Req, seg: &[&str]) -> Option<R> {
    Some(match (r.method.as_str(), seg) {
        ("GET", ["proposals"]) => (|| { let st = store(a, r)?; let ps = proposals::listing(&st, Some(r.q("status").unwrap_or("pending"))); ok(json!({"proposals": ps.iter().map(|p| proposals::public(&st, p)).collect::<Vec<_>>()})) })(),
        ("POST", ["proposals", id, "reject"]) => (|| { let st = store(a, r)?; let b = r.json()?; let p = proposals::reject(&st, id, b["note"].as_str()).map_err(|e| ApiErr::new(409, "proposal_error", &e.to_string()))?; a.touch(); ok(json!({"id": p.id, "status": p.status})) })(),
        ("POST", ["proposals", _, "accept"]) => Err(ApiErr::new(403, "terminal_only", "accepting a memory proposal is done in a terminal: `finch memory accept <id>`")),
        ("GET", ["questions"]) => (|| { let st = store(a, r)?; let all = questions::listing(&st, None); let s = r.q("status").unwrap_or("open");
            let mut counts = Map::new(); for q in &all { let k = q["status"].as_str().unwrap_or("open").to_string(); counts.insert(k.clone(), json!(counts.get(&k).and_then(Value::as_u64).unwrap_or(0) + 1)); }
            ok(json!({"questions": all.into_iter().filter(|q| s == "all" || q["status"].as_str().unwrap_or("open") == s).map(|mut q| { if let Some(x) = q["stake"].as_f64() { q["stake"] = crate::util::money_str(crate::util::to_cents(x)).into(); } q }).collect::<Vec<_>>(), "counts": counts})) })(),
        ("POST", ["questions", id, what]) => (|| { let st = store(a, r)?; let b = r.json()?; let (dry, today) = (r.qb("dry_run", false), a.today());
            let res = match *what { "answer" => questions::answer(&st, id, b["text"].as_str().or(b["answer"].as_str()).unwrap_or(""), today, dry), "dismiss" => questions::dismiss(&st, id, b["reason"].as_str(), today, dry), "reopen" => questions::reopen(&st, id, dry), _ => return Err(ApiErr::not_found("no such action")) }.map_err(rej)?;
            out(a, &res, dry, json!({"id": id})) })(),
        ("POST", ["budgets"]) => budget(a, r),
        ("POST", ["budgets", id, "delete"]) => (|| { if !exists_in(&store(a, r)?, "budgets.yaml", "budgets", id) { return Err(ApiErr::not_found(&format!("no budget {id:?}"))); } edit(a, r, "budgets.yaml", vec![op("remove", &format!("budgets[{id}]"), Value::Null)], "remove-budget", Some(format!("budget {id} removed")), id, json!({"id": id})) })(),
        ("POST", ["goals"]) => goal(a, r),
        ("GET", ["annotations"]) => (|| { let st = store(a, r)?; let s = a.snap()?; ok(json!({"annotations": st.items("categorization.yaml").into_iter().map(|mut x| {
            let rx = |k: &str| x["match"][k].as_str().and_then(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()); let (mk, de) = (rx("merchant_key"), rx("description")); let keys: Vec<&str> = x["match"]["tx_keys"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).collect();
            let whole = s.ds.whole(); let hit: Vec<&crate::analytics::Tx> = whole.iter().filter(|t| (keys.is_empty() || keys.contains(&t.key.as_str())) && mk.as_ref().is_none_or(|r| r.is_match(&t.mkey).unwrap_or(false)) && de.as_ref().is_none_or(|r| r.is_match(&t.desc).unwrap_or(false))).collect();
            x["matched"] = json!(hit.len()); x["applies_to"] = json!(hit.len()); x["total"] = json!(crate::util::money_str(hit.iter().map(|t| t.amount_c).sum())); x }).collect::<Vec<_>>()})) })(),
        ("POST", ["annotations"]) => (|| { let b = r.json()?; annotate(a, r, &b) })(),
        ("POST", ["annotations", id, "delete"]) => (|| { if !exists_in(&store(a, r)?, "categorization.yaml", "annotations", id) { return Err(ApiErr::not_found(&format!("no annotation {id:?}"))); } edit(a, r, "categorization.yaml", vec![op("remove", &format!("annotations[{id}]"), Value::Null)], "remove-annotation", Some(format!("annotation {id} removed")), id, json!({"id": id})) })(),
        ("PUT", ["household", "attribution", "rules", id]) => (|| { slug(id)?; let b = r.json()?; let m: Map<String, Value> = b["match"].as_object().cloned().unwrap_or_default().into_iter().filter(|(_, v)| !v.is_null() && v != "").collect();
            let mut v = json!({"id": id, "member": b["member"], "match": m}); if let Some(n) = b["note"].as_str().filter(|n| !n.is_empty()) { v["note"] = n.into(); }
            edit(a, r, "household.yaml", upsert(&store(a, r)?, "attribution", id, v), "attribution-rule", b["reason"].as_str().map(String::from), id, json!({"id": id})) })(),
        ("PUT", ["household", "kid-budgets", id]) => (|| { slug(id)?; let b = r.json()?; let mut v = json!({"id": id, "member": b["member"], "period": b["period"], "limit": num(money(&b, "limit")?.unwrap_or(0.0))});
            for k in ["category", "group", "note"] { if let Some(x) = b[k].as_str().filter(|x| !x.is_empty()) { v[k] = x.into(); } }
            edit(a, r, "household.yaml", upsert(&store(a, r)?, "kid_budgets", id, v), "kid-budget", b["reason"].as_str().map(String::from), id, json!({"id": id})) })(),
        ("PUT", ["household", "allocations", id]) => (|| { slug(id)?; let b = r.json()?; let m: Map<String, Value> = b["match"].as_object().cloned().unwrap_or_default().into_iter().filter(|(_, v)| !v.is_null() && v != "").collect();
            let mut v = json!({"id": id, "match": m, "method": b["method"].as_str().unwrap_or("equal")}); if let Some(t) = b["title"].as_str().filter(|x| !x.is_empty()) { v["title"] = t.into(); }
            if b["among"].as_array().is_some_and(|x| !x.is_empty()) { v["among"] = b["among"].clone(); } if b["shares"].as_object().is_some_and(|x| !x.is_empty()) { v["shares"] = b["shares"].clone(); } if let Some(n) = b["note"].as_str().filter(|x| !x.is_empty()) { v["note"] = n.into(); }
            edit(a, r, "household.yaml", upsert(&store(a, r)?, "allocations", id, v), "allocation", b["reason"].as_str().map(String::from), id, json!({"id": id})) })(),
        ("POST", ["household", key @ ("attribution" | "kid-budgets" | "allocations"), ..]) if seg.last() == Some(&"delete") => (|| { let (k, id) = match (*key, seg) { ("attribution", [_, _, "rules", id, _]) => ("attribution", *id), ("kid-budgets", [_, _, id, _]) => ("kid_budgets", *id), ("allocations", [_, _, id, _]) => ("allocations", *id), _ => return Err(ApiErr::not_found("no such item")) };
            if !exists_in(&store(a, r)?, "household.yaml", k, id) { return Err(ApiErr::not_found(&format!("no such item {id:?}"))); }
            edit(a, r, "household.yaml", vec![op("remove", &format!("{k}[{id}]"), Value::Null)], &format!("{}-delete", k.replace('_', "-")), None, id, json!({"id": id})) })(),
        ("PUT", ["household", "members", m, "pocket-money"]) => (|| { let b = r.json()?; let st = store(a, r)?; if !exists_in(&st, "household.yaml", "members", m) { return Err(ApiErr::new(404, "unknown_member", "no such household member")); }
            let path = format!("members[{m}].pocket_money"); let has = crate::memstore::jsonify(&st.load("household.yaml").map_err(ApiErr::from)?)["members"].as_array().and_then(|l| l.iter().find(|x| x["id"] == *m)).is_some_and(|x| !x["pocket_money"].is_null());
            let ops = match money(&b, "amount")? { None if !has => return ok(json!({"dry_run": r.qb("dry_run", false), "changed": false, "diff": "", "change_id": null, "warnings": [], "id": m})), None => vec![op("unset", &path, Value::Null)],
                Some(x) => { let p = b["period"].as_str().filter(|p| ["weekly", "monthly"].contains(p)).ok_or_else(|| ApiErr::new(422, "missing_period", "weekly or monthly"))?; let mut v = json!({"amount": num(x), "period": p}); if let Some(d) = b["day"].as_i64() { v["day"] = d.into(); } vec![op("set", &path, v)] } };
            edit(a, r, "household.yaml", ops, "pocket-money", b["reason"].as_str().map(String::from), m, json!({"id": m})) })(),
        ("GET", ["memory", "history"]) => (|| { let st = store(a, r)?; ok(json!({"enabled": a.cfg.memory_history, "changes": st.repo().log(r.q("file"), r.qi("limit", Some(50), 1, 500)?.unwrap() as usize).map_err(ApiErr::from)?})) })(),
        ("GET", ["memory", "history", id, "diff"]) => (|| { let st = store(a, r)?; ok(json!({"diff": st.repo().diff(id).map_err(|e| ApiErr::not_found(&e.to_string()))?, "revert_command": format!("uv run coach memory revert {id}")})) })(),
        ("GET", ["memory", "check"]) => (|| { let st = store(a, r)?; let issues = check(&st); ok(json!({"summary": summary(&issues), "issues": issues})) })(),
        ("GET", ["memory", "overview"]) => (|| { let st = store(a, r)?; let issues = check(&st); let qs = questions::listing(&st, Some("open")); let n = |f: &str| st.items(f).len();
            let files: Vec<String> = std::fs::read_dir(&st.root).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str().map(String::from)).filter(|f| !f.starts_with('.') && (f.ends_with(".yaml") || f.ends_with(".md"))).collect();
            ok(json!({"files": files, "history_enabled": a.cfg.memory_history, "counts": {"members": n("household.yaml"), "events": n("events.yaml"), "liabilities": 0, "contracts": 0, "assets": 0, "annotations": n("categorization.yaml"), "budgets": n("budgets.yaml"), "goals": n("goals.yaml"),
                "open_questions": qs.len(), "pending_proposals": proposals::listing(&st, Some("pending")).len()}, "check": summary(&issues)})) })(),
        _ => return None,
    })
}
fn check(st: &Store) -> Vec<Value> {
    let mut out = vec![];
    for (f, _) in crate::memstore::FILES { if !st.exists(f) { continue; } match st.validate(f, &st.read_text(f)) { Ok(w) => out.extend(w.into_iter().map(|m| json!({"level": "warning", "code": "check", "file": f, "message": m, "path": "", "line": null, "msg": null}))),
        Err(e) => out.push(json!({"level": "error", "code": "invalid", "file": f, "message": e.to_string(), "path": "", "line": null, "msg": null})) } }
    if questions::md_hand_edited(st) { out.push(json!({"level": "warning", "code": "questions_view", "file": "open-questions.md", "message": "open-questions.md differs from open-questions.yaml (hand edit or out of date)", "path": "", "line": null, "msg": null})); }
    out
}
fn summary(issues: &[Value]) -> Value { let n = |l: &str| issues.iter().filter(|i| i["level"] == l).count(); json!({"errors": n("error"), "warnings": n("warning"), "info": n("info"), "by_code": {}}) }

fn budget(a: &App, r: &Req) -> R {
    let b = r.json()?; let st = store(a, r)?; let target = b["target"].as_str().unwrap_or("");
    let key = if let Some(g) = target.strip_prefix("group:").or((!target.contains('.')).then_some(target)) { if !st.categories.iter().any(|c| c.split('.').next() == Some(g)) { return Err(ApiErr::new(422, "unknown_group", &format!("unknown category group {g:?}"))); } ("group", g.to_string()) }
        else { if !st.categories.contains(target) { return Err(ApiErr::new(422, "unknown_category", &format!("unknown category {target:?}"))); } ("category", target.to_string()) };
    let amount = money(&b, "monthly")?.filter(|x| *x > 0.0).ok_or_else(|| ApiErr::new(422, "bad_amount", "the monthly amount must be positive"))?;
    let (owner, account) = (b["owner"].as_str().filter(|x| !x.is_empty()), b["account"].as_str().filter(|x| !x.is_empty()));
    let items = st.items("budgets.yaml");
    let same = items.iter().find(|x| x[key.0] == key.1.as_str() && x[if key.0 == "group" { "category" } else { "group" }].is_null() && x["owner"].as_str() == owner && x["account"].as_str() == account);
    let ids: Vec<String> = items.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
    let base = if key.0 == "group" { format!("group-{}", key.1) } else { key.1.replace(['.', '_'], "-") };
    let bid = b["id"].as_str().map(String::from).or(same.and_then(|x| x["id"].as_str().map(String::from))).unwrap_or_else(|| std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find(|c| !ids.contains(c)).unwrap());
    slug(&bid)?; let clash = items.iter().find(|x| x["id"] == bid.as_str()); let retarget = clash.is_some_and(|c| c[key.0] != key.1.as_str());
    if retarget && !b["replace"].as_bool().unwrap_or(false) { return Err(ApiErr::new(409, "budget_exists", &format!("budget {bid:?} already exists for another target"))); }
    let mut f = Map::new(); if clash.is_none() || retarget { f.insert(key.0.into(), key.1.clone().into()); } f.insert("monthly".into(), num(amount));
    if let Some(x) = b["rollover"].as_bool() { f.insert("rollover".into(), x.into()); } if let Some(o) = owner { f.insert("owner".into(), o.into()); } if let Some(x) = account { f.insert("account".into(), x.into()); }
    if let Some(s) = date(&b, "start")? { f.insert("start".into(), s.into()); } else if b["rollover"] == true && clash.is_none_or(|c| c["start"].is_null()) { f.insert("start".into(), crate::util::month_start(&crate::util::month_key(a.today())).to_string().into()); }
    if let Some(n) = b["note"].as_str().filter(|x| !x.is_empty()) { f.insert("note".into(), n.into()); }
    let ops = if clash.is_some() && !retarget { f.into_iter().map(|(k, v)| op("set", &format!("budgets[{bid}].{k}"), v)).collect() } else { let mut o = vec![]; if retarget { o.push(op("remove", &format!("budgets[{bid}]"), Value::Null)); } let mut v = Map::new(); v.insert("id".into(), bid.clone().into()); v.extend(f); o.push(op("append", "budgets", Value::Object(v))); o };
    edit(a, r, "budgets.yaml", ops, "set-budget", Some(b["reason"].as_str().map(String::from).unwrap_or_else(|| format!("budget {bid}: {amount:.2} EUR/month"))), &bid.clone(), json!({"id": bid}))
}
fn goal(a: &App, r: &Req) -> R {
    let b = r.json()?; let st = store(a, r)?; let id = b["id"].as_str().unwrap_or("").to_string(); slug(&id)?; let exists = exists_in(&st, "goals.yaml", "goals", &id);
    let mut f = Map::new();
    match money(&b, "target")? { Some(x) if x > 0.0 => { f.insert("target_amount".into(), num(x)); } Some(_) => return Err(ApiErr::new(422, "bad_amount", "target must be positive")), None if !exists => return Err(ApiErr::new(422, "missing_target", "a new goal needs a target amount")), None => {} }
    for k in ["asset", "account", "tag", "title"] { if let Some(x) = b[k].as_str().filter(|x| !x.is_empty()) { f.insert(k.into(), x.into()); } }
    if let Some(d) = date(&b, "date")? { f.insert("target_date".into(), d.into()); }
    for (k, n) in [("monthly", "monthly_contribution"), ("baseline", "baseline")] { if let Some(x) = money(&b, k)? { if x < 0.0 { return Err(ApiErr::new(422, "bad_amount", &format!("{k} must not be negative"))); } f.insert(n.into(), num(x)); } }
    if let Some(s) = date(&b, "start")? { f.insert("start".into(), s.into()); } else if b["tag"].is_string() && !exists { f.insert("start".into(), a.today().to_string().into()); }
    let ops = if exists { f.into_iter().map(|(k, v)| op("set", &format!("goals[{id}].{k}"), v)).collect() } else { let mut v = Map::new(); v.insert("id".into(), id.clone().into()); v.extend(f); vec![op("append", "goals", Value::Object(v))] };
    edit(a, r, "goals.yaml", ops, "set-goal", Some(b["reason"].as_str().map(String::from).unwrap_or_else(|| format!("goal {id}"))), &id.clone(), json!({"id": id}))
}
/// A memory annotation (categorization.yaml) on transactions or a merchant key; an annotation with the same match is updated.
pub fn annotate(a: &App, r: &Req, b: &Value) -> R {
    let st = store(a, r)?; let cat = b["category"].as_str().filter(|x| !x.is_empty()); let tags: Vec<String> = b["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(|t| t.trim().to_string())).filter(|t| !t.is_empty()).collect();
    if cat.is_none() && tags.is_empty() && b["event"].as_str().is_none_or(str::is_empty) { return Err(ApiErr::new(422, "no_effect", "an annotation needs a category, tags or an event")); }
    if let Some(c) = cat { if !st.categories.contains(c) { return Err(ApiErr::new(422, "unknown_category", &format!("unknown category {c:?}"))); } }
    let keys: Vec<String> = b["tx_keys"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(String::from)).collect(); let mut m = Map::new();
    if !keys.is_empty() { let con = a.con()?; let unknown = keys.iter().filter(|k| con.query_row("SELECT 1 FROM transactions WHERE tx_key=?", [k], |_| Ok(())).is_err()).count(); if unknown > 0 { return Err(ApiErr::new(422, "unknown_transaction", &format!("{unknown} transaction key(s) do not exist"))); } m.insert("tx_keys".into(), json!(keys)); }
    if let Some(k) = b["merchant_key"].as_str().filter(|x| !x.is_empty()) { m.insert("merchant_key".into(), format!("^{}$", regex::escape(k)).into()); }
    if m.is_empty() { return Err(ApiErr::new(422, "no_match", "say which transactions: tx_keys or merchant_key")); }
    let mut ann = Map::new(); if let Some(c) = cat { ann.insert("category".into(), c.into()); } if !tags.is_empty() { ann.insert("tags".into(), json!(tags)); }
    for k in ["event", "note"] { if let Some(x) = b[k].as_str().filter(|x| !x.is_empty()) { ann.insert(k.into(), x.into()); } }
    let items = st.items("categorization.yaml"); let same = b["id"].is_null().then(|| items.iter().find(|x| x["match"] == Value::Object(m.clone()))).flatten();
    let (ops, id) = match same { Some(s) => { let id = s["id"].as_str().unwrap_or("").to_string(); (ann.into_iter().map(|(k, v)| op("set", &format!("annotations[{id}].{k}"), v)).collect::<Vec<_>>(), id) }
        None => { let ids: Vec<String> = items.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
            let base = b["id"].as_str().map(String::from).unwrap_or_else(|| format!("ui-{}", cat.unwrap_or("note").replace(['.', '_'], "-"))); slug(&base)?;
            let id = if b["id"].is_string() { base } else { std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find(|c| !ids.contains(c)).unwrap() };
            let mut v = Map::new(); v.insert("id".into(), id.clone().into()); v.insert("match".into(), Value::Object(m)); v.extend(ann); (vec![op("append", "annotations", Value::Object(v))], id) } };
    edit(a, r, "categorization.yaml", ops, "annotate", Some(b["reason"].as_str().or(b["note"].as_str()).unwrap_or("annotation from the web app").to_string()), &id.clone(), json!({"id": id, "affected": {"count": null}}))
}
