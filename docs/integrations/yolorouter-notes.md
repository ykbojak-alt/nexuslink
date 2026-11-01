# YoloRouter integration notes

## Source and license

- Reference checkout: `D:\refs\yolorouter`.
- License: Apache-2.0 (`D:\refs\yolorouter\LICENSE`). No source was copied.
- Relevant design: `internal/gateway/keypool.go` (`walkOrder`, `cooldownFromRetryAfter`) and `README.md` sections “Per-model scheduling mode” and “Scheduling modes”.

## Adopted in NexusLink

| Pattern | NexusLink implementation | Compatibility / behavior |
|---|---|---|
| Per-provider key pool with healthy keys first | Existing `config::key_pool` expansion plus `proxy::candidate_order` | Candidates for the same provider remain contiguous; key rotation remains within the provider pool. |
| `Retry-After` bench | `rotation::retry_after`, `Blacklist::bench_key`, and `key_bench_remaining`; applied in `proxy::try_candidate`/`messages` | Delta-seconds and HTTP-date accepted. Valid values clamp to 1 second–10 minutes; absent/malformed values use the existing 15-minute TTL. A bench demotes rather than hard-blocks; if all candidates are benched, earliest expiry is attempted. 401/402/403 and model 404 retain hard blacklist behavior. |
| Failover scheduling | `ModelEntry.scheduling_mode`, default `failover` | Legacy configs deserialize unchanged. Failover starts with the first configured provider and rotates keys within its pool; failures proceed through later providers. |
| Balanced scheduling | `ModelEntry.scheduling_mode = "balanced"` | Monotonic in-memory per-slot cursor rotates the first provider group, then the provider's key pool. This balances request starts by configured provider order, including pools of unequal sizes. It is not sticky per caller key: the current gateway has one shared incoming credential and no caller-identity routing table. |

## Deliberate differences and limits

- YoloRouter's Retry-After bench contains generation-aware/concurrent-verdict handling and persistent retest invalidation. NexusLink's existing gateway state is a simpler in-memory map; this patch does not add generation/CAS state because NexusLink has no provider-key record/version model. Cooldowns reset on process restart.
- The current slot chain is assembled from `Provider.models`; the first candidate's `scheduling_mode` governs the slot. If duplicated slot entries disagree, the first configured mapping wins. Keep entries consistent until configuration validation/UI can enforce this.
- Provider order remains the failover priority. No automatic provider reordering or cost-based routing was added.
- The queue also mentioned model-catalog import. That is outside this routing patch: no catalog fetch or persistence semantics were introduced.

## Verification

Commands run with isolated `CARGO_TARGET_DIR=D:\Temp\cargo-openbridge-wave-20261031` because the existing `target\debug\nexuslink.exe` was held by a live process:

- `cargo test rotation::tests -- --nocapture` — 6 passed.
- `cargo test scheduling_tests -- --nocapture` — 2 passed.
- `cargo test scheduling_mode_defaults_to_failover_and_round_trips_balanced -- --nocapture` — 1 passed.
- `cargo test --test rotation_tests -- --nocapture` — 6 passed.

`rustfmt --check` could not run because the rustfmt component is not installed. No live upstream calls were made. No commit was created.
