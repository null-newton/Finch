//! The analytics engine: pure functions of a [`Dataset`] loaded once per call. Money is integer cents internally and a
//! two-decimal STRING in every JSON result (never a float); ratios are floats rounded to 4 decimals.
//! Rules applied once for everybody: excluded / needs-review accounts are out; only EUR transactions are analysed (others are
//! counted in `foreign`); spending = every category except `transfer.*` and `income.*`; refunds lower spending.
pub mod anomalies;
pub mod averages;
pub mod cashflow;
pub mod planning;
pub mod recurring;

use crate::{classify::rules::{categorised, Rules}, config::Config, memory, util::*};
use anyhow::{bail, Result};
use chrono::NaiveDate;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const SAVED_TAGS: [&str; 2] = ["savings", "investment"];
pub const NO_AVERAGE_TAGS: [&str; 2] = ["one_off", "exclude_from_averages"];
pub const CAPITAL_TAG: &str = "capital";
pub fn is_transfer(c: &str) -> bool { c.starts_with("transfer.") }
/// Real income: `income.refund` is a refund (negative spending), not income.
pub fn is_income(c: &str) -> bool { c.starts_with("income.") && c != "income.refund" }
pub fn is_spending(c: &str) -> bool { !c.starts_with("transfer.") && !c.starts_with("income.") }

// ---------------------------------------------------------------- settings ([analytics] of config.toml)
macro_rules! settings {
    ($($name:ident : $t:ident = $def:expr),* $(,)?) => {
        /// Tunable thresholds of the analytics engine; config.toml only overrides the defaults.
        #[derive(Debug, Clone)] pub struct Settings { $(pub $name: settings!(@ty $t)),* }
        impl Default for Settings { fn default() -> Self { Self { $($name: settings!(@def $t $def)),* } } }
        impl Settings {
            pub fn from_table(t: &toml::Table) -> Result<Self> {
                let mut s = Self::default();
                for (k, v) in t { match k.as_str() { $(stringify!($name) => s.$name = settings!(@get $t k v),)* _ => bail!("unknown [analytics] setting(s): {k}") } }
                if !(0.0 < s.recurring_amount_tolerance && s.recurring_amount_tolerance < 1.0) { bail!("[analytics] recurring_amount_tolerance must be between 0 and 1 (0.15 = +-15 %)"); }
                if s.recurring_min_occurrences < 2 || s.average_window_months < 1 { bail!("[analytics] recurring_min_occurrences must be >= 2 and average_window_months >= 1"); }
                Ok(s)
            }
        }
    };
    (@ty u) => { usize }; (@ty f) => { f64 }; (@ty l) => { Vec<String> };
    (@def l $d:expr) => { $d.iter().map(|x: &&str| x.to_string()).collect() }; (@def $t:ident $d:expr) => { $d };
    (@get u $k:ident $v:ident) => { match $v { toml::Value::Integer(i) if *i >= 0 => *i as usize, _ => bail!("[analytics] {} must be a whole number >= 0, got {}", $k, $v) } };
    (@get f $k:ident $v:ident) => { match $v { toml::Value::Integer(i) if *i >= 0 => *i as f64, toml::Value::Float(x) if *x >= 0.0 => *x, _ => bail!("[analytics] {} must be a number >= 0, got {}", $k, $v) } };
    (@get l $k:ident $v:ident) => { match $v.as_array() { Some(a) if a.iter().all(|x| x.is_str()) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(), _ => bail!("[analytics] {} must be a list of strings", $k) } };
}
settings! {
    average_window_months: u = 12, average_min_months: u = 3, coverage_min_share: f = 0.10, lumpy_categories: l = ["travel.", "housing.renovation", "taxes."], lumpy_min_months: u = 12, household_mismatch_pct: f = 0.10,
    recurring_amount_tolerance: f = 0.15, recurring_min_occurrences: u = 3,
    recurring_loose_amount_categories: l = ["housing.rental_property_loan", "housing.mortgage", "housing.property_charges", "debt.", "insurance."],
    recurring_discretionary_groups: l = ["food", "shopping", "leisure", "travel", "personal_care", "cash", "transport.fuel", "transport.parking_tolls", "transport.taxi_rideshare"],
    recurring_discretionary_min_occurrences: u = 5,
    recurring_yearly_pair_groups: l = ["subscriptions", "insurance", "housing", "taxes", "fees", "health", "education", "charity", "debt", "income"],
    recurring_ended_factor: f = 1.5,
    recurring_variable_categories: l = ["housing.energy", "housing.water", "subscriptions.telecom", "housing.property_charges", "health.health_insurance", "taxes.taxes"],
    price_change_threshold_pct: f = 3.0, price_change_min_abs: f = 0.50, price_change_income_pct: f = 5.0, price_change_variable_pct: f = 20.0,
    anomaly_min_history_months: u = 6, anomaly_z: f = 3.5, anomaly_min_excess: f = 50.0, anomaly_min_ratio: f = 1.5, anomaly_duplicate_days: u = 3, anomaly_duplicate_min_amount: f = 10.0,
    anomaly_new_merchant_days: u = 60, anomaly_new_merchant_min: f = 150.0, anomaly_large_tx_min: f = 100.0, anomaly_large_tx_iqr_factor: f = 3.0, anomaly_lookback_days: u = 90, anomaly_months: u = 3,
    forecast_band_z: f = 1.28, forecast_variable_months: u = 6, forecast_balance_stale_days: u = 3, budget_suggest_months: u = 6, calendar_days: u = 60, asset_stale_months: u = 3,
    loan_grace_days: u = 5, loan_reminder_months: u = 6, rental_reminder_months: u = 12, rental_rent_grace_days: u = 7, rental_rate_gap_pts: f = 0.5,
}
pub fn in_list(cat: &str, names: &[String]) -> bool { names.iter().any(|n| cat == n || (n.ends_with('.') && cat.starts_with(n.as_str()))) }

