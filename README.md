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
| `device-registry` | A device (its own keypair) registers itself: owner, type, coarse location. Owner/admin can deactivate it. | `register_device`, `deactivate_device`, `is_active`, `get_device` |
| `submission-validator` | Accepts signed readings, checks the device is active (cross-call), rejects readings that are wildly off the region's rolling average, counts valid submissions per device per epoch. | `submit_reading`, `get_submission_count`, `get_region_average` |
| `payment-streamer` | Pays a device `submission_count * reward_rate` in a stablecoin, capped by its own token balance. A device claims once per epoch. | `claim`, `pool_balance`, `set_reward_rate` |
| `subscription-market` | Consumers pay for feed access; payment goes straight into `payment-streamer`'s token balance, closing the funding loop. | `subscribe`, `is_active`, `set_price` |

### Honest limitations (v1 scope)

- **Device data is self-reported.** A sensor can lie. The outlier check
  catches a single device being wildly out of line with its neighbours —
  it is not a consensus mechanism and does not stop coordinated fraud.
  This is the same trust model every DePIN network uses (Helium, WeatherXM,
  DIMO); hardening it with stake-weighted scoring or slashing is a
  deliberate v2, not something glossed over here.
- **No real hardware yet.** `device-sdk` ships as a reading simulator for
  v1 so the full loop is demoable without physical sensors.
- **No storage TTL/rent management** is implemented on the temporary
  per-epoch counters yet — see the issues list below.
- **No live-network testing yet.** All 13 tests (unit + cross-contract +
  one full end-to-end flow in `tests-e2e`, covering register → submit →
  subscribe → claim across all four contracts) run in Soroban's in-process
  test environment, not against an actual testnet/futurenet deployment.
- **Deprecated event API.** All four contracts currently use
  `env.events().publish(...)` directly, which soroban-sdk 29 flags as
  deprecated in favor of the `#[contractevent]` macro. Left as-is because
  it still compiles and works; migrating is a good low-complexity issue.

## Building

Requires Rust stable with the `wasm32v1-none` target, and the Stellar CLI
(v25.2.0+ — soroban-sdk 29 requires it to drive the build, plain
`cargo build --target wasm32-unknown-unknown` will not work on Rust
1.82+).

```bash
rustup target add wasm32v1-none
cargo test --workspace        # 13 tests: per-contract units + one full e2e flow in tests-e2e
stellar contract build        # produces target/wasm32v1-none/release/*.wasm
```

## Suggested Stellar Wave issue breakdown

Scoped so each is a separable GitHub issue at a realistic complexity tier:

**Trivial (100 pts)**
- Migrate `env.events().publish` calls to the `#[contractevent]` macro in each contract
- Add `pool_balance`-style view helpers to `subscription-market` (e.g. `get_price`)
- Add doc comments / a CONTRIBUTING.md

**Medium (150 pts)**
- Add explicit `extend_ttl` / archival handling for `device-registry`'s persistent entries so long-lived devices don't get archived
- Add a `batch_submit_reading` entrypoint so a device can submit several readings in one call
- Add an admin `pause`/`unpause` switch to `payment-streamer`
- Add geohash-bucket helper utilities to `device-sdk` so `region_code` computation is consistent across clients

**High (200 pts)**
- Design and implement stake-weighted outlier scoring (replace the simple rolling-average check in `submission-validator` with something resistant to a device slowly dragging its own region average)
- Implement partial/dispute-based slashing for devices caught submitting bad data
- Add a global per-epoch submission total to `submission-validator` so `payment-streamer` can pay *proportional* shares instead of a flat per-submission rate (closer to how Stellar Wave itself does quadratic-scaled payouts)
- Deploy all four contracts to Stellar testnet and extend `tests-e2e`'s flow into a real integration test against a live network (today it only runs in Soroban's in-process test environment)
