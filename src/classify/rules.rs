//! Category resolution: override -> internal-transfer link -> type rule (account purpose first) -> user merchant label ->
//! regex rule (direction-aware) -> canonical-entity default -> LLM / kNN merchant label -> other.uncategorized.
//! Memory annotations (`memory/categorization.yaml`) are applied on top of the pipeline result.
use crate::{config::Config, memory::Annotation, util::*};
use anyhow::{Context, Result};
use chrono::Datelike;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
pub const DEFAULT_TAXONOMY: &str = include_str!("../../assets/taxonomy.yaml");
pub const DEFAULT_RULES: &str = include_str!("../../assets/rules.yaml");

pub struct MerchantRule { pub rx: fancy_regex::Regex, pub category: String, pub direction: Option<String>, pub pattern: String }
pub struct Rules { pub type_rules: BTreeMap<String, String>, pub by_purpose: BTreeMap<String, BTreeMap<String, String>>, pub merchant: Vec<MerchantRule>, pub categories: BTreeMap<String, String> }

fn pick(cfg: &Config, env: &str, name: &str, def: &'static str) -> Result<String> {
    if let Some(p) = std::env::var_os(env) { return std::fs::read_to_string(&p).with_context(|| format!("{env}={}", p.to_string_lossy())); }
    let u = cfg.user_config_dir().join(name);
    Ok(if u.exists() { std::fs::read_to_string(&u)? } else { def.to_string() })
}
impl Rules {
    pub fn load(cfg: &Config) -> Result<Self> { Self::from_text(&pick(cfg, "COACH_TAXONOMY_FILE", "taxonomy.yaml", DEFAULT_TAXONOMY)?, &pick(cfg, "COACH_RULES_FILE", "rules.yaml", DEFAULT_RULES)?) }
    pub fn defaults() -> Self { Self::from_text(DEFAULT_TAXONOMY, DEFAULT_RULES).expect("packaged rules") }
    pub fn from_text(tax: &str, rules: &str) -> Result<Self> {
        let t: BTreeMap<String, BTreeMap<String, String>> = serde_yaml::from_str(tax).context("taxonomy.yaml")?;
        let categories = t.iter().flat_map(|(g, l)| l.iter().map(move |(k, d)| (format!("{g}.{k}"), d.clone()))).collect();
        #[derive(serde::Deserialize)] struct MR { #[serde(rename = "match")] m: String, category: String, direction: Option<String> }
        #[derive(serde::Deserialize)] struct RF { #[serde(default)] type_rules: BTreeMap<String, String>, #[serde(default)] type_rules_by_account_purpose: BTreeMap<String, BTreeMap<String, String>>, #[serde(default)] merchant_rules: Vec<MR> }
        let r: RF = serde_yaml::from_str(rules).context("rules.yaml")?;
        let merchant = r.merchant_rules.into_iter().map(|m| Ok(MerchantRule { rx: rx_i(&m.m).with_context(|| format!("rules.yaml: bad regex {:?}", m.m))?, category: m.category, direction: m.direction, pattern: m.m })).collect::<Result<_>>()?;
        Ok(Self { type_rules: r.type_rules, by_purpose: r.type_rules_by_account_purpose, merchant, categories })
    }
    /// Categories the rules name that the taxonomy does not have (a user copy behind the package).
    pub fn unknown_references(&self) -> Vec<String> {
        let mut refs: HashSet<&String> = self.type_rules.values().collect(); refs.extend(self.by_purpose.values().flat_map(|d| d.values())); refs.extend(self.merchant.iter().map(|m| &m.category));
        let mut v: Vec<String> = refs.into_iter().filter(|c| !self.categories.contains_key(*c)).cloned().collect(); v.sort(); v
    }
    pub fn rule_category(&self, key: &str, direction: Option<&str>) -> Option<&str> {
        self.merchant.iter().find(|r| r.direction.as_deref().is_none_or(|d| Some(d) == direction) && is(&r.rx, key)).map(|r| r.category.as_str())
    }
}