// ---------------------------------------------------------------- dataset
#[derive(Debug, Clone)]
pub struct AccountInfo { pub uid: String, pub label: String, pub bank: Option<String>, pub owner: Option<String>, pub purpose: Option<String>, pub currency: Option<String> }
#[derive(Debug, Clone)]
pub struct Tx { pub key: String, pub date: NaiveDate, pub amount_c: i64, pub category: String, pub source: String, pub tags: BTreeSet<String>, pub event: Option<String>, pub entity: String, pub merchant: Option<String>, pub mkey: String, pub account: String, pub ty: Option<String>, pub desc: String, pub split_index: Option<usize>, pub split_of_c: Option<i64> }
impl Tx {
    pub fn month(&self) -> String { month_key(self.date) }
    pub fn is_saved(&self) -> bool { SAVED_TAGS.iter().any(|t| self.tags.contains(*t)) }
    pub fn is_one_off(&self) -> bool { NO_AVERAGE_TAGS.iter().any(|t| self.tags.contains(*t)) || self.tags.contains(CAPITAL_TAG) }
}

#[derive(Debug, Clone, Default)]
pub struct Scope { pub owners: Option<BTreeSet<String>>, pub purposes: Option<BTreeSet<String>>, pub accounts: Option<BTreeSet<String>> }
impl Scope {
    pub fn is_all(&self) -> bool { self.owners.is_none() && self.purposes.is_none() && self.accounts.is_none() }
    pub fn accepts(&self, a: &AccountInfo) -> bool {
        self.owners.as_ref().is_none_or(|o| o.contains(a.owner.as_deref().unwrap_or(""))) && self.purposes.as_ref().is_none_or(|p| p.contains(a.purpose.as_deref().unwrap_or(""))) && self.accounts.as_ref().is_none_or(|x| x.contains(&a.uid) || x.contains(&a.label))
    }
    pub fn describe(&self) -> Value { let f = |s: &Option<BTreeSet<String>>| s.as_ref().map_or(Value::Null, |s| json!(s)); json!({"owners": f(&self.owners), "purposes": f(&self.purposes), "accounts": f(&self.accounts)}) }
}

