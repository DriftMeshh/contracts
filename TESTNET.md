# Live testnet deployment

Deployed 2026-10-28 and exercised through a full end-to-end flow — not
just deployed and left idle. This is the evidence that the contracts and
the `indexer` repo actually work together against a real network, not
only in the in-process test environment.

## Contract IDs (Stellar testnet)

| Contract | ID |
|---|---|
| `device-registry` | `CDH22S3L52HJNPF7SSGXEKEMU5IYSWPJCZ4MFK33ZIMFSGDYE2OTR4JN` |
| `submission-validator` | `CAODW624PJOUXKLQVKGOPEVNS6KB7TIEXENMERYLV67WPW3ID7XMZWQH` |
| `payment-streamer` | `CBTLTCT5YEWELJHCEP3NYAKLSW5BOGW5FUFUZ3R44VZVLS7RYTCVCB26` |
| `subscription-market` | `CASBPGQZ5HC4NNDC2BMUJ2E6VKT56RTY2BWSBPHWNX3HJ7QWLLTBWJGB` |
| DMU test token (SAC-wrapped classic asset, issuer = `dm_admin`) | `CAQKKB7WWC52HAWZUVGPQNSRMG5YKVOH7BW5D6N5BASAMTIKWU6QXM6X` |

DMU is a throwaway testnet-only asset for exercising the payment flow —
not a real stablecoin. A production deployment would point
`payment-streamer`/`subscription-market` at a real asset (e.g. testnet
USDC) instead.

## What was actually run, on-chain, for real

1. Registered a device (`dm_device`, signed by its own key) with
   `device-registry` — confirmed `is_active` returns `true`.
2. Submitted 3 valid readings + 1 deliberate outlier (999999) to
   `submission-validator` for epoch 1 — **the outlier was correctly
   rejected on-chain** (`accepted: false` in the emitted event),
   `get_submission_count` returned `3`, not `4`.
3. `dm_consumer` subscribed for 2 periods (500 DMU/period) via
   `subscription-market` — confirmed the full `1000` DMU landed directly
   in `payment-streamer`'s balance (`pool_balance` → `"1000"`), with no
   separate funding step.
4. `dm_device` claimed epoch 1 — payout was exactly `3 × 25 = 75` DMU,
   confirmed both in the `claimed` event and the device's actual token
   balance afterward.
5. Repeated steps 2 and 4 for a second epoch (2 readings, claim = `50`
   DMU) **after** starting `indexer` against these same contract IDs, to
   prove live polling/ingestion rather than a one-time backfill (the
   poller's cursor seeds from "now" on first sight — see `indexer`'s
   README — so events from before it started wouldn't have been caught;
   this is why step 5 exists as a separate, later action).
6. `indexer`'s REST API, polling real testnet RPC every 4s, correctly
   reflected the new readings and claim within one poll cycle:
   `GET /devices/:address/readings`, `GET /regions/42/average?epoch=2`
   (computed `2535`, exactly right), and `GET /devices/:address/claims`
   all matched on-chain state exactly.

## Reproducing this

Identities used (stored as local CLI aliases, not committed — generate
your own with `stellar keys generate <name> --network testnet --fund`):
`dm_admin`, `dm_device`, `dm_owner`, `dm_consumer`.

```bash
# from contracts/
stellar contract build
stellar contract deploy --wasm target/wasm32v1-none/release/device_registry.wasm --source-account dm_admin --network testnet
# ...repeat per contract, then stellar contract asset deploy --asset "DMU:$(stellar keys address dm_admin)" ...
# establish trustlines: stellar tx new change-trust --source-account dm_device --network testnet --line "DMU:$ADMIN"
# initialize each contract via `stellar contract invoke --id <ID> --source-account dm_admin --network testnet -- initialize ...`
```

See each contract's README for the exact `initialize` argument lists.
