//! Read-only view of the household memory (`memory/`): members of household.yaml and the categorization annotations.
//! Changes to memory are never made here (the Python app's proposal flow owns them).
use crate::classify::parsers::common::{members_yaml, household_yaml};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub merchant_key: Option<String>, pub description: Option<String>, pub tx_keys: Option<Vec<String>>, pub date_from: Option<String>, pub date_to: Option<String>,
    pub amount_min: Option<f64>, pub amount_max: Option<f64>, pub category_in: Option<Vec<String>>, pub weekdays: Option<Vec<String>>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation { pub id: String, #[serde(rename = "match")] pub m: Match, pub category: Option<String>, #[serde(default)] pub tags: Vec<String>, pub event: Option<String>, pub note: Option<String> }
#[derive(Deserialize)]
struct CatFile { #[serde(default)] annotations: Vec<Annotation> }

#[derive(Debug, Clone)]
pub struct Member { pub id: String, pub name: String, pub role: String, pub aliases: Vec<String> }

fn yaml_date(v: &serde_yaml::Value) -> serde_yaml::Value { v.clone() }
/// Validated annotations; an invalid or unreadable file yields none and ONE loud warning (never an error).
pub fn load_annotations(memory_dir: &Path) -> (Vec<Annotation>, Option<String>) {
    let p = memory_dir.join("categorization.yaml");
    let Ok(text) = std::fs::read_to_string(&p) else { return (vec![], None) };
    let parsed: Result<CatFile, String> = serde_yaml::from_str::<serde_yaml::Value>(&text).map_err(|e| e.to_string()).and_then(|mut v| {
        // YAML dates (2025-01-01) arrive as strings already; numbers in amount fields stay numbers
        if let Some(a) = v.get_mut("annotations").and_then(|a| a.as_sequence_mut()) { for x in a { if let Some(m) = x.get_mut("match") { for k in ["date_from", "date_to"] { if let Some(d) = m.get(k).map(yaml_date) { m[k] = serde_yaml::Value::String(match d { serde_yaml::Value::String(s) => s, o => serde_yaml::to_string(&o).unwrap_or_default().trim().into() }); } } } } }
        serde_yaml::from_value(v).map_err(|e| e.to_string())
    });
    let check = |a: &Annotation| -> Result<(), String> {
        for (k, p) in [("merchant_key", &a.m.merchant_key), ("description", &a.m.description)] { if let Some(p) = p { if fancy_regex::Regex::new(p).is_err() { return Err(format!("{}: match.{k} is not a valid regex", a.id)); } } }
        if let Some(c) = &a.category { if !crate::util::is(crate::re!(r"^[a-z0-9_]+\.[a-z0-9_]+$"), c) { return Err(format!("{}: category must look like group.leaf", a.id)); } }
        for d in [&a.m.date_from, &a.m.date_to].into_iter().flatten() { if crate::util::d(d).is_none() { return Err(format!("{}: {d:?} is not a date (YYYY-MM-DD)", a.id)); } }
        if let Some(w) = &a.m.weekdays { if w.iter().any(|x| !crate::classify::rules::WEEKDAYS.contains(&x.as_str())) { return Err(format!("{}: weekdays must be mon..sun", a.id)); } }
        Ok(())
    };
    match parsed.and_then(|f| f.annotations.iter().try_for_each(check).map(|_| f.annotations)) {
        Ok(a) => (a, None),
        Err(e) => (vec![], Some(format!("{} is invalid ({e}). Memory annotations are IGNORED until it is fixed", p.display()))),
    }
}

pub fn members(memory_dir: &Path) -> Vec<Member> {
    members_yaml(Some(memory_dir)).into_iter().filter(|m| !m.id.is_empty()).map(|m| Member {
        id: m.id, name: m.name.or(m.full_name).unwrap_or_default(), role: m.role.unwrap_or_else(|| "adult".into()),
        aliases: m.aliases.unwrap_or_default().into_iter().filter_map(|a| a.as_str().map(String::from)).collect(),
    }).collect()
}
/// household.yaml list field (employers / places / schools), strings only.
pub fn household_list(memory_dir: &Path, field: &str) -> Vec<String> {
    household_yaml(Some(memory_dir)).get(field).and_then(|v| v.as_sequence()).map(|s| s.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// A contract (`merchant_match`) or liability (`payment_match`) pattern that links bank payments to a memory item.
#[derive(Debug, Clone)]
pub struct Matcher { pub kind: &'static str, pub id: String, pub pattern: String, pub amount: Option<(f64, f64)>, pub liability_kind: Option<String> }
pub fn matchers(memory_dir: &Path) -> Vec<Matcher> {
    let mut out = vec![];
    for (dir, kind, field) in [("contracts", "contract", "merchant_match"), ("liabilities", "liability", "payment_match")] {
        let mut files: Vec<_> = std::fs::read_dir(memory_dir.join(dir)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml") && !p.file_name().unwrap().to_string_lossy().starts_with('_')).collect();
        files.sort();
        for f in files {
            let Some(v) = std::fs::read_to_string(&f).ok().and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok()) else { continue };
            let (Some(id), Some(pat)) = (v.get("id").and_then(|x| x.as_str()), v.get(field).and_then(|x| x.as_str())) else { continue };
            let amount = v.get("amount_match").and_then(|a| Some((a.get("amount")?.as_f64()?, a.get("tolerance_pct").and_then(|t| t.as_f64()).unwrap_or(3.0))));
            out.push(Matcher { kind, id: id.into(), pattern: pat.into(), amount, liability_kind: v.get("kind").and_then(|k| k.as_str()).map(String::from).filter(|_| kind == "liability") });
        }
    }
    out
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget { pub id: String, pub category: Option<String>, pub group: Option<String>, pub monthly: f64, #[serde(default)] pub rollover: bool, pub owner: Option<String>, pub account: Option<String>, pub start: Option<String>, pub note: Option<String> }
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal { pub id: String, pub title: Option<String>, pub target_amount: f64, pub target_date: Option<String>, pub asset: Option<String>, pub account: Option<String>, pub tag: Option<String>, pub monthly_contribution: Option<f64>, pub baseline: Option<f64>, pub start: Option<String>, pub note: Option<String> }

/// Entries of a list file (budgets.yaml / goals.yaml): valid ones, and "id: why" for each invalid one (an invalid entry never stops the others).
pub fn list_file<T: serde::de::DeserializeOwned>(memory_dir: &Path, file: &str, key: &str) -> (Vec<T>, Vec<String>) {
    let Some(v) = std::fs::read_to_string(memory_dir.join(file)).ok().and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok()) else { return (vec![], vec![]) };
    let (mut ok, mut bad) = (vec![], vec![]);
    for (i, e) in v.get(key).and_then(|x| x.as_sequence()).cloned().unwrap_or_default().into_iter().enumerate() {
        let mut e = e;
        if let Some(m) = e.as_mapping_mut() { for (_, val) in m.iter_mut() { if let serde_yaml::Value::Tagged(t) = val { *val = t.value.clone(); } } }
        let id = e.get("id").and_then(|x| x.as_str()).map_or(format!("#{}", i + 1), String::from);
        match serde_yaml::from_value::<T>(e) { Ok(x) => ok.push(x), Err(err) => bad.push(format!("{id}: {err}")) }
    }
    (ok, bad)
}