#[derive(Debug, Clone)]
pub struct AccountCoverage { pub account: String, pub label: String, pub bank: Option<String>, pub owner: Option<String>, pub purpose: Option<String>, pub first: Option<NaiveDate>, pub last: Option<NaiveDate>, pub last_booked: Option<NaiveDate>, pub last_sync: Option<NaiveDate>, pub n_tx: i64, pub months: Vec<String>, pub partial_months: Vec<String> }
/// Which months each account has data for: covered = first <= month start, last >= month end, month closed before today.
pub struct Coverage { pub today: NaiveDate, pub cov: BTreeMap<String, AccountCoverage> }
fn ds(x: Option<NaiveDate>) -> Value { x.map_or(Value::Null, |d| d.to_string().into()) }
impl Coverage {
    pub fn new(accounts: &BTreeMap<String, AccountInfo>, stats: &HashMap<String, (NaiveDate, NaiveDate, i64)>, last_sync: &HashMap<String, NaiveDate>, today: NaiveDate) -> Self {
        let closed = last_closed_month(today);
        let cov = accounts.iter().map(|(u, a)| {
            let (first, lb, n) = stats.get(u).map_or((None, None, 0), |s| (Some(s.0), Some(s.1), s.2));
            let sync = last_sync.get(u).copied(); let last = [lb, sync].into_iter().flatten().max().map(|l| l.min(today));
            let (mut months, mut partial) = (vec![], vec![]);
            if let (Some(f), Some(l)) = (first, last) { for m in months_between(&month_key(f), month_key(l).min(month_key(today)).as_str()) { if f <= month_start(&m) && l >= month_end(&m) && m <= closed { months.push(m) } else { partial.push(m) } } }
            (u.clone(), AccountCoverage { account: u.clone(), label: a.label.clone(), bank: a.bank.clone(), owner: a.owner.clone(), purpose: a.purpose.clone(), first, last, last_booked: lb, last_sync: sync, n_tx: n, months, partial_months: partial })
        }).collect();
        Self { today, cov }
    }
    pub fn covered(&self, uid: &str) -> BTreeSet<String> { self.cov.get(uid).map(|c| c.months.iter().cloned().collect()).unwrap_or_default() }
    pub fn common_months(&self, uids: &[String]) -> Vec<String> {
        let Some((h, t)) = uids.split_first() else { return vec![] };
        t.iter().fold(self.covered(h), |s, u| s.intersection(&self.covered(u)).cloned().collect()).into_iter().collect()
    }
    pub fn closed_months(&self, uids: &[String]) -> Vec<String> {
        let Some(lo) = uids.iter().filter_map(|u| self.cov.get(u)?.first).min().map(month_key) else { return vec![] };
        let hi = last_closed_month(self.today); if lo <= hi { months_between(&lo, &hi) } else { vec![] }
    }
    pub fn first(&self, uid: &str) -> Option<NaiveDate> { self.cov.get(uid)?.first }
    pub fn account_ref(&self, uid: &str) -> Value { let c = &self.cov[uid]; json!({"uid": uid, "label": c.label, "owner": c.owner, "purpose": c.purpose, "first": ds(c.first), "last": ds(c.last)}) }
    /// The `coverage` block every result carries.
    pub fn info(&self, uids: &[String], months: &[String], rule: &str, notes: Vec<String>) -> Value {
        let mut u = uids.to_vec(); u.sort(); u.dedup();
        let lo = months.first();
        let skipped: Vec<_> = self.closed_months(&u).into_iter().filter(|m| !months.contains(m) && lo.is_none_or(|l| m >= l)).collect();
        json!({"rule": rule, "months": months, "n_months": months.len(), "accounts": u.iter().filter(|x| self.cov.contains_key(*x)).map(|x| self.account_ref(x)).collect::<Vec<_>>(), "skipped_months": skipped, "partial_current_month": month_key(self.today), "notes": notes})
    }
}
pub fn last_n(m: &[String], n: usize) -> Vec<String> { if n > 0 && m.len() > n { m[m.len() - n..].to_vec() } else { m.to_vec() } }

#[derive(Debug, Clone)]
pub struct Balance { pub account: String, pub amount_c: i64, pub ty: String, pub fetched_at: String, pub as_of: NaiveDate }
pub const BALANCE_PREFERENCE: [&str; 8] = ["CLBD", "ITBD", "XPCD", "CLAV", "ITAV", "OPBD", "OPAV", "FWAV"];
pub struct Dataset { pub today: NaiveDate, pub settings: Settings, pub accounts: BTreeMap<String, AccountInfo>, pub txs: Vec<Tx>, pub coverage: Coverage, pub foreign: Vec<String>, pub memory_warnings: Vec<String>,
    /// tx_key -> (creditor id, mandate ref) from the parsers
    pub parse_meta: HashMap<String, (Option<String>, Option<String>)>,
    /// stored recurring series id -> its tx keys (id continuity)
    pub stored_members: BTreeMap<String, BTreeSet<String>>,
    /// uid -> latest balance of the most "booked" type
    pub balances: BTreeMap<String, Balance>, pub memory_dir: std::path::PathBuf }