/// Everything `resolve` reads from the database, loaded once.
pub struct Lookups { pub overrides: HashMap<String, String>, pub linked: HashSet<String>, pub merchants: HashMap<String, (String, String)>, pub entity: HashMap<String, (Option<String>, bool, String)> }
impl Lookups {
    pub fn load(con: &Connection) -> Result<Self> {
        let q2 = |sql: &str| -> Result<Vec<(String, String)>> { Ok(con.prepare(sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?) };
        Ok(Self {
            overrides: q2("SELECT tx_key, category FROM tx_overrides WHERE category IS NOT NULL")?.into_iter().collect(),
            linked: q2("SELECT out_tx_key, in_tx_key FROM transfer_links")?.into_iter().flat_map(|(a, b)| [a, b]).collect(),
            merchants: con.prepare("SELECT merchant_key, category, source FROM merchants WHERE category IS NOT NULL")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default()))))?.collect::<rusqlite::Result<_>>()?,
            entity: con.prepare("SELECT a.merchant_key, e.category, e.category_user, e.name FROM merchant_aliases a JOIN merchant_entities e ON e.id=a.entity_id")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?))))?.collect::<rusqlite::Result<_>>()?,
        })
    }
}

pub fn resolve(lk: &Lookups, rules: &Rules, tx_key: &str, tx_type: Option<&str>, mkey: &str, purpose: Option<&str>, amount: Option<f64>) -> (String, &'static str) {
    if let Some(c) = lk.overrides.get(tx_key) { return (c.clone(), "override"); }
    if lk.linked.contains(tx_key) { return ("transfer.internal".into(), "transfer_link"); }
    let ty = tx_type.unwrap_or("");
    if let Some(c) = rules.by_purpose.get(purpose.unwrap_or("")).and_then(|d| d.get(ty)) { return (c.clone(), "type_rule"); }
    if let Some(c) = rules.type_rules.get(ty) { return (c.clone(), "type_rule"); }
    let m = lk.merchants.get(mkey);
    if let Some((c, s)) = m { if s == "user" { return (c.clone(), "user"); } }
    if let Some(c) = rules.rule_category(mkey, amount.map(|a| if a > 0.0 { "in" } else { "out" })) { return (c.into(), "rule"); }
    if let Some((Some(c), user, _)) = lk.entity.get(mkey) { if m.is_none() || *user { return (c.clone(), "entity"); } }
    if let Some((c, s)) = m { return (c.clone(), match s.as_str() { "llm" => "llm", "knn" => "knn", "rule" => "rule", "entity" => "entity", _ => "label" }); }
    ("other.uncategorized".into(), "none")
}

/// Why annotation `a` does NOT apply (first failing criterion), or None when it matches.
pub fn annotation_mismatch(a: &Annotation, tx_key: &str, mkey: &str, desc: &str, bdate: &str, amount: f64, category: &str, op_date: Option<&str>, aliases: &[String]) -> Option<String> {
    let m = &a.m;
    if let Some(ci) = &m.category_in { if !ci.iter().any(|c| c == category) { return Some(format!("category_in {ci:?} does not include the pre-memory category {category:?}")); } }
    if let Some(w) = &m.weekdays { let day = d(op_date.unwrap_or(bdate)).map(|x| WEEKDAYS[x.weekday().num_days_from_monday() as usize]).unwrap_or(""); if !w.iter().any(|x| x == day) { return Some(format!("weekdays {w:?} does not include {day}")); } }
    if let Some(k) = &m.tx_keys { if !k.iter().any(|x| x == tx_key || aliases.contains(x)) { return Some("tx_keys does not list this transaction".into()); } }
    if let Some(p) = &m.merchant_key { if !search_i(p, mkey) { return Some(format!("merchant_key /{p}/ does not match {mkey:?}")); } }
    if let Some(p) = &m.description { if !search_i(p, desc) { return Some(format!("description /{p}/ does not match the bank description")); } }
    if let Some(f) = &m.date_from { if bdate < f.as_str() { return Some(format!("date {bdate} is before date_from {f}")); } }
    if let Some(t) = &m.date_to { if bdate > t.as_str() { return Some(format!("date {bdate} is after date_to {t}")); } }
    if let Some(x) = m.amount_min { if amount < x { return Some(format!("amount {amount} is below amount_min {x}")); } }
    if let Some(x) = m.amount_max { if amount > x { return Some(format!("amount {amount} is above amount_max {x}")); } }
    None
}

#[derive(Debug, Clone)]
pub struct Categorised {
    pub tx_key: String, pub date: String, pub amount: f64, pub ty: Option<String>, pub key: String, pub merchant: Option<String>, pub entity: Option<String>, pub category: String, pub source: String,
    pub desc: String, pub pre_category: String, pub pre_source: String, pub annotation: Option<String>, pub tags: Vec<String>, pub event: Option<String>, pub account: String, pub bank: Option<String>,
    pub currency: Option<String>, pub split_index: Option<usize>, pub split_of: Option<f64>, pub note: Option<String>,
}

