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

- **B4 Web endpoint parity [agent].** List every endpoint `web/src/api/hooks.ts` and the pages call that answers 404 `not_ported`
  in `src/web/routes.rs` / `memory_routes.rs`, then port them one by one (incl. dismissing every insight kind, not only anomalies and
  price changes). Each new endpoint gets a test in `tests/web.rs`; a child-visible one goes into `CHILD_ALLOWED` on purpose only.
- **B5 MCP tool parity [agent].** The Python server has tools finch lacks (e.g. `explain_transaction`, `memory_context`,
  `monthly_review`, `explain_spike`, `what_if`, `mortgage_check`, `savings_estimate`, `subscription_audit`, `onboarding_status`).
  Audit against `../src/coach/mcp/`, port the missing ones through `mcp::Session::publish`, a leak assertion each in `tests/e2e.rs`.
  `tax_candidates` is excluded until B9 is decided.
- **B6 Migration numbering audit [done: `migrations/README.md`; follow-up B12].** finch has 0001–0024 with gaps (0007, 0010, 0020, 0023, ...). Check each gap against the
  Python `migrations/` and record in `migrations/README.md` why it is absent (data migration rewritten in Rust, not applicable, ...).

## Waiting on the owner

- **B7 Verify the Belgian parsers with real exports [needs owner].** `src/classify/parsers/belgian.rs` and the CODA field positions
  follow published layouts only. The owner runs an import on a scratch copy and reports which lines come out as `other` or with a
  wrong merchant, as *synthetic look-alikes* (never a real line). Agents then add tests and adjust patterns.
- **B8 Release readiness [decided: secrets are set; cut v0.1.0 only after B4/B5 land].** The updater pubkey is set in `desktop/tauri.conf.json`; confirm the repository secrets
  `TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)` exist, that the repository stays public (the updater needs it), and when to cut v0.1.0.
- **B9 Belgian tax helper [decided: port a Belgian version, candidates only; agent].** The Python `tax_candidates` is FR / IT. Port a Belgian version (which deductions:
  mortgage / woonbonus, pension saving, childcare, gifts, service vouchers?) or drop it. Candidates only, never advice.
- **B10 The two "not ported" choices [decided: web enrichment stays out; build the local HTTPS bank callback; agent. Owner registers the redirect address in the Enable Banking application].** Keep them out (web enrichment of merchants; local HTTPS bank callback) or build
  the callback (needs the redirect address registered in the Enable Banking application).

## Follow-ups found by the audits

- **B12 Migration and home fixes [agent].** `m0023` in `src/db.rs`: fall back to `transaction_date` when `value_date` is empty or null
  (as Python does), and treat a non-string `status` like Python. `config::find_root` walks up into the parent Python repo (its
  `config.example.toml`) and could open the Python app's database: stop at finch's own root, and log the chosen home on `mcp serve`.

## Later

- **B11 Python app retirement plan [needs owner].** Once B4/B5 land, decide whether finch becomes the only app and the Python one is
  frozen (shared schema rule then relaxes).
