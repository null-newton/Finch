# Migrations

The schema is shared with the Python app (`../src/coach/migrations/`). Both runners record `version` + `name` in `schema_migrations`,
so a database migrated by one opens in the other. The list that counts is `db::migrations()` in `src/db.rs`, not this folder: a
number missing here is a Python **data** migration (`.py`), ported as a Rust function in `src/db.rs`. SQL files are byte-identical to
Python's (line endings aside). A new migration takes the next number in both apps and never changes an existing one.

| # | Python file | finch | Lives in |
|---|---|---|---|
| 0001 | `0001_initial.sql` | identical | `migrations/0001_initial.sql` |
| 0002 | `0002_indexes.sql` | identical | `migrations/0002_indexes.sql` |
| 0003 | `0003_multibank_consents_imports_transfers.sql` | identical | `migrations/0003_...sql` |
| 0004 | `0004_account_needs_review.sql` | identical | `migrations/0004_...sql` |
| 0005 | `0005_pending_auth_hardening.sql` | identical | `migrations/0005_...sql` |
| 0006 | `0006_e2_parsers_llm_entities_splits.sql` | identical | `migrations/0006_...sql` |
| 0007 | `0007_rekey_position_refs.py` | Rust port (data: re-key position-based refs, log to `tx_key_remap`) | `src/db.rs` `m0007` |
| 0008 | `0008_batches_entity_category_user.sql` | identical | `migrations/0008_...sql` |
| 0009 | `0009_batch_jobs.sql` | identical | `migrations/0009_...sql` |
| 0010 | `0010_batch_purpose_usage_ref_rename_journal.py` | Rust port (idempotent `ADD COLUMN`s, index, `taxonomy_renames`) | `src/db.rs` `m0010` |
| 0011 | `0011_analytics.sql` | identical | `migrations/0011_...sql` |
| 0012 | `0012_price_change_dismissals.sql` | identical | `migrations/0012_...sql` |
| 0013 | `0013_insights.sql` | identical | `migrations/0013_...sql` |
| 0014 | `0014_alternatives.sql` | identical | `migrations/0014_...sql` |
| 0015 | `0015_decisions.sql` | identical | `migrations/0015_...sql` |
| 0016 | `0016_net_worth_history.sql` | identical | `migrations/0016_...sql` |
| 0017 | `0017_alerts.sql` | identical | `migrations/0017_...sql` |
| 0018 | `0018_alert_channel_state.sql` | identical | `migrations/0018_...sql` |
| 0019 | `0019_egress_journal.sql` | identical | `migrations/0019_...sql` |
| 0020 | `0020_ai_label_compliance.py` | Rust port, schema part only (1) | `src/db.rs` `m0020` |
| 0021 | `0021_quality_gold_eval.sql` | identical | `migrations/0021_...sql` |
| 0022 | `0022_household_people.sql` | identical | `migrations/0022_...sql` |
| 0023 | `0023_dateless_rows_to_pending.py` | Rust port (data: dateless rows to `pending_transactions`) (2) | `src/db.rs` `m0023` |
| 0024 | `0024_passkeys.sql` | identical | `migrations/0024_passkeys.sql` |
| 0025 | `0025_merge_parked_reference_duplicates.py` | Rust port (data: merge `:cfp:` / `:ref:` duplicate pairs) | `src/db.rs` `m0025` |

(1) The one-off compliance check of insights that already existed is not ported: a database finch creates has no insights at that
point, and one the Python app migrated already ran it. Only a Python database below version 20 with insights, opened first by finch,
would skip it.

(2) Same semantics as Python (B12): `value_date or transaction_date` with Python truthiness (an empty or `null` `value_date` falls back to
`transaction_date`), and a row is booked only when `status` is absent, `null`, empty or `BOOK` (a non-string `status` moves it to pending).
