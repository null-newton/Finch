//! LLM labelling of unique MERCHANTS (never of single transactions, never of person-to-person transfers).
//! * Privacy guard (default-deny for anything that is not a shop): a transfer-like key is sent only when it shows it is an
//!   organisation (legal form / organisation word / a single brand-like word), is a known merchant or matches `[classify]
//!   llm_allowlist`; card keys are checked for titled people and person-like names. Held-back keys are listed, never sent.
//! * Every item is redacted (IBAN, e-mail, phone, long numbers, titled names, the household's own name tokens), towns are cut,
//!   amounts are an order of magnitude. `--dry-run` prints the exact requests.
//! * kNN: character-trigram TF-IDF over labelled shops; a near-duplicate (>= `knn_threshold`, all close neighbours agreeing) is
//!   labelled WITHOUT the LLM (source `knn`); the nearest labelled shops are passed to the LLM as hints.
//! * Backends: `claude-code` (`claude -p`, isolated), `anthropic-api`, `ollama` (loopback unless allowed), `openai-compatible`.
//!   Every call goes through `egress::allow` and is logged in `llm_usage`. User labels (`source = user`) are never overwritten.
use super::{parsers::common::{family_in_lead, household, looks_like_person, CONNECTORS, FIRST_NAMES, LEGAL_FORMS, ORG_WORDS, P2P_PREFIXES}, rules::Rules};
use crate::{config::Config, egress, net::{Http, Runner}, re, rs, util::*};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

const HOLD_TYPES: [&str; 4] = ["transfer_in", "transfer_out", "other", "direct_debit"];
const CARDISH: [&str; 3] = ["card", "card_refund", "direct_debit"];
const TITLE_ANY: &str = r"(?i)^(?:DR|DOKTER|DOCTEUR|MEESTER|MAITRE|ME|PROF|PROFESSOR|PROFESSEUR|DHR|MEVR|MEVROUW|MME|MR|MLLE|MONSIEUR|MADAME)\b";
fn toks(t: &str) -> Vec<String> { rs!(r"[^A-Z0-9']+").split(&strip_accents(t).to_uppercase()).filter(|x| !x.is_empty()).map(String::from).collect() }
type Set = HashSet<String>;

pub fn organisation_like(text: &str, family: &Set, first: &Set) -> bool {
    let t: Vec<String> = toks(text).into_iter().filter(|x| !x.chars().all(|c| c.is_ascii_digit())).collect();
    if t.is_empty() { return false; }
    if t.iter().any(|x| ORG_WORDS.contains(x) || LEGAL_FORMS.contains(x)) { return true; }
    t.len() == 1 && t[0].chars().count() >= 4 && t[0].chars().all(char::is_alphabetic) && !FIRST_NAMES.contains(&t[0]) && !first.contains(&t[0]) && !family.contains(&t[0])
}
/// A given name next to another name-like word ('PEETERS LOTTE GENT').
pub fn name_adjacent_to_first_name(t: &[String], first: &Set) -> bool {
    if t.iter().any(|x| ORG_WORDS.contains(x) || LEGAL_FORMS.contains(x)) { return false; }
    t.iter().enumerate().any(|(i, x)| (FIRST_NAMES.contains(x.split('-').next().unwrap()) || first.contains(x) || x.chars().count() == 1)
        && [i.wrapping_sub(1), i + 1].iter().any(|j| t.get(*j).is_some_and(|y| !CONNECTORS.contains(y) && y.chars().all(char::is_alphabetic) && y != x)))
}
/// Towns a descriptor may end with: declared places, and last words shared by `min_keys` merchant keys.
pub fn known_places(con: &Connection, memory_dir: &std::path::Path, min_keys: usize) -> Vec<Vec<String>> {
    let mut places: Vec<Vec<String>> = crate::memory::household_list(memory_dir, "places").iter().map(|p| toks(p)).filter(|t| !t.is_empty()).collect();
    let keys: Vec<String> = con.prepare("SELECT DISTINCT merchant_key FROM tx_enriched WHERE merchant_key <> ''").and_then(|mut s| s.query_map([], |r| r.get(0)).map(|i| i.flatten().collect())).unwrap_or_default();
    let mut last: HashMap<String, Set> = HashMap::new();
    for k in keys { let t = toks(&k); if let Some(l) = t.last().filter(|l| t.len() >= 2 && l.chars().all(char::is_alphabetic) && l.chars().count() >= 4 && !ORG_WORDS.contains(*l) && !LEGAL_FORMS.contains(*l)) { last.entry(l.clone()).or_default().insert(k); } }
    places.extend(last.into_iter().filter(|(_, ks)| ks.len() >= min_keys).map(|(w, _)| vec![w]));
    places.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b))); places.dedup(); places
}
/// The descriptor without its known towns (and dangling links: SUR / LEZ / DE ...).
pub fn strip_places(text: &str, places: &[Vec<String>]) -> String {
    let mut t = toks(text);
    'outer: loop { for p in places { if let Some(i) = (0..=t.len().saturating_sub(p.len())).find(|i| t.len() >= p.len() && t[*i..*i + p.len()] == p[..]) { t.drain(i..i + p.len()); continue 'outer; } } break; }
    while t.last().is_some_and(|x| ["LEZ", "LES", "LA", "LE", "SUR", "SOUS", "EN", "AUX", "DE", "DU", "DES", "SINT", "SAINT", "ST", "AAN", "OP", "BIJ"].contains(&x.as_str())) { t.pop(); }
    t.join(" ")
}
/// True when an item must not be sent to an LLM (a possible person).
#[allow(clippy::too_many_arguments)]
pub fn hold_back(key: &str, raw: Option<&str>, types: &Set, family: &Set, first: &Set, known: &Set, allow: &[String], strict_shop: bool, places: &[Vec<String>]) -> bool {
    let text = raw.unwrap_or(key); let tk: Set = toks(key).into_iter().chain(toks(text)).collect(); let up = strip_accents(key).to_uppercase();
    let has = |s: &[&str]| types.iter().any(|t| s.contains(&t.as_str()));
    if tk.iter().any(|t| P2P_PREFIXES.contains(t)) && (has(&HOLD_TYPES) || has(&["person_transfer_in", "person_transfer_out"])) { return true; }
    if tk.iter().any(|t| family.contains(t)) && !types.iter().all(|t| t == "card" || t == "card_refund") { return true; }
    if is(re!(TITLE_ANY), &up) { return true; }
    let allowed = allow.iter().any(|a| search_i(a, key));
    if !has(&HOLD_TYPES) {
        if (known.contains(key) && !strict_shop) || allowed { return false; }
        let bare = strip_places(key, places); let mut bt = toks(&bare); if bt.is_empty() { bt = toks(key); }
        return looks_like_person(key, first) || looks_like_person(&bare, first) || family_in_lead(key, family) || name_adjacent_to_first_name(&bt, first);
    }
    if known.contains(key) || allowed { return false; }
    !(organisation_like(key, family, first) || organisation_like(text, family, first))
}
fn known_merchants(con: &Connection) -> Result<Set> { Ok(con.prepare("SELECT DISTINCT m.merchant_key FROM merchants m JOIN tx_enriched e ON e.merchant_key=m.merchant_key WHERE e.tx_type IN ('card','card_refund','direct_debit') AND m.category IS NOT NULL")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?) }
fn needs_llm_sql(rules: &Rules) -> String { format!("COALESCE(e.tx_type,'') NOT IN ({})", rules.type_rules.keys().map(|t| format!("'{}'", t.replace('\'', ""))).collect::<Vec<_>>().join(",")) }

