//! The memory store (E3 / E6): validated, recorded edits of the household memory files, shared with the Python app.
//! Edits are structured operations (`set`, `unset`, `append`, `remove`, `create`, `append_text`, `replace_text`) on a path
//! (`budgets[groceries].monthly`, `members[kiki].pocket_money`). The YAML is patched in place: only the list item (or top-level
//! key) an operation touches is re-rendered in the house style (mapping indent 2, sequence offset 2, short scalar lists in flow
//! style), so the rest of the file, comments included, stays byte for byte. Every write is validated (schema + categories +
//! member references), diffed and recorded in the same git history (`.history.git`) as the Python app.
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value as J};
use serde_yaml::{Mapping, Value as Y};
use std::{collections::BTreeSet, path::{Path, PathBuf}, process::Command};

pub const GIT_DIRNAME: &str = ".history.git";
const EXCLUDES: [&str; 8] = ["/documents/", "/.proposals/", "/.backups/", "/.lock", "/.history.git/", "*.tmp", ".DS_Store", "/.lock.finch"];
/// (file, list key) of the YAML files finch can edit (validated here); other memory files are edited in the Python app.
pub const FILES: [(&str, &str); 6] = [("budgets.yaml", "budgets"), ("goals.yaml", "goals"), ("open-questions.yaml", "questions"), ("categorization.yaml", "annotations"), ("household.yaml", "members"), ("events.yaml", "events")];
const MARKDOWN: [&str; 3] = ["preferences.md", "events.md", "notes.md"];
pub const QUESTIONS_MD: &str = "open-questions.md";

#[derive(Debug, Clone, Default)]
pub struct EditResult { pub file: String, pub old: String, pub new: String, pub diff: String, pub change_id: Option<String>, pub warnings: Vec<String> }
impl EditResult { pub fn changed(&self) -> bool { self.old != self.new } }
#[derive(Default)]
pub struct EditOpts<'a> { pub action: &'a str, pub reason: Option<&'a str>, pub source: Option<&'a str>, pub detail: Option<&'a str>, pub dry_run: bool }

pub struct Store { pub root: PathBuf, pub history: bool, pub source: String, pub categories: BTreeSet<String> }
impl Store {
    pub fn new(cfg: &crate::config::Config, source: &str) -> Result<Self> {
        Ok(Self { root: cfg.memory_dir.clone(), history: cfg.memory_history, source: source.into(), categories: crate::classify::rules::Rules::load(cfg)?.categories.keys().cloned().collect() })
    }
    pub fn path(&self, rel: &str) -> Result<PathBuf> {
        if rel.is_empty() || rel.contains("..") || rel.contains('\\') || Path::new(rel).is_absolute() || rel.starts_with('.') { bail!("{rel:?} is not a memory file"); }
        Ok(self.root.join(rel))
    }
    pub fn exists(&self, rel: &str) -> bool { self.path(rel).is_ok_and(|p| p.is_file()) }
    pub fn read_text(&self, rel: &str) -> String { std::fs::read_to_string(self.path(rel).unwrap_or_default()).map(|s| s.trim_start_matches('\u{feff}').to_string()).unwrap_or_default() }
    pub fn list_key(rel: &str) -> Option<&'static str> { FILES.iter().find(|f| f.0 == rel).map(|f| f.1) }
    pub fn editable(rel: &str) -> bool { Self::list_key(rel).is_some() || MARKDOWN.contains(&rel) }
    pub fn load(&self, rel: &str) -> Result<Y> { let t = self.read_text(rel); if t.trim().is_empty() { return Ok(Y::Mapping(Mapping::new())); } serde_yaml::from_str(&t).with_context(|| format!("{rel} is not valid YAML")) }
    /// The items of the list of a file (`budgets`, `questions` ...), as JSON.
    pub fn items(&self, rel: &str) -> Vec<J> { let k = Self::list_key(rel).unwrap_or(""); self.load(rel).ok().and_then(|v| v.get(k).cloned()).and_then(|v| serde_json::to_value(v).ok()).and_then(|v| v.as_array().cloned()).unwrap_or_default() }

    /// Apply operations to one file: validated, diffed, written and recorded (or only previewed with `dry_run`).
    pub fn edit(&self, rel: &str, ops: &[J], o: &EditOpts) -> Result<EditResult> { self.edit_with(rel, ops, o, &[]) }
    pub fn edit_with(&self, rel: &str, ops: &[J], o: &EditOpts, extra: &[(&str, String)]) -> Result<EditResult> {
        if !Self::editable(rel) { bail!("{rel} is not a memory file finch edits: change it in the Python app (or propose it there)"); }
        let old = self.read_text(rel);
        let new = if MARKDOWN.contains(&rel) { ops.iter().try_fold(old.clone(), |t, op| text_op(&t, op))? } else { yaml_edit(&old, self.exists(rel), ops, Self::list_key(rel).unwrap())? };
        let mut warnings = if MARKDOWN.contains(&rel) { vec![] } else { self.validate(rel, &new)? };
        warnings.extend(lost_comments(&old, &new).into_iter().map(|c| format!("this edit drops the comment {c:?} of the item it rewrites")));
        let diff = unified(&old, &new, rel);
        let mut res = EditResult { file: rel.into(), old: old.clone(), new: new.clone(), diff, change_id: None, warnings };
        let extra: Vec<&(&str, String)> = extra.iter().filter(|(f, t)| self.read_text(f) != *t).collect();
        if o.dry_run || (new == old && extra.is_empty()) { return Ok(res); }
        let _lock = Lock::take(&self.root)?;
        if self.read_text(rel) != old { bail!("{rel} changed while this edit was being prepared; try again"); }
        if self.history { self.repo().ensure()?; self.repo().snapshot_external()?; }
        let bom = std::fs::read(self.path(rel)?).is_ok_and(|b| b.starts_with(b"\xef\xbb\xbf"));
        atomic_write(&self.path(rel)?, &format!("{}{new}", if bom { "\u{feff}" } else { "" }))?;
        let mut files = vec![rel.to_string()]; for (f, t) in extra { atomic_write(&self.path(f)?, t)?; files.push(f.to_string()); }
        if self.history { res.change_id = self.repo().record(o.action, &files, o.reason, o.source.unwrap_or(&self.source), o.detail)?; }
        Ok(res)
    }
    pub fn repo(&self) -> Repo { Repo { root: self.root.clone() } }

