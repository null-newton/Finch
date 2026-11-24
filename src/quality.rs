//! Quality (E12): the gold set (transactions whose category the USER confirmed, marked by origin, never invented), the offline
//! evaluation of the classifier and the pipeline against it, the checks of the coach's stored answers, the plan (and, at a terminal,
//! the run) of a model comparison, and the `eval_runs` history. Nothing here changes a category of the app.
use crate::{classify::{llm, rules::{categorised, resolve, Categorised, Lookups, Rules}}, config::Config, memory::Annotation, util::*};
use anyhow::{bail, Result};
use rand::{Rng, SeedableRng};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const ORIGINS: [&str; 5] = ["manual", "merchant_label", "annotation", "override", "split"];
const UNCAT: &str = "other.uncategorized";
const AUTO: [&str; 6] = ["llm", "knn", "entity", "rule", "label", "none"];
fn rank(o: &str) -> usize { ORIGINS.iter().position(|x| *x == o).unwrap_or(9) }
fn labeler(o: &str) -> &'static str { match o { "annotation" => "memory", "override" | "split" => "correction", _ => "user" } }
fn label_source(o: &str) -> &'static str { match o { "annotation" => "memory", "override" => "override", "split" => "split", _ => "user" } }

pub fn set_gold(con: &Connection, rules: &Rules, tx_key: &str, cat: &str, labeled_by: &str, origin: &str, note: Option<&str>) -> Result<()> {
    if cat == UNCAT || !rules.categories.contains_key(cat) { bail!("{cat:?} is not a category of the taxonomy that can be a label (see `finch taxonomy`)"); }
    if !["user", "memory", "correction"].contains(&labeled_by) || !ORIGINS.contains(&origin) { bail!("bad labeler or origin"); }
    if con.query_row("SELECT 1 FROM transactions WHERE tx_key=?", [tx_key], |_| Ok(())).optional()?.is_none() { bail!("no transaction {tx_key:?}"); }
    con.execute("INSERT INTO gold_labels(tx_key, category, labeled_by, labeled_at, note, origin) VALUES (?,?,?,?,?,?) ON CONFLICT(tx_key) DO UPDATE SET category=excluded.category, labeled_by=excluded.labeled_by, labeled_at=excluded.labeled_at, note=excluded.note, origin=excluded.origin",
        params![tx_key, cat, labeled_by, now_iso(), note.map(|n| n.chars().take(300).collect::<String>()), origin])?; Ok(())
}
pub fn remove_gold(con: &Connection, tx_key: &str) -> Result<bool> { Ok(con.execute("DELETE FROM gold_labels WHERE tx_key=?", [tx_key])? > 0) }
/// (tx_key, category, labeled_by, origin, note)
pub fn gold_rows(con: &Connection) -> Result<Vec<(String, String, String, String, Option<String>)>> { Ok(con.prepare("SELECT tx_key, category, labeled_by, origin, note FROM gold_labels ORDER BY tx_key")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?) }
pub fn counts(con: &Connection) -> Result<Value> {
    let g = gold_rows(con)?; let (mut by, mut lb): (BTreeMap<String, usize>, BTreeMap<String, usize>) = Default::default(); for r in &g { *by.entry(r.3.clone()).or_default() += 1; *lb.entry(r.2.clone()).or_default() += 1; }
    Ok(json!({"total": g.len(), "by_origin": by, "by_labeled_by": lb, "categories": g.iter().map(|r| &r.1).collect::<HashSet<_>>().len()}))
}
/// Refreshes the rows the user's own decisions support (overrides, splits, annotations, merchant labels); a hand label is never touched.
pub fn bootstrap(con: &Connection, rules: &Rules, anns: &[Annotation], dry_run: bool) -> Result<Value> {
    let mut want: HashMap<String, (String, String, Option<String>)> = HashMap::new();
    let mut offer = |k: &str, c: &str, o: &str, note: Option<String>| { if c == UNCAT || !rules.categories.contains_key(c) { return; } if want.get(k).is_none_or(|w| rank(o) < rank(&w.1)) { want.insert(k.into(), (c.into(), o.into(), note)); } };
    for r in categorised(con, rules, anns, true)?.iter().filter(|r| r.split_index.is_none()) { match r.source.as_str() { "override" => offer(&r.tx_key, &r.category, "override", None), "memory" => offer(&r.tx_key, &r.category, "annotation", r.annotation.as_ref().map(|a| format!("annotation {a}"))), "user" => offer(&r.tx_key, &r.category, "merchant_label", None), _ => {} } }
    for (k, c) in con.prepare("SELECT o.tx_key, o.category FROM tx_overrides o JOIN transactions t USING(tx_key) WHERE o.category IS NOT NULL")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()? { offer(&k, &c, "override", None); }
    let mut parts: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new(); for (k, a, c) in con.prepare("SELECT s.tx_key, s.amount, s.category FROM tx_splits s JOIN transactions t USING(tx_key) ORDER BY s.id")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()? { parts.entry(k).or_default().push((a.abs(), c)); }
    for (k, ps) in &parts { let top = ps.iter().max_by(|a, b| a.0.total_cmp(&b.0)).unwrap(); offer(k, &top.1, "split", Some(format!("dominant part of {}", ps.len()))); }
    let have: HashMap<String, (String, String)> = gold_rows(con)?.into_iter().map(|r| (r.0, (r.1, r.3))).collect(); let (mut add, mut upd, mut rem) = (BTreeMap::<String, usize>::new(), BTreeMap::<String, usize>::new(), BTreeMap::<String, usize>::new());
    for (k, (c, o, n)) in &want { match have.get(k) { Some((_, ho)) if ho == "manual" => continue, None => *add.entry(o.clone()).or_default() += 1, Some((hc, ho)) if hc != c || ho != o => *upd.entry(o.clone()).or_default() += 1, _ => continue } if !dry_run { set_gold(con, rules, k, c, labeler(o), o, n.as_deref())?; } }
    for (k, (_, o)) in &have { if o != "manual" && !want.contains_key(k) { *rem.entry(o.clone()).or_default() += 1; if !dry_run { remove_gold(con, k)?; } } }
    Ok(json!({"added": add, "updated": upd, "removed": rem, "manual_kept": have.values().filter(|h| h.1 == "manual").count(), "dry_run": dry_run, "gold": if dry_run { Value::Null } else { counts(con)? }}))
}
/// Transactions worth labelling next (automatic decisions, not in the gold set): by money (probability by amount) or stratified.
pub fn sample(con: &Connection, rules: &Rules, anns: &[Annotation], n: usize, strategy: &str, seed: u64) -> Result<Vec<Value>> {
    let gold: HashSet<String> = gold_rows(con)?.into_iter().map(|r| r.0).collect(); let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let c: Vec<Categorised> = categorised(con, rules, anns, false)?.into_iter().filter(|r| r.split_index.is_none() && !gold.contains(&r.tx_key) && AUTO.contains(&r.source.as_str()) && r.amount != 0.0).collect();
    let pick: Vec<&Categorised> = match strategy {
        "money" => { let mut k: Vec<(f64, &Categorised)> = c.iter().map(|r| (rng.gen::<f64>().powf(1.0 / r.amount.abs()), r)).collect(); k.sort_by(|a, b| b.0.total_cmp(&a.0)); k.into_iter().take(n).map(|x| x.1).collect() }
        "stratified" => { let mut g: BTreeMap<(String, String), Vec<&Categorised>> = BTreeMap::new(); for r in &c { g.entry((r.source.clone(), r.category.split('.').next().unwrap_or("").into())).or_default().push(r); }
            for v in g.values_mut() { for i in (1..v.len()).rev() { v.swap(i, rng.gen_range(0..=i)); } } let mut out = vec![]; let mut i = 0; while out.len() < n && g.values().any(|v| v.len() > i) { for v in g.values() { if let Some(x) = v.get(i) { if out.len() < n { out.push(*x); } } } i += 1; } out }
        s => bail!("strategy must be money or stratified, not {s:?}") };
    Ok(pick.into_iter().map(|r| json!({"tx_key": r.tx_key, "date": r.date, "amount": r.amount, "description": r.desc, "account": r.account, "merchant_key": r.key, "merchant": r.merchant.as_deref().unwrap_or(&r.key), "category": r.category, "predicted": r.category, "source": r.source, "confidence": Value::Null, "type": r.ty})).collect())
}

