//! CSV mapping profiles: TOML files under `config/import_profiles/` (same format as the Python app).
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use toml::Value;

pub const COLUMN_KEYS: [&str; 10] = ["date", "value_date", "description", "amount", "debit", "credit", "counterparty", "reference", "currency", "fee"];

#[derive(Debug, Clone, PartialEq)]
pub enum Col { Name(String), Index(usize), Many(Vec<String>) }

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String, pub description: String, pub encoding: String, pub delimiter: String, pub skip_rows: usize, pub skip_footer: usize,
    pub header_contains: Vec<String>, pub has_header: bool, pub date_format: Vec<String>, pub decimal: String, pub thousands: String,
    pub currency: String, pub negate: bool, pub columns: Vec<(String, Col)>, pub filter_column: Option<String>, pub filter_keep: Vec<String>,
}
impl Default for Profile {
    fn default() -> Self { Self { name: "(inline)".into(), description: String::new(), encoding: "auto".into(), delimiter: "auto".into(), skip_rows: 0, skip_footer: 0, header_contains: vec![], has_header: true, date_format: vec!["%d/%m/%Y".into()], decimal: "auto".into(), thousands: String::new(), currency: "EUR".into(), negate: false, columns: vec![], filter_column: None, filter_keep: vec![] } }
}
impl Profile {
    pub fn col(&self, k: &str) -> Option<&Col> { self.columns.iter().find(|(c, _)| c == k).map(|x| &x.1) }
    pub fn validate(&self) -> Result<()> {
        if self.col("date").is_none() { bail!("profile needs columns.date"); }
        if self.col("description").is_none() { bail!("profile needs columns.description"); }
        if self.col("amount").is_none() && self.col("debit").is_none() && self.col("credit").is_none() { bail!("profile needs columns.amount, or columns.debit / columns.credit"); }
        if !["," , ".", "auto"].contains(&self.decimal.as_str()) { bail!("format.decimal must be ',' or '.' (or 'auto')"); }
        if let Some((k, _)) = self.columns.iter().find(|(k, _)| !COLUMN_KEYS.contains(&k.as_str())) { bail!("unknown column key {k:?}; allowed: {}", COLUMN_KEYS.join(", ")); }
        Ok(())
    }
}

fn strs(v: Option<&Value>) -> Vec<String> { v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() }
pub fn from_toml(d: &toml::Table, name: &str) -> Result<Profile> {
    let empty = toml::Table::new();
    let sec = |k: &str| d.get(k).and_then(Value::as_table).unwrap_or(&empty);
    let (f, cols, flt) = (sec("format"), sec("columns"), sec("filter"));
    let s = |t: &toml::Table, k: &str, def: &str| t.get(k).and_then(Value::as_str).unwrap_or(def).to_string();
    let n = |k: &str| f.get(k).and_then(Value::as_integer).unwrap_or(0).max(0) as usize;
    let mut columns = vec![];
    for (k, v) in cols { columns.push((k.clone(), match v { Value::Integer(i) => Col::Index(*i as usize), Value::String(x) => Col::Name(x.clone()), Value::Array(_) => Col::Many(strs(Some(v))), _ => bail!("columns.{k} must be a header name, a 0-based index or a list of names") })); }
    let p = Profile { name: s(d, "name", name), description: s(d, "description", ""), encoding: s(f, "encoding", "auto"), delimiter: s(f, "delimiter", "auto"), skip_rows: n("skip_rows"), skip_footer: n("skip_footer"),
        header_contains: strs(f.get("header_contains")), has_header: f.get("has_header").and_then(Value::as_bool).unwrap_or(true),
        date_format: match f.get("date_format") { Some(Value::String(x)) => vec![x.clone()], Some(v @ Value::Array(_)) => strs(Some(v)), _ => vec!["%d/%m/%Y".into()] },
        decimal: s(f, "decimal", "auto"), thousands: s(f, "thousands", ""), currency: s(f, "currency", "EUR"), negate: f.get("negate").and_then(Value::as_bool).unwrap_or(false),
        columns, filter_column: flt.get("column").and_then(Value::as_str).map(String::from), filter_keep: strs(flt.get("keep")) };
    p.validate()?; Ok(p)
}
pub fn profile_path(dir: &Path, name: &str) -> Result<PathBuf> {
    if !crate::util::is(crate::re!(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$"), name) { bail!("invalid profile name {name:?} (letters, digits, '-', '_', '.')"); }
    Ok(dir.join(format!("{name}.toml")))
}
pub fn load_profile(dir: &Path, name: &str) -> Result<Profile> {
    let p = profile_path(dir, name)?;
    if !p.exists() { bail!("profile {name:?} not found in {} (see `finch import --list-profiles`)", dir.display()); }
    from_toml(&std::fs::read_to_string(&p)?.parse().with_context(|| p.display().to_string())?, name)
}
pub fn list_profiles(dir: &Path) -> Vec<(String, String)> {
    let mut v: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
    v.sort();
    v.iter().map(|p| (p.file_stem().unwrap().to_string_lossy().into(), std::fs::read_to_string(p).ok().and_then(|t| t.parse::<toml::Table>().ok()).map_or("(invalid TOML)".into(), |t| t.get("description").and_then(Value::as_str).unwrap_or("").to_string()))).collect()
}
fn q(s: &str) -> String { serde_json::to_string(s).unwrap() }
fn ql(v: &[String]) -> String { serde_json::to_string(v).unwrap() }
pub fn to_toml(p: &Profile) -> String {
    let mut l = vec![format!("name = {}", q(&p.name)), format!("description = {}", q(&p.description)), String::new(), "[format]".into(),
        format!("encoding = {}", q(&p.encoding)), format!("delimiter = {}", q(&p.delimiter)), format!("skip_rows = {}", p.skip_rows), format!("skip_footer = {}", p.skip_footer),
        format!("has_header = {}", p.has_header), format!("header_contains = {}", ql(&p.header_contains)), format!("date_format = {}", ql(&p.date_format)),
        format!("decimal = {}", q(&p.decimal)), format!("thousands = {}", q(&p.thousands)), format!("currency = {}", q(&p.currency)), format!("negate = {}", p.negate), String::new(), "[columns]".into()];
    l.extend(p.columns.iter().map(|(k, c)| format!("{k} = {}", match c { Col::Name(n) => q(n), Col::Index(i) => i.to_string(), Col::Many(v) => ql(v) })));
    if let Some(c) = &p.filter_column { l.extend([String::new(), "[filter]".into(), format!("column = {}", q(c)), format!("keep = {}", ql(&p.filter_keep))]); }
    l.join("\n") + "\n"
}
pub fn save_profile(dir: &Path, p: &Profile, overwrite: bool) -> Result<PathBuf> {
    let path = profile_path(dir, &p.name)?;
    if path.exists() && !overwrite { bail!("{} already exists (use --overwrite-profile)", path.display()); }
    p.validate()?; std::fs::create_dir_all(dir)?; std::fs::write(&path, to_toml(p))?; Ok(path)
}