    /// Schema + cross-file checks of a YAML file's new text. Errors refuse the write; the returned strings are warnings.
    pub fn validate(&self, rel: &str, text: &str) -> Result<Vec<String>> {
        let v: Y = if text.trim().is_empty() { Y::Mapping(Mapping::new()) } else { serde_yaml::from_str(text).map_err(|e| anyhow!("{rel}: the result is not valid YAML ({e})"))? };
        if !v.is_mapping() { bail!("{rel}: the file must be a mapping"); }
        let key = Self::list_key(rel).unwrap(); let list = v.get(key).cloned().unwrap_or(Y::Sequence(vec![]));
        let list = if list.is_null() { Y::Sequence(vec![]) } else { list };
        let items = list.as_sequence().ok_or_else(|| anyhow!("{rel}: {key} must be a list"))?;
        let mut ids = BTreeSet::new(); let mut warn = vec![];
        for (i, it) in items.iter().enumerate() {
            let id = it.get("id").and_then(Y::as_str).ok_or_else(|| anyhow!("{rel}: {key}[{i}]: id is required"))?;
            if !crate::rs!(r"^[a-z0-9][a-z0-9_-]*$").is_match(id) { bail!("{rel}: {key}[{i}]: id must be lowercase letters, digits, '-' or '_' (got {id:?})"); }
            if !ids.insert(id.to_string()) { bail!("{rel}: duplicate {key} id {id:?}"); }
        }
        let cat = |c: &str, w: &str| -> Result<()> { if !self.categories.contains(c) { bail!("{rel}: {w}: unknown category {c:?}"); } Ok(()) };
        let group = |g: &str, w: &str| -> Result<()> { if !self.categories.iter().any(|c| c.split('.').next() == Some(g)) { bail!("{rel}: {w}: unknown category group {g:?}"); } Ok(()) };
        let date = |d: &Option<String>, w: &str| -> Result<()> { if let Some(d) = d { if crate::util::d(d).filter(|x| x.to_string() == *d).is_none() { bail!("{rel}: {w}: {d:?} is not a date (YYYY-MM-DD)"); } } Ok(()) };
        fn typed<T: serde::de::DeserializeOwned>(rel: &str, key: &str, it: &Y) -> Result<T> { serde_yaml::from_value(it.clone()).map_err(|e| anyhow!("{rel}: {key}[{}]: {e}", it.get("id").and_then(Y::as_str).unwrap_or("?"))) }
        match rel {
            "budgets.yaml" => for it in items { let b: VBudget = typed(rel, key, it)?; let w = format!("budget {}", b.id);
                if b.category.is_some() == b.group.is_some() { bail!("{rel}: {w}: give exactly one of category or group"); }
                if let Some(c) = &b.category { cat(c, &w)?; } if let Some(g) = &b.group { group(g, &w)?; } if !(b.monthly > 0.0 && b.monthly.is_finite()) { bail!("{rel}: {w}: monthly must be a positive amount"); } date(&b.start, &w)?; },
            "goals.yaml" => for it in items { let g: VGoal = typed(rel, key, it)?; let w = format!("goal {}", g.id);
                if !(g.target_amount > 0.0 && g.target_amount.is_finite()) { bail!("{rel}: {w}: target_amount must be a positive amount"); }
                if [&g.asset, &g.account, &g.tag].iter().filter(|x| x.is_some()).count() > 1 { bail!("{rel}: {w}: progress comes from ONE source: asset, account or tag"); }
                for x in [g.monthly_contribution, g.baseline].into_iter().flatten() { if !(x >= 0.0 && x.is_finite()) { bail!("{rel}: {w}: amounts must not be negative"); } }
                date(&g.target_date, &w)?; date(&g.start, &w)?; },
            "open-questions.yaml" => for it in items { let q: VQuestion = typed(rel, key, it)?; let w = format!("question {}", q.id);
                if !["open", "answered", "dismissed"].contains(&q.status.as_str()) { bail!("{rel}: {w}: status must be open, answered or dismissed"); }
                if !["manual", "generated", "migrated", "coach"].contains(&q.origin.as_str()) { bail!("{rel}: {w}: unknown origin {:?}", q.origin); }
                if q.status == "answered" && q.answer.as_deref().unwrap_or("").trim().is_empty() { bail!("{rel}: {w}: an answered question needs an answer text"); }
                if q.status != "answered" && q.answer.is_some() { bail!("{rel}: {w}: only an answered question can carry an answer (reopen it first)"); }
                date(&q.created, &w)?; date(&q.answered, &w)?; },
            "categorization.yaml" => for it in items { let a: crate::memory::Annotation = typed(rel, key, it)?; let w = format!("annotation {}", a.id);
                if a.category.is_none() && a.tags.is_empty() && a.event.is_none() { bail!("{rel}: {w}: an annotation needs a category, tags or an event"); }
                if let Some(c) = &a.category { cat(c, &w)?; } for p in [&a.m.merchant_key, &a.m.description].into_iter().flatten() { if fancy_regex::Regex::new(p).is_err() { bail!("{rel}: {w}: {p:?} is not a valid pattern"); } }
                if a.m.merchant_key.is_none() && a.m.description.is_none() && a.m.tx_keys.as_ref().is_none_or(|k| k.is_empty()) && a.m.date_from.is_none() { bail!("{rel}: {w}: say which transactions it matches"); } },
            "events.yaml" => for it in items { let e: VEvent = typed(rel, key, it)?; date(&e.start, &format!("event {}", e.id))?; date(&e.end, &format!("event {}", e.id))?; },
            "household.yaml" => {
                let members: Vec<crate::household::Member> = items.iter().map(|it| typed(rel, key, it)).collect::<Result<_>>()?;
                for m in &members { if !["adult", "child"].contains(&m.role.as_str()) { bail!("{rel}: member {}: role must be adult or child", m.id); } if m.name.trim().is_empty() { bail!("{rel}: member {}: name is required", m.id); }
                    if let Some(p) = &m.pocket_money { if !(p.amount > 0.0) || !["weekly", "monthly"].contains(&p.period.as_str()) { bail!("{rel}: member {}: pocket_money needs a positive amount and weekly / monthly", m.id); } if m.role != "child" { warn.push(format!("member {}: pocket money is meant for a child", m.id)); } } }
                let known = |x: &str| x == crate::household::JOINT || members.iter().any(|m| m.id == x);
                for (k, f) in [("attribution", 0), ("kid_budgets", 1), ("allocations", 2)] {
                    let l = v.get(k).cloned().unwrap_or(Y::Null); if l.is_null() { continue; } let l = l.as_sequence().ok_or_else(|| anyhow!("{rel}: {k} must be a list"))?; let mut seen = BTreeSet::new();
                    for it in l { let id = it.get("id").and_then(Y::as_str).unwrap_or(""); if !crate::rs!(r"^[a-z0-9][a-z0-9_-]*$").is_match(id) || !seen.insert(id.to_string()) { bail!("{rel}: {k}: every item needs a unique lowercase id (got {id:?})"); }
                        match f { 0 => { let r: crate::household::Rule = typed(rel, k, it)?; if !known(&r.member) { bail!("{rel}: attribution {}: {:?} is not a member or 'joint'", r.id, r.member); }
                                if r.m.card_last4.as_ref().is_some_and(|c| !crate::rs!(r"^(\d{4}|\d{6}\*\d)$").is_match(c)) { bail!("{rel}: attribution {}: card_last4 must be four digits", r.id); }
                                if serde_json::to_value(&r.m).ok().and_then(|m| m.as_object().map(|o| o.is_empty())).unwrap_or(true) { bail!("{rel}: attribution {}: the match needs at least one key", r.id); }
                                for p in [&r.m.merchant_key, &r.m.description].into_iter().flatten() { if regex::Regex::new(p).is_err() { bail!("{rel}: attribution {}: {p:?} is not a valid pattern", r.id); } }
                                if r.m.direction.as_deref().is_some_and(|d| d != "in" && d != "out") { bail!("{rel}: attribution {}: direction is in or out", r.id); } }
                            1 => { let b: crate::household::KidBudget = typed(rel, k, it)?; if !members.iter().any(|m| m.id == b.member) { bail!("{rel}: kid budget {}: {:?} is not a member", b.id, b.member); }
                                if !["weekly", "monthly"].contains(&b.period.as_str()) || !(b.limit > 0.0) { bail!("{rel}: kid budget {}: a weekly / monthly period and a positive limit", b.id); }
                                if let Some(c) = &b.category { cat(c, &format!("kid budget {}", b.id))?; } if let Some(g) = &b.group { group(g, &format!("kid budget {}", b.id))?; } }
                            _ => { let a: crate::household::Allocation = typed(rel, k, it)?; if !["equal", "income", "custom"].contains(&a.method.as_str()) { bail!("{rel}: allocation {}: method is equal, income or custom", a.id); }
                                if serde_json::to_value(&a.m).ok().and_then(|m| m.as_object().map(|o| o.is_empty())).unwrap_or(true) { bail!("{rel}: allocation {}: an allocation needs at least one of category, group, tag, merchant_key, account", a.id); }
                                for m in a.among.iter().chain(a.shares.keys()) { if !members.iter().any(|x| &x.id == m) { bail!("{rel}: allocation {}: {m:?} is not a member", a.id); } }
                                if a.method == "custom" && (a.shares.values().sum::<f64>() - 100.0).abs() > 0.01 { bail!("{rel}: allocation {}: custom shares must add up to 100", a.id); }
                                if let Some(c) = &a.m.category { cat(c, &format!("allocation {}", a.id))?; } if let Some(g) = &a.m.group { group(g, &format!("allocation {}", a.id))?; } } }
                    }
                }
            }
            _ => {}
        }
        Ok(warn)
    }
}
#[derive(Deserialize)] #[serde(deny_unknown_fields)] #[allow(dead_code)]
struct VBudget { id: String, category: Option<String>, group: Option<String>, monthly: f64, #[serde(default)] rollover: bool, owner: Option<String>, account: Option<String>, start: Option<String>, note: Option<String> }
#[derive(Deserialize)] #[serde(deny_unknown_fields)] #[allow(dead_code)]
struct VGoal { id: String, title: Option<String>, target_amount: f64, target_date: Option<String>, asset: Option<String>, account: Option<String>, tag: Option<String>, monthly_contribution: Option<f64>, baseline: Option<f64>, start: Option<String>, note: Option<String> }
#[derive(Deserialize)] #[serde(deny_unknown_fields)] #[allow(dead_code)]
struct VQuestion { id: String, #[serde(default = "open")] status: String, #[serde(default)] topic: Option<String>, topic_code: Option<String>, question: String, question_msg: Option<Y>, context: Option<String>, context_msg: Option<Y>,
    #[serde(default)] evidence: Option<Y>, suggested_target: Option<VTarget>, created: Option<String>, answered: Option<String>, answer: Option<String>, note: Option<String>, key: Option<String>, #[serde(default = "manual")] origin: String, stake: Option<f64>, source_text: Option<String> }
#[derive(Deserialize)] #[serde(deny_unknown_fields)] #[allow(dead_code)]
struct VTarget { file: String, field: Option<String> }
#[derive(Deserialize)] #[allow(dead_code)]
struct VEvent { id: String, title: Option<String>, start: Option<String>, end: Option<String>, budget: Option<f64>, status: Option<String>, note: Option<String> }
fn open() -> String { "open".into() }
fn manual() -> String { "manual".into() }

// ---------------------------------------------------------------- operations on the parsed document
#[derive(Debug, Clone, PartialEq)]
enum Step { K(String), I(usize) }
fn parse_path(path: &str) -> Result<Vec<(String, Vec<String>)>> {
    if path.trim().is_empty() { bail!("empty path"); }
    path.split('.').map(|part| { let c = crate::rs!(r"^([A-Za-z0-9_-]*)((?:\[[^\[\]]+\])*)$").captures(part).filter(|c| !(c[1].is_empty() && c[2].is_empty())).ok_or_else(|| anyhow!("bad path segment {part:?} in {path:?} (use key, key[id] or key[0])"))?;
        Ok((c[1].to_string(), crate::rs!(r"\[([^\[\]]+)\]").captures_iter(&c[2]).map(|x| x[1].to_string()).collect())) }).collect()
}
fn find_item(seq: &[Y], sel: &str, what: &str) -> Result<usize> {
    if let Ok(i) = sel.parse::<usize>() { return if i < seq.len() { Ok(i) } else { Err(anyhow!("{what}: no item at position {i} (list has {})", seq.len())) }; }
    seq.iter().position(|it| it.get("id").map(scalar_str).as_deref() == Some(sel)).ok_or_else(|| anyhow!("{what}: no item with id {sel:?}"))
}
fn scalar_str(v: &Y) -> String { match v { Y::String(s) => s.clone(), Y::Number(n) => n.to_string(), Y::Bool(b) => b.to_string(), _ => String::new() } }
/// `item-id.field` -> `<list_key>[item-id].field` when the first segment is an item id of the file's list.
fn expand(doc: &Y, path: &str, lk: &str) -> String {
    let first = path.split('.').next().unwrap_or(""); if first.contains('[') || first == lk || doc.get(first).is_some() { return path.into(); }
    if doc.get(lk).and_then(Y::as_sequence).is_some_and(|s| s.iter().any(|it| it.get("id").map(scalar_str).as_deref() == Some(first))) { format!("{lk}[{first}]{}", &path[first.len()..]) } else { path.into() }
}
/// The steps to the slot a path designates; with `create`, missing mappings on the way are allowed (created by `set`).
fn resolve(doc: &Y, path: &str, create: bool, lk: &str) -> Result<Vec<Step>> {
    let path = expand(doc, path, lk); let segs = parse_path(&path)?; let mut cur = Some(doc); let mut steps = vec![]; let mut here = String::new();
    for (si, (name, sels)) in segs.iter().enumerate() {
        let last = si == segs.len() - 1; here += &if here.is_empty() { name.clone() } else { format!(".{name}") };
        if !name.is_empty() {
            let next = cur.and_then(|c| c.get(name.as_str())).filter(|v| !v.is_null());
            if let Some(c) = cur { if !c.is_mapping() { bail!("{here}: not a mapping"); } }
            if next.is_none() && !(create && sels.is_empty()) { bail!("{here}: no such {}", if sels.is_empty() { "field" } else { "list" }); }
            steps.push(Step::K(name.clone())); cur = next; if last && sels.is_empty() { return Ok(steps); }
        }
        for sel in sels { let seq = cur.and_then(Y::as_sequence).ok_or_else(|| anyhow!("{here}: not a list"))?; let i = find_item(seq, sel, &here)?; steps.push(Step::I(i)); cur = seq.get(i); here += &format!("[{sel}]"); }
    }
    Ok(steps)
}
fn get_mut<'a>(doc: &'a mut Y, steps: &[Step], create: bool) -> Result<&'a mut Y> {
    let mut cur = doc;
    for s in steps { cur = match s { Step::K(k) => { if cur.is_null() && create { *cur = Y::Mapping(Mapping::new()); } let m = cur.as_mapping_mut().ok_or_else(|| anyhow!("{k}: not a mapping"))?; if !m.contains_key(k.as_str()) { m.insert(Y::String(k.clone()), Y::Null); } m.get_mut(k.as_str()).unwrap() }
        Step::I(i) => cur.as_sequence_mut().and_then(|s| s.get_mut(*i)).ok_or_else(|| anyhow!("no item {i}"))? }; }
    Ok(cur)
}
fn to_yaml(v: &J) -> Result<Y> { Ok(serde_yaml::to_value(v)?) }
/// What part of the text an operation touches: a whole top-level key, or one item of a top-level list.
#[derive(Debug, Clone, PartialEq)]
enum Unit { Top(String), Item(String, usize), Append(String), Remove(String, usize) }
fn apply(doc: &mut Y, op: &J, lk: &str) -> Result<Unit> {
    let kind = op["op"].as_str().unwrap_or(""); let path = op["path"].as_str().unwrap_or("");
    let unit = |steps: &[Step]| match steps { [Step::K(k)] => Unit::Top(k.clone()), [Step::K(k), Step::I(i), ..] => Unit::Item(k.clone(), *i), [Step::K(k), ..] => Unit::Top(k.clone()), _ => Unit::Top(String::new()) };
    match kind {
        "set" => { let st = resolve(doc, path, true, lk)?; *get_mut(doc, &st, true)? = to_yaml(&op["value"])?; Ok(unit(&st)) }
        "unset" => { let st = resolve(doc, path, false, lk)?; let (last, parent) = st.split_last().unwrap(); let Step::K(k) = last else { bail!("{path}: use `remove` to delete a list item") };
            get_mut(doc, parent, false)?.as_mapping_mut().ok_or_else(|| anyhow!("{path}: not a mapping"))?.remove(k.as_str()); Ok(unit(&st)) }
        "append" => { let st = resolve(doc, path, false, lk)?; let seq = get_mut(doc, &st, false)?.as_sequence_mut().ok_or_else(|| anyhow!("{path}: not a list"))?; seq.push(to_yaml(&op["value"])?);
            Ok(match st.as_slice() { [Step::K(k)] => Unit::Append(k.clone()), _ => unit(&st) }) }
        "remove" => { let st = resolve(doc, path, false, lk)?; let (last, parent) = st.split_last().unwrap(); let Step::I(i) = last else { bail!("{path}: only list items can be removed (use `unset` for a field)") };
            get_mut(doc, parent, false)?.as_sequence_mut().unwrap().remove(*i); Ok(match st.as_slice() { [Step::K(k), Step::I(i)] => Unit::Remove(k.clone(), *i), _ => unit(&st) }) }
        k => bail!("unknown operation {k:?}"),
    }
}
fn text_op(text: &str, op: &J) -> Result<String> {
    match op["op"].as_str().unwrap_or("") {
        "append_text" => { let add = op["value"].as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow!("append_text needs a non-empty text value"))?; let add = add.trim_matches('\n');
            Ok(if text.trim().is_empty() { format!("{add}\n") } else { format!("{}\n\n{add}\n", text.trim_end_matches('\n')) }) }
        "replace_text" => { let (old, new) = (op["old"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("replace_text needs `old`"))?, op["value"].as_str().ok_or_else(|| anyhow!("replace_text needs `value`"))?);
            let n = text.matches(old).count(); if n != 1 { bail!("replace_text: the old text occurs {n} times (it must occur exactly once)"); } Ok(text.replacen(old, new, 1)) }
        k => bail!("{k:?} is not an operation on a Markdown file (append_text, replace_text)"),
    }
}