fn ratio(a: usize, b: usize) -> Value { if b == 0 { Value::Null } else { json!(((a as f64 / b as f64) * 10000.0).round() / 10000.0) } }
fn ratio_c(a: i64, b: i64) -> Value { if b == 0 { Value::Null } else { json!(((a as f64 / b as f64) * 10000.0).round() / 10000.0) } }
/// One scored row: (gold, predicted, amount cents, source).
type Row = (String, String, i64, String);
pub fn metrics(rows: &[Row]) -> Value {
    let n = rows.len(); let tot: i64 = rows.iter().map(|r| r.2.abs()).sum(); let ok: Vec<&Row> = rows.iter().filter(|r| r.0 == r.1).collect(); let okc: i64 = ok.iter().map(|r| r.2.abs()).sum();
    let cov: Vec<&Row> = rows.iter().filter(|r| r.1 != UNCAT).collect(); let grp = rows.iter().filter(|r| r.0.split('.').next() == r.1.split('.').next()).count();
    let cats: std::collections::BTreeSet<&String> = rows.iter().flat_map(|r| [&r.0, &r.1]).collect(); let mut per = vec![]; let (mut f1s, mut wsum, mut sup_all) = (vec![], 0.0, 0usize);
    for c in cats { let tp = rows.iter().filter(|r| &r.0 == c && &r.1 == c).count(); let fp = rows.iter().filter(|r| &r.1 == c && &r.0 != c).count(); let fn_ = rows.iter().filter(|r| &r.0 == c && &r.1 != c).count(); let sup = tp + fn_;
        let f1 = ratio(2 * tp, 2 * tp + fp + fn_); if sup > 0 { let f = f1.as_f64().unwrap_or(0.0); f1s.push(f); wsum += f * sup as f64; sup_all += sup; }
        per.push(json!({"category": c, "support": sup, "predicted": tp + fp, "precision": ratio(tp, tp + fp), "recall": ratio(tp, sup), "f1": f1, "money": money_str(rows.iter().filter(|r| &r.0 == c).map(|r| r.2.abs()).sum())})); }
    per.sort_by(|a, b| b["support"].as_u64().cmp(&a["support"].as_u64()).then(a["category"].as_str().cmp(&b["category"].as_str())));
    let mut pairs: BTreeMap<(String, String), (usize, i64)> = BTreeMap::new(); for r in rows.iter().filter(|r| r.0 != r.1) { let e = pairs.entry((r.0.clone(), r.1.clone())).or_default(); e.0 += 1; e.1 += r.2.abs(); }
    let mut conf: Vec<_> = pairs.into_iter().collect(); conf.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(b.1 .1.cmp(&a.1 .1)));
    let r4 = |x: f64| (x * 10000.0).round() / 10000.0;
    json!({"n": n, "correct": ok.len(), "accuracy_tx": ratio(ok.len(), n), "money": money_str(tot), "money_correct": money_str(okc), "accuracy_money": ratio_c(okc, tot), "group_accuracy_tx": ratio(grp, n), "coverage_tx": ratio(cov.len(), n), "coverage_money": ratio_c(cov.iter().map(|r| r.2.abs()).sum(), tot),
        "macro_f1": (!f1s.is_empty()).then(|| r4(f1s.iter().sum::<f64>() / f1s.len() as f64)), "weighted_f1": (sup_all > 0).then(|| r4(wsum / sup_all as f64)), "categories": per,
        "confusions": conf.into_iter().take(10).map(|((g, p), (k, m))| json!({"gold": g, "predicted": p, "n": k, "money": money_str(m)})).collect::<Vec<_>>()})
}
fn short(m: &Value) -> Value { let mut o = m.clone(); if let Some(x) = o.as_object_mut() { x.remove("categories"); x.remove("confusions"); } o }
fn context_free(a: &Annotation) -> bool { let m = &a.m; m.category_in.is_none() && m.weekdays.is_none() && m.tx_keys.is_none() && m.amount_min.is_none() && m.amount_max.is_none() && m.date_from.is_none() && m.date_to.is_none() }
fn fingerprint(con: &Connection, rules: &Rules) -> Result<String> { let g = gold_rows(con)?; Ok(sha256_hex(format!("{:?}|{:?}|{}|{:?}", rules.categories, rules.type_rules, rules.merchant.len(), g.iter().map(|r| (&r.0, &r.1, &r.3)).collect::<Vec<_>>()).as_bytes())[..16].to_string()) }

