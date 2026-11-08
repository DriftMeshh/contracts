# DriftMesh — Contracts

A Stellar/Soroban-native DePIN protocol: physical sensors register
on-chain, submit readings, and get paid in real time per valid reading —
not in batched, delayed rewards like most DePIN networks on other chains,
because Stellar's sub-cent fees and ~3-5s finality make true per-reading
micropayments economically viable.

Data consumers (researchers, insurers, agtech apps) subscribe to the feed;
their payments fund the pool devices get paid from. No category like this
currently exists on Stellar — it has zero presence in current DePIN
rankings despite being architecturally well suited to it.

This repo is the on-chain core: four Soroban contracts. The rest of the
project lives in sibling repos under the `DriftMeshh` org — `indexer`
(the off-chain event indexer / API backend), `device-sdk` (a client
library + simulator a sensor runs to sign and submit readings), and `app`
(the frontend — operator dashboard + consumer data portal).

## Architecture

```
            ┌─────────────────────┐
  device ──▶│ submission-validator │──is_active?──▶ ┌────────────────┐
  (signed   │  outlier check per   │                │ device-registry │
  reading)  │  region/device-type  │◀──register──── │  (owner, type,  │
            └──────────┬───────────┘                │  location)      │
                        │ valid reading count            └────────────────┘
                        │ per device per epoch
                        ▼
            ┌─────────────────────┐        funds pool       ┌──────────────────┐
            │  payment-streamer   │◀────────────────────────│ subscription-market│
            │  pays devices per   │   subscriber payment,   │  consumers pay for │
            │  valid submission,  │   sent straight to      │  feed access       │
            │  capped by balance  │   streamer's own balance│                    │
            └─────────────────────┘                         └──────────────────┘
```

Each arrow is a real cross-contract call (`#[contractclient]`-generated
interfaces, not shared implementation code — see the note at the top of
`submission-validator` and `payment-streamer` for why: two independently
deployed contracts that happen to share a function name like
`initialize` will collide at the wasm symbol level if one depends on the
other's *implementation* crate directly. Each contract only knows the one
or two methods it actually needs to call on its neighbour.)

### Contracts

| Contract | Purpose | Key entrypoints |
|---|---|---|
| `device-registry` | A device (its own keypair) registers itself: owner, type, coarse location. Owner/admin can deactivate it. Admin can reactivate. Any address can raise a dispute; admin resolves it (upheld = deactivation). | `register_device`, `deactivate_device`, `reactivate_device`, `is_active`, `get_device`, `raise_dispute`, `resolve_dispute`, `get_dispute_counts` |
| `submission-validator` | Accepts signed readings, checks the device is active (cross-call), rejects readings that are wildly off the region's rolling average, and counts valid submissions per device per epoch — and per epoch overall. Tracks cumulative rejected readings per device; emits `AutoSlashThresholdReached` when a device's rejection count crosses a configurable threshold. Epochs are time-boxed; once an epoch's window closes its total is frozen (`EpochClosed`). `batch_submit_reading` submits up to 40 readings in one call for devices with intermittent connectivity, reusing the same validation as `submit_reading`. | `submit_reading`, `batch_submit_reading`, `get_submission_count`, `get_rejected_count`, `get_total_submissions`, `is_epoch_final`, `epoch_end`, `get_epoch_zero`, `get_region_average` |
| `payment-streamer` | Pays a device a *proportional share* of an epoch's pool — `epoch_pool * device_count / total_count`, where `total_count` is the epoch's total valid submissions (from `submission-validator`) and `epoch_pool` is snapshotted when the epoch closes. Payout is independent of claim order; a device claims once per epoch. Admin can pause/unpause claim operations (emitting `Paused`/`Unpaused`). `preview_payout` shows the expected payout without claiming. | `claim`, `preview_payout`, `pool_balance`, `epoch_pool`, `has_claimed`, `pause`, `unpause`, `is_paused` |
| `subscription-market` | Consumers pay for feed access; payment goes straight into `payment-streamer`'s token balance, closing the funding loop. Admin can update price; price changes emit `PriceUpdated` events. | `subscribe`, `is_active`, `get_expiry`, `get_price`, `get_period_seconds`, `set_price` |

### Honest limitations (v1 scope)

- **Device data is self-reported.** A sensor can lie. The outlier check
  catches a single device being wildly out of line with its neighbours —
  it is not a consensus mechanism and does not stop coordinated fraud.
  v1 now includes a dispute/slashing mechanism (see `raise_dispute`,
  `resolve_dispute` in `device-registry` and `get_rejected_count` /
  `AutoSlashThresholdReached` in `submission-validator`). An upheld dispute
  deactivates the device. The admin is trusted to make the resolution
  decision honestly — a multi-sig admin hardens this without any contract
  changes. Hardening further with stake-weighted scoring is a deliberate v2.
- **No real hardware yet.** `device-sdk` ships as a reading simulator for
  v1 so the full loop is demoable without physical sensors.
- **No storage TTL/rent management** is fully solved: persistent entries in
  `device-registry` and `subscription-market` now call `extend_ttl` at
  write time to prevent archival. Temporary counters in
  `submission-validator` and `payment-streamer` use TTL extension on every
  write (120,960 ledgers / ~1 week).
