# finch backlog (product owner view, 2026-11-24)

What is left after phase 5d. Each item says who can move it: **[agent]** can be done by an agent from the docs alone,
**[needs owner]** waits for a decision or real data from the owner before any work starts. Agents read `CLAUDE.md`
(rules + project map) and this file first; they do not re-discover the project.

## Now: get the tree into a shippable state

- **B1 Commit the phase 5d follow-ups [done 2026-11-24: three commits, not pushed].** The working tree holds ~1.8k lines of staged work (batch classify, docs,
  loans/subs extras, quality, qgen) plus untracked `desktop/`, `.github/`, `scripts/`, `src/home.rs`, `package.json`. Decide
  whether this is one commit ("phase 5e") or several, and whether `desktop/target`, `desktop/gen` and `node_modules/` stay ignored.
- **B2 Green build [agent; cargo test green 2026-11-24, keep it so].** `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cd web && pnpm test && pnpm build` must pass
  locally exactly as `.github/workflows/check.yml` runs them. Fix failures; no behaviour changes beyond the fix.
- **B3 `finance` MCP server does not start from Claude Code [done; owner: `cargo install --path . --locked`, set `COACH_HOME`, approve with `/mcp`].** `.mcp.json` runs `finch mcp serve`, which needs `finch` on
  PATH (connection closed in this session). Document the install step (`cargo install --path .`) or point `.mcp.json` at a
  documented path; never point it at the real data with `--insecure`.

## Next: parity with the Python app

- **B4 Web endpoint parity [agent; in progress 2026-11-24: done `GET /household`, `/events`, `/contracts`, `POST /transactions/split/clear`, the
  filters / overview / session / audit / health stubs, dismiss / snooze / restore of every insight card kind (`ui-state.json`), `POST /coach/resolve`,
  the rental extension / vacancy / market-rate writes and `GET /rental/{id}/indicators`; left to other decisions: `/setup/wizard`, `/onboarding*`,
  `/review/suggest` + `/review/ai`, asset rename, passkeys].** List every endpoint `web/src/api/hooks.ts` and the pages call that answers 404 `not_ported`
  in `src/web/routes.rs` / `memory_routes.rs`, then port them one by one (incl. dismissing every insight kind, not only anomalies and
  price changes). Each new endpoint gets a test in `tests/web.rs`; a child-visible one goes into `CHILD_ALLOWED` on purpose only.
- **B5 MCP tool parity [agent].** The Python server has tools finch lacks (e.g. `explain_transaction`, `memory_context`,
  `monthly_review`, `explain_spike`, `what_if`, `mortgage_check`, `savings_estimate`, `subscription_audit`, `onboarding_status`).
  Audit against `../src/coach/mcp/`, port the missing ones through `mcp::Session::publish`, a leak assertion each in `tests/e2e.rs`.
  `tax_candidates` is done (B9, Belgian version).
- **B6 Migration numbering audit [done: `migrations/README.md`; follow-up B12].** finch has 0001–0024 with gaps (0007, 0010, 0020, 0023, ...). Check each gap against the
  Python `migrations/` and record in `migrations/README.md` why it is absent (data migration rewritten in Rust, not applicable, ...).

## Waiting on the owner

- **B7 Verify the Belgian parsers with real exports [needs owner].** `src/classify/parsers/belgian.rs` and the CODA field positions
  follow published layouts only. The owner runs an import on a scratch copy and reports which lines come out as `other` or with a
  wrong merchant, as *synthetic look-alikes* (never a real line). Agents then add tests and adjust patterns.
- **B8 Release readiness [decided: secrets are set; cut v0.1.0 only after B4/B5 land].** The updater pubkey is set in `desktop/tauri.conf.json`; confirm the repository secrets
  `TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)` exist, that the repository stays public (the updater needs it), and when to cut v0.1.0.
- **B9 Belgian tax helper [done 2026-11-24: `src/tax.rs`, `finch tax candidates --year`, MCP `tax_candidates`; owner checks the legal wording].** Candidates only, never advice:
  pension saving (descriptor PENSIOENSPAREN / EPARGNE-PENSION, tag `pension_saving`, asset kind `pension`), long-term saving (`insurance.life`, tag `long_term_saving`,
  asset kind `life_insurance_savings`), childcare (`kids.childcare`, tag `childcare`), gifts (`charity.donations`, tag `approved_gift`), service vouchers
  (`housing.household_help`, tag `service_vouchers`; regional), home loan (`housing.mortgage`, `insurance.borrower`, tag `home_loan`, mortgage liabilities with the
  year from their schedule; flag only), rental cadastral income (always listed as missing). Optional `region: flanders|wallonia|brussels` in household.yaml.
  No rate or ceiling is quoted; disclaimers now live in `src/disclaimers.rs`. Follow-up: a web page / endpoint for it is not built (B4 if wanted).
- **B10 The two "not ported" choices [done 2026-11-24: web enrichment stays out; local HTTPS bank callback built, opt-in; owner registers the redirect address in the Enable Banking application].**
  `src/ingest/callback.rs` (`[callback] enabled`, 127.0.0.1 only, self-signed certificate in `<data_dir>/tls/`, one valid request or
  the timeout, paste as fallback); the owner's steps are in README "Bank login callback". The web app's connect still asks to paste.

## Follow-ups found by the audits

- **B12 Migration and home fixes [done: `m0023` matches Python; the root lookup stops at the finch crate; `mcp serve` logs its home].** `m0023` in `src/db.rs`: fall back to `transaction_date` when `value_date` is empty or null
  (as Python does), and treat a non-string `status` like Python. `config::find_root` walks up into the parent Python repo (its
  `config.example.toml`) and could open the Python app's database: stop at finch's own root, and log the chosen home on `mcp serve`.

## Later

- **B11 Python app retirement plan [needs owner].** Once B4/B5 land, decide whether finch becomes the only app and the Python one is
  frozen (shared schema rule then relaxes).

## Owner decisions 2026-11-24 (queued: start after the B4 / B5 branches are merged, they touch the same files)

- **B13 AI category suggestion [done 2026-11-24: `src/classify/suggest.rs`, opt-in `[classify] review_suggest`, egress flow `classify.suggest`].** Port `POST /review/suggest` + `GET /review/suggest/{job}` and fix `GET /review/ai`'s shape,
  LABEL-ONLY (redacted label, no web search), as a new opt-in egress flow. After B4. `/review/ai` reports web search as `not_ported`; held-back merchants
  now carry the reason `held_back_person_like` (the page hides the AI button for them); only low-confidence LLM / kNN labels can be asked about.
- **B14 `add_insight` MCP tool [agent].** Port with the Python safeguards (refs seen in the session, unverified numbers flagged, privacy
  masking, max 10 per session), `readOnlyHint: false`. After B5.
- **B15 Setup and onboarding pages [agent].** `/setup/wizard` (finch command names), `/onboarding`, `PUT /onboarding/household` (country
  `BE` allowed in finch; the Python app may still reject it), `POST /onboarding/preferences`; share the library with B5's `onboarding_status`.
- **B16 Larger ports [agent].** `memory_context` and `what_if` MCP tools (extend `validate()` for `oneOf` / `items`), asset rename across
  files (a multi-file `memstore` change, one history entry).