/// The classifier (memory off, without the user's own decision the label comes from) on merchant truths, and the pipeline as shown.
pub fn evaluate(con: &Connection, cfg: &Config, rules: &Rules, store: bool) -> Result<Value> {
    let t0 = std::time::Instant::now(); let (anns, _) = crate::memory::load_annotations(&cfg.memory_dir); let golds = gold_rows(con)?; if golds.is_empty() { bail!("the gold set is empty: run `finch eval gold bootstrap` or label transactions with `finch eval gold label`"); }
    let all: HashMap<String, Categorised> = categorised(con, rules, &anns, true)?.into_iter().filter(|r| r.split_index.is_none() || r.split_index == Some(0)).map(|r| (r.tx_key.clone(), r)).collect();
    let purpose: HashMap<String, Option<String>> = con.prepare("SELECT uid, purpose FROM accounts")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut no_ov = Lookups::load(con)?; no_ov.overrides.clear(); let mut no_user = Lookups::load(con)?; no_user.merchants.retain(|_, v| v.1 != "user");
    let mut parts: HashMap<String, Vec<(f64, String)>> = HashMap::new(); for (k, a, c) in con.prepare("SELECT tx_key, amount, category FROM tx_splits ORDER BY id")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()? { parts.entry(k).or_default().push((a.abs(), c)); }
    let ann_of: HashMap<&str, &Annotation> = anns.iter().map(|a| (a.id.as_str(), a)).collect();
    let (mut cls, mut taut, mut nocf, mut pipe, mut by_src, mut missing) = (vec![], (0usize, 0usize), 0usize, vec![], BTreeMap::<String, Vec<Row>>::new(), 0usize);
    for (k, gold, _by, origin, note) in &golds {
        let Some(r) = all.get(k) else { missing += 1; continue }; let pu = purpose.get(&r.account).cloned().flatten(); let amt = to_cents(r.amount);
        let (pred, src) = match origin.as_str() { "override" => { let (c, s) = resolve(&no_ov, rules, k, r.ty.as_deref(), &r.key, pu.as_deref(), Some(r.amount)); (c, s.to_string()) } "merchant_label" => { let (c, s) = resolve(&no_user, rules, k, r.ty.as_deref(), &r.key, pu.as_deref(), Some(r.amount)); (c, s.to_string()) } _ => (r.pre_category.clone(), r.pre_source.clone()) };
        let fin = parts.get(k).map(|p| p.iter().max_by(|a, b| a.0.total_cmp(&b.0)).unwrap().1.clone()).unwrap_or_else(|| r.category.clone()); pipe.push((gold.clone(), fin, amt, r.source.clone()));
        let truth = match origin.as_str() { "manual" | "merchant_label" => true, "annotation" => note.as_deref().and_then(|n| n.strip_prefix("annotation ")).and_then(|i| ann_of.get(i.trim())).is_some_and(|a| context_free(a)), _ => false };
        if !truth { continue; } if src == label_source(origin) { taut.0 += 1; if &pred == gold { taut.1 += 1; } continue; } if origin == "merchant_label" && src == "none" { nocf += 1; continue; }
        let row = (gold.clone(), pred, amt, src.clone()); by_src.entry(src).or_default().push(row.clone()); cls.push(row);
    }
    let fp = fingerprint(con, rules)?; let head = metrics(&cls);
    let res = json!({"headline": head, "by_source": by_src.iter().map(|(s, v)| (s.clone(), short(&metrics(v)))).collect::<serde_json::Map<_, _>>(), "pipeline": short(&metrics(&pipe)), "missing": missing, "fingerprint": fp,
        "tautological": {"n": taut.0, "agree": taut.1, "note": "gold label and prediction come from the same layer: left out of the classifier figures"}, "no_counterfactual": {"n": nocf, "note": "merchants you labelled yourself that nothing else would have classified: nothing to score"}, "gold": counts(con)?});
    let prev = last_run(con, "classify")?; let mut out = res; out["regressions"] = regressions(&out, prev.as_ref()).into();
    let (h, pl) = (&out["headline"], &out["pipeline"]); let llm_rows: Vec<Row> = cls.iter().filter(|r| r.3 == "llm").cloned().collect(); let lo = metrics(&llm_rows);
    let summary = json!({"method": "v2", "n": h["n"], "accuracy_tx": h["accuracy_tx"], "accuracy_money": h["accuracy_money"], "group_accuracy_tx": h["group_accuracy_tx"], "coverage_tx": h["coverage_tx"], "macro_f1": h["macro_f1"], "llm_accuracy_tx": lo["accuracy_tx"], "llm_accuracy_money": lo["accuracy_money"], "llm_n": lo["n"],
        "pipeline_n": pl["n"], "pipeline_accuracy_tx": pl["accuracy_tx"], "pipeline_accuracy_money": pl["accuracy_money"], "gold_total": out["gold"]["total"]}); out["summary"] = summary.clone(); out["method"] = "v2".into();
    if store { con.execute("INSERT INTO eval_runs(ts, kind, label, backend, model, n, fingerprint, cost_usd, duration_s, summary, result) VALUES (?,?,?,?,?,?,?,?,?,?,?)", params![now_iso(), "classify", "offline", Option::<String>::None, Option::<String>::None, cls.len() as i64, fp, 0.0, t0.elapsed().as_secs_f64(), summary.to_string(), out.to_string()])?; }
    Ok(out)
}
fn last_run(con: &Connection, kind: &str) -> Result<Option<Value>> { Ok(con.query_row("SELECT result FROM eval_runs WHERE kind=? ORDER BY id DESC LIMIT 1", [kind], |r| r.get::<_, String>(0)).optional()?.and_then(|s| serde_json::from_str(&s).ok())) }
/// Headline figures that dropped by more than one point since the previous run (on the same inputs or not: said in `fingerprint`).
pub fn regressions(now: &Value, before: Option<&Value>) -> Vec<String> {
    let Some(b) = before else { return vec![] }; ["accuracy_tx", "accuracy_money", "macro_f1", "coverage_tx"].iter().filter_map(|k| { let (x, y) = (now["headline"][k].as_f64()?, b["headline"][k].as_f64()?); (x + 0.01 < y).then(|| format!("{k}: {y:.3} -> {x:.3}{}", if now["fingerprint"] != b["fingerprint"] { " (inputs changed)" } else { "" })) }).collect()
}
pub fn runs(con: &Connection, kind: Option<&str>, limit: usize) -> Result<Vec<Value>> {
    Ok(con.prepare("SELECT id, ts, kind, label, backend, model, n, fingerprint, cost_usd, duration_s, summary FROM eval_runs WHERE ?1 IS NULL OR kind=?1 ORDER BY id DESC LIMIT ?2")?.query_map(params![kind, limit as i64], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "label": r.get::<_, Option<String>>(3)?, "backend": r.get::<_, Option<String>>(4)?, "model": r.get::<_, Option<String>>(5)?, "n": r.get::<_, i64>(6)?, "fingerprint": r.get::<_, Option<String>>(7)?, "cost_usd": r.get::<_, Option<f64>>(8)?, "duration_s": r.get::<_, Option<f64>>(9)?, "summary": serde_json::from_str::<Value>(&r.get::<_, String>(10)?).unwrap_or_default()})))?.collect::<rusqlite::Result<_>>()?)
}

