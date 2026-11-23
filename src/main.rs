//! `finch`: the command line. Local output keeps raw values; only the MCP server redacts (it talks to a model).
use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use finch::{analytics::{self, anomalies, averages::category_averages, cashflow::cashflow, planning, recurring, Dataset, Scope}, classify::{self, rules::Rules}, config, db, ingest::{self, accounts, profiles}, mcp, secrets, transfers};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "finch", version, about = "Private personal-finance coach for one household: numbers come from code")]
struct Cli {
    /// config.toml to use (default: $COACH_CONFIG, else the nearest config.toml, else ~/.ai-finance-coach)
    #[arg(long, global = true)] config: Option<PathBuf>,
    /// allow a plaintext (unencrypted) database
    #[arg(long, global = true)] insecure: bool,
    #[command(subcommand)] cmd: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    /// Show the effective configuration (never a secret)
    Config,
    /// Which secrets are set (masked)
    Secrets,
    #[command(subcommand)] Db(DbCmd),
    #[command(subcommand)] Accounts(AccCmd),
    /// Import a statement file (CSV with a profile, OFX/QFX, CAMT.053, CODA)
    Import(ImportArgs),
    /// Parse every descriptor with its bank's parser (tx_enriched)
    Normalize,
    /// Which accounts and months the data covers
    Coverage(Out),
    /// Income, spending and savings per month
    Cashflow { #[arg(long, default_value_t = 6)] months: usize, #[arg(long)] end: Option<String>, #[arg(long)] include_current: bool, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    /// Usual monthly spending per category
    Averages { #[arg(long)] window: Option<usize>, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    #[command(subcommand)] Mcp(McpCmd),
    #[command(subcommand)] Transfers(TrCmd),
    /// Recurring payments and income
    Recurring { /// store the detection (recurring_series table)
        #[arg(long)] refresh: bool, #[arg(long)] active: bool, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    /// Price changes of recurring payments
    PriceChanges { #[arg(long)] since: Option<String>, #[arg(long)] confirmed: bool, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    /// Unusual spending (spikes, duplicates, new merchants, large payments)
    Anomalies { /// store them (anomalies table)
        #[arg(long)] refresh: bool, /// dismiss an anomaly by id (it stays dismissed)
        #[arg(long)] dismiss: Option<String>, #[arg(long)] note: Option<String>, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    /// Cash-flow forecast of the balances
    Forecast { #[arg(long, default_value_t = 90)] days: i64, #[command(flatten)] scope: ScopeArgs, #[command(flatten)] out: Out },
    #[command(subcommand)] Budgets(BudCmd),
    /// Savings goals and their progress
    Goals(Out),
    /// Upcoming payments and deadlines
    Calendar { #[arg(long)] days: Option<usize>, #[arg(long)] transfers: bool, #[command(flatten)] out: Out },
    /// Recompute and store recurring series and anomalies, and list transfer proposals (the daily job)
    Refresh,
}
#[derive(Subcommand)] enum DbCmd { /// Apply pending migrations (a safety copy is taken first)
    Migrate { #[arg(long)] create: bool }, /// Applied and pending migrations, table counts
    Status }
#[derive(Subcommand)] enum AccCmd { /// List accounts
    List, /// Set label / owner / purpose / exclusion of an account
    Set { ident: String, #[arg(long)] label: Option<String>, #[arg(long)] owner: Option<String>, #[arg(long)] purpose: Option<String>, #[arg(long)] exclude: bool, #[arg(long)] include: bool, #[arg(long)] resolve: bool } }
#[derive(Subcommand)] enum TrCmd { /// Find internal-transfer pairs (proposals only, unless --link)
    Match { #[arg(long)] link: bool }, /// Stored links
    List, /// Link a debit and a credit by hand
    Link { out_key: String, in_key: String, #[arg(long)] allow_amount_mismatch: bool }, /// Remove a link (the pair is never proposed again)
    Unlink { reference: String } }
#[derive(Subcommand)] enum BudCmd { /// This month's progress of every budget (memory/budgets.yaml)
    Status { #[arg(long)] as_of: Option<String>, #[command(flatten)] out: Out }, /// Suggested budgets from recent months
    Suggest { #[arg(long)] months: Option<usize>, #[arg(long)] limit: Option<usize>, #[command(flatten)] out: Out } }
#[derive(Subcommand)] enum McpCmd { /// Serve the finance tools over stdio (for Claude Code: `.mcp.json`)
    Serve, /// List the tools and their schemas
    Tools }
#[derive(Args)] struct Out { /// print the JSON result
    #[arg(long)] json: bool }
#[derive(Args)] struct ScopeArgs { #[arg(long)] account: Option<String>, #[arg(long)] owner: Option<String>, #[arg(long)] purpose: Option<String> }
#[derive(Args)]
struct ImportArgs {
    file: Option<PathBuf>,
    /// account uid / label, or new:Label for a manual account
    #[arg(long)] account: Option<String>,
    #[arg(long)] profile: Option<String>,
    #[arg(long)] format: Option<String>,
    #[arg(long)] dry_run: bool,
    #[arg(long)] bank: Option<String>,
    #[arg(long)] currency: Option<String>,
    #[arg(long)] iban: Option<String>,
    #[arg(long)] force: bool,
    #[arg(long)] list_profiles: bool,
    /// inline CSV mapping (instead of --profile)
    #[arg(long)] date_col: Option<String>, #[arg(long)] desc_col: Vec<String>, #[arg(long)] amount_col: Option<String>, #[arg(long)] debit_col: Option<String>, #[arg(long)] credit_col: Option<String>,
    #[arg(long)] date_format: Vec<String>, #[arg(long)] delimiter: Option<String>, #[arg(long)] decimal: Option<String>,
    /// save the inline mapping as a profile
    #[arg(long)] save_profile: Option<String>, #[arg(long)] overwrite_profile: bool,
}

const SHIPPED_PROFILES: [(&str, &str); 3] = [("generic-csv-be-nl", include_str!("../assets/import_profiles/generic-csv-be-nl.toml")), ("generic-csv-be-fr", include_str!("../assets/import_profiles/generic-csv-be-fr.toml")), ("revolut-csv", include_str!("../assets/import_profiles/revolut-csv.toml"))];

fn scope(s: &ScopeArgs) -> Scope { let one = |x: &Option<String>| x.clone().map(|v| [v].into()); Scope { accounts: one(&s.account), owners: one(&s.owner), purposes: one(&s.purpose) } }
fn print_json(v: &Value) { println!("{}", serde_json::to_string_pretty(v).unwrap()); }
fn s(v: &Value) -> String { v.as_str().map(String::from).unwrap_or_else(|| if v.is_null() { "-".into() } else { v.to_string() }) }
fn lines(v: &Value, key: &str, f: impl Fn(&Value) -> String) { for x in v[key].as_array().into_iter().flatten() { println!("{}", f(x)); } }
fn notes(v: &Value) { for n in v["coverage"]["notes"].as_array().into_iter().flatten() { println!("note: {}", s(n)); } }
fn date_arg(x: &Option<String>) -> Result<Option<chrono::NaiveDate>> { x.as_deref().map(|v| finch::util::d(v).ok_or_else(|| anyhow::anyhow!("{v:?} is not a date (YYYY-MM-DD)"))).transpose() }
fn rate(v: &Value) -> String { v.as_f64().map_or("-".into(), |r| format!("{:.1}%", r * 100.0)) }

fn main() {
    if let Err(e) = run() { eprintln!("finch: {e:#}"); std::process::exit(1); }
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load_config(cli.config.as_deref())?;
    let open = |migrate| db::connect(&cfg, db::Opts { insecure: cli.insecure, migrate, ..Default::default() });
    match cli.cmd {
        Cmd::Config => println!("{}", config::show(&cfg)),
        Cmd::Secrets => { for (n, st) in secrets::describe() { println!("{n:<26} {st}"); } }
        Cmd::Db(DbCmd::Migrate { create }) => { let con = db::connect(&cfg, db::Opts { insecure: cli.insecure, migrate: false, create, ..Default::default() })?; let ap = db::apply_migrations(&con, true, secrets::get_secret("db_key", false).ok().flatten().as_deref())?; println!("{}", if ap.is_empty() { "database is up to date".into() } else { format!("applied: {}", ap.join(", ")) }); }
        Cmd::Db(DbCmd::Status) => { let con = open(false)?; let (a, p) = db::status(&con)?; println!("{} applied, {} pending", a.len(), p.len()); for (v, n) in p { println!("  pending {v:04}_{n}"); } for (t, n) in db::table_counts(&con)? { println!("  {t:<28} {n}"); } }
        Cmd::Accounts(AccCmd::List) => println!("{}", accounts::format_accounts(&accounts::list_accounts(&open(true)?)?)),
        Cmd::Accounts(AccCmd::Set { ident, label, owner, purpose, exclude, include, resolve }) => {
            if exclude && include { bail!("--exclude and --include are exclusive"); }
            let members: Vec<String> = finch::memory::members(&cfg.memory_dir).into_iter().map(|m| m.id).collect();
            let uid = accounts::set_account(&open(true)?, &ident, accounts::AccountSet { label, owner, purpose, exclude: (exclude || include).then_some(exclude), resolve }, &members)?; println!("updated {uid}");
        }
        Cmd::Import(a) => import(&cfg, &open, a)?,
        Cmd::Normalize => { let con = open(true)?; println!("normalised {} transaction(s)", classify::normalize_all(&con, Some(&cfg.memory_dir))?); }
        Cmd::Coverage(o) => { let ds = Dataset::load(&open(true)?, &cfg, None)?; let v = analytics::coverage(&ds); if o.json { print_json(&v); } else {
            println!("{:<34}{:<22}{:<12}{:<12}{:>6}{:>8}", "account", "label", "first", "last", "tx", "months");
            for a in v["accounts"].as_array().unwrap() { println!("{:<34}{:<22}{:<12}{:<12}{:>6}{:>8}", s(&a["account"]), s(&a["label"]), s(&a["first"]), s(&a["last"]), s(&a["n_tx"]), s(&a["n_months"])); }
            for n in v["notes"].as_array().unwrap() { println!("note: {}", s(n)); } } }
        Cmd::Cashflow { months, end, include_current, scope: sc, out } => { let ds = Dataset::load(&open(true)?, &cfg, None)?; let v = cashflow(&ds, &scope(&sc), months, end.as_deref(), include_current, true); if out.json { print_json(&v); } else {
            println!("{:<9}{:>12}{:>12}{:>12}{:>12}{:>9}  complete", "month", "income", "spending", "saved", "net", "rate");
            for m in v["household"]["months"].as_array().unwrap() { println!("{:<9}{:>12}{:>12}{:>12}{:>12}{:>9}  {}", s(&m["month"]), s(&m["income"]), s(&m["spending"]), s(&m["saved"]), s(&m["net"]), rate(&m["savings_rate"]), if m["complete"] == true { "yes" } else { "no" }); }
            if let Some(t) = v["household"]["totals_complete"].as_object() { println!("complete months: income {} spending {} net {} savings rate {}", s(&t["income"]), s(&t["spending"]), s(&t["net"]), rate(&t["savings_rate"])); }
            for n in v["coverage"]["notes"].as_array().unwrap() { println!("note: {}", s(n)); } } }
        Cmd::Averages { window, scope: sc, out } => { let ds = Dataset::load(&open(true)?, &cfg, None)?; let v = category_averages(&ds, &scope(&sc), window); if out.json { print_json(&v); } else {
            println!("household: {} / month ({} months)", s(&v["household_monthly_avg"]), v["household_months"].as_array().map_or(0, |a| a.len()));
            println!("{:<36}{:>12}{:>14}{:>8}", "category", "per month", "with one-offs", "months");
            for c in v["categories"].as_array().unwrap() { println!("{:<36}{:>12}{:>14}{:>8}{}", s(&c["category"]), s(&c["monthly_avg"]), s(&c["monthly_avg_with_one_offs"]), s(&c["n_months"]), if c["low_confidence"] == true { "  (low confidence)" } else { "" }); }
            for u in v["unavailable"].as_array().unwrap() { println!("{:<36}  unavailable: {}", s(&u["category"]), s(&u["reason"])); }
            for n in v["coverage"]["notes"].as_array().unwrap() { println!("note: {}", s(n)); } } }
        Cmd::Mcp(McpCmd::Serve) => mcp::serve(mcp::Session::open(cfg, cli.insecure)?)?,
        Cmd::Transfers(t) => { let con = open(true)?; let o = transfers::Opts::from(&cfg); let rules = Rules::load(&cfg)?; let (ann, _) = finch::memory::load_annotations(&cfg.memory_dir);
            match t {
                TrCmd::Match { link } => { let r = transfers::match_transfers(&con, &rules, &ann, &cfg.memory_dir, &o, link || cfg.transfer_auto_link)?; println!("{}", transfers::format_result(&r, link || cfg.transfer_auto_link)); }
                TrCmd::List => { let l = transfers::list_links(&con)?; if l.is_empty() { println!("no transfer links (run `finch transfers match`)"); } for x in l { println!("{:>4} {:>10.2} {:>5.2} {:<7} {:<24} -> {:<24} {} / {}", s(&x["id"]), x["amount"].as_f64().unwrap_or(0.0), x["confidence"].as_f64().unwrap_or(0.0), s(&x["method"]), s(&x["out_account"]), s(&x["in_account"]), s(&x["out_date"]), s(&x["in_date"])); } }
                TrCmd::Link { out_key, in_key, allow_amount_mismatch } => println!("linked #{}", transfers::link(&con, &out_key, &in_key, allow_amount_mismatch)?),
                TrCmd::Unlink { reference } => { let (id, ..) = transfers::unlink(&con, &reference)?; println!("removed link #{id}; the pair will not be proposed again"); }
            } }
        Cmd::Recurring { refresh, active, scope: sc, out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?;
            if refresh { println!("{}", recurring::refresh(&con, &ds)?); return Ok(()); }
            let r = recurring::detect(&ds, &scope(&sc), !active); if out.json { print_json(&r.json); return Ok(()); }
            println!("{:<15}{:<7}{:<11}{:<34}{:>11}{:>11}  {:<11}{}", "id", "status", "cadence", "entity", "amount", "per year", "next", "category");
            for x in &r.series { println!("{:<15}{:<7}{:<11}{:<34}{:>11}{:>11}  {:<11}{}", x.id, x.status, x.cadence, finch::util::truncate_chars(&x.entity, 33), finch::util::money_str(x.expected_c), finch::util::money_str(x.yearly_c), x.next_expected.map_or("-".into(), |d| d.to_string()), x.category); }
            notes(&r.json); }
        Cmd::PriceChanges { since, confirmed, scope: sc, out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &scope(&sc), true);
            let dis: std::collections::HashSet<String> = con.prepare("SELECT id FROM price_change_dismissals")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let v = recurring::price_changes(&ds, &rec, date_arg(&since)?, confirmed, &dis, false); if out.json { print_json(&v); } else { lines(&v, "changes", |c| format!("{} {:<34} {:>10} -> {:<10} {:>7.1}%  {}/year  {}{}", s(&c["date"]), finch::util::truncate_chars(&s(&c["entity"]), 33), s(&c["old"]), s(&c["new"]), c["pct"].as_f64().unwrap_or(0.0) * 100.0, s(&c["yearly_impact"]), s(&c["effect"]), if c["confirmed"] == true { "" } else { " (unconfirmed)" })); } }
        Cmd::Anomalies { refresh, dismiss, note, scope: sc, out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &scope(&sc), true);
            if let Some(id) = dismiss { anomalies::refresh(&con, &ds, &rec)?; println!("{}", if anomalies::dismiss(&con, &id, note.as_deref())? { format!("dismissed {id}") } else { format!("no anomaly {id}") }); return Ok(()); }
            if refresh { println!("{}", anomalies::refresh(&con, &ds, &rec)?); return Ok(()); }
            let v = anomalies::detect(&ds, &scope(&sc), &rec, &anomalies::dismissed_ids(&con)?, false); if out.json { print_json(&v); } else { lines(&v, "anomalies", |a| format!("{} {:<7} {}", s(&a["id"]), s(&a["severity"]), s(&a["message"]))); notes(&v); } }
        Cmd::Forecast { days, scope: sc, out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true); let v = planning::forecast(&ds, &rec, &scope(&sc), days, out.json); if out.json { print_json(&v); } else {
            for b in std::iter::once(&v["household"]).chain(v["accounts"].as_array().unwrap()) { println!("{:<24} start {:>11}  {}  min {} on {}  flags: {}", s(&b["label"]), s(&b["start_balance"]), b["milestones"].as_array().unwrap().iter().map(|m| format!("{}d {}", s(&m["days"]), s(&m["balance"]))).collect::<Vec<_>>().join("  "), s(&b["min_balance"]), s(&b["min_date"]), b["flags"].as_array().unwrap().iter().map(s).collect::<Vec<_>>().join(", ")); }
            lines(&v, "assumptions", |a| format!("assumption: {}", s(a))); } }
        Cmd::Budgets(BudCmd::Status { as_of, out }) => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true); let v = planning::status(&ds, &rec, date_arg(&as_of)?); if out.json { print_json(&v); } else {
            lines(&v, "budgets", |b| format!("{:<24}{:<8}spent {:>10} of {:>10}  projected {:>10}  {}", s(&b["id"]), s(&b["status"]), s(&b["spent"]), s(&b["available"]), s(&b["projected"]), s(&b["target"]))); lines(&v, "warnings", |w| format!("warning: {}", s(w))); notes(&v); } }
        Cmd::Budgets(BudCmd::Suggest { months, limit, out }) => { let ds = Dataset::load(&open(true)?, &cfg, None)?; let v = planning::suggest(&ds, &Scope::default(), months, limit); if out.json { print_json(&v); } else { lines(&v, "suggestions", |x| format!("{:<36}{:>10}/month  (median {}, {} months){}", s(&x["category"]), s(&x["suggested"]), s(&x["median"]), s(&x["n_months"]), if x["low_confidence"] == true { " low confidence" } else { "" })); } }
        Cmd::Goals(out) => { let ds = Dataset::load(&open(true)?, &cfg, None)?; let v = planning::goals(&ds); if out.json { print_json(&v); } else { lines(&v, "goals", |g| format!("{:<20}{:<10}{:>10} of {:>10}  pace {}  projected {}", s(&g["id"]), s(&g["status"]), s(&g["current"]), s(&g["target"]), s(&g["pace"]), s(&g["projected_date"]))); notes(&v); } }
        Cmd::Calendar { days, transfers, out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true); let v = planning::calendar(&ds, &con, &rec, days, transfers); if out.json { print_json(&v); } else { lines(&v, "items", |i| format!("{} {:>4}d {:<9} {:<40} {:>10}", s(&i["date"]), s(&i["days_until"]), s(&i["source"]), finch::util::truncate_chars(&s(&i["title"]), 39), s(&i["amount"]))); } }
        Cmd::Refresh => { let con = open(true)?; let rules = Rules::load(&cfg)?; let (ann, _) = finch::memory::load_annotations(&cfg.memory_dir);
            let tr = transfers::match_transfers(&con, &rules, &ann, &cfg.memory_dir, &transfers::Opts::from(&cfg), cfg.transfer_auto_link)?;
            let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true);
            println!("transfers: {} {}, {} proposal(s), {} ambiguous", tr.linked.len(), if cfg.transfer_auto_link { "linked" } else { "auto-linkable (set [transfers] auto_link = true to link them)" }, tr.proposals.len(), tr.ambiguous.len());
            println!("recurring: {}", recurring::refresh(&con, &ds)?); println!("anomalies: {}", anomalies::refresh(&con, &ds, &rec)?); }
        Cmd::Mcp(McpCmd::Tools) => { for t in mcp::tools() { println!("{}: {}", t.name, t.schema); } }
    }
    Ok(())
}

fn import(cfg: &config::Config, open: &dyn Fn(bool) -> Result<rusqlite::Connection>, a: ImportArgs) -> Result<()> {
    if a.list_profiles {
        let mut l = profiles::list_profiles(&cfg.import_profiles_dir);
        for (n, t) in SHIPPED_PROFILES { if !l.iter().any(|x| x.0 == n) { let d = t.parse::<toml::Table>().ok().and_then(|x| x.get("description").and_then(|d| d.as_str()).map(String::from)).unwrap_or_default(); l.push((n.into(), format!("{d} [shipped]"))); } }
        for (n, d) in l { println!("{n:<24} {d}"); } return Ok(());
    }
    let (Some(file), Some(account)) = (a.file.as_ref(), a.account.as_deref()) else { bail!("usage: finch import FILE --account <uid|label|new:Label> [--profile NAME | --date-col ...]") };
    let profile = if let Some(p) = &a.profile {
        Some(match profiles::load_profile(&cfg.import_profiles_dir, p) { Ok(x) => x, Err(e) => match SHIPPED_PROFILES.iter().find(|s| s.0 == p) { Some((n, t)) => profiles::from_toml(&t.parse()?, n)?, None => return Err(e) } })
    } else if let Some(dc) = &a.date_col {
        let mut p = profiles::Profile { name: a.save_profile.clone().unwrap_or_else(|| "(inline)".into()), ..Default::default() };
        p.columns.push(("date".into(), profiles::Col::Name(dc.clone())));
        p.columns.push(("description".into(), if a.desc_col.len() == 1 { profiles::Col::Name(a.desc_col[0].clone()) } else { profiles::Col::Many(a.desc_col.clone()) }));
        for (k, v) in [("amount", &a.amount_col), ("debit", &a.debit_col), ("credit", &a.credit_col)] { if let Some(v) = v { p.columns.push((k.into(), profiles::Col::Name(v.clone()))); } }
        if !a.date_format.is_empty() { p.date_format = a.date_format.clone(); }
        if let Some(d) = &a.delimiter { p.delimiter = d.clone(); } if let Some(d) = &a.decimal { p.decimal = d.clone(); }
        p.validate()?;
        if a.save_profile.is_some() { println!("profile saved to {}", profiles::save_profile(&cfg.import_profiles_dir, &p, a.overwrite_profile)?.display()); }
        Some(p)
    } else { None };
    let con = open(true)?;
    let rep = ingest::import_file(&con, file, ingest::ImportOpts { account, profile: profile.as_ref(), fmt: a.format.as_deref(), dry_run: a.dry_run, bank: a.bank.as_deref(), currency: a.currency.as_deref(), iban: a.iban.as_deref(), force: a.force })?;
    println!("{}", ingest::format_report(&rep, 10));
    if !a.dry_run && rep.rows_new > 0 { println!("normalised {} transaction(s)", classify::normalize_all(&con, Some(&cfg.memory_dir))?); }
    Ok(())
}