- **No live-network testing yet.** All 30 tests (unit + cross-contract +
  one full end-to-end flow in `tests-e2e`) run in Soroban's in-process
  test environment, not against an actual testnet/futurenet deployment.
- **Deprecated event API.** All four contracts now use the `#[contractevent]`
  macro — the migration from `env.events().publish()` was completed in a
  prior PR (#9).

## Building

Requires Rust stable with the `wasm32v1-none` target, and the Stellar CLI
(v25.2.0+ — soroban-sdk 29 requires it to drive the build, plain
`cargo build --target wasm32-unknown-unknown` will not work on Rust
1.82+).

```bash
rustup target add wasm32v1-none
cargo test --workspace        # 35 tests: per-contract units + two e2e flows in tests-e2e
stellar contract build        # produces target/wasm32v1-none/release/*.wasm
```

## Dispute and slashing mechanism

The dispute/slashing mechanism gives real economic weight to data quality
without requiring a separate staking contract (issue #6 / stake-weighted
scoring has not landed yet — this design deliberately does not depend on it).

### How it works

1. **Evidence accumulation** (`submission-validator`): Every rejected reading
   increments `RejectedCount(device)` in temporary storage. When a device
   crosses the `auto_slash_threshold` set at deploy time, an
   `AutoSlashThresholdReached` event is emitted. This event is the signal for
   an off-chain monitor (admin bot) to investigate.

2. **Raising a dispute** (`device-registry.raise_dispute`): Any address can
   call `raise_dispute(disputer, device, evidence_hash)`. The `evidence_hash`
   is a 64-bit truncated hash of an off-chain evidence bundle (e.g.
   `hash(device_address || epoch || url_of_logs)`). Only one open dispute per
   device is permitted at a time — this prevents griefing through dispute
   flooding. The device remains **active** while a dispute is pending; it
   continues submitting readings.

3. **Admin resolution** (`device-registry.resolve_dispute`): The admin reviews
   the off-chain evidence and calls `resolve_dispute(admin, device, upheld)`.
   - `upheld=true` → device is immediately deactivated (slashed). The
     `DeviceDeactivated(reason="dispute")` and `DisputeResolved` events are
     emitted. The lifetime `UpheldDisputeCount` is incremented.
   - `upheld=false` → device is untouched. `DisputeResolved(upheld=false)` is
     emitted. No lasting effect on the device's state.

4. **Reactivation** (`device-registry.reactivate_device`): If a dispute is
   upheld but later found to have been in error (e.g. a hardware replacement
   verified off-chain), the admin can reactivate the device.

### Trust assumptions (explicit)

- **Admin is trusted to make honest resolution decisions.** A corrupted admin
  can uphold fabricated disputes or dismiss legitimate ones. A Stellar
  multi-sig admin (M-of-N threshold signing on the admin keypair) hardens this
  without any contract changes.
- **Disputer accountability.** A disputer's address is permanently on-chain in
  the `DisputeRaised` event. Raising a false dispute costs gas and creates a
  public record.
- **One-dispute-at-a-time cap.** Prevents dispute flooding by malicious
  competitors. A disputer must wait for resolution before raising another,
  slowing any campaign of harassment.
- **No bond in v1.** Without a deposit requirement (issue #6 hasn't landed),
  the one-dispute cap is the only cost imposed on a bad-faith disputer. A
  bond-based extension is a natural follow-up.
- **Deactivation is the only v1 'slash'.** No stake is burned (no staking
  contract exists yet). Economic consequence is loss of future payouts while
  deactivated.

## Suggested Stellar Wave issue breakdown

Scoped so each is a separable GitHub issue at a realistic complexity tier:

**Trivial (100 pts)**
- ~~Migrate `env.events().publish` calls to the `#[contractevent]` macro in each contract~~ ✅ Done (PR #9)
- ~~Add `pool_balance`-style view helpers to `subscription-market` (e.g. `get_price`)~~ ✅ Done — `get_price` first, `get_period_seconds` as a follow-up
- ~~Add doc comments / a CONTRIBUTING.md~~ ✅ Done

**Medium (150 pts)**
- ~~Add explicit `extend_ttl` / archival handling for `device-registry`'s persistent entries so long-lived devices don't get archived~~ ✅ Done — `register_device`'s one-time ~1yr extension landed first; `is_active` (the hot path on every `submit_reading`) also refreshing it on every call was a follow-up closing the remaining gap (a device active beyond a year with no other contract calls)
- ~~Add a `batch_submit_reading` entrypoint so a device can submit several readings in one call~~ ✅ Done — capped at 40 per call after empirically finding the originally-planned 50 exceeds Soroban's resource budget
- ~~Add an admin `pause`/`unpause` switch to `payment-streamer`~~ ✅ Done (this PR)
- Add geohash-bucket helper utilities to `device-sdk` so `region_code` computation is consistent across clients

**High (200 pts)**
- Design and implement stake-weighted outlier scoring (replace the simple rolling-average check in `submission-validator` with something resistant to a device slowly dragging its own region average)
- ~~Implement partial/dispute-based slashing for devices caught submitting bad data~~ ✅ Done (this PR — raise_dispute + resolve_dispute + RejectedCount + AutoSlashThresholdReached)
- Deploy all four contracts to Stellar testnet and extend `tests-e2e`'s flow into a real integration test against a live network (today it only runs in Soroban's in-process test environment)
