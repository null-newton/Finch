# finch

A private personal-finance coach for one Belgian household, written in Rust. It is a port of `ai-finance-coach` (Python).
The rule is unchanged: **numbers come from code, words come from the model**. Every figure a model sees comes from a tool
result, and every tool result is redacted and privacy-checked before it leaves the machine.

finch reads the **same database schema, `config.toml`, `memory/` folder and `COACH_*` environment variables** as the
Python app, so both can work on the same data while the port is in progress.

## Status (phase 4)

| Area | State |
|---|---|
| Config, secrets (env, file backend, OS keychain with `--features keychain`) | ported |
| Database: same schema, 25 versioned migrations (the Python data migrations rewritten in Rust), safety copies, `REGEXP` | ported |
| SQLCipher encryption | behind `--features sqlcipher` (see Build) |
| File imports: CSV (profiles or an inline mapping), OFX/QFX, CAMT.053, **CODA**; multiset dedup, re-import adds nothing | ported |
| Descriptor parsing: **Belgian banks** (NL / FR / EN), Revolut | new (replaces the French / Italian parsers) |
| Categories: override, transfer link, type rules, user labels, Belgian merchant rules, entities, LLM labels, memory annotations, splits | ported |
| LLM classification of merchants: person guard (default deny), redaction, towns cut, coarse amounts, kNN near-duplicates without the LLM, backends `claude-code` / `anthropic-api` / `ollama` / `openai-compatible`, usage log, `--dry-run`, review, user corrections | ported (phase 3); the Batch API and web enrichment are not |
| Enable Banking: RS256 JWT client, consents (expiry, live status), connect / reconnect / finish (paste the redirect address; `--replay`), account matching across reconnects, sync (pages, dedup, pending, balances, daily limit), account merge | ported (phase 3); no local HTTPS callback server |
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
| Web app (`web/`) | restyled with the toolbox look (Manrope / DM Mono, ink-teal and lime, drafting grid); pages and interactions unchanged; the bank connection asks you to paste the redirect address |

Not ported yet (phase 5): per-person views (`?member=`), previews (`dry_run`) and memory annotations in the web app; memory proposals and questions, the coach runtime, subscriptions and contracts, loans and net worth,
rental property, alerts, household attribution, year review, quality / eval, backups and export. Until then, use the Python app for
them on the same database. Budgets and goals are read from `memory/`; writing them stays a memory proposal.

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
finch bank connect --bank KBC     # log in at the bank, then paste the address the browser lands on
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

To use the tools from Claude Code, `.mcp.json` declares the server `finance` (`finch mcp serve`).

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