/// Apply the operations to the YAML text, re-rendering only what they touch.
pub fn yaml_edit(old: &str, exists: bool, ops: &[J], lk: &str) -> Result<String> {
    let (mut doc, rest, mut text) = match ops.first() {
        Some(o) if o["op"] == "create" => { if exists { bail!("the file already exists"); } let d = to_yaml(&o["value"])?; let t = render_doc(&d); (d, &ops[1..], t) }
        _ => { if !exists { bail!("the file does not exist"); } (if old.trim().is_empty() { Y::Mapping(Mapping::new()) } else { serde_yaml::from_str(old).context("the file is not valid YAML")? }, ops, old.to_string()) }
    };
    for op in rest {
        let unit = apply(&mut doc, op, lk).map_err(|e| anyhow!("{e}"))?;
        text = patch(&text, &doc, &unit)?;
    }
    if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
    Ok(text)
}

// ---------------------------------------------------------------- the text side: ranges of top-level keys and list items
fn content(l: &str) -> bool { let t = l.trim(); !t.is_empty() && !t.starts_with('#') }
fn indent_of(l: &str) -> usize { l.len() - l.trim_start_matches(' ').len() }
/// Line range [start, end) of a top-level key (trailing blank / comment lines excluded: they belong to the next key).
fn top_range(lines: &[&str], key: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':')) || l.strip_prefix(&format!("'{key}'")).or(l.strip_prefix(&format!("\"{key}\""))).is_some_and(|r| r.starts_with(':')))?;
    let mut end = (start + 1..lines.len()).find(|&j| content(lines[j]) && indent_of(lines[j]) == 0 && !lines[j].starts_with('-')).unwrap_or(lines.len());
    while end > start + 1 && !content(lines[end - 1]) { end -= 1; }
    Some((start, end))
}
/// Line ranges of the items of a block list.
fn item_ranges(lines: &[&str], (s, e): (usize, usize)) -> Vec<(usize, usize)> {
    let dash = |l: &str| { let t = l.trim_start_matches(' '); t == "-" || t.starts_with("- ") };
    let Some(first) = (s + 1..e).find(|&j| dash(lines[j])) else { return vec![] }; let ind = indent_of(lines[first]);
    let starts: Vec<usize> = (first..e).filter(|&j| dash(lines[j]) && indent_of(lines[j]) == ind).collect();
    starts.iter().enumerate().map(|(n, &a)| { let mut b = starts.get(n + 1).copied().unwrap_or(e); while b > a + 1 && (!content(lines[b - 1]) && (lines[b - 1].trim().is_empty() || indent_of(lines[b - 1]) <= ind)) { b -= 1; } (a, b) }).collect()
}
fn patch(text: &str, doc: &Y, unit: &Unit) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect(); let join = |a: &[&str]| a.iter().map(|l| format!("{l}\n")).collect::<String>();
    let top = |k: &str| doc.get(k).cloned().unwrap_or(Y::Null);
    let replace_top = |k: &str| -> String { match top_range(&lines, k) {
        Some((s, e)) => format!("{}{}{}", join(&lines[..s]), if doc.get(k).is_some() { render_entry(k, &top(k), 0) } else { String::new() }, join(&lines[e..])),
        None if doc.get(k).is_some() => format!("{}{}{}", text.trim_end_matches('\n'), if text.trim().is_empty() { "" } else { "\n" }, render_entry(k, &top(k), 0)),
        None => text.to_string() } };
    let items = |k: &str| -> Option<(Vec<(usize, usize)>, usize)> { let r = top_range(&lines, k)?; let it = item_ranges(&lines, r); let n = top(k).as_sequence().map_or(0, |s| s.len()); Some((it, n)) };
    Ok(match unit {
        Unit::Top(k) => replace_top(k),
        Unit::Item(k, i) => match items(k) { Some((it, n)) if it.len() == n && *i < n => { let (a, b) = it[*i]; let ind = indent_of(lines[a]); format!("{}{}{}", join(&lines[..a]), render_item(&top(k)[*i], ind), join(&lines[b..])) } _ => replace_top(k) },
        Unit::Remove(k, i) => match items(k) { Some((it, n)) if it.len() == n + 1 => { let (a, b) = it[*i]; let a2 = if a > 0 && lines[a - 1].trim().is_empty() && (*i > 0 || b == it.last().unwrap().1) { a - 1 } else { a };
            let mut b2 = b; if a2 == a { while b2 < lines.len() && lines[b2].trim().is_empty() && (a == 0 || (indent_of(lines[a - 1]) == 0 && content(lines[a - 1]))) { b2 += 1; } }
            format!("{}{}", join(&lines[..a2]), join(&lines[b2..])) } _ => replace_top(k) },
        Unit::Append(k) => match items(k) { Some((it, n)) if it.len() + 1 == n && !it.is_empty() => { let (_, b) = *it.last().unwrap(); let ind = indent_of(lines[it[0].0]);
                format!("{}\n{}{}", join(&lines[..b]), render_item(&top(k)[n - 1], ind), join(&lines[b..])) } _ => replace_top(k) },
    })
}