/// The coach's stored answers checked offline: no unverified amount, no product-advice flag, the word limit of the skill, the disclaimer and the three actions where the skill asks for them.
pub fn coach_offline(con: &Connection, skill: Option<&str>, limit: usize, store: bool) -> Result<Value> {
    let items: Vec<(String, String, Option<String>, String)> = con.prepare("SELECT id, body, skill, unverified_numbers FROM insights WHERE ?1 IS NULL OR skill=?1 ORDER BY created DESC LIMIT ?2")?.query_map(params![skill, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let lim = |s: &str| match s { "digest-weekly" => Some(180), "digest-monthly" | "subscription-audit" => Some(300), "mortgage-check" => Some(250), _ => None };
    let scored: Vec<Value> = items.iter().map(|(id, body, sk, un)| { let sk = sk.as_deref().unwrap_or("ask"); let words = body.split_whitespace().count(); let low = body.to_lowercase();
        let actions = low.find("three actions").map(|i| body[i..].lines().skip(1).filter(|l| crate::rs!(r"^\s*\d+[.)]\s").is_match(l)).count());
        let checks = json!({"numbers_verified": serde_json::from_str::<Vec<Value>>(un).map(|v| v.is_empty()).unwrap_or(true), "no_product_advice": crate::coach::compliance(body)["ok"] == true, "length": lim(sk).map(|l| words as f64 <= l as f64 * 1.2),
            "disclaimer": (["subscription-audit", "mortgage-check"].contains(&sk)).then(|| low.contains("not financial advice") || low.contains("geen financieel advies") || low.contains("pas un conseil financier")), "three_actions": (sk == "digest-monthly").then(|| actions == Some(3))});
        let pass = checks.as_object().unwrap().values().all(|v| v.as_bool() != Some(false)); json!({"id": id, "skill": sk, "words": words, "checks": checks, "pass": pass}) }).collect();
    let n = scored.len(); let ok = scored.iter().filter(|s| s["pass"] == true).count(); let mut per: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for s in &scored { for (k, v) in s["checks"].as_object().unwrap() { if let Some(b) = v.as_bool() { let e = per.entry(k.clone()).or_default(); e.0 += 1; if b { e.1 += 1; } } } }
    let res = json!({"n": n, "passed": ok, "pass_rate": ratio(ok, n), "by_check": per.iter().map(|(k, v)| (k.clone(), json!({"n": v.0, "passed": v.1, "rate": ratio(v.1, v.0)}))).collect::<serde_json::Map<_, _>>(), "items": scored});
    if store && n > 0 { con.execute("INSERT INTO eval_runs(ts, kind, label, n, summary, result) VALUES (?,?,?,?,?,?)", params![now_iso(), "coach", "offline", n as i64, json!({"pass_rate": res["pass_rate"], "n": n}).to_string(), res.to_string()])?; }
    Ok(res)
}

/// The merchant truths a model comparison can score: merchant key -> gold category (the most frequent one).
fn model_targets(con: &Connection, cfg: &Config, rules: &Rules) -> Result<Vec<(String, String)>> {
    let (anns, _) = crate::memory::load_annotations(&cfg.memory_dir); let all: HashMap<String, Categorised> = categorised(con, rules, &anns, true)?.into_iter().map(|r| (r.tx_key.clone(), r)).collect(); let mut by: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for (k, c, _, o, _) in gold_rows(con)? { if !["manual", "merchant_label"].contains(&o.as_str()) { continue; } if let Some(r) = all.get(&k).filter(|r| !r.key.is_empty()) { *by.entry(r.key.clone()).or_default().entry(c).or_default() += 1; } }
    let ok = llm::eligible_keys(con, cfg, &by.keys().cloned().collect::<Vec<_>>())?; Ok(by.into_iter().filter(|(k, _)| ok.contains(k)).map(|(k, m)| (k, m.into_iter().max_by_key(|x| x.1).unwrap().0)).collect())
}
/// What a comparison of `models` would send and cost (nothing is sent).
pub fn models_plan(con: &Connection, cfg: &Config, rules: &Rules, models: &[String], batch: usize) -> Result<Value> {
    let t = model_targets(con, cfg, rules)?; let keys: Vec<String> = t.iter().map(|x| x.0.clone()).collect(); let jobs = llm::prepare_jobs(con, cfg, rules, &keys, batch, None)?;
    let tin: i64 = jobs.iter().map(|j| ((j.stat.len() + j.dynamic.len()) / 4) as i64).sum(); let tout = keys.len() as i64 * 40;
    Ok(json!({"merchants": keys.len(), "requests_per_model": jobs.len(), "backend": cfg.llm_backend, "estimated_tokens_in_per_model": tin, "estimated_tokens_out_per_model": tout,
        "models": models.iter().map(|m| { let id = llm::model_id(m); json!({"model": id, "estimated_cost_usd": llm::estimate_cost(&id, tin, tout, 0, 0)}) }).collect::<Vec<_>>(), "note": "estimates (4 characters a token); the merchants are sent redacted as for `finch classify llm`; nothing is stored in merchants"}))
}
/// Runs the comparison: each model labels the gold merchants; its labels are scored and stored in eval_runs only.
pub fn models_run(con: &Connection, cfg: &Config, rules: &Rules, be: &llm::Backend, models: &[String], batch: usize) -> Result<Vec<Value>> {
    let t = model_targets(con, cfg, rules)?; let keys: Vec<String> = t.iter().map(|x| x.0.clone()).collect(); let gold: HashMap<&str, &str> = t.iter().map(|(k, c)| (k.as_str(), c.as_str())).collect(); let jobs = llm::prepare_jobs(con, cfg, rules, &keys, batch, None)?; let schema = llm::label_schema(rules); let mut out = vec![];
    for m in models { let t0 = std::time::Instant::now(); let (mut rows, mut cost, mut fail): (Vec<Row>, f64, usize) = (vec![], 0.0, 0);
        for j in &jobs { match be.complete(&j.stat, &j.dynamic, &schema, m, "eval", j.keys.len()) { Ok((d, u)) => { llm::record_usage(con, &u)?; cost += u.cost_usd.unwrap_or(0.0); let got: HashMap<usize, String> = llm::validate_results(&d["results"], j.keys.len(), rules).into_iter().map(|x| (x.0, x.2)).collect();
            for (i, k) in j.keys.iter().enumerate() { rows.push((gold[k.as_str()].to_string(), got.get(&i).cloned().unwrap_or_else(|| UNCAT.into()), 100, be.name.clone())); } } Err(_) => fail += j.keys.len() } }
        let mt = metrics(&rows); con.execute("INSERT INTO eval_runs(ts, kind, label, backend, model, n, fingerprint, cost_usd, duration_s, summary, result) VALUES (?,?,?,?,?,?,?,?,?,?,?)", params![now_iso(), "models", m, be.name, llm::model_id(m), rows.len() as i64, fingerprint(con, rules)?, cost, t0.elapsed().as_secs_f64(), short(&mt).to_string(), mt.to_string()])?;
        out.push(json!({"model": m, "failed_merchants": fail, "cost_usd": cost, "metrics": short(&mt)})); }
    Ok(out)
}