/// One row per transaction with its final category (one per part for a split transaction). Excluded / needs-review accounts are left out
/// unless `include_excluded`.
pub fn categorised(con: &Connection, rules: &Rules, annotations: &[Annotation], include_excluded: bool) -> Result<Vec<Categorised>> {
    let lk = Lookups::load(con)?;
    let excluded: HashSet<String> = if include_excluded { HashSet::new() } else { con.prepare("SELECT uid FROM accounts WHERE exclude=1 OR needs_review=1")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()? };
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
    for r in con.prepare("SELECT old_key, new_key FROM tx_key_remap")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? { let (o, n) = r?; aliases.entry(n).or_default().push(o); }
    let mut splits: HashMap<String, Vec<(f64, String, Option<String>)>> = HashMap::new();
    for r in con.prepare("SELECT tx_key, amount, category, note FROM tx_splits ORDER BY id")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))? { let (k, v) = r?; splits.entry(k).or_default().push(v); }
    let mut st = con.prepare("SELECT t.tx_key, t.booking_date, t.amount, e.tx_type, e.merchant_key, COALESCE(m.merchant_name, e.merchant_raw), t.description, e.op_date, t.account_uid, a.purpose, a.bank, t.currency
        FROM transactions t JOIN tx_enriched e USING(tx_key) LEFT JOIN merchants m USING(merchant_key) LEFT JOIN accounts a ON a.uid=t.account_uid")?;
    type R = (String, Option<String>, Option<f64>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<R> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (tx_key, bdate, amount, ty, mkey, mname, desc, op, account, purpose, bank, currency) in rows {
        if excluded.contains(&account) { continue; }
        let (mkey, amount, bdate, desc) = (mkey.unwrap_or_default(), amount.unwrap_or(0.0), bdate.unwrap_or_default(), desc.unwrap_or_default());
        let (cat, src) = resolve(&lk, rules, &tx_key, ty.as_deref(), &mkey, purpose.as_deref(), Some(amount));
        let al = aliases.get(&tx_key).cloned().unwrap_or_default();
        let ann = annotations.iter().find(|a| annotation_mismatch(a, &tx_key, &mkey, &desc, &bdate, amount, &cat, op.as_deref(), &al).is_none());
        let (fc, fs) = match ann.and_then(|a| a.category.clone()) { Some(c) => (c, "memory".to_string()), None => (cat.clone(), src.to_string()) };
        let base = Categorised { entity: lk.entity.get(&mkey).map(|e| e.2.clone()).or_else(|| mname.clone()), tx_key: tx_key.clone(), date: bdate, amount, ty, key: mkey, merchant: mname, category: fc, source: fs, desc, pre_category: cat, pre_source: src.into(),
            annotation: ann.map(|a| a.id.clone()), tags: ann.map(|a| a.tags.clone()).unwrap_or_default(), event: ann.and_then(|a| a.event.clone()), account, bank, currency, split_index: None, split_of: None, note: None };
        match splits.get(&tx_key) {
            Some(parts) => for (i, (a, c, n)) in parts.iter().enumerate() { out.push(Categorised { amount: *a, category: c.clone(), source: "split".into(), split_index: Some(i), split_of: Some(amount), note: n.clone(), ..base.clone() }); },
            None => out.push(base),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn packaged_rules_are_consistent() { let r = Rules::defaults(); assert!(r.unknown_references().is_empty(), "{:?}", r.unknown_references()); }
    #[test] fn rules_match() {
        let r = Rules::defaults();
        assert_eq!(r.rule_category("PROXIMUS NV", Some("out")), Some("subscriptions.telecom"));
        assert_eq!(r.rule_category("HUUR APPARTEMENT MAART", Some("in")), Some("income.rental"));
        assert_eq!(r.rule_category("HUURWAARBORG TERUG", Some("in")), None);
        assert_eq!(r.rule_category("CHRISTELIJKE MUTUALITEIT", Some("in")), Some("income.refund"));
        assert_eq!(r.rule_category("CHRISTELIJKE MUTUALITEIT", Some("out")), Some("health.health_insurance"));
        assert_eq!(r.rule_category("FOD FINANCIEN", Some("out")), Some("taxes.taxes"));
        assert_eq!(r.rule_category("COLRUYT GENT", Some("out")), Some("food.groceries"));
    }
}