// ---------------------------------------------------------------- rendering in the house style
pub fn render_doc(d: &Y) -> String { d.as_mapping().map(|m| m.iter().map(|(k, v)| render_entry(&scalar_str(k), v, 0)).collect()).unwrap_or_default() }
fn is_scalar(v: &Y) -> bool { !matches!(v, Y::Mapping(_) | Y::Sequence(_)) }
fn render_entry(k: &str, v: &Y, ind: usize) -> String {
    let p = " ".repeat(ind); let key = scalar(&Y::String(k.into()));
    match v {
        Y::String(s) if s.contains('\n') => { let body = if s.ends_with('\n') { s.clone() } else { format!("{s}\n") }; format!("{p}{key}: |\n{}", body.lines().map(|l| if l.is_empty() { "\n".to_string() } else { format!("{p}  {l}\n") }).collect::<String>()) }
        Y::Mapping(m) if m.is_empty() => format!("{p}{key}: {{}}\n"),
        Y::Mapping(m) => format!("{p}{key}:\n{}", m.iter().map(|(k2, v2)| render_entry(&scalar_str(k2), v2, ind + 2)).collect::<String>()),
        Y::Sequence(s) if s.iter().all(is_scalar) => format!("{p}{key}: [{}]\n", s.iter().map(scalar).collect::<Vec<_>>().join(", ")),
        Y::Sequence(s) => format!("{p}{key}:\n{}", s.iter().map(|x| render_item(x, ind + 2)).collect::<String>()),
        Y::Null => format!("{p}{key}:\n"),
        x => format!("{p}{key}: {}\n", scalar(x)),
    }
}
fn render_item(v: &Y, ind: usize) -> String {
    let p = " ".repeat(ind);
    match v {
        Y::Mapping(m) if !m.is_empty() => { let body: String = m.iter().map(|(k, x)| render_entry(&scalar_str(k), x, ind + 2)).collect(); format!("{p}- {}", &body[ind + 2..]) }
        Y::Mapping(_) => format!("{p}- {{}}\n"),
        Y::Sequence(s) if s.iter().all(is_scalar) => format!("{p}- [{}]\n", s.iter().map(scalar).collect::<Vec<_>>().join(", ")),
        Y::Sequence(s) => { let body: String = s.iter().map(|x| render_item(x, ind + 2)).collect(); format!("{p}- {}", &body[ind + 2..]) }
        x => format!("{p}- {}\n", scalar(x)),
    }
}
/// One scalar: plain when YAML reads it back as the same string, single-quoted otherwise (double-quoted with escapes when needed).
pub fn scalar(v: &Y) -> String {
    match v {
        Y::Null => "null".into(), Y::Bool(b) => b.to_string(),
        Y::Number(n) => if let Some(f) = n.as_f64().filter(|_| n.is_f64()) { if f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { format!("{f}") } } else { n.to_string() },
        Y::String(s) => {
            if crate::rs!(r"^\d{4}-\d{2}-\d{2}$").is_match(s) && crate::util::d(s).is_some() { return s.clone(); }
            let special = s.is_empty() || s.trim() != s || s.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`']) || s.contains(": ") || s.contains(" #") || s.ends_with(':')
                || crate::rs!(r"(?i)^(true|false|yes|no|on|off|null|~|[-+]?(\.inf|\.nan)|[-+]?[0-9][0-9_]*(\.[0-9_]*)?([eE][-+]?[0-9]+)?|0x[0-9a-f]+|0o[0-7]+|\d{4}-\d\d?-\d\d?([Tt ].*)?|[0-9]+:[0-9]+(:[0-9]+)?)$").is_match(s);
            if s.chars().any(|c| c.is_control()) { serde_json::to_string(s).unwrap() } else if special { format!("'{}'", s.replace('\'', "''")) } else { s.clone() }
        }
        x => serde_yaml::to_string(x).unwrap_or_default().trim().to_string(),
    }
}

// ---------------------------------------------------------------- comments, diffs, files, lock
fn comments(t: &str) -> Vec<String> { t.lines().filter_map(|l| { let s = l.trim(); if s.starts_with('#') { Some(s.to_string()) } else { l.find(" #").map(|i| l[i + 1..].trim().to_string()) } }).collect() }
/// Comments of the old text that are gone from the new one.
pub fn lost_comments(old: &str, new: &str) -> Vec<String> { let mut n = comments(new); comments(old).into_iter().filter(|c| match n.iter().position(|x| x == c) { Some(i) => { n.remove(i); false } None => true }).collect() }
pub fn unified(old: &str, new: &str, rel: &str) -> String { if old == new { return String::new(); } similar::TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(&format!("a/{rel}"), &format!("b/{rel}")).to_string() }
pub fn atomic_write(p: &Path, text: &str) -> Result<()> { atomic_write_bytes(p, text.as_bytes()) }
/// Write a file atomically, readable by the owner only.
pub fn atomic_write_bytes(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    let tmp = p.with_extension(format!("{}.tmp", p.extension().and_then(|e| e.to_str()).unwrap_or(""))); std::fs::write(&tmp, data)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?; }
    std::fs::rename(&tmp, p)?; Ok(())
}
/// Exclusive writer lock on <memory>/.lock (the server, the CLI and the Python app's writers).
pub struct Lock(PathBuf);
impl Lock {
    pub fn take(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?; let p = root.join(".lock.finch"); let t0 = std::time::Instant::now();
        loop { match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) { Ok(_) => return Ok(Self(p)), Err(_) if t0.elapsed() < std::time::Duration::from_secs(10) => std::thread::sleep(std::time::Duration::from_millis(20)), Err(_) => { let _ = std::fs::remove_file(&p); } } }
    }
}
impl Drop for Lock { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }

// ---------------------------------------------------------------- the change history (git, the same layout and messages as the Python app)
pub struct Repo { pub root: PathBuf }
/// A path git understands: Windows verbatim paths (`\\?\C:\...`) lose their prefix.
fn plain(p: &Path) -> PathBuf { let s = p.to_string_lossy(); match s.strip_prefix(r"\\?\") { Some(r) if !r.starts_with("UNC") => PathBuf::from(r), _ => p.to_path_buf() } }
#[derive(Debug, Clone, serde::Serialize)]
pub struct Change { pub id: String, pub date: String, pub subject: String, pub files: Vec<String>, pub reason: Option<String>, pub source: Option<String> }
impl Repo {
    fn gitdir(&self) -> PathBuf { plain(&self.root).join(GIT_DIRNAME) }
    pub fn exists(&self) -> bool { self.gitdir().join("HEAD").exists() }
    pub fn git(&self, args: &[&str], check: bool) -> Result<std::process::Output> {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut c = Command::new("git");
        for (k, _) in std::env::vars() { if k.starts_with("GIT_") { c.env_remove(k); } }
        c.args(["-c", &format!("core.hooksPath={null}"), "-c", &format!("core.attributesFile={null}"), "-c", "core.pager=cat", "-c", "core.sshCommand=false", "-c", "diff.external=", "-c", "commit.gpgsign=false", "-c", "core.quotepath=off", "-c", "core.autocrlf=false", "-c", "core.abbrev=8", "-c", "core.fsmonitor=false"])
            .args(args).current_dir(plain(&self.root)).env("LC_ALL", "C").env("LANG", "C").env("GIT_DIR", self.gitdir()).env("GIT_WORK_TREE", plain(&self.root)).env("GIT_CONFIG_GLOBAL", null).env("GIT_CONFIG_SYSTEM", null).env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1").env("GIT_PAGER", "cat").env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true").env("GIT_AUTHOR_NAME", "coach").env("GIT_AUTHOR_EMAIL", "coach@localhost").env("GIT_COMMITTER_NAME", "coach").env("GIT_COMMITTER_EMAIL", "coach@localhost");
        let out = c.output().map_err(|_| anyhow!("git is not installed: the memory change history needs it (or set [memory] history = false in config.toml)"))?;
        if check && !out.status.success() { bail!("git {} failed: {}", args[0], String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().chars().take(300).collect::<String>()); }
        Ok(out)
    }
    fn out(&self, args: &[&str]) -> Result<String> { Ok(String::from_utf8_lossy(&self.git(args, true)?.stdout).into_owned()) }
    pub fn ensure(&self) -> Result<()> {
        if self.exists() { let ex = self.gitdir().join("info").join("exclude"); let cur = std::fs::read_to_string(&ex).unwrap_or_default();
            if !cur.lines().any(|l| l == "/.lock.finch") { std::fs::create_dir_all(self.gitdir().join("info"))?; std::fs::write(&ex, format!("{}{}/.lock.finch\n", cur, if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" }))?; } return Ok(()); }
        std::fs::create_dir_all(&self.root)?; self.git(&["init", "-q", "--initial-branch=main"], true)?; self.git(&["config", "--unset-all", "core.worktree"], false)?;
        std::fs::create_dir_all(self.gitdir().join("info"))?; std::fs::write(self.gitdir().join("info").join("exclude"), EXCLUDES.join("\n") + "\n")?;
        self.git(&["add", "-A"], true)?; self.commit("coach: initial snapshot of the memory folder", true)?; Ok(())
    }
    fn commit(&self, msg: &str, allow_empty: bool) -> Result<Option<String>> {
        if !allow_empty && self.git(&["diff", "--cached", "--quiet"], false)?.status.success() { return Ok(None); }
        let mut a = vec!["commit", "-q", "-m", msg]; if allow_empty { a.push("--allow-empty"); } self.git(&a, true)?;
        Ok(Some(self.out(&["rev-parse", "--short=8", "HEAD"])?.trim().to_string()))
    }
    pub fn dirty(&self) -> Result<Vec<String>> { Ok(self.out(&["status", "--porcelain", "--untracked-files=all"])?.lines().filter(|l| !l.trim().is_empty()).map(|l| l[3..].trim().trim_matches('"').to_string()).collect()) }
    /// Record hand edits so a later change never hides them.
    pub fn snapshot_external(&self) -> Result<Option<String>> {
        self.ensure()?; let mut f = self.dirty()?; if f.is_empty() { return Ok(None); } f.sort();
        self.git(&["add", "-A"], true)?; self.commit(&format!("coach: snapshot external edits ({}{})\n\nSource: external", f.iter().take(6).cloned().collect::<Vec<_>>().join(", "), if f.len() > 6 { " ..." } else { "" }), false)
    }
    fn body(source: &str, reason: Option<&str>) -> String { let flat = reason.map(|r| r.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default(); format!("Source: {source}{}", if flat.is_empty() { String::new() } else { format!("\nReason: {flat}") }) }
    pub fn record(&self, action: &str, files: &[String], reason: Option<&str>, source: &str, detail: Option<&str>) -> Result<Option<String>> {
        self.ensure()?; let mut a = vec!["add", "-A", "--"]; a.extend(files.iter().map(String::as_str)); self.git(&a, true)?;
        self.commit(&format!("coach: {action} {}{}\n\n{}", files.join(", "), detail.map(|d| format!(" ({d})")).unwrap_or_default(), Self::body(source, reason)), false)
    }
    /// An event that changes no file (a rejected proposal), as an empty commit.
    pub fn note(&self, subject: &str, source: &str, reason: Option<&str>) -> Result<Option<String>> { self.ensure()?; self.commit(&format!("{subject}\n\n{}", Self::body(source, reason)), true) }
    pub fn log(&self, file: Option<&str>, limit: usize) -> Result<Vec<Change>> {
        if !self.exists() { return Ok(vec![]); }
        let max = format!("--max-count={limit}"); let mut a = vec!["log", max.as_str(), "--format=\x1e%h\x1f%aI\x1f%s\x1f%b\x1d", "--name-only"]; if let Some(f) = file { a.extend(["--", f]); }
        Ok(self.out(&a)?.split('\x1e').skip(1).filter_map(|rec| { let (head, names) = rec.split_once('\x1d')?; let f: Vec<&str> = head.splitn(4, '\x1f').collect(); if f.len() < 4 { return None; }
            let tr = |k: &str| f[3].lines().find_map(|l| l.strip_prefix(&format!("{k}: ")).map(String::from));
            Some(Change { id: f[0].into(), date: f[1].into(), subject: f[2].into(), files: names.lines().filter(|x| !x.trim().is_empty()).map(String::from).collect(), reason: tr("Reason"), source: tr("Source") }) }).collect())
    }
    pub fn diff(&self, rf: &str) -> Result<String> {
        if !crate::rs!(r"^[0-9a-f]{4,40}$").is_match(rf) { bail!("{rf:?} is not a change id"); }
        if !self.git(&["rev-parse", "--verify", "--quiet", &format!("{rf}^{{commit}}")], false)?.status.success() { bail!("unknown change id {rf:?}"); }
        self.out(&["show", "--format=%h %s%n", "--no-color", "--no-ext-diff", "--no-textconv", rf])
    }
    /// 'accepted' / 'rejected' if the history holds a commit for this proposal id.
    pub fn verdict(&self, pid: &str) -> Option<&'static str> {
        if !self.exists() { return None; } let g = format!("--grep={pid}");
        let out = self.out(&["log", "--all", "--format=%s", "--fixed-strings", &g]).ok()?;
        out.lines().find_map(|l| if l.contains(&format!("accept {pid}")) { Some("accepted") } else if l.contains(&format!("reject {pid}")) { Some("rejected") } else { None })
    }
}

pub fn jsonify(y: &Y) -> J { serde_json::to_value(y).unwrap_or(J::Null) }
pub fn op(kind: &str, path: &str, value: J) -> J { if value.is_null() && kind != "set" { json!({"op": kind, "path": path}) } else { json!({"op": kind, "path": path, "value": value}) } }

#[cfg(test)]
mod tests {
    use super::*;
    const T: &str = "# my budgets\nbudgets:\n  - id: groceries # weekly shop\n    category: food.groceries\n    monthly: 400\n\n  # the car\n  - id: fuel\n    category: transport.fuel\n    monthly: 80\nnotes: keep\n";
    fn ed(ops: J) -> String { yaml_edit(T, true, ops.as_array().unwrap(), "budgets").unwrap() }
    #[test] fn set_keeps_everything_else() {
        let t = ed(json!([{"op": "set", "path": "budgets[fuel].monthly", "value": 90}]));
        assert_eq!(t, T.replace("monthly: 80", "monthly: 90"), "only the touched item is re-rendered; comments outside it stay");
        let t = ed(json!([{"op": "set", "path": "groceries.rollover", "value": true}])); assert!(t.contains("  - id: groceries\n    category: food.groceries\n    monthly: 400\n    rollover: true\n\n  # the car\n"), "{t}");
    }
    #[test] fn append_and_remove() {
        let t = ed(json!([{"op": "append", "path": "budgets", "value": {"id": "fun", "group": "leisure", "monthly": 50.5, "start": "2025-01-01", "note": "yes"}}]));
        assert!(t.contains("    monthly: 80\n\n  - id: fun\n    group: leisure\n    monthly: 50.5\n    start: 2025-01-01\n    note: 'yes'\nnotes: keep\n"), "{t}");
        let t = ed(json!([{"op": "remove", "path": "budgets[groceries]"}])); assert!(t.starts_with("# my budgets\nbudgets:\n  # the car\n  - id: fuel"), "{t}");
        let back: Y = serde_yaml::from_str(&t).unwrap(); assert_eq!(back["budgets"].as_sequence().unwrap().len(), 1);
    }
    #[test] fn create_and_nested() {
        let t = yaml_edit("", false, &[json!({"op": "create", "value": {"questions": []}}), json!({"op": "append", "path": "questions", "value": {"id": "q-001", "question": "What is: this?", "evidence": {"n": 3, "months": ["2025-01", "2025-02"]}}})], "questions").unwrap();
        assert_eq!(t, "questions:\n  - id: q-001\n    question: 'What is: this?'\n    evidence:\n      n: 3\n      months: [2025-01, 2025-02]\n");
        let h = "members:\n  - id: kiki\n    name: Kiki\n    role: child\n";
        let t = yaml_edit(h, true, &[json!({"op": "set", "path": "members[kiki].pocket_money", "value": {"amount": 10, "period": "weekly"}})], "members").unwrap();
        assert_eq!(t, "members:\n  - id: kiki\n    name: Kiki\n    role: child\n    pocket_money:\n      amount: 10\n      period: weekly\n");
        let t = yaml_edit(h, true, &[json!({"op": "set", "path": "attribution", "value": [{"id": "r1", "member": "kiki", "match": {"card_last4": "0123"}}]})], "members").unwrap();
        assert!(t.ends_with("attribution:\n  - id: r1\n    member: kiki\n    match:\n      card_last4: '0123'\n"), "{t}");
        assert!(yaml_edit(h, true, &[json!({"op": "unset", "path": "members[0]"})], "members").is_err());
    }
    #[test] fn comments_lost() { assert_eq!(lost_comments(T, &T.replace(" # weekly shop", "")), vec!["# weekly shop".to_string()]); }
}