#[derive(Debug, Clone)]
pub struct Candidate { pub key: String, pub n: i64, pub total: f64, pub types: Set, pub raw: Option<String>, pub held: bool }
/// (candidates, held back) of keys that need a label: no label yet (or an LLM / kNN one with `refresh`), no rule, not decided by memory.
pub fn candidates(con: &Connection, cfg: &Config, rules: &Rules, refresh: bool) -> Result<(Vec<Candidate>, Vec<Candidate>)> {
    let h = household(con, Some(&cfg.memory_dir)); let places = known_places(con, &cfg.memory_dir, 3); let known = known_merchants(con)?;
    let (ann, _) = crate::memory::load_annotations(&cfg.memory_dir);
    let cat = super::rules::categorised(con, rules, &ann, false)?;
    let (mut total, mut decided): (HashMap<String, usize>, HashMap<String, usize>) = Default::default();
    for t in &cat { *total.entry(t.key.clone()).or_default() += 1; if t.source == "memory" { *decided.entry(t.key.clone()).or_default() += 1; } }
    let q = format!("SELECT e.merchant_key, COUNT(*), ROUND(SUM(ABS(t.amount)),2), GROUP_CONCAT(DISTINCT e.tx_type), MAX(e.merchant_raw), SUM(CASE WHEN t.amount > 0 THEN 1 ELSE 0 END), SUM(CASE WHEN t.amount < 0 THEN 1 ELSE 0 END) FROM tx_enriched e JOIN transactions t USING(tx_key) LEFT JOIN merchants m ON m.merchant_key=e.merchant_key
        WHERE {} AND e.merchant_key <> '' AND t.tx_key NOT IN (SELECT out_tx_key FROM transfer_links) AND t.tx_key NOT IN (SELECT in_tx_key FROM transfer_links)
          AND ((m.merchant_key IS NULL AND e.merchant_key NOT IN (SELECT a2.merchant_key FROM merchant_aliases a2 JOIN merchant_entities en ON en.id=a2.entity_id WHERE en.category IS NOT NULL)) {})
        GROUP BY 1 ORDER BY 3 DESC", needs_llm_sql(rules), if refresh { "OR m.source IN ('llm','knn')" } else { "" });
    type Row = (String, i64, f64, Option<String>, Option<String>, i64, i64);
    let rows: Vec<Row> = con.prepare(&q)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<f64>>(2)?.unwrap_or(0.0), r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let (mut out, mut held) = (vec![], vec![]);
    for (key, n, tot, ty, raw, n_in, n_out) in rows {
        // a key a rule decides (in the direction of ALL its payments) needs no label
        let ruled = rules.rule_category(&key, None).is_some() || (n_out == 0 && rules.rule_category(&key, Some("in")).is_some()) || (n_in == 0 && rules.rule_category(&key, Some("out")).is_some());
        if ruled || decided.get(&key).is_some_and(|d| *d == total[&key]) { continue; }
        let types: Set = ty.unwrap_or_default().split(',').filter(|x| !x.is_empty()).map(String::from).collect();
        let hb = hold_back(&key, raw.as_deref(), &types, &h.family, &h.first_names, &known, &cfg.llm_allowlist, false, &places);
        let c = Candidate { key, n, total: tot, types, raw, held: hb }; if hb { held.push(c) } else { out.push(c) }
    }
    Ok((out, held))
}
/// Labelled shops that may be shown to an LLM as example / hint, and form the kNN corpus.
pub fn eligible_label_keys(con: &Connection, cfg: &Config) -> Result<Set> {
    let h = household(con, Some(&cfg.memory_dir));
    let rows: Vec<(String, Option<String>, Option<String>)> = con.prepare("SELECT m.merchant_key, MAX(e.merchant_raw), GROUP_CONCAT(DISTINCT e.tx_type) FROM merchants m JOIN tx_enriched e ON e.merchant_key=m.merchant_key
        WHERE m.category IS NOT NULL AND m.category <> 'other.uncategorized' AND m.category NOT LIKE 'transfer.%' AND m.category NOT LIKE 'income.%' GROUP BY m.merchant_key")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows.into_iter().filter_map(|(k, raw, ty)| {
        let ts: Set = ty.unwrap_or_default().split(',').map(String::from).collect();
        if !ts.iter().all(|t| CARDISH.contains(&t.as_str())) { return None; }
        let known: Set = if ts.contains("card") || ts.contains("card_refund") { [k.clone()].into() } else { Set::new() };
        (!hold_back(&k, raw.as_deref(), &ts, &h.family, &h.first_names, &known, &cfg.llm_allowlist, true, &[])).then_some(k)
    }).collect())
}

