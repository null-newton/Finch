//! The MCP memory tools: `open_questions` (redacted), `memory_propose` and `questions_propose`. A proposal changes nothing: it is
//! sealed and queued; the user accepts it themself at a terminal. Pseudonyms in a proposal are turned back into the real ids here.
use crate::{config::Config, memstore::Store, privacy::Redactor, proposals, questions};
use serde_json::{json, Value};

pub const MAX_PROPOSALS: usize = 5;
const MAX_OPS: usize = 20;
/// Pseudonyms (adult-1, account-main-1) back to the real ids, in every string of the operations.
fn reverse(red: &Redactor, v: &Value) -> Value {
    let pairs: Vec<(String, String)> = red.sc.pseudo.iter().filter(|(k, p)| k != p).map(|(k, p)| (p.clone(), k.clone())).chain(red.account.iter().map(|(u, p)| (p.clone(), u.clone()))).collect();
    let fix = |s: &str| pairs.iter().fold(s.to_string(), |acc, (p, real)| regex::Regex::new(&format!(r"(^|[^\w-]){}($|[^\w-])", regex::escape(p))).unwrap().replace_all(&acc, |c: &regex::Captures| format!("{}{real}{}", &c[1], &c[2])).into_owned());
    match v { Value::String(s) => fix(s).into(), Value::Array(a) => Value::Array(a.iter().map(|x| reverse(red, x)).collect()), Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (fix(k), reverse(red, x))).collect()), x => x.clone() }
}
fn clip(v: &Value, depth: usize) -> Result<(), String> {
    if depth > 6 { return Err("a proposed value is nested too deeply".into()); }
    match v { Value::String(s) if s.chars().count() > 2000 => Err("a proposed text value is longer than 2000 characters".into()), Value::Object(o) if o.len() > 40 => Err("a proposed object has too many fields".into()),
        Value::Array(a) if a.len() > 60 => Err("a proposed list is too long".into()), Value::Object(o) => o.values().try_for_each(|x| clip(x, depth + 1)), Value::Array(a) => a.iter().try_for_each(|x| clip(x, depth + 1)), _ => Ok(()) }
}
fn touches_contact(ops: &[Value]) -> bool { ops.iter().any(|o| { let p = o["path"].as_str().unwrap_or(""); p == "contact" || p.starts_with("contact.") || p.starts_with("contact[") || (matches!(o["op"].as_str(), Some("create" | "set")) && o["value"].get("contact").is_some()) }) }
/// A refusal safe to show a model: names pseudonymised, quoted values dropped.
fn neutral(red: &Redactor, e: &anyhow::Error) -> String { let m = red.text(&e.to_string()); let m = crate::rs!(r#"'[^']*'|"[^"]*""#).replace_all(&m, "'...'"); format!("the proposal was refused: {}", m.chars().take(200).collect::<String>()) }

pub fn propose(cfg: &Config, red: &Redactor, a: &Value, suspicious: bool) -> Result<Value, String> {
    let file = red_rev_str(red, a["file"].as_str().unwrap_or("")); let ops: Vec<Value> = reverse(red, &a["ops"]).as_array().cloned().unwrap_or_default(); let reason = a["reason"].as_str().unwrap_or("").trim().to_string();
    if ops.len() > MAX_OPS { return Err(format!("at most {MAX_OPS} operations per proposal")); }
    if file.contains("..") || file.starts_with('.') || file.starts_with('/') || file.contains('\\') { return Err("file must be a relative path of the memory folder".into()); }
    if !Store::editable(&file) { return Err("that is not a memory file finch validates (household.yaml, budgets.yaml, goals.yaml, events.yaml, categorization.yaml, open-questions.yaml, preferences.md, events.md, notes.md); propose the others in the Python app".into()); }
    if file == "household.yaml" && touches_contact(&ops) { return Err("the household's contact details are local only: the user sets them themselves; they cannot be proposed".into()); }
    for o in &ops { clip(&o["value"], 0)?; }
    let evidence: Vec<Value> = if suspicious { let mut seen = vec![]; ops.iter().filter_map(|o| { let p = o["path"].as_str().unwrap_or("(text)").to_string(); (!seen.contains(&p)).then(|| { seen.push(p.clone()); json!({"path": p, "suspicious": true, "snippet": "proposed in a session where tool data contained instruction-like text"}) }) }).collect() } else { vec![] };
    let st = Store { history: false, ..Store::new(cfg, "coach-llm").map_err(|e| e.to_string())? };
    let replacing = ops.iter().any(|o| o["op"] == "replace_text");
    let p = proposals::create(&st, &file, &ops, &reason, "coach-llm", evidence).map_err(|e| if replacing { "the proposal could not be created (details withheld)".into() } else { neutral(red, &e) })?;
    Ok(json!({"proposal_id": p.id, "status": "pending", "file": file, "operations": ops.len(), "suspicious_session": suspicious,
        "next": "Tell the user the proposal id: they review it in the web app (Memory > Proposals) and accept it themselves with `finch memory accept <id>`. You cannot accept it."}))
}
fn red_rev_str(red: &Redactor, s: &str) -> String { reverse(red, &Value::String(s.into())).as_str().unwrap_or(s).to_string() }

pub fn questions_propose(cfg: &Config, red: &Redactor, ds: &crate::analytics::Dataset, a: &Value, suspicious: bool, today: chrono::NaiveDate) -> Result<Value, String> {
    let custom = a["custom"].as_array().cloned().unwrap_or_default(); let series: Vec<String> = a["series"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(); let libs: Vec<String> = a["liabilities"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    if custom.is_empty() && series.is_empty() && libs.is_empty() { return Err("give at least one of series, liabilities or custom".into()); }
    let st = Store::new(cfg, "coach-llm").map_err(|e| e.to_string())?; let existing = st.items(questions::YAML);
    let keys: Vec<String> = existing.iter().filter_map(|q| q["key"].as_str().map(String::from)).collect(); let (mut qs, mut skipped) = (vec![], 0);
    if !series.is_empty() { let rec = crate::analytics::recurring::detect(ds, &crate::analytics::Scope::default(), true); let cs = crate::subs::contracts(&cfg.memory_dir);
        for s in &series { let x = rec.json["series"].as_array().into_iter().flatten().find(|x| x["id"] == s.as_str()).ok_or("unknown series: use a `rec_` id from `recurring` or `subscriptions_inventory`")?; let key = x["key"].as_str().unwrap_or("");
            let c = cs.iter().find(|c| c.merchant_match.as_deref().and_then(|p| fancy_regex::Regex::new(&format!("(?i){p}")).ok()).is_some_and(|r| r.is_match(key).unwrap_or(false)));
            let q = crate::qgen::usage_question(x, red.text(key) != key, c.map(|c| c.id.as_str()), today); if keys.contains(&q["key"].as_str().unwrap_or("").to_string()) { skipped += 1; } else { qs.push(q); } } }
    if !libs.is_empty() { let ls = crate::loans::liabilities(&cfg.memory_dir).0;
        for r in &libs { let l = r.strip_prefix("loan-").and_then(|n| n.parse::<usize>().ok()).and_then(|n| ls.get(n.wrapping_sub(1))).ok_or("unknown liability: use a loan ref (loan-N) from net_worth or loans_overview")?;
            let s = crate::loans::compute(l, today); let miss: Vec<String> = s.missing.clone(); if miss.is_empty() { skipped += 1; continue; } let key = format!("fill:loan-schedule:{}", l.id); if keys.contains(&key) { skipped += 1; continue; }
            qs.push(questions::doc(&json!({"id": crate::qgen::qid("fill", &key), "status": "open", "topic": "Liabilities", "topic_code": "liabilities", "question": format!("Liability {} ({}): to show its amortization schedule I need {}. The loan offer or the latest annual statement has them.", l.id, l.kind, miss.join(", ")),
                "evidence": {"missing": miss}, "suggested_target": {"file": l.file, "field": miss.join(",")}, "key": key, "origin": "coach", "stake": l.monthly_payment.map(|m| m * 12.0), "created": today.to_string()}))); } }
    for c in &custom {
        let text = red.text(c["question"].as_str().unwrap_or("")).trim().to_string(); let key = format!("custom:{}", &crate::util::sha1_hex(&text.to_lowercase())[..10]);
        if keys.contains(&key) || qs.iter().any(|q: &Value| q["key"] == key.as_str()) { skipped += 1; continue; }
        qs.push(questions::doc(&json!({"id": format!("q-custom-{}", &crate::util::sha1_hex(&key)[..10]), "status": "open", "topic": red.text(c["topic"].as_str().unwrap_or("General")).chars().take(40).collect::<String>(), "question": text, "stake": c["stake_eur"], "key": key, "origin": "coach", "created": today.to_string()})));
    }
    if qs.is_empty() { return Ok(json!({"status": "nothing_to_propose", "skipped_already_asked": skipped, "note": "every question was already asked (open, answered or dismissed)"})); }
    let mut ops: Vec<Value> = qs.iter().map(|q| json!({"op": "append", "path": "questions", "value": q})).collect(); if !st.exists(questions::YAML) { ops.insert(0, json!({"op": "create", "value": {"questions": []}})); }
    let mut res = propose(cfg, red, &json!({"file": questions::YAML, "ops": ops, "reason": a["reason"].as_str().unwrap_or("questions the coach needs answered").chars().take(480).collect::<String>()}), suspicious)?;
    res["questions"] = qs.len().into(); res["skipped_already_asked"] = skipped.into(); Ok(res)
}
pub fn open_questions(cfg: &Config, red: &Redactor, a: &Value) -> Result<Value, String> {
    let st = Store::new(cfg, "coach-llm").map_err(|e| e.to_string())?; let qs = questions::listing(&st, Some("open")); let lim = a["limit"].as_u64().unwrap_or(20) as usize;
    let raw = json!({"count": qs.len(), "questions": qs.iter().take(lim).map(|q| json!({"id": q["id"], "question": q["question"], "stake_eur": q["stake"], "target_file": q["suggested_target"]["file"]})).collect::<Vec<_>>(), "note": "To record an answer, propose it with memory_propose; the user decides."});
    Ok(red.walk(&raw, None))
}
