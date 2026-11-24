# finch

A private personal-finance coach for one Belgian household, written in Rust. It is a port of `ai-finance-coach` (Python).
The rule is unchanged: **numbers come from code, words come from the model**. Every figure a model sees comes from a tool
result, and every tool result is redacted and privacy-checked before it leaves the machine.

finch reads the **same database schema, `config.toml`, `memory/` folder and `COACH_*` environment variables** as the
Python app, so both can work on the same data while the port is in progress.

## Status (phase 5d)

| Area | State |
|---|---|
| Config, secrets (env, file backend, OS keychain with `--features keychain`) | ported |
| Database: same schema, 25 versioned migrations (the Python data migrations rewritten in Rust), safety copies, `REGEXP` | ported |
| SQLCipher encryption | behind `--features sqlcipher` (see Build) |
| File imports: CSV (profiles or an inline mapping), OFX/QFX, CAMT.053, **CODA**; multiset dedup, re-import adds nothing | ported |
| Descriptor parsing: **Belgian banks** (NL / FR / EN), Revolut | new (replaces the French / Italian parsers) |
| Categories: override, transfer link, type rules, user labels, Belgian merchant rules, entities, LLM labels, memory annotations, splits | ported |
| LLM classification of merchants: person guard (default deny), redaction, towns cut, coarse amounts, kNN near-duplicates without the LLM, backends `claude-code` / `anthropic-api` / `ollama` / `openai-compatible`, usage log, `--dry-run`, review, user corrections | ported (phase 3); Anthropic Message Batches (`finch classify run --batch-api`, collect with `finch classify batches`: half price, resumable) added; web enrichment is not ported (it would send merchant names to a search service) |
| Enable Banking: RS256 JWT client, consents (expiry, live status), connect / reconnect / finish (paste the redirect address; `--replay`), account matching across reconnects, sync (pages, dedup, pending, balances, daily limit), account merge | ported (phase 3); the local HTTPS callback is opt-in (`[callback] enabled = true`, see [Bank login callback](#bank-login-callback)), pasting the address stays the default and the fallback |
| Egress policy: one check before every outbound call, `[privacy] offline` / `local_only`, local journal (host and purpose only) | ported (phase 3) |
| Analytics: coverage model, cash flow, category averages | ported (phase 1) |
| Internal-transfer matching: auto-link (opt-in), proposals, ambiguous pairs, manual link / unlink, cross-bank window, member evidence | ported (phase 2) |
| Recurring series (stored, stable ids), price changes, anomalies (spikes, duplicates, new merchants, large payments; dismissable) | ported (phase 2) |
| Budgets (status, rollover, suggestions), goals, cash-flow forecast with band, upcoming-payments calendar | ported (phase 2); loan schedules and Belgian notice rules come with the loans / contracts phase |
| CODA closing balance stored as the account balance (the forecast's starting point for file-imported accounts) | new (phase 2) |
| Privacy: pseudonyms, hashed refs, household scrubber, merchant generalisation, fail-closed guard, injection scan | ported |
| MCP server `finance` (stdio): `coverage`, `cashflow`, `category_averages`, `transactions_search`, `recurring`, `price_changes`, `anomalies`, `forecast`, `budget_status`, `budget_suggestions`, `calendar`, `goals` | ported |
| Web API `finch ui` (loopback only): one-time login link (token in the URL fragment), signed HttpOnly SameSite=Strict cookie, CSRF, Host / Origin / `Sec-Fetch-Site` checks, rate limits, strict headers (CSP), revocable sessions, per-person logins with a deny-by-default child scope, audit log; serves `static/` | ported (phase 4) |
| Web endpoints: session, taxonomy, filters, health, balances, cash flow, averages, month categories, forecast, categories (drill-down), calendar (+ `.ics`), budgets, goals, subscriptions, insights (dismiss), transactions (filters, totals, CSV export, detail, category fix, override), transfers (link / unlink), connections, account edit, sync and connect jobs, banks, review queue | ported (phase 4); the rest answers 404 `not_ported` |
| Household & people: members, attribution (manual reassignment > rule incl. Belgian card digits > account owner), logged and reversible; per-person views (`?member=` in the web app, `member` on the MCP tools); the children's money (pocket money vs extra top-ups with their source, balance trend), kid budgets, who pays what; a child login's own `/me` pages | ported (phase 5a); `finch household ...`; editing household.yaml stays a memory proposal |
| MCP `household_overview`, `kids_money`, `who_pays` (pseudonyms and roles only) | ported (phase 5a) |
| Memory store: validated edits (schema, categories, member references), comment-preserving YAML patches (only the touched item is re-rendered), unified diffs, the same git history (`.history.git`) and messages as the Python app; web edits of budgets, goals, annotations, attribution rules, kid budgets, pocket money and allocations with `dry_run` previews | ported (phase 5b) for budgets, goals, categorization, household, events, open questions, assets, documents, `liabilities/*.yaml`, `contracts/*.yaml` and the Markdown notes; web edits of assets, loans, contracts, members and events; `finch memory revert ID` |
| Open questions (yaml + the generated Markdown view, byte for byte), sealed proposals in the Python app's format (seal, registry, history verdict); `finch memory proposals / show / accept / reject / history / diff / check`, `finch questions ...` (incl. `generate`: questions from the data, by stake); MCP `open_questions`, `memory_propose`, `questions_propose` (custom questions, subscription usage, missing loan fields) | ported (phase 5b); accepting is a terminal action with a typed yes |
| Loans and net worth: amortization schedules (Belgian EQUIVALENT monthly rate by default, `rate.monthly_convention: proportional` for i / 12), deferrals, borrower insurance, the future table from a dated outstanding capital, payment and capital checks, the legal maximum early-repayment indemnity (mortgage: 3 months of interest; fixed-rate consumer credit: 1 % / 0.5 %), net worth with unknown items listed, never zero | ported (phase 5c, Belgian rules); also lease end-of-contract views, loan facts inferred from the payments (as a proposal), prepayment / renegotiation / insurance scenarios and net-worth history (`finch loans scenario|snapshot`) |
| Subscriptions and contracts: inventory of recurring costs with their `contracts/*.yaml`, price history, usage as stated, and Belgian cancellation rules (telecom 6 months, energy 1 month, insurance 3 months before the due date, tacitly renewed services 2 months) with their legal basis | ported (phase 5c, Belgian rules); also stored cheaper offers (https source, dated; savings computed by finch, outdated after 30 days), decisions checked against the bank data (verified / pending / contradicted), cancellation letters in Dutch, French or English filled locally from the contract and the local contact block (`finch subs letter`, nothing is sent), contract files drafted from a recurring series |
| Rental property: linked account and loan, months of rent / costs / net / effort, missing rents, yearly totals, Belgian rent indexation from the owner's own health-index figures (`lease:` on the asset); tax: private letting is taxed on the cadastral income | ported (phase 5c, Belgian rules); no French scheme (Pinel) |
| MCP `net_worth`, `loans_overview`, `rental_overview`, `subscriptions_inventory`, `cancellability`, `savings_tracker`, `alternatives_record`, `decision_propose` and `contracts_draft` (proposals only; kinds and refs only: never a lender, description or contract number) | ported (phase 5c) |
| Tax helper, Belgian version: candidate items of an income year for the personal income tax return from the household's own data (pension saving, long-term saving, childcare, gifts to approved institutions, service vouchers, the home loan flagged by region and loan date, a rental property's cadastral income): amounts spent, evidence, conditions, certificates to keep, what is missing; no rate or ceiling quoted (FPS Finance / Tax-on-web named as the source), the central tax disclaimer (`src/disclaimers.rs`). `finch tax candidates --year`, MCP `tax_candidates`; tags and the optional household `region` in `docs/BACKLOG.md` B9 | new (B9); replaces the Python FR / IT helper; a reminder list, not a return |
| Backups and export: AES-256-GCM / scrypt archives in the Python app's formats (`AFCBK1` backups of the database + memory/, verified after writing and pruned to `backup.retention`; `AFCEX1` exports of every table, a transactions CSV and the memory files; `--plain` only at a terminal with a typed phrase) | ported (phase 5d) |
| Alerts: signals (consents, failing syncs, unusual charges, price increases, low-balance forecasts, budgets, kid budgets local only) into the shared `alert_events` feed, dedupe / escalation / resolve, ack / snooze / mute, ntfy and Telegram channels (off by default, egress policy, quiet hours, weekly budget, minimal guarded text, first-run baseline), weekly digest | ported (phase 5d); also the e-mail channel (SMTP over TLS: STARTTLS or implicit, never plain; password in the `smtp_password` secret) and the macOS notification (`osascript`, fixed script, text as argv) |
| LLM usage per job / day with the egress destinations; the year in review (also as the MCP `year_review` tool) | ported (phase 5d) |
| Coach: `finch coach ask` and the web chat (server-sent events) through `claude -p`, isolated, with finch's finance MCP server as its only tools; fails closed on an unexpected tool, plugin or MCP server; timeout, cancel, tool budget, unverified amounts and product-advice flags | ported (phase 5d); also the `anthropic-api`, `ollama` and `openai-compatible` backends (the tool loop runs in finch over the same guarded tools), skills with narrower tool sets (`digest-weekly`, `digest-monthly`, `subscription-audit`, `contract-check`, `mortgage-check`: `finch coach ask --skill`, `finch coach skills`), answers stored as AI-labelled insight cards (read / done / snooze / dismiss) |
| Quality: the gold set (bootstrap from your own decisions, sampling by money or stratified, labelling at the terminal or on the Gold set page), the offline evaluation of the classifier and the pipeline (`finch eval classify`, history in `eval_runs`, regressions flagged), offline checks of the coach's stored answers (`finch eval coach --offline`), the plan and cost of a model comparison (`finch eval models --dry-run`; a run needs a terminal and a typed yes) | new in finch (ported from the Python app's E12) |
| Documents: `finch memory doc add|list|extract`: hashed storage in `memory/documents/`, text read locally (PDF text layer or text file, no OCR), redaction (IBAN, card / account numbers, e-mails, phones, household names, addresses), the exact request shown, `--send` asks the model, each field checked against a snippet of the text, the result is a proposal | ported |
| Web app (`web/`) | restyled with the toolbox look (Manrope / DM Mono, ink-teal and lime, drafting grid); pages and interactions unchanged; the bank connection asks you to paste the redirect address |

Not ported: web enrichment of merchant names (it sends merchant names to a search service, which finch does not do). The web app edits memory for the person at the keyboard; a model only ever proposes.

## Desktop app

`desktop/` is a Tauri app: it starts finch's own web server on a loopback port inside the app and shows it in a native window, with
the `finch` command line installed next to it (the coach runs `finch mcp serve`). The page gets no access to the Tauri API; links to
other sites open in your browser.

The app keeps its own local home in the OS app-data folder (Windows `%APPDATA%e.finch.desktop`, macOS
`~/Library/Application Support/be.finch.desktop`, Linux `~/.local/share/be.finch.desktop`) and sets it up on first start: `config.toml`,
`data/`, `memory/`, a random database key kept in the OS keychain (service `finch-desktop`, apart from the command line's), and the
encrypted database with every migration. Nothing is changed in an existing home. `COACH_HOME` or `COACH_CONFIG` point the app at
another home (for example the one the command line uses).

Updates are signed (minisign key, see Release) and **off by default**: `[update] check = true` in config.toml makes the app look for a
newer release at start and every 6 hours; the menu's *Check for updates…* asks once. Either way the check only contacts GitHub, sends
no data, is journalled as the `update.github` egress flow, is refused under `[privacy] offline` / `local_only`, and installs only after
you click *Install and restart*.

```
npm install                          # the Tauri CLI
npm run desktop                      # dev: builds the command line sidecar, runs the app
npm run bundle                       # installers in desktop/target/release/bundle
```

Without perl (SQLCipher) try it plain on a scratch home: `FINCH_FEATURES=keychain node scripts/sidecar.mjs`, then
`npx tauri build --debug --bundles nsis --config '{"bundle":{"createUpdaterArtifacts":false}}' -- --no-default-features`.

## Release

1. Once: create the update signing key and give it to GitHub (a human step: the private key never goes into the repository):
   `npx tauri signer generate -w ~/.tauri/finch.key`, paste the public key into `plugins.updater.pubkey` of
   `desktop/tauri.conf.json`, and add the repository secrets `TAURI_SIGNING_PRIVATE_KEY` (the content of the key file) and
   `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
2. `npm run release patch` (or `minor`, `major`, `1.2.3`; `-- --dry-run` to preview) on a clean `main`: bumps `Cargo.toml`,
   `desktop/Cargo.toml`, both lockfiles and `web/package.json`, commits "Release vX", tags and pushes.
3. The tag runs `.github/workflows/build.yml`: Windows (.exe / .msi), macOS universal for Apple Silicon + Intel (.dmg) and Linux
   (.AppImage / .deb / .rpm) with SQLCipher and the OS keychain, the update bundles signed, everything plus `latest.json` attached to a
   draft release. Publish the draft: the updater reads `releases/latest/download/latest.json`.

`.github/workflows/check.yml` runs the web tests and build, clippy and the Rust tests on every push to `main` and every pull request.
The repository must stay public for the updater to fetch `latest.json` without a login.

## Bank login callback

By default `finch bank connect` / `reconnect` ask you to paste the address the browser lands on after the bank login. Opt in to
let finch receive it itself:

1. In the Enable Banking control panel, add the **exact** redirect address to your application, for example
   `https://localhost:8443/callback` (scheme, host, port and path must match character for character).
2. In `config.toml`: `[enable_banking] redirect_url = "https://localhost:8443/callback"` and `[callback] enabled = true`
   (optional `timeout_seconds = 600`). The port and path come from `redirect_url`; the host must be `localhost` or `127.0.0.1`.
3. Run `finch bank connect --bank KBC`. finch listens on 127.0.0.1 only, logs in at the bank in your browser, and on the
   redirect checks the one-time `state`, finishes the consent exactly like `finch bank finish`, shows "Bank connected" and stops.
   A request with another state is answered "not this login" and the server keeps waiting; one valid request or the timeout ends it.
4. **The browser warns once** ("Your connection is not private"): the certificate is self-signed, generated locally on first use
   into `<data_dir>/tls/` (`localhost.crt`, `localhost.key`, owner-only, renewed before it expires; never commit it). Choose
   "Advanced" > "Proceed to localhost". Nothing it receives is logged and it makes no outbound call.

If the port is taken, the certificate cannot be made, or nothing arrives in time, finch falls back to the paste prompt (the login
stays valid); `--no-server` always pastes.

## Belgian banks

The Belgian parser reads the usual Dutch, French and English descriptor wording: Bancontact / debit-card / contactless
payments, Payconiq, ATM withdrawals (GELDOPNEMING / RETRAIT), SEPA transfers (OVERSCHRIJVING / VIREMENT, instant, standing
orders), direct debits (DOMICILIERING / DOMICILIATION with creditor id and mandate), loan instalments, fees, credit interest.
Structured communications (`+++123/4567/89012+++`) become the transaction reference.
KBC / CBC, Belfius, BNP Paribas Fortis / Hello bank!, ING, Argenta, Crelan, Keytrade, Beobank, bpost bank, Triodos, vdk,
Nagelmackers and Europabank all go through this parser. Revolut has its own.

**To verify with real exports:** the patterns follow the commonly published layouts, and each bank words its lines a bit
differently. If a line comes out as `other` or with a wrong merchant, add a synthetic test case to
`src/classify/parsers/belgian.rs` (never a real line) and adjust the pattern. The CODA field positions follow the published
CODA 2 layout and should be checked against a real file too.

The merchant rules (`assets/rules.yaml`) cover the Belgian tax offices, mutualities, child benefit and RVA / ONEM, service
vouchers, utilities (Fluvius, Engie, Luminus, De Watergroep, Proximus, Telenet...), public transport, fuel and national
supermarket chains. Your own copy goes in `config/rules.yaml` (or `$COACH_RULES_FILE`) and replaces the shipped one; the
same applies to `taxonomy.yaml`.

## Build

```powershell
cargo build --release
```

The default build uses plain bundled SQLite. To use an encrypted database (SQLCipher, the `db_key` secret) build with the
`sqlcipher` feature. The vendored OpenSSL needs a full perl: on Windows install Strawberry Perl first (Git Bash's perl is not
enough).

```powershell
cargo build --release --features "sqlcipher keychain"
```

Without `sqlcipher`, finch refuses an encrypted database with a clear message, and only creates or opens a plaintext one
when you pass `--insecure` or set `insecure_plaintext_db = true`.

## Use

```powershell
finch db migrate --create
finch import statement.cod --account new:Zichtrekening --bank KBC
finch import export.csv --account new:Spaarrekening --profile generic-csv-be-nl
finch import export.csv --account Zichtrekening --date-col Datum --desc-col Omschrijving --amount-col Bedrag --save-profile my-bank
finch accounts list
finch accounts set imp-zichtrekening --owner joint --purpose main
finch coverage
finch cashflow --months 6
finch averages
finch transfers match            # proposals; --link stores the confident ones
finch recurring                  # --refresh stores the series
finch anomalies                  # --refresh, --dismiss anm_...
finch forecast --days 60
finch budgets status
finch calendar
finch refresh                    # the daily analytics step
finch bank connect --bank KBC     # log in at the bank, then paste the address the browser lands on (or the local callback receives it)
finch sync                       # new transactions and balances (normalised automatically)
finch classify run --dry-run     # the exact redacted requests; without --dry-run they are sent
finch classify review            # held-back (possible people) and low-confidence merchants
finch classify correct "SHOP KEY" food.groceries
finch privacy                    # the privacy mode and the last outbound calls
finch mcp serve
(cd web && pnpm install && pnpm build)   # builds the page into static/
finch ui                         # http://127.0.0.1:8765, prints a one-time login link (valid 2 minutes)
finch ui --login-link            # a fresh link for a running server; --rotate-session-key signs every browser out
```

`--dry-run` shows what an import would write. `--json` prints the raw result of `coverage`, `cashflow` and `averages`.
The CLI prints local values; only the MCP server redacts, because it talks to a model.

To use the tools from Claude Code, `.mcp.json` declares the server `finance` (`finch mcp serve`). Claude Code starts it
as a child process, so before you approve it with `/mcp`:

1. **Put `finch` on PATH**: `cargo install --path . --locked` (add `--features "sqlcipher keychain"` for an encrypted
   database; it installs into `~/.cargo/bin`, re-run it after pulling changes). Or point at a build without installing:
   set `FINCH_BIN` to the full path of `target/release/finch.exe` before you start Claude Code.
2. **Give it a home with a database.** The server reads `$COACH_HOME`, else the nearest parent of the folder Claude Code
   runs in that holds `config.toml` / `config.example.toml` (from this repository that is the Python app's folder `..`),
   else `~/.ai-finance-coach`. Set `COACH_HOME` (or `COACH_DB` and friends) to your finch home before you start Claude
   Code, and check it with `finch config` and `finch coverage`. Without a database the server exits at once ("No database
   at ...") and Claude Code shows "connection closed".
3. A plaintext database needs `insecure_plaintext_db = true` in that home's `config.toml` (a `sqlcipher` build needs
   none). Never add `--insecure` to `.mcp.json`.

If it still says "connection closed", run `finch mcp serve` in the same shell: it prints the reason and exits.

## Develop

```powershell
cargo test
```

Tests use synthetic data only: no real name, merchant line, amount, IBAN or path. For a realistic check, work on a restored
copy of the database (`COACH_DB`, `COACH_MEMORY_DIR`, `COACH_DATA_DIR`, `COACH_CONFIG_DIR` pointing at a scratch folder),
never on the real one.

Layout: `src/util.rs` (regex, folding, cents, months), `config.rs`, `secrets.rs`, `db.rs` + `migrations/`, `ingest/`
(imports, profiles, fingerprints, accounts), `classify/` (parsers, rules, normalize), `memory.rs` (read-only memory),
`analytics/`, `privacy/`, `mcp/`, `main.rs` (CLI). Shared data: `assets/` (taxonomy, rules, CSV profiles, the example
configuration).

## Licence

MIT, see `LICENSE`.