// ---------------------------------------------------------------- kNN
pub struct Index { items: Vec<(String, String, String)>, idf: HashMap<String, f64>, vecs: Vec<HashMap<String, f64>>, n: f64 }
fn ngrams(key: &str) -> HashMap<String, f64> {
    let s = format!(" {} ", collapse(&rs!(r"[^A-Z0-9 ]+").replace_all(&key.to_uppercase(), " "))); let c: Vec<char> = s.chars().collect();
    let mut m = HashMap::new(); for w in c.windows(3) { *m.entry(w.iter().collect::<String>()).or_insert(0.0) += 1.0; } m
}
impl Index {
    pub fn new(items: Vec<(String, String, String)>) -> Self {
        let grams: Vec<_> = items.iter().map(|i| ngrams(&i.0)).collect(); let mut df: HashMap<String, f64> = HashMap::new();
        for g in &grams { for t in g.keys() { *df.entry(t.clone()).or_default() += 1.0; } }
        let n = items.len().max(1) as f64; let idf = df.into_iter().map(|(t, c)| (t, ((1.0 + n) / (1.0 + c)).ln() + 1.0)).collect();
        let mut me = Self { items, idf, vecs: vec![], n }; me.vecs = grams.iter().map(|g| me.vec(g)).collect(); me
    }
    fn vec(&self, g: &HashMap<String, f64>) -> HashMap<String, f64> {
        let v: HashMap<String, f64> = g.iter().map(|(t, c)| (t.clone(), (1.0 + c.ln()) * self.idf.get(t).copied().unwrap_or((1.0 + self.n).ln() + 1.0))).collect();
        let norm = v.values().map(|x| x * x).sum::<f64>().sqrt().max(f64::MIN_POSITIVE); v.into_iter().map(|(t, x)| (t, x / norm)).collect()
    }
    pub fn is_empty(&self) -> bool { self.items.is_empty() }
    /// (similarity, key, name, category) of the k nearest labelled keys.
    pub fn neighbours(&self, key: &str, k: usize) -> Vec<(f64, String, String, String)> {
        let q = self.vec(&ngrams(key));
        let mut s: Vec<_> = self.items.iter().zip(&self.vecs).filter(|(i, _)| i.0 != key).filter_map(|(i, v)| { let sim: f64 = q.iter().map(|(t, x)| x * v.get(t).unwrap_or(&0.0)).sum(); (sim > 0.0).then(|| ((sim * 10000.0).round() / 10000.0, i.0.clone(), i.1.clone(), i.2.clone())) }).collect();
        s.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1))); s.truncate(k); s
    }
    /// (category, similarity, nearest key) when the nearest neighbours above the threshold all agree.
    pub fn auto_label(&self, key: &str, thr: f64) -> Option<(String, f64, String)> {
        let strong: Vec<_> = self.neighbours(key, 3).into_iter().filter(|n| n.0 >= thr).collect();
        (!strong.is_empty() && strong.iter().all(|n| n.3 == strong[0].3)).then(|| (strong[0].3.clone(), strong[0].0, strong[0].1.clone()))
    }
}
pub fn build_index(con: &Connection, cfg: &Config) -> Result<Index> {
    let ok = eligible_label_keys(con, cfg)?;
    let rows: Vec<(String, String, String)> = con.prepare("SELECT merchant_key, COALESCE(merchant_name, merchant_key), category FROM merchants WHERE category IS NOT NULL AND category <> 'other.uncategorized' AND (source='user' OR (source IN ('llm','llm_web') AND confidence >= 0.85)) ORDER BY merchant_key")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(Index::new(rows.into_iter().filter(|r| ok.contains(&r.0)).collect()))
}

