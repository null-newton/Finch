//! `finch`: the command line. Local output keeps raw values; only the MCP server redacts (it talks to a model).
use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use finch::{analytics::{self, averages::category_averages, cashflow::cashflow, Dataset, Scope}, classify, config, db, ingest::{self, accounts, profiles}, mcp, secrets};
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
}
#[derive(Subcommand)] enum DbCmd { /// Apply pending migrations (a safety copy is taken first)
    Migrate { #[arg(long)] create: bool }, /// Applied and pending migrations, table counts
    Status }
#[derive(Subcommand)] enum AccCmd { /// List accounts
    List, /// Set label / owner / purpose / exclusion of an account
    Set { ident: String, #[arg(long)] label: Option<String>, #[arg(long)] owner: Option<String>, #[arg(long)] purpose: Option<String>, #[arg(long)] exclude: bool, #[arg(long)] include: bool, #[arg(long)] resolve: bool } }
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