impl Dataset {
    pub fn load(con: &Connection, cfg: &Config, today: Option<NaiveDate>) -> Result<Self> {
        let rules = Rules::load(cfg)?; let (ann, warn) = memory::load_annotations(&cfg.memory_dir);
        let mut ds = Self::load_with(con, &rules, &ann, today.unwrap_or_else(|| chrono::Local::now().date_naive()), Settings::from_table(&cfg.analytics)?, warn.into_iter().collect())?;
        ds.memory_dir = cfg.memory_dir.clone(); Ok(ds)
    }
    pub fn load_with(con: &Connection, rules: &Rules, ann: &[memory::Annotation], today: NaiveDate, settings: Settings, memory_warnings: Vec<String>) -> Result<Self> {
        let accounts: BTreeMap<String, AccountInfo> = con.prepare("SELECT uid, COALESCE(NULLIF(label,''), NULLIF(name,''), uid), bank, owner, purpose, currency FROM accounts WHERE exclude=0 AND needs_review=0 ORDER BY uid")?
            .query_map([], |r| Ok(AccountInfo { uid: r.get(0)?, label: r.get(1)?, bank: r.get(2)?, owner: r.get(3)?, purpose: r.get(4)?, currency: r.get(5)? }))?.map(|a| a.map(|a| (a.uid.clone(), a))).collect::<rusqlite::Result<_>>()?;
        let (mut txs, mut foreign) = (vec![], BTreeSet::new());
        for t in categorised(con, rules, ann, false)? {
            let Some(date) = d(&t.date).filter(|x| *x <= today) else { continue };
            if !accounts.contains_key(&t.account) { continue; }
            if !matches!(t.currency.as_deref(), None | Some("") | Some("EUR")) { foreign.insert(t.tx_key); continue; }
            txs.push(Tx { entity: t.entity.clone().or(t.merchant.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| if t.key.is_empty() { t.tx_key.clone() } else { t.key.clone() }), key: t.tx_key, date, amount_c: to_cents(t.amount), category: t.category, source: t.source,
                tags: t.tags.into_iter().collect(), event: t.event, merchant: t.merchant, mkey: t.key, account: t.account, ty: t.ty, desc: t.desc, split_index: t.split_index, split_of_c: t.split_of.map(to_cents) });
        }
        txs.sort_by(|a, b| (a.date, &a.key, a.split_index.unwrap_or(0)).cmp(&(b.date, &b.key, b.split_index.unwrap_or(0))));
        let mut stats = HashMap::new();
        for r in con.prepare("SELECT account_uid, MIN(booking_date), MAX(booking_date), COUNT(*) FROM transactions WHERE booking_date IS NOT NULL AND booking_date <= ? GROUP BY 1")?.query_map([today.to_string()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
            let (u, lo, hi, n) = r?; if accounts.contains_key(&u) { if let (Some(lo), Some(hi)) = (d(&lo), d(&hi)) { stats.insert(u, (lo, hi, n)); } }
        }
        let mut last_sync = HashMap::new();
        for r in con.prepare("SELECT account_uid, MAX(ran_at) FROM sync_log WHERE ok=1 AND substr(ran_at,1,10) <= ? GROUP BY 1")?.query_map([today.to_string()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (u, ts) = r?; if let Some(x) = ts.as_deref().and_then(d) { if accounts.contains_key(&u) { last_sync.insert(u, x); } }
        }
        let coverage = Coverage::new(&accounts, &stats, &last_sync, today);
        let parse_meta = con.prepare("SELECT tx_key, creditor_id, mandate_ref FROM tx_parse_meta WHERE creditor_id IS NOT NULL OR mandate_ref IS NOT NULL")?.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?;
        let mut stored_members: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in con.prepare("SELECT series_id, tx_key FROM recurring_members")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? { let (s, k) = r?; stored_members.entry(s).or_default().insert(k); }
        let mut latest: BTreeMap<String, BTreeMap<String, (String, i64)>> = BTreeMap::new();
        for r in con.prepare("SELECT account_uid, fetched_at, balance_type, amount, currency FROM balances ORDER BY fetched_at, rowid")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<String>>(4)?)))? {
            let (u, f, t, a, c) = r?; let Some(a) = a else { continue };
            if !accounts.contains_key(&u) || !matches!(c.as_deref(), None | Some("") | Some("EUR")) || d(&f).is_none_or(|x| x > today) { continue; }
            latest.entry(u).or_default().insert(t.unwrap_or_else(|| "?".into()), (f, to_cents(a)));
        }
        let balances = latest.into_iter().map(|(u, by)| { let t = BALANCE_PREFERENCE.iter().find(|t| by.contains_key(**t)).map(|t| t.to_string()).unwrap_or_else(|| by.keys().next().unwrap().clone()); let (f, c) = by[&t].clone(); (u.clone(), Balance { account: u, amount_c: c, ty: t, as_of: d(&f).unwrap(), fetched_at: f }) }).collect();
        Ok(Self { today, settings, accounts, txs, coverage, foreign: foreign.into_iter().collect(), memory_warnings, parse_meta, stored_members, balances, memory_dir: std::path::PathBuf::new() })
    }
    pub fn accounts_in(&self, s: &Scope) -> Vec<&AccountInfo> { self.accounts.values().filter(|a| s.accepts(a)).collect() }
    pub fn uids_in(&self, s: &Scope) -> Vec<String> { self.accounts_in(s).into_iter().map(|a| a.uid.clone()).collect() }
    pub fn txs_in(&self, s: &Scope) -> Vec<&Tx> { if s.is_all() { return self.txs.iter().collect(); } let ok: BTreeSet<String> = self.uids_in(s).into_iter().collect(); self.txs.iter().filter(|t| ok.contains(&t.account)).collect() }
    pub fn label(&self, uid: &str) -> String { self.accounts.get(uid).map_or(uid.into(), |a| a.label.clone()) }
    /// One Tx per bank transaction: split parts merged back (whole amount, category of the largest part).
    pub fn whole(&self) -> Vec<Tx> {
        let (mut w, mut parts): (Vec<Tx>, BTreeMap<String, Vec<&Tx>>) = (vec![], BTreeMap::new());
        for t in &self.txs { if t.split_index.is_none() { w.push(t.clone()) } else { parts.entry(t.key.clone()).or_default().push(t) } }
        for (_, p) in parts { let big = *p.iter().max_by_key(|x| (x.amount_c.abs(), -(x.split_index.unwrap_or(0) as i64))).unwrap(); let mut t = big.clone(); t.amount_c = p[0].split_of_c.unwrap_or_else(|| p.iter().map(|x| x.amount_c).sum()); t.source = "split".into(); t.split_index = None; t.split_of_c = None; w.push(t); }
        w.sort_by(|a, b| (a.date, &a.key).cmp(&(b.date, &b.key))); w
    }
    /// (major, minor) accounts carrying `category` (major = >= coverage_min_share of its money over the last 12 closed months).
    pub fn carriers_split(&self, category: &str, s: &Scope) -> (Vec<String>, Vec<String>) {
        let allowed: BTreeSet<String> = self.uids_in(s).into_iter().collect();
        let txs: Vec<&Tx> = self.txs.iter().filter(|t| t.category == category && allowed.contains(&t.account)).collect();
        let lo = format!("{}-01", add_months_key(&last_closed_month(self.today), -11));
        let recent: Vec<&Tx> = { let r: Vec<_> = txs.iter().copied().filter(|t| t.date.to_string() >= lo).collect(); if r.is_empty() { txs.clone() } else { r } };
        let tot: i64 = recent.iter().map(|t| t.amount_c.abs()).sum();
        let mut per: BTreeMap<&str, i64> = BTreeMap::new(); for t in &recent { *per.entry(&t.account).or_default() += t.amount_c.abs(); }
        let every: BTreeSet<String> = txs.iter().map(|t| t.account.clone()).collect();
        let mut major: Vec<String> = every.iter().filter(|u| tot != 0 && *per.get(u.as_str()).unwrap_or(&0) as f64 / tot as f64 >= self.settings.coverage_min_share).cloned().collect();
        if major.is_empty() { major = every.iter().cloned().collect(); }
        let minor = every.into_iter().filter(|u| !major.contains(u)).collect(); (major, minor)
    }
    /// Accounts carrying at least one income / spending / saved transaction.
    pub fn flow_accounts(&self) -> BTreeSet<String> { self.txs.iter().filter(|t| t.is_saved() || !is_transfer(&t.category)).map(|t| t.account.clone()).collect() }
    pub fn base_notes(&self) -> Vec<String> {
        let mut n = vec![]; if !self.foreign.is_empty() { n.push(format!("{} non-EUR transaction(s) left out", self.foreign.len())); }
        n.extend(self.memory_warnings.iter().take(3).map(|w| format!("memory: {w}"))); n
    }
}

/// The `coverage` tool: which accounts and months the data covers.
pub fn coverage(ds: &Dataset) -> Value {
    let accounts: Vec<Value> = ds.coverage.cov.values().map(|c| json!({"account": c.account, "label": c.label, "bank": c.bank, "owner": c.owner, "purpose": c.purpose, "first": ds_(c.first), "last": ds_(c.last), "last_booked": ds_(c.last_booked), "last_sync": ds_(c.last_sync), "n_tx": c.n_tx, "n_months": c.months.len(), "months": c.months, "partial_months": c.partial_months})).collect();
    json!({"as_of": ds.today.to_string(), "accounts": accounts, "foreign_transactions": ds.foreign.len(), "notes": ds.base_notes(), "rule": "a month is covered by an account when its data spans the whole month and the month is closed"})
}
fn ds_(x: Option<NaiveDate>) -> Value { ds(x) }