// ---------------------------------------------------------------- prompt
const LABEL_HEAD: &str = "You label merchants from a Belgian household's bank statements (descriptors in Dutch, French or English).
Bank descriptors are truncated, upper-cased and often end with the town.
Payment-processor prefixes (SUMUP, SQ, PAYPAL, MOL = Mollie, STRIPE, PAYCONIQ...) precede the real merchant.
Descriptors reach you without their town, and avg_amount is an order of magnitude (1-2-5 series), not an exact figure.
For each item return: a clean human merchant name (brand, no town), the single best category id from the list, a confidence
0..1 (use < 0.6 when you are guessing from a vague name), and recurring_hint=true if this merchant is typically a subscription /
recurring bill. Classify by WHAT THE MERCHANT SELLS, also when direction=in: a refund from a shop or a utility goes in that
merchant's spending category so it nets out; use income.* only for employers, benefits, interest and similar real income.
Use \"other.uncategorized\" only if nothing plausible fits.";
pub fn label_schema(rules: &Rules) -> Value {
    json!({"type": "object", "properties": {"results": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "integer"}, "merchant": {"type": "string"}, "category": {"type": "string", "enum": rules.categories.keys().collect::<Vec<_>>()},
        "confidence": {"type": "number"}, "recurring_hint": {"type": "boolean"}}, "required": ["id", "merchant", "category", "confidence", "recurring_hint"]}}}, "required": ["results"]})
}
/// An amount as an order of magnitude on the 1-2-5 series (12.40 -> 10, 37 -> 50, 843 -> 1000).
pub fn coarse_amount(x: f64) -> f64 { let x = x.abs(); if x < 0.5 { return 0.0; } let e = 10f64.powf(x.log10().floor()); let b = [e, 2.0 * e, 5.0 * e, 10.0 * e].into_iter().min_by(|a, b| (a.ln() - x.ln()).abs().total_cmp(&(b.ln() - x.ln()).abs())).unwrap(); if b >= 1.0 { b.round() } else { (b * 100.0).round() / 100.0 } }
/// The household's own name tokens removed from a text (on top of the technical redaction).
pub fn redact_names(t: &str, names: &[String]) -> String {
    let r = crate::privacy::redact(t); if names.is_empty() { return r; }
    let mut n = names.to_vec(); n.sort_by(|a, b| b.len().cmp(&a.len()));
    regex::Regex::new(&format!(r"(?i)\b({})\b", n.iter().map(|x| escape(x)).collect::<Vec<_>>().join("|"))).map_or(r.clone(), |rx| rx.replace_all(&r, "[NAME]").into())
}
/// Never trust model output: ids in range, unique, a known category (else other.uncategorized, low confidence), confidence clamped.
pub fn validate_results(results: &Value, n: usize, rules: &Rules) -> Vec<(usize, String, String, f64, bool)> {
    let mut seen = HashSet::new(); let mut out = vec![];
    for r in results.as_array().into_iter().flatten() {
        let (Some(id), Some(m), Some(conf)) = (r["id"].as_u64().map(|x| x as usize), r["merchant"].as_str().filter(|m| !m.trim().is_empty()), r["confidence"].as_f64()) else { continue };
        if id >= n || !seen.insert(id) || r["category"].is_null() { continue; }
        let mut conf = if conf.is_nan() { 0.0 } else { conf.clamp(0.0, 1.0) };
        let cat = match r["category"].as_str() { Some(c) if rules.categories.contains_key(c) => c.to_string(), _ => { conf = conf.min(0.3); "other.uncategorized".into() } };
        let hint = match &r["recurring_hint"] { Value::Bool(b) => *b, Value::String(s) => ["true", "yes", "1", "ja", "oui"].contains(&s.trim().to_lowercase().as_str()), Value::Number(x) => x.as_f64() == Some(1.0), _ => false };
        out.push((id, truncate_chars(m, 120), cat, conf, hint));
    }
    out
}
pub struct Job { pub keys: Vec<String>, pub stat: String, pub dynamic: String }
/// The exact (redacted) requests of a run.
pub fn prepare_jobs(con: &Connection, cfg: &Config, rules: &Rules, keys: &[String], batch: usize, index: Option<&Index>) -> Result<Vec<Job>> {
    let places = known_places(con, &cfg.memory_dir, 3); let h = household(con, Some(&cfg.memory_dir)); let names: Vec<String> = h.family.iter().filter(|x| x.chars().count() >= 3).cloned().collect();
    let bare = |s: &str| { let b = strip_places(s, &places); if b.is_empty() { s.to_string() } else { b } };
    let ok = eligible_label_keys(con, cfg)?;
    let ex: Vec<(String, String, String)> = con.prepare("SELECT merchant_key, COALESCE(merchant_name, merchant_key), category FROM merchants WHERE source='user' ORDER BY updated_at DESC")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().filter(|e: &(String, String, String)| ok.contains(&e.0)).take(40).collect();
    let shots = if ex.is_empty() { "(none yet)".into() } else { ex.iter().map(|(k, n, c)| format!("- \"{}\" -> {c} ({})", redact_names(k, &names), redact_names(n, &names))).collect::<Vec<_>>().join("\n") };
    let stat = format!("{LABEL_HEAD}\n\nCategories:\n{}", rules.categories.iter().map(|(k, v)| format!("- {k}: {v}")).collect::<Vec<_>>().join("\n"));
    let mut jobs = vec![];
    for b in keys.chunks(batch.max(1)) {
        let mut items = vec![];
        for (i, key) in b.iter().enumerate() {
            let (n, avg, ex, types, fx): (i64, Option<f64>, Option<String>, Option<String>, Option<String>) = con.query_row(&format!("SELECT COUNT(*), AVG(t.amount), MAX(e.merchant_raw), GROUP_CONCAT(DISTINCT e.tx_type), GROUP_CONCAT(DISTINCT e.fx_currency) FROM tx_enriched e JOIN transactions t USING(tx_key) WHERE e.merchant_key=? AND {}", needs_llm_sql(rules)), [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            let mut it = json!({"id": i, "key": redact_names(&bare(key), &names), "raw_example": ex.as_deref().map(|e| redact_names(&bare(e), &names)), "n": n, "direction": if avg.unwrap_or(0.0) > 0.0 { "in" } else { "out" }, "avg_amount": coarse_amount(avg.unwrap_or(0.0)), "types": types});
            if let Some(f) = fx { it["foreign_currency"] = f.into(); }
            if let Some(ix) = index { if types.unwrap_or_default().split(',').all(|t| CARDISH.contains(&t)) { let sim: Vec<Value> = ix.neighbours(key, cfg.knn_examples as usize).into_iter().map(|(_, k, nm, c)| json!({"key": redact_names(&bare(&k), &names), "name": redact_names(&nm, &names), "category": c})).collect(); if !sim.is_empty() { it["similar"] = sim.into(); } } }
            items.push(it);
        }
        let note = if items.iter().any(|i| i.get("similar").is_some()) { "Items may carry `similar`: the nearest already-labelled merchants (key, category). They are hints: follow them when the item is clearly the same business or chain, ignore them otherwise.\n\n" } else { "" };
        jobs.push(Job { keys: b.to_vec(), stat: stat.clone(), dynamic: format!("\nLabels confirmed by the user (follow these conventions):\n{shots}\n{note}Items (direction=out means money spent; avg_amount in EUR):\n{}", Value::Array(items)) });
    }
    Ok(jobs)
}

// ---------------------------------------------------------------- backends
#[derive(Debug, Clone, Default)]
pub struct Usage { pub backend: String, pub model: String, pub purpose: String, pub items: i64, pub tokens_in: i64, pub tokens_out: i64, pub cache_read: i64, pub cache_write: i64, pub cost_usd: Option<f64>, pub estimate: bool, pub duration_s: f64 }
const PRICES: [(&str, f64, f64); 8] = [("claude-haiku-4-5", 1.0, 5.0), ("claude-haiku-5-5", 0.1, 0.5), ("claude-sonnet-5-5", 2.0, 10.0), ("claude-sonnet-4-6", 3.0, 15.0), ("claude-opus-5-5", 4.0, 20.0), ("claude-opus-4-8", 5.0, 25.0), ("claude-fable-5-2", 10.0, 50.0), ("claude-fable-5-1", 10.0, 50.0)];
pub fn model_id(m: &str) -> String { match m { "haiku" => "claude-haiku-4-5", "sonnet" => "claude-sonnet-5-5", "opus" => "claude-opus-5-5", x => x }.into() }
pub fn estimate_cost(model: &str, tin: i64, tout: i64, cr: i64, cw: i64) -> Option<f64> { PRICES.iter().find(|p| p.0 == model).map(|(_, pi, po)| ((tin as f64 * pi + cr as f64 * pi * 0.1 + cw as f64 * pi * 1.25 + tout as f64 * po) / 1e6 * 1e6).round() / 1e6) }
fn with_no_additional(v: &Value) -> Value { match v { Value::Object(o) => { let mut o: serde_json::Map<String, Value> = o.iter().map(|(k, x)| (k.clone(), with_no_additional(x))).collect(); if o.get("type") == Some(&json!("object")) { o.entry("additionalProperties").or_insert(json!(false)); } Value::Object(o) } Value::Array(a) => Value::Array(a.iter().map(with_no_additional).collect()), x => x.clone() } }
fn parse_json_text(t: &str) -> Result<Value> { let t = t.trim(); let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).map_or(t, |x| x.trim_end().trim_end_matches("```")); serde_json::from_str(t.trim()).context("the model's answer is not JSON") }
/// A model error text without any line that repeats the prompt.
fn safe_diag(text: &str, prompt: &str) -> String { truncate_chars(&text.lines().map(str::trim).filter(|l| !l.is_empty() && !(l.len() >= 12 && (0..l.len() - 11).step_by(6).any(|i| l.get(i..i + 12).is_some_and(|w| prompt.contains(w))))).collect::<Vec<_>>().join(" "), 200) }

pub fn check_ollama_url(u: &str, allow_remote: bool) -> Result<()> {
    let p = url::Url::parse(u).map_err(|_| anyhow::anyhow!("llm.ollama_url {u:?} is not an http(s) URL"))?;
    if !["http", "https"].contains(&p.scheme()) || p.host_str().is_none() { bail!("llm.ollama_url {u:?} is not an http(s) URL"); }
    if !allow_remote && !egress::is_loopback(p.host_str().unwrap()) { bail!("llm.ollama_url {u:?} is not a loopback address: your bank data would leave this machine. Set [llm] ollama_allow_remote = true to allow it knowingly."); }
    Ok(())
}
pub fn check_openai_url(u: &str) -> Result<()> {
    let p = url::Url::parse(u).map_err(|_| anyhow::anyhow!("llm.openai_base_url {u:?} is not an http(s) URL"))?; let host = p.host_str().unwrap_or("");
    if host.is_empty() || !["http", "https"].contains(&p.scheme()) { bail!("llm.openai_base_url {u:?} is not an http(s) URL (for example https://openrouter.ai/api/v1)"); }
    if p.scheme() == "http" && !egress::is_loopback(host) { bail!("llm.openai_base_url {u:?} must use https: your redacted bank data would cross the network in clear text"); }
    if p.query().is_some() || p.fragment().is_some() || !p.username().is_empty() || p.password().is_some() { bail!("llm.openai_base_url must not carry a query, a fragment or credentials (the key is the openai_api_key secret)"); }
    Ok(())
}
pub enum Transport<'a> { Http(&'a dyn Http), Run(&'a dyn Runner) }
pub struct Backend<'a> { pub name: String, pub cfg: &'a Config, pub con: &'a Connection, pub t: Transport<'a> }
const DENIED: &str = "Bash,Read,Write,Edit,MultiEdit,Glob,Grep,WebSearch,WebFetch,Task,NotebookEdit,TodoWrite";
const ENV_ALLOW: [&str; 14] = ["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR", "TERM", "SHELL", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "SYSTEMROOT", "DISABLE_AUTOUPDATER", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"];
const SECRET_ENV: [&str; 5] = ["ANTHROPIC_API_KEY", "COACH_DB_KEY", "COACH_BACKUP_KEY", "COACH_PROPOSAL_KEY", "ANTHROPIC_AUTH_TOKEN"];
impl Backend<'_> {
    pub fn default_model(&self) -> String { match self.name.as_str() { "anthropic-api" => self.cfg.llm_anthropic_model.clone(), "ollama" => self.cfg.llm_ollama_model.clone(), "openai-compatible" => self.cfg.llm_openai_model.clone(), _ => self.cfg.llm_model.clone() } }
    fn secret(name: &str) -> Result<String> { crate::secrets::get_secret(name, true)?.ok_or_else(|| anyhow::anyhow!("secret {name} not set")) }
    pub fn complete(&self, stat: &str, dynamic: &str, schema: &Value, model: &str, purpose: &str, items: usize) -> Result<(Value, Usage)> {
        let t0 = std::time::Instant::now(); let bytes = stat.len() + dynamic.len(); let jp = format!("classify.{purpose}");
        let mut u = Usage { backend: self.name.clone(), model: model.into(), purpose: purpose.into(), items: items as i64, estimate: true, ..Default::default() };
        let http = |kind: &str, url: &str| -> Result<&dyn Http> { egress::allow(Some(self.con), self.cfg, kind, &egress::host_of(url), bytes, &jp)?; match self.t { Transport::Http(h) => Ok(h), _ => bail!("internal: no HTTP transport") } };
        let data = match self.name.as_str() {
            "claude-code" => {
                egress::allow(Some(self.con), self.cfg, "llm.claude-code", "", bytes, &jp)?;
                let Transport::Run(r) = self.t else { bail!("internal: no process runner") };
                let mut argv: Vec<String> = ["claude", "-p", "--model", model, "--output-format", "json", "--no-session-persistence", "--strict-mcp-config", "--setting-sources", "", "--settings", r#"{"disableAllHooks":true}"#, "--disable-slash-commands", "--tools", "", "--disallowedTools", DENIED, "--permission-mode", "dontAsk", "--json-schema"].iter().map(|s| s.to_string()).collect();
                argv.push(schema.to_string()); if self.cfg.coach_claude_restricted { argv.push("--restricted".into()); }
                let mut env: Vec<(String, String)> = ENV_ALLOW.iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
                env.extend(std::env::vars().filter(|(k, _)| k.starts_with("LC_") || (self.cfg.coach_claude_env.contains(k) && !SECRET_ENV.contains(&k.as_str()))));
                if let Ok(Some(t)) = crate::secrets::get_secret("claude_code_oauth_token", false) { env.push(("CLAUDE_CODE_OAUTH_TOKEN".into(), t)); }
                let (code, out, err) = r.run(&argv, &format!("{stat}{dynamic}"), &env, 600)?;
                let res: Value = serde_json::from_str(out.trim()).unwrap_or_default();
                if code != 0 || res["is_error"] == true { bail!("claude -p failed (exit {code}{}): {}", res["subtype"].as_str().map(|s| format!(", {s}")).unwrap_or_default(), safe_diag(&err, &format!("{stat}{dynamic}"))); }
                let us = &res["usage"]; u.tokens_in = us["input_tokens"].as_i64().unwrap_or(0); u.tokens_out = us["output_tokens"].as_i64().unwrap_or(0); u.cache_read = us["cache_read_input_tokens"].as_i64().unwrap_or(0); u.cache_write = us["cache_creation_input_tokens"].as_i64().unwrap_or(0); u.cost_usd = Some(res["total_cost_usd"].as_f64().unwrap_or(0.0));
                if res["structured_output"].is_object() { res["structured_output"].clone() } else { parse_json_text(res["result"].as_str().unwrap_or(""))? }
            }
            "anthropic-api" => {
                let base = self.cfg.llm_anthropic_base_url.clone().unwrap_or_else(|| "https://api.anthropic.com".into()); let mid = model_id(model); u.model = mid.clone();
                let body = json!({"model": mid, "max_tokens": 16000, "system": [{"type": "text", "text": stat, "cache_control": {"type": "ephemeral"}}], "messages": [{"role": "user", "content": dynamic}], "output_config": {"format": {"type": "json_schema", "schema": with_no_additional(schema)}}});
                let r = http("llm.anthropic-api", &base)?.send("POST", &format!("{}/v1/messages", base.trim_end_matches('/')), &[("x-api-key", Self::secret("anthropic_api_key")?), ("anthropic-version", "2023-06-01".into()), ("content-type", "application/json".into())], Some(&body))?;
                if r.status >= 400 { bail!("api.anthropic.com answered HTTP {}: {}", r.status, safe_diag(&r.body, &format!("{stat}{dynamic}"))); }
                let m = r.json()?; let us = &m["usage"]; u.tokens_in = us["input_tokens"].as_i64().unwrap_or(0); u.tokens_out = us["output_tokens"].as_i64().unwrap_or(0); u.cache_read = us["cache_read_input_tokens"].as_i64().unwrap_or(0); u.cache_write = us["cache_creation_input_tokens"].as_i64().unwrap_or(0);
                u.cost_usd = estimate_cost(&mid, u.tokens_in, u.tokens_out, u.cache_read, u.cache_write);
                if let Some(s) = m["stop_reason"].as_str().filter(|s| *s == "refusal" || *s == "max_tokens") { record_usage(self.con, &u)?; bail!("model stopped with {s}"); }
                parse_json_text(m["content"].as_array().into_iter().flatten().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str()).unwrap_or(""))?
            }
            "ollama" => {
                check_ollama_url(&self.cfg.llm_ollama_url, self.cfg.llm_ollama_allow_remote)?; let base = self.cfg.llm_ollama_url.trim_end_matches('/').to_string();
                let r = http("llm.ollama", &base)?.send("POST", &format!("{base}/api/chat"), &[], Some(&json!({"model": model, "stream": false, "format": schema, "options": {"temperature": 0}, "messages": [{"role": "system", "content": stat}, {"role": "user", "content": dynamic}]})))?;
                if r.status >= 400 { bail!("ollama answered HTTP {}", r.status); }
                let b = r.json()?; u.tokens_in = b["prompt_eval_count"].as_i64().unwrap_or(0); u.tokens_out = b["eval_count"].as_i64().unwrap_or(0); u.cost_usd = Some(0.0); u.estimate = false;
                parse_json_text(b["message"]["content"].as_str().unwrap_or(""))?
            }
            "openai-compatible" => {
                check_openai_url(&self.cfg.llm_openai_base_url)?; let base = self.cfg.llm_openai_base_url.trim_end_matches('/').to_string(); let host = egress::host_of(&base); let s = with_no_additional(schema);
                let mut body = json!({"model": model, "temperature": 0, "messages": [{"role": "system", "content": stat}, {"role": "user", "content": dynamic}], "response_format": {"type": "json_schema", "json_schema": {"name": "result", "strict": true, "schema": s}}});
                if host == "openrouter.ai" && self.cfg.llm_openrouter_deny_data_collection { body["provider"] = json!({"data_collection": "deny"}); }
                let r = http("llm.openai-compatible", &base)?.send("POST", &format!("{base}/chat/completions"), &[("Authorization", format!("Bearer {}", Self::secret("openai_api_key")?)), ("Content-Type", "application/json".into())], Some(&body))?;
                if r.status >= 400 { bail!("{host} answered HTTP {}: {}", r.status, safe_diag(&r.body, &format!("{stat}{dynamic}"))); }
                let m = r.json()?; let us = &m["usage"]; u.tokens_in = us["prompt_tokens"].as_i64().unwrap_or(0); u.tokens_out = us["completion_tokens"].as_i64().unwrap_or(0); u.cost_usd = us["cost"].as_f64(); u.estimate = u.cost_usd.is_none();
                let ch = &m["choices"][0]; if let Some(f) = ch["finish_reason"].as_str().filter(|f| *f == "length" || *f == "content_filter") { record_usage(self.con, &u)?; bail!("model stopped with {f}"); }
                parse_json_text(ch["message"]["content"].as_str().unwrap_or(""))?
            }
            n => bail!("unknown llm backend {n:?}"),
        };
        u.duration_s = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0; Ok((data, u))
    }
}
pub fn record_usage(con: &Connection, u: &Usage) -> Result<()> {
    con.execute("INSERT INTO llm_usage(ts, backend, model, purpose, items, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, cost_is_estimate, duration_s) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        params![now_iso(), u.backend, u.model, u.purpose, u.items, u.tokens_in, u.tokens_out, u.cache_read, u.cache_write, u.cost_usd, u.estimate as i64, u.duration_s])?; Ok(())
}
/// Store a label unless the user already labelled the merchant.
pub fn upsert_label(con: &Connection, key: &str, name: &str, cat: &str, conf: Option<f64>, hint: Option<bool>, source: &str, model: &str) -> Result<()> {
    con.execute("INSERT INTO merchants VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(merchant_key) DO UPDATE SET merchant_name=excluded.merchant_name, category=excluded.category, confidence=excluded.confidence, recurring_hint=excluded.recurring_hint, source=excluded.source, model=excluded.model, updated_at=excluded.updated_at WHERE merchants.source <> 'user' OR excluded.source = 'user'",
        params![key, name, cat, conf, hint.map(|h| h as i64), source, model, now_iso()])?; Ok(())
}

pub struct RunOpts { pub dry_run: bool, pub refresh: bool, pub limit: Option<usize>, pub batch: usize, pub model: Option<String>, pub knn: bool }
#[derive(Debug, Default)]
pub struct RunReport { pub held_back: Vec<String>, pub knn: usize, pub sent: usize, pub labelled: usize, pub cost: f64, pub unknown_cost: bool, pub failed: Vec<String>, pub dry_payload: Option<Value> }
/// Label every merchant that needs it: kNN first (no LLM), then the backend, batch by batch, each batch stored as soon as it is paid for.
pub fn run(con: &Connection, cfg: &Config, rules: &Rules, be: Option<&Backend>, o: &RunOpts) -> Result<RunReport> {
    let mut rep = RunReport::default();
    let index = build_index(con, cfg)?; let thr = cfg.knn_threshold;
    if !o.dry_run && !index.is_empty() {   // similarity labels follow their sources: re-derived, or dropped so the key is asked again
        for (k, c) in con.prepare("SELECT merchant_key, category FROM merchants WHERE source='knn'")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()? {
            match index.auto_label(&k, thr) { None => { con.execute("DELETE FROM merchants WHERE merchant_key=? AND source='knn'", [&k])?; } Some((nc, s, near)) if nc != c => { con.execute("UPDATE merchants SET category=?, confidence=?, model=? WHERE merchant_key=?", params![nc, (s * 1000.0).round() / 1000.0, truncate_chars(&format!("knn:{near}"), 60), k])?; } _ => {} }
        }
    }
    let (cands, held) = candidates(con, cfg, rules, o.refresh)?; rep.held_back = held.into_iter().map(|c| c.key).collect();
    let mut keys = vec![];
    for c in &cands {
        let hit = (o.knn && cfg.knn_enabled && !index.is_empty() && c.types.iter().all(|t| CARDISH.contains(&t.as_str()))).then(|| index.auto_label(&c.key, thr)).flatten();
        match hit { Some((cat, s, near)) => { rep.knn += 1; if !o.dry_run { upsert_label(con, &c.key, &titlecase(&c.key), &cat, Some((s * 1000.0).round() / 1000.0), None, "knn", &truncate_chars(&format!("knn:{near}"), 60))?; } } None => keys.push(c.key.clone()) }
    }
    if let Some(l) = o.limit { keys.truncate(l); }
    let idx = (o.knn && cfg.knn_enabled && !index.is_empty()).then_some(&index);
    let jobs = prepare_jobs(con, cfg, rules, &keys, o.batch, idx)?; rep.sent = keys.len();
    if o.dry_run { rep.dry_payload = Some(Value::Array(jobs.iter().enumerate().map(|(i, j)| json!({"request": i + 1, "merchants": j.keys.len(), "static_prompt_chars": j.stat.len(), "static_prompt_sha256": &sha256_hex(j.stat.as_bytes())[..16], "dynamic_prompt": j.dynamic})).collect())); return Ok(rep); }
    let Some(be) = be else { bail!("no backend") };
    let model = o.model.clone().unwrap_or_else(|| be.default_model()); let schema = label_schema(rules);
    for j in &jobs {
        match be.complete(&j.stat, &j.dynamic, &schema, &model, "label", j.keys.len()) {
            Ok((data, u)) => {
                record_usage(con, &u)?; match u.cost_usd { Some(c) => rep.cost += c, None => rep.unknown_cost = true }
                for (id, name, cat, conf, hint) in validate_results(&data["results"], j.keys.len(), rules) { upsert_label(con, &j.keys[id], &name, &cat, Some(conf), Some(hint), "llm", &model)?; rep.labelled += 1; }
            }
            Err(e) => rep.failed.push(format!("{} merchant(s): {}", j.keys.len(), e.root_cause())),
        }
    }
    Ok(rep)
}
fn titlecase(k: &str) -> String { k.split_whitespace().map(|w| { let mut c = w.chars(); c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()) }).collect::<Vec<_>>().join(" ") }
/// The user's own label of a merchant: wins over every automatic one (and is used as an example and kNN source).
pub fn correct(con: &Connection, rules: &Rules, key: &str, cat: &str, name: Option<&str>) -> Result<usize> {
    if !rules.categories.contains_key(cat) { bail!("unknown category {cat:?} (see assets/taxonomy.yaml or your config/taxonomy.yaml)"); }
    let n: i64 = con.query_row("SELECT COUNT(*) FROM tx_enriched WHERE merchant_key=?", [key], |r| r.get(0))?;
    if n == 0 { bail!("no transaction has the merchant key {key:?} (see `finch classify review`)"); }
    upsert_label(con, key, name.unwrap_or(&titlecase(key)), cat, Some(1.0), None, "user", "user")?; Ok(n as usize)
}
/// What needs the user: held-back (possible people) and low-confidence / uncategorised labels, biggest money first.
pub fn review(con: &Connection, cfg: &Config, rules: &Rules) -> Result<Value> {
    let (_, held) = candidates(con, cfg, rules, false)?;
    let low: Vec<Value> = con.prepare("SELECT m.merchant_key, m.category, m.confidence, m.source, ROUND(SUM(ABS(t.amount)),2) FROM merchants m JOIN tx_enriched e USING(merchant_key) JOIN transactions t USING(tx_key) WHERE m.source IN ('llm','knn') AND (m.confidence < 0.6 OR m.category='other.uncategorized') GROUP BY 1 ORDER BY 5 DESC LIMIT 50")?
        .query_map([], |r| Ok(json!({"key": r.get::<_, String>(0)?, "category": r.get::<_, String>(1)?, "confidence": r.get::<_, Option<f64>>(2)?, "source": r.get::<_, String>(3)?, "total": r.get::<_, f64>(4)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(json!({"held_back_person_like": held.iter().map(|c| json!({"key": c.key, "n": c.n, "total": c.total, "types": c.types.iter().collect::<BTreeSet<_>>()})).collect::<Vec<_>>(), "low_confidence": low}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(x: &[&str]) -> Set { x.iter().map(|t| t.to_string()).collect() }
    #[test] fn guard() {
        let (f, fi, k) = (s(&["TESTMANS"]), Set::new(), Set::new());
        assert!(hold_back("PEETERS LOTTE", None, &s(&["transfer_out"]), &f, &fi, &k, &[], false, &[]), "a person on a transfer");
        assert!(!hold_back("ACME BV", None, &s(&["transfer_in"]), &f, &fi, &k, &[], false, &[]), "an organisation");
        assert!(hold_back("JANSSENS KOEN", None, &s(&["card"]), &f, &fi, &k, &[], false, &[]), "a person-like card key");
        assert!(!hold_back("COLRUYT GENT", None, &s(&["card"]), &f, &fi, &k, &[], false, &[]));
        assert!(hold_back("DR WOUTERS", None, &s(&["card"]), &f, &fi, &k, &[], false, &[]), "titled");
        assert!(hold_back("TESTMANS ZORRO", None, &s(&["direct_debit"]), &f, &fi, &k, &[], false, &[]), "family name");
        assert!(!hold_back("PEETERS LOTTE", None, &s(&["transfer_out"]), &f, &fi, &k, &["^PEETERS".into()], false, &[]), "allowlisted");
        assert_eq!(strip_places("BAKKER LEUVEN", &[vec!["LEUVEN".into()]]), "BAKKER");
    }
    #[test] fn misc() {
        assert_eq!((coarse_amount(12.4), coarse_amount(37.0), coarse_amount(843.0), coarse_amount(-0.3)), (10.0, 50.0, 1000.0, 0.0));
        let ix = Index::new(vec![("NETFLIX COM".into(), "Netflix".into(), "subscriptions.video_streaming".into()), ("COLRUYT GENT".into(), "Colruyt".into(), "food.groceries".into())]);
        assert_eq!(ix.auto_label("COLRUYT GENTBRUGGE", 0.6).map(|x| x.0), Some("food.groceries".into())); assert!(ix.auto_label("SHELL", 0.92).is_none());
        let r = Rules::defaults(); let v = validate_results(&json!([{"id": 0, "merchant": "X", "category": "food.groceries", "confidence": 2, "recurring_hint": "yes"}, {"id": 0, "merchant": "dup", "category": "x", "confidence": 0.5, "recurring_hint": false}, {"id": 9, "merchant": "Y", "category": "nope", "confidence": 0.9, "recurring_hint": false}, {"id": 1, "merchant": "Z", "category": "nope", "confidence": 0.9, "recurring_hint": false}]), 2, &r);
        assert_eq!(v, vec![(0, "X".into(), "food.groceries".into(), 1.0, true), (1, "Z".into(), "other.uncategorized".into(), 0.3, false)]);
        assert!(check_ollama_url("http://127.0.0.1:11434", false).is_ok() && check_ollama_url("http://10.1.1.1:11434", false).is_err() && check_openai_url("http://example.com/v1").is_err());
        assert_eq!(redact_names("BETALING TESTMANS jan@x.be", &["TESTMANS".into()]), "BETALING [NAME] [EMAIL]");
    }
}
