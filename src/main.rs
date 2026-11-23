//! `finch`: the command line. Local output keeps raw values; only the MCP server redacts (it talks to a model).
use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use finch::{analytics::{self, anomalies, averages::category_averages, cashflow::cashflow, planning, recurring, Dataset, Scope}, classify::{self, llm, rules::Rules}, config, db, egress, ingest::{self, accounts, bank, profiles}, mcp, net, secrets, transfers, web};
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
    #[command(subcommand)] Classify(ClsCmd),
    #[command(subcommand)] Bank(BankCmd),
    /// Pull new transactions and balances from the connected banks
    Sync { #[arg(long)] account: Option<String>, /// ask for the longest history again
        #[arg(long)] full: bool, /// ignore the daily limit
        #[arg(long)] force: bool },
    /// The privacy mode and the last outbound calls (egress journal)
    Privacy { #[arg(long, default_value_t = 20)] last: usize },
    /// Serve the web app on this machine and print a one-time login link
    #[command(subcommand)] Household(HhCmd),
    #[command(subcommand)] Memory(MemCmd),
    #[command(subcommand)] Backup(BackupCmd),
    /// Everything finch holds, as one archive you own (encrypted unless --plain)
    Export { #[arg(long)] out: Option<PathBuf>, /// an UNENCRYPTED zip: only at a terminal, after typing a confirmation phrase
        #[arg(long)] plain: bool, /// decrypt an export archive into --to
        #[arg(long)] decrypt: Option<PathBuf>, #[arg(long)] to: Option<PathBuf> },
    #[command(subcommand)] Alerts(AlertCmd),
    /// LLM calls, tokens and cost per job and day; the outbound destinations
    Usage { #[arg(long, default_value_t = 30)] days: i64, #[command(flatten)] out: Out },
    /// The year in review (default: the year of the last closed month)
    Review { #[arg(long)] year: Option<i32>, #[command(flatten)] scope: ScopeArgs },
    #[command(subcommand)] Coach(CoachCmd),
    /// Net worth: balances + assets - capital still due
    Wealth { #[command(flatten)] out: Out },
    #[command(subcommand)] Loans(LoanCmd),
    #[command(subcommand)] Subs(SubsCmd),
    #[command(subcommand)] Rental(RentCmd),
    #[command(subcommand)] Questions(QCmd),
    Ui { #[arg(long)] host: Option<String>, #[arg(long)] port: Option<u16>, /// API only: the page comes from `pnpm dev` (port 5173)
        #[arg(long)] dev: bool, #[arg(long)] no_browser: bool, /// print a fresh one-time login link for a running `finch ui` and exit
        #[arg(long)] login_link: bool, /// the login (`ui_users` id) the link opens; default the owner
        #[arg(long)] user: Option<String>, /// sign every browser out (new session key) and exit
        #[arg(long)] rotate_session_key: bool, /// folder of the built web app (default: static/ next to the binary, then the crate's)
        #[arg(long)] static_dir: Option<PathBuf> },
}
#[derive(Subcommand)] enum DbCmd { /// Apply pending migrations (a safety copy is taken first)
    Migrate { #[arg(long)] create: bool }, /// Applied and pending migrations, table counts
    Status }
#[derive(Subcommand)] enum BackupCmd { /// Write an encrypted, verified backup of the database and memory/ (then prune to the retention)
    Create,
    List,
    /// Decrypt and check an archive
    Verify { file: PathBuf },
    /// Restore into an empty folder (never overwrites unless --force)
    Restore { file: PathBuf, #[arg(long)] to: PathBuf, #[arg(long)] force: bool } }
#[derive(Subcommand)] enum AlertCmd { /// Evaluate the signals and send to the enabled channels (--dry-run: write and send nothing; --no-send: keep the events only)
    Check { #[arg(long)] dry_run: bool, #[arg(long)] no_send: bool },
    /// The open alerts
    List { #[arg(long)] all: bool },
    /// The channels and whether they are ready
    Channels,
    Ack { id: String },
    Snooze { id: String, #[arg(long, default_value_t = 7)] days: i64 },
    /// The weekly summary of the feed (local)
    Digest }
#[derive(Subcommand)] enum CoachCmd { /// Ask the coach a question (Claude Code with finch's redacted finance tools only)
    Ask { question: String } }
#[derive(Subcommand)] enum LoanCmd { /// Every loan with its capital still due
    List,
    /// One loan: the amortization schedule (Belgian equivalent monthly rate unless the file says proportional)
    Show { id: String, #[arg(long)] rows: bool },
    /// The legal maximum indemnity of repaying AMOUNT (EUR) early (all of it when omitted)
    Repay { id: String, amount: Option<f64> } }
#[derive(Subcommand)] enum SubsCmd { /// Every recurring cost with its contract and Belgian cancellation rule
    Inventory { #[arg(long)] ended: bool, #[command(flatten)] out: Out },
    /// Can this contract (id or series ref) be ended, when and how
    Cancel { r#ref: String } }
#[derive(Subcommand)] enum RentCmd { /// Rental properties and their last month
    List,
    /// One property: months, yearly totals, rent indexation
    Show { id: String, #[arg(long)] year: Option<i32> } }
#[derive(Subcommand)] enum MemCmd { /// Pending proposals (or --status all / accepted / rejected)
    Proposals { #[arg(long, default_value = "pending")] status: String },
    /// One proposal: what it changes, recomputed against the file as it is now
    Show { id: String },
    /// Apply a proposal after reading its diff (at a terminal, with a typed yes)
    Accept { id: String, /// apply although the file changed since the proposal was made
        #[arg(long)] force: bool, /// confirm a field flagged as coming from instruction-like text
        #[arg(long = "confirm-field")] confirm_field: Vec<String> },
    /// Close a proposal without applying it
    Reject { id: String, #[arg(long)] note: Option<String> },
    /// The change history of the memory folder
    History { #[arg(long)] file: Option<String>, #[arg(long, default_value_t = 30)] limit: usize },
    /// The patch of one recorded change
    Diff { id: String },
    /// Validate the memory files finch edits
    Check }
#[derive(Subcommand)] enum QCmd { /// Open questions (or --status answered / dismissed / all)
    List { #[arg(long, default_value = "open")] status: String },
    /// Ask a question yourself
    Add { question: String, #[arg(long, default_value = "General")] topic: String },
    /// Record an answer (turning it into memory changes is a separate step)
    Answer { id: String, text: String },
    Dismiss { id: String, #[arg(long)] reason: Option<String> },
    Reopen { id: String } }
#[derive(Subcommand)] enum HhCmd { /// Members, account owners, rules and how many transactions belong to whom
    Overview { #[command(flatten)] out: Out },
    /// The children's money: pocket money, extra top-ups and their source, spending, balance trend
    Kids { member: Option<String>, #[arg(long, default_value_t = 6)] months: usize },
    /// Kid budgets with this week's / month's progress
    KidBudgets,
    /// Shared costs split by the allocation rules: shares, what each paid, the settlement
    WhoPays { #[arg(long, default_value_t = 12)] months: usize },
    /// Whose a transaction is, and why
    Explain { tx: String },
    /// Reassign a transaction to a member (or joint) by hand; recorded and reversible
    Assign { tx: String, member: String, #[arg(long)] note: Option<String> },
    /// Remove a manual reassignment
    Clear { tx: String },
    /// The recorded manual reassignments, newest first
    Log { #[arg(long)] tx: Option<String>, #[arg(long, default_value_t = 50)] limit: i64 },
    /// Undo one recorded reassignment
    Undo { id: i64 } }
#[derive(Subcommand)] enum ClsCmd { /// Label the merchants that need it: similarity first, then the LLM (redacted, person-like keys held back)
    Run { /// print the exact redacted requests, send nothing
        #[arg(long)] dry_run: bool, /// ask again for LLM / kNN labels too
        #[arg(long)] refresh: bool, #[arg(long)] limit: Option<usize>, #[arg(long, default_value_t = 40)] batch: usize, #[arg(long)] model: Option<String>, #[arg(long)] no_knn: bool },
    /// Merchants held back (possible people) and low-confidence labels
    Review { #[command(flatten)] out: Out },
    /// Your own label of a merchant key (always wins)
    Correct { key: String, category: String, #[arg(long)] name: Option<String> } }
#[derive(Subcommand)] enum BankCmd { /// Start a bank authorisation (opens the bank's login page)
    Connect { #[arg(long)] bank: String, /// ISO country code of the bank (BE)
        #[arg(long, default_value = "BE")] country: String, #[arg(long, default_value_t = 180)] days: i64, /// connect although a live consent exists
        #[arg(long)] replace: bool, #[arg(long)] no_browser: bool },
    /// Renew the consent of a connected bank (bank name or session id)
    Reconnect { target: String, #[arg(long, default_value_t = 180)] days: i64, #[arg(long)] no_browser: bool },
    /// Finish an authorisation with the address the browser shows after the bank login
    Finish { url: Option<String>, /// store the saved answer of a failed completion again (no bank call)
        #[arg(long)] replay: Option<String> },
    /// Consents and their expiry
    Consents { /// ask the bank for the live status first
        #[arg(long)] refresh: bool } }
#[derive(Subcommand)] enum AccCmd { /// Fold account NEW into OLD (after a reconnect created a duplicate)
    Merge { old: String, new: String },
    /// List accounts
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
fn print_session(info: &Value) {
    println!("Session {} with {} valid until {}", s(&info["session_id"]), s(&info["bank"]), s(&info["valid_until"]));
    lines(info, "accounts", |a| format!("  account {}  {}{}{}", s(&a["uid"]), a["name"].as_str().unwrap_or(""), if a["remapped"] == true { format!("  (same account as before: new bank uid {}, history kept)", s(&a["api_uid"])) } else if a["new"] == true { "  (new)".into() } else { String::new() }, if a["needs_review"] == true { "  NEEDS REVIEW: not synced until `finch accounts merge OLD NEW` or `finch accounts set UID --resolve`" } else { "" }));
    if info["retired"].as_array().is_some_and(|a| !a.is_empty()) { println!("Retired replaced session(s): {}", info["retired"]); }
    if info["orphans"].as_array().is_some_and(|a| !a.is_empty()) { println!("warning: account(s) not returned by the new session, left on the retired one (not synced): {}", info["orphans"]); }
    println!("\nNext: finch sync");
}
fn open_browser(url: &str) -> std::io::Result<std::process::Child> { if cfg!(windows) { std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn() } else if cfg!(target_os = "macos") { std::process::Command::new("open").arg(url).spawn() } else { std::process::Command::new("xdg-open").arg(url).spawn() } }
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
        Cmd::Mcp(McpCmd::Serve) => { if let Some(w) = egress::evaluate(&cfg, "mcp.finance", "") { anyhow::bail!("the finance MCP server does not start: {w}"); } mcp::serve(mcp::Session::open(cfg, cli.insecure)?)? }
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
        Cmd::Accounts(AccCmd::Merge { old, new }) => println!("merged into {}", bank::merge_accounts(&open(true)?, &old, &new)?),
        Cmd::Classify(c) => { let con = open(true)?; let rules = Rules::load(&cfg)?;
            match c {
                ClsCmd::Run { dry_run, refresh, limit, batch, model, no_knn } => {
                    let (http, runner) = (net::UreqHttp::new(600), net::ProcessRunner);
                    let be = (!dry_run).then(|| llm::Backend { name: cfg.llm_backend.clone(), cfg: &cfg, con: &con, t: if cfg.llm_backend == "claude-code" { llm::Transport::Run(&runner) } else { llm::Transport::Http(&http) } });
                    if let Some(b) = &be { if let Some(w) = egress::evaluate(&cfg, &format!("llm.{}", b.name), &egress::host_of(if b.name == "ollama" { &cfg.llm_ollama_url } else { &cfg.llm_openai_base_url })) { anyhow::bail!("classify run is refused: {w}"); } }
                    let r = llm::run(&con, &cfg, &rules, be.as_ref(), &llm::RunOpts { dry_run, refresh, limit, batch, model, knn: !no_knn })?;
                    if !r.held_back.is_empty() { println!("{} merchant(s) held back from the LLM because they may be people (label them with `finch classify correct`; see `finch classify review`)", r.held_back.len()); }
                    if r.knn > 0 { println!("{} merchant(s) {}labelled by similarity to labelled ones (no LLM)", r.knn, if dry_run { "would be " } else { "" }); }
                    if let Some(p) = r.dry_payload { println!("DRY RUN: {} merchant(s) in {} request(s); nothing is sent, nothing is written.", r.sent, p.as_array().map_or(0, |a| a.len())); print_json(&p); }
                    else { println!("labelled {}/{} merchants with {}; cost {}", r.labelled, r.sent, cfg.llm_backend, if r.unknown_cost { "unknown for this model".to_string() } else { format!("${:.3}{}", r.cost, if cfg.llm_backend == "claude-code" { " (notional on a subscription)" } else { " (estimate)" }) }); for f in &r.failed { println!("  FAILED {f}"); } if !r.failed.is_empty() { anyhow::bail!("{} batch(es) failed; the others were stored; run again to retry", r.failed.len()); } }
                }
                ClsCmd::Review { out } => { let v = llm::review(&con, &cfg, &rules)?; if out.json { print_json(&v); } else { println!("held back (possible people, never sent to an LLM):"); lines(&v, "held_back_person_like", |x| format!("  {:<40} {:>4} tx {:>10.2}", s(&x["key"]), s(&x["n"]), x["total"].as_f64().unwrap_or(0.0))); println!("low confidence / uncategorised:"); lines(&v, "low_confidence", |x| format!("  {:<40} {:<32} {} ({})", s(&x["key"]), s(&x["category"]), s(&x["confidence"]), s(&x["source"]))); } }
                ClsCmd::Correct { key, category, name } => println!("labelled {key} as {category} ({} transaction(s))", llm::correct(&con, &rules, &key, &category, name.as_deref())?),
            } }
        Cmd::Bank(b) => { let con = open(true)?; let http = net::UreqHttp::new(60);
            let finish = |cl: &bank::Client, state: &str| -> Result<()> { println!("\nAfter the bank login your browser lands on the redirect address (it may show an error page). Paste that whole address here and press Enter:"); let mut line = String::new(); std::io::stdin().read_line(&mut line)?; let (code, st) = bank::parse_finish_arg(cl.con, &line)?; if st.as_deref().is_some_and(|x| x != state) { anyhow::bail!("that address belongs to another authorisation"); } print_session(&bank::complete_auth(cl, &code, Some(st.as_deref().unwrap_or(state)))?); Ok(()) };
            let open_url = |url: &str, no_browser: bool| { println!("Open this address to log in at your bank:\n  {url}"); if !no_browser { let _ = open_browser(url); } };
            match b {
                BankCmd::Connect { bank: name, country, days, replace, no_browser } => { let cl = bank::Client::from_config(&cfg, &con, &http)?; let (state, url) = bank::start_auth(&cl, &name, &country, days, None, replace)?; open_url(&url, no_browser); finish(&cl, &state)?; }
                BankCmd::Reconnect { target, days, no_browser } => { let cl = bank::Client::from_config(&cfg, &con, &http)?; let c = bank::find_session(&con, &target)?; let (state, url) = bank::start_auth(&cl, &c.bank, &c.country, days, Some(&c.session_id), true)?; open_url(&url, no_browser); finish(&cl, &state)?; }
                BankCmd::Finish { url, replay } => { if let Some(st) = replay { print_session(&bank::replay_auth(&con, &st)?); } else { let cl = bank::Client::from_config(&cfg, &con, &http)?; let (code, st) = bank::parse_finish_arg(&con, url.as_deref().ok_or_else(|| anyhow::anyhow!("give the redirect address, or --replay STATE"))?)?; print_session(&bank::complete_auth(&cl, &code, st.as_deref())?); } }
                BankCmd::Consents { refresh } => { if refresh { let cl = bank::Client::from_config(&cfg, &con, &http)?; for n in bank::refresh_live(&cl)? { println!("{n}"); } } println!("{}", bank::format_consents(&bank::list_consents(&con, false)?)); }
            } }
        Cmd::Sync { account, full, force } => { let con = open(true)?; let http = net::UreqHttp::new(60); let cl = bank::Client::from_config(&cfg, &con, &http)?;
            let res = bank::sync_all(&cl, account.as_deref(), full, force)?; let mut failed = 0;
            for r in &res { match r["status"].as_str() { Some("ok") => println!("  {}: pages={} new={} pending={}{}", s(&r["uid"]), s(&r["pages"]), s(&r["new"]), s(&r["pending"]), if r["replaced_imports"].as_u64().unwrap_or(0) > 0 { format!(" replaced_imports={}", s(&r["replaced_imports"])) } else { String::new() }), Some("failed") => { failed += 1; println!("  {}: FAILED - {}", s(&r["uid"]), s(&r["note"])); } _ => println!("  {}{}", if r["uid"].is_null() { String::new() } else { format!("{}: ", s(&r["uid"])) }, s(&r["note"])) } }
            if res.iter().any(|r| r["status"] == "ok" && r["new"].as_i64().unwrap_or(0) > 0) { println!("normalised {} transaction(s)", classify::normalize_all(&con, Some(&cfg.memory_dir))?); }
            if failed > 0 { anyhow::bail!("{failed} account(s) failed (the others were synced)"); } }
        Cmd::Wealth { out } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let v = finch::loans::net_worth(&ds, ds.today, ds.settings.asset_stale_months as i32);
            if out.json { print_json(&v); } else { println!("net worth {} EUR (assets: bank {} + other {}; liabilities {}){}", s(&v["net_worth"]), s(&v["bank"]["total"]), s(&v["assets"]["total"]), s(&v["liabilities"]["total"]), if v["complete"] == true { String::new() } else { format!("  -- {} item(s) unknown, not counted", s(&v["n_unknown"])) });
                for (k, x) in v["by_category"].as_object().unwrap() { println!("  {k:<12} {:>12}", s(x)); } for u in v["unknown"].as_array().into_iter().flatten() { println!("  unknown {} {}: {}", s(&u["kind"]), s(&u["id"]), s(&u["reason"])); } } }
        Cmd::Loans(l) => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let today = ds.today;
            match l {
                LoanCmd::List => for x in finch::loans::liabilities_view(&ds, today)["liabilities"].as_array().into_iter().flatten() { println!("  {:<20} {:<14} still due {:>12}  instalment {:>10}  {}", s(&x["id"]), s(&x["kind"]), x["remaining_capital"].as_str().unwrap_or("unknown"), x["schedule"]["payment"].as_str().unwrap_or("-"), x["schedule"]["status"].as_str().unwrap_or("")); },
                LoanCmd::Show { id, rows } => { let (ls, _) = finch::loans::liabilities(&cfg.memory_dir); let lb = ls.iter().find(|x| x.id == id).ok_or_else(|| anyhow::anyhow!("no loan {id:?}"))?; print_json(&finch::loans::compute(lb, today).to_json(rows)); }
                LoanCmd::Repay { id, amount } => { let (ls, _) = finch::loans::liabilities(&cfg.memory_dir); let lb = ls.iter().find(|x| x.id == id).ok_or_else(|| anyhow::anyhow!("no loan {id:?}"))?; print_json(&finch::loans::early_repayment(lb, &finch::loans::compute(lb, today), amount.map(finch::loans::cents), today)); }
            } }
        Cmd::Subs(c) => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true);
            match c {
                SubsCmd::Inventory { ended, out } => { let v = finch::subs::inventory(&ds, &rec, ended, ds.today); if out.json { print_json(&v); } else { println!("{} services, {} EUR / month, {} EUR / year", s(&v["totals"]["services"]), s(&v["totals"]["monthly"]), s(&v["totals"]["yearly"]));
                    lines(&v, "rows", |r| format!("  {:<28} {:<14} {:>9} /month  contract {:<8} can end now: {}", r["name"].as_str().unwrap_or("").chars().take(28).collect::<String>(), s(&r["group"]), r["monthly"].as_str().unwrap_or("?"), s(&r["contract"]["status"]), r["cancellation"]["can_cancel_now"].as_bool().map_or("unknown", |b| if b { "yes" } else { "no" }))); } }
                SubsCmd::Cancel { r#ref } => { let v = finch::subs::inventory(&ds, &rec, true, ds.today); let row = v["rows"].as_array().into_iter().flatten().find(|x| x["ref"] == r#ref.as_str() || x["contract_id"] == r#ref.as_str()).ok_or_else(|| anyhow::anyhow!("no service {:?} (see `finch subs inventory`)", r#ref))?; print_json(&row["cancellation"]); }
            } }
        Cmd::Rental(c) => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?;
            match c { RentCmd::List => print_json(&finch::rental::list(&ds)), RentCmd::Show { id, year } => print_json(&finch::rental::detail(&ds, &id, year).ok_or_else(|| anyhow::anyhow!("no rental property {id:?}"))?) } }
        Cmd::Backup(b) => match b {
            BackupCmd::Create => { let con = open(true)?; let (p, pruned) = finch::backup::create(&cfg, &con, None)?; println!("backup written and verified: {}", p.display()); for x in pruned { println!("pruned {}", x.display()); } }
            BackupCmd::List => for p in finch::backup::list(&cfg) { println!("{}  {} bytes", p.display(), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)); },
            BackupCmd::Verify { file } => println!("ok: {} entries", finch::backup::verify(&file, None)?.len()),
            BackupCmd::Restore { file, to, force } => println!("restored {} file(s) into {}", finch::backup::restore(&file, &to, force, None)?.len(), to.display()),
        },
        Cmd::Export { out, plain, decrypt, to } => {
            if let Some(f) = decrypt { let to = to.ok_or_else(|| anyhow::anyhow!("--decrypt needs --to DIR"))?; println!("decrypted into {}", finch::backup::decrypt_export(&f, &to, None)?.display()); return Ok(()); }
            if plain { use std::io::{IsTerminal, Write}; if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) { bail!("--plain writes an UNENCRYPTED archive of your whole financial life: run it in an interactive terminal"); }
                print!("This writes an UNENCRYPTED archive. Type {} to continue: ", finch::backup::PLAIN_PHRASE); std::io::stdout().flush()?; let mut l = String::new(); std::io::stdin().read_line(&mut l)?; if l.trim() != finch::backup::PLAIN_PHRASE { println!("nothing written"); return Ok(()); } }
            let con = open(true)?; println!("export written: {}", finch::backup::export(&cfg, &con, out, plain, None)?.display()); }
        Cmd::Alerts(c) => { let con = open(true)?; let st = finch::alerts::Settings::load(&cfg)?;
            match c {
                AlertCmd::Check { dry_run, no_send } => { let ds = Dataset::load(&con, &cfg, None)?; let rec = recurring::detect(&ds, &Scope::default(), true); let now = chrono::Utc::now();
                    let v = finch::alerts::evaluate(&con, &ds, &rec, &st, now, dry_run)?; println!("{} signal(s): {} new, {} escalated, {} resolved{}", s(&v["candidates"]), v["new"].as_array().map_or(0, |a| a.len()), v["escalated"].as_array().map_or(0, |a| a.len()), v["resolved"].as_array().map_or(0, |a| a.len()), if dry_run { " (dry run: nothing stored)" } else { "" });
                    if !no_send { for r in finch::alerts::dispatch(&con, &cfg, &st, now, dry_run, &net::UreqHttp::new(30))? { println!("  {:<9} {:<10} {}{}", s(&r["channel"]), s(&r["status"]), r["reason"].as_str().unwrap_or(""), r["message"]["body"].as_str().map(|b| format!("\n    {}", b.replace('\n', "\n    "))).unwrap_or_default()); } } }
                AlertCmd::List { all } => for e in finch::alerts::listing(&con, Some(all))? { if !all && !["new", "sent"].contains(&e["status"].as_str().unwrap_or("")) { continue; } println!("  {} {:<7} {:<10} {:<18} {}", s(&e["id"]), s(&e["severity"]), s(&e["status"]), s(&e["kind"]), s(&e["title"])); },
                AlertCmd::Channels => print_json(&finch::alerts::channels_view(&con, &cfg, &st)?),
                AlertCmd::Ack { id } => println!("{}", if finch::alerts::act(&con, &id, "ack", None, chrono::Local::now().date_naive())? { "acknowledged" } else { "no such alert" }),
                AlertCmd::Snooze { id, days } => println!("{}", if finch::alerts::act(&con, &id, "snooze", Some(days), chrono::Local::now().date_naive())? { "snoozed" } else { "no such alert" }),
                AlertCmd::Digest => print!("{}", s(&finch::alerts::digest(&con, chrono::Local::now().date_naive())?["markdown"])),
            } }
        Cmd::Usage { days, out } => { let con = open(true)?; let v = finch::usage::summary(&con, &cfg, days, chrono::Utc::now())?; if out.json { print_json(&v); } else { let t = &v["totals"]; println!("LLM usage, last {days} days: {} call(s), {} tokens in / {} out, {} USD ({} notional on a subscription)", s(&t["calls"]), s(&t["tokens_in"]), s(&t["tokens_out"]), s(&t["cost_usd"]), s(&t["notional_cost_usd"]));
            lines(&v, "lines", |l| format!("  {:<16} {:<14} {:<20} {:>5} calls  {:>8} USD", s(&l["job"]), s(&l["backend"]), l["model"].as_str().unwrap_or("?"), s(&l["calls"]), l["cost_usd"].as_f64().map_or("unknown".into(), |c| format!("{c:.4}")))); } }
        Cmd::Review { year, scope: sa } => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?; print_json(&finch::analytics::review::year_review(&ds, year, &scope(&sa), 10)); }
        Cmd::Coach(CoachCmd::Ask { question }) => { use std::io::Write; let con = open(true)?; let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut emit = |ev: &str, d: serde_json::Value| { match ev { "delta" => { print!("{}", d["text"].as_str().unwrap_or("")); let _ = std::io::stdout().flush(); } "tool_call" => eprintln!("[tool {}]", d["name"].as_str().unwrap_or("")), "proposal" => eprintln!("\n[proposal {}: review it, then `{}`]", s(&d["id"]), s(&d["command"])),
                "notice" | "error" => eprintln!("\n[{}] {}", ev, d["message"].as_str().unwrap_or("")), "answer" => { println!(); if let Some(u) = d["unverified_numbers"].as_array().filter(|u| !u.is_empty()) { eprintln!("[warning: amounts not returned by a tool: {}]", u.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")); } } _ => {} } };
            finch::coach::run(&cfg, cli.insecure, &question, &finch::coach::ProcessSpawner, cancel, Some(&con), &mut emit)?; }
        Cmd::Memory(m) => { let st = finch::memstore::Store::new(&cfg, "cli")?;
            match m {
                MemCmd::Proposals { status } => { let ps = finch::proposals::listing(&st, Some(&status)); if ps.is_empty() { println!("no {status} proposal"); }
                    for p in ps { println!("{}  {:<9} {:<22} {} op(s)  by {}  {}", p.id, p.status, p.file, p.ops.len(), p.source, p.reason.chars().take(70).collect::<String>()); } }
                MemCmd::Show { id } => { let p = finch::proposals::get(&st, &id)?; let v = finch::proposals::public(&st, &p);
                    println!("{} ({}) on {}\nreason: {}\nproposed by: {}\nsealed: {}{}\n\n{}", p.id, s(&v["status"]), p.file, p.reason, p.source, s(&v["sealed"]), v["error"].as_str().map(|e| format!("\nNOT APPLICABLE: {e}")).unwrap_or_default(), s(&v["diff"])); }
                MemCmd::Accept { id, force, confirm_field } => { use std::io::{IsTerminal, Write};
                    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) { bail!("accepting a proposal is a human action: run it in an interactive terminal"); }
                    let (p, res, sealed, stale) = finch::proposals::preview(&st, &id)?;
                    println!("{} on {}\nreason: {}\nproposed by: {}{}{}\n\n{}", p.id, p.file, p.reason, p.source, if sealed { "" } else { "\nWARNING: the proposal was modified after it was created: it will be refused" }, if stale { "\nWARNING: the file changed since the proposal was made" } else { "" }, res.diff);
                    for w in &res.warnings { println!("warning: {w}"); }
                    print!("Apply this change to {}? Type yes: ", p.file); std::io::stdout().flush()?; let mut line = String::new(); std::io::stdin().read_line(&mut line)?;
                    if line.trim() != "yes" { println!("not applied"); return Ok(()); }
                    let (p, res) = finch::proposals::accept(&st, &id, true, force, "cli", &confirm_field)?; println!("accepted {} ({})", p.id, res.change_id.unwrap_or_else(|| "no history".into())); }
                MemCmd::Reject { id, note } => { let p = finch::proposals::reject(&st, &id, note.as_deref())?; println!("rejected {}", p.id); }
                MemCmd::History { file, limit } => for c in st.repo().log(file.as_deref(), limit)? { println!("{}  {}  {:<10} {}{}", c.id, &c.date[..c.date.len().min(19)], c.source.unwrap_or_default(), c.subject, c.reason.map(|r| format!("  ({r})")).unwrap_or_default()); },
                MemCmd::Diff { id } => print!("{}", st.repo().diff(&id)?),
                MemCmd::Check => { let mut bad = 0; for (f, _) in finch::memstore::FILES { if !st.exists(f) { continue; } match st.validate(f, &st.read_text(f)) { Ok(w) => { println!("  ok       {f}"); for x in w { println!("  warning  {x}"); } } Err(e) => { bad += 1; println!("  ERROR    {e}"); } } }
                    if finch::questions::md_hand_edited(&st) { println!("  warning  open-questions.md differs from open-questions.yaml"); } if bad > 0 { bail!("{bad} file(s) are invalid"); } }
            } }
        Cmd::Questions(q) => { let st = finch::memstore::Store::new(&cfg, "cli")?; let today = chrono::Local::now().date_naive();
            let done = |r: finch::memstore::EditResult| { if r.changed() { print!("{}", r.diff); } else { println!("nothing changed"); } };
            match q {
                QCmd::List { status } => for x in finch::questions::listing(&st, (status != "all").then_some(status.as_str())) { println!("{}", finch::questions::render_line(&x)); },
                QCmd::Add { question, topic } => { let (id, r) = finch::questions::add(&st, &question, &topic, today, false)?; println!("added {id}"); done(r); }
                QCmd::Answer { id, text } => done(finch::questions::answer(&st, &id, &text, today, false)?),
                QCmd::Dismiss { id, reason } => done(finch::questions::dismiss(&st, &id, reason.as_deref(), today, false)?),
                QCmd::Reopen { id } => done(finch::questions::reopen(&st, &id, false)?),
            } }
        Cmd::Household(h) => { let con = open(true)?; let ds = Dataset::load(&con, &cfg, None)?;
            match h {
                HhCmd::Overview { out } => { let v = finch::household::overview(&ds, &con)?; if out.json { print_json(&v); } else {
                    lines(&v, "members", |m| format!("  {:<12} {:<6} {:>5} tx  accounts: {}", s(&m["id"]), s(&m["role"]), s(&m["attributed_transactions"]), m["accounts"].as_array().map_or(String::new(), |a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))));
                    lines(&v, "accounts", |a| format!("  account {:<24} owner {:<10} {}", s(&a["label"]), a["owner"].as_str().unwrap_or("?"), a["attributed"]));
                    println!("  attribution rules: {}, manual reassignments: {}", v["rules"].as_array().map_or(0, |a| a.len()), s(&v["attribution"]["manual"])); for w in v["warnings"].as_array().into_iter().flatten() { println!("  warning: {}", s(w)); } } }
                HhCmd::Kids { member, months } => print_json(&match member { Some(m) => finch::household::kid_report(&ds, &con, &m, months)?, None => serde_json::json!(finch::household::children_overview(&ds, &con, months)?) }),
                HhCmd::KidBudgets => print_json(&serde_json::json!(finch::household::kid_budget_status(&ds, None))),
                HhCmd::WhoPays { months } => print_json(&finch::household::who_pays(&ds, months, None)),
                HhCmd::Explain { tx } => print_json(&finch::household::explain(&con, &ds.people, &transfers::resolve_tx(&con, &tx)?)?),
                HhCmd::Assign { tx, member, note } => print_json(&finch::household::assign(&con, &ds.people, &tx, &member, "cli", note.as_deref())?),
                HhCmd::Clear { tx } => print_json(&finch::household::clear(&con, &tx, "cli")?),
                HhCmd::Log { tx, limit } => for l in finch::household::log(&con, tx.as_deref(), limit)? { println!("  #{} {} {} {} -> {} ({})", s(&l["id"]), s(&l["at"]), s(&l["tx_key"]), l["old_member"].as_str().unwrap_or("-"), l["new_member"].as_str().unwrap_or("-"), s(&l["action"])); },
                HhCmd::Undo { id } => print_json(&finch::household::revert(&con, &ds.people, id, "cli")?),
            } }
        Cmd::Ui { host, port, dev, no_browser, login_link, user, rotate_session_key, static_dir } => {
            let (host, port) = (host.unwrap_or(cfg.ui_host.clone()), port.unwrap_or(cfg.ui_port as u16));
            if rotate_session_key { web::security::rotate_secret(&cfg.data_dir)?; println!("session key rotated: every browser session is signed out; run `finch ui` for a new login link"); return Ok(()); }
            if user.is_some() && !login_link { bail!("--user goes with --login-link"); }
            if let Some(u) = &user { if web::get_user(&open(true)?, u).is_none_or(|x| x.disabled) { bail!("no enabled login {u:?} (ui_users)"); } }
            if login_link { println!("{}", web::login_url(port, dev, &web::security::LoginTokens::new(&cfg.data_dir).issue(user.as_deref())?)); eprintln!("one-time link, valid 2 minutes: open it in the browser that should get access (works with a running `finch ui`)"); return Ok(()); }
            if !web::security::is_loopback(&host) { bail!("refusing to listen on {host:?}: the web app shows your bank data and is local only in finch"); }
            let exe_static = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("static")));
            let sdir = static_dir.or(exe_static.filter(|p| p.join("index.html").is_file())).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
            if !dev && !sdir.join("index.html").is_file() { eprintln!("warning: the web app is not built ({}/index.html is missing): run `pnpm install && pnpm build` in web/. The API still works.", sdir.display()); }
            open(true)?;
            let app = std::sync::Arc::new(web::App::new(cfg.clone(), cli.insecure, port, dev, sdir, std::sync::Arc::new(|| Box::new(net::UreqHttp::new(60)) as Box<dyn net::Http>))?);
            let _ = app.me.set(std::sync::Arc::downgrade(&app));
            let url = web::login_url(port, dev, &app.sec.tokens.issue(None)?);
            println!("finch web app on port {port}  (Ctrl-C to stop){}", if dev { "  [dev mode: API only, run `pnpm dev` in web/]" } else { "" });
            println!("
Open this one-time login link in your browser (valid 2 minutes, single use):

  {url}

Need another one later? Run `finch ui --login-link`.");
            if cfg.ui_open_browser && !no_browser && !dev { let _ = open_browser(&url); }
            web::serve(app, &host, port)?;
        }
        Cmd::Privacy { last } => { println!("privacy mode: {}  (llm.backend {}, egress journal {})", egress::mode(&cfg), cfg.llm_backend, if cfg.privacy_egress_journal { "on" } else { "off" });
            for f in egress::FLOWS { println!("  {:<24} {}", f.0, egress::evaluate(&cfg, f.0, if f.0 == "llm.ollama" { egress::host_of(&cfg.llm_ollama_url) } else { String::new() }.as_str()).unwrap_or_else(|| "allowed".into())); }
            if let Ok(con) = open(true) { println!("last outbound calls:"); for (ts, k, h, b, p, o) in egress::journal(&con, last)? { println!("  {ts} {o:<8} {k:<22} {h:<28} {b:>7} B  {p}"); } } }
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
