# Live testnet deployment

Redeployed again 2026-11-06 (fresh contract IDs — see below) to pick up
the dispute-slashing feature (device-registry disputes/reactivation,
submission-validator's auto-slash threshold, payment-streamer's
pause/unpause), and exercised through a full end-to-end flow — not just
deployed and left idle. This is the evidence that the contracts and the
`indexer` repo actually work together against a real network, not only
in the in-process test environment. There is no upgrade mechanism in any
of these contracts (see the open issue for this), so adopting new
contract code means new addresses and a fresh on-chain state, not an
in-place update — previous deployments' IDs are no longer live.

## Contract IDs (Stellar testnet)

| Contract | ID |
|---|---|
| `device-registry` | `CDIM2GOGOW2BUBF4MOE4TUJO3EBULO5L4ES4VF6EEL33YE6ORJJHHL2T` |
| `submission-validator` | `CCKYFNIVBS34UBH4OK7FSANBLLLDNGMMPCHSFVSJLUMX6XGRFJMN7L4G` |
| `payment-streamer` | `CAHGKJ624HJC5GSXFF2NUAYPEULQRF66WP67EP3FCRC5ERYCMTM2M5C7` |
| `subscription-market` | `CAWE7DOPOORGQO65V44MBG43JFFEAOVJ66VABDWTHD3HDIFWZUHKZ7XL` |
| DMU test token (SAC-wrapped classic asset, issuer = `dm_admin`), reused from every prior deployment | `CAQKKB7WWC52HAWZUVGPQNSRMG5YKVOH7BW5D6N5BASAMTIKWU6QXM6X` |

`submission-validator` was additionally initialized with
`auto_slash_threshold = 3` (fires `AutoSlashThresholdReached` after 3
cumulative rejected readings from one device — see the open issue about
this not yet triggering a real consequence on its own).

DMU is a throwaway testnet-only asset for exercising the payment flow —
not a real stablecoin. A production deployment would point
`payment-streamer`/`subscription-market` at a real asset (e.g. testnet
USDC) instead.

`submission-validator` was initialized with `epoch_seconds = 3600`.
Epoch numbers are **relative to this contract's own deployment time**,
not Unix time: `initialize` records `epoch_zero` as the ledger timestamp
at that call, and epoch `e` covers `[epoch_zero + e*3600, epoch_zero +
(e+1)*3600)`. There's no direct getter for `epoch_zero`, but it can be
read back via `epoch_end(0) - epoch_seconds` — `device-sdk`'s
`fetchEpochZero` does exactly this once at startup and caches it. This
caught a real bug during this redeploy: the first verification attempt
assumed epoch 0 was Unix epoch 0 and picked an epoch number centuries in
the future, which could never close — see `device-sdk`'s commit fixing
`currentEpoch`.

## What was actually run, on-chain, for real

This flow was run twice against two different deployments (the second
replacing the first once the dispute-slashing contracts landed), and
both times produced an exact match against the
`payout = epoch_pool * device_count / total_count` formula — including
once with genuine third-party contention from another device, not just
a clean two-device split.

**First deployment** (proportional-payout formula, before dispute-slashing):
1. Registered `dm_device`/`dm_device2`, submitted 3 and 2 accepted
   readings respectively for epoch 24 (`get_total_submissions` → `5`).
2. `dm_consumer` subscribed for 2 periods (1000 DMU landed directly in
   `payment-streamer`'s balance, `pool_balance` → `"10000000000"`).
3. After `is_epoch_final(24)` became `true`, `claim` paid out exactly
   `10000000000 * 3/5 = 6000000000` stroops to `dm_device` and
   `10000000000 * 2/5 = 4000000000` to `dm_device2` — matching both the
   `claimed` event and the devices' token balances, and summing to
   exactly the pre-claim pool balance (confirming the pool is
   snapshotted once, not re-read per claim).
4. Hit a genuine prerequisite along the way, not a contract bug:
   `dm_device2`'s claim initially failed with
   `"trustline entry is missing for account"` — a classic-Stellar asset
   (DMU, SAC-wrapped) requires the receiving account to hold a
   trustline before it can receive a transfer. Fixed with
   `stellar tx new change-trust --source-account dm_device2 --network
   testnet --line "DMU:<issuer>"`. See the open issue about surfacing
   this prerequisite automatically.
5. `indexer`'s REST API correctly reflected both claims within one poll
   cycle, confirming the `#[contractevent]` map-shaped event parsing fix
   works against this contract code's real on-chain event encoding.

**Second deployment** (adds dispute-slashing; same devices re-registered
against the new `device-registry`, same trustlines carried over since
they're an account property, not tied to a contract):
1. Re-registered `dm_device`/`dm_device2`, re-subscribed `dm_consumer`
   for 2 periods (another `10000000000` stroops into the new
   `payment-streamer`), and submitted 3 and 2 accepted readings for
   the then-current epoch (3).
2. This time `get_total_submissions(3)` came back as `57`, not `5` —
   the indexer's own bundled `device-sdk` instance (see `indexer`'s
   README, "device-sdk runs here too") was submitting continuously
   against the same live contracts throughout that epoch's hour window,
   contributing the other 52 accepted readings. This is **organic
   multi-device contention**, not a staged two-device demo.
3. After `is_epoch_final(3)` became `true`, `claim` paid out exactly
   `10000000000 * 3/57 = 526315789` stroops to `dm_device` and
   `10000000000 * 2/57 = 350877192` to `dm_device2` (both floor-divided,
   matching Soroban's integer arithmetic exactly) — confirmed via both
   the `claimed` events and `indexer`'s `GET /devices/<address>/claims`.

## Reproducing this

Identities used (stored as local CLI aliases, not committed — generate
your own with `stellar keys generate <name> --network testnet --fund`):
`dm_admin`, `dm_device`, `dm_device2`, `dm_owner`, `dm_consumer`.

```bash
# from contracts/
stellar contract build
stellar contract deploy --wasm target/wasm32v1-none/release/device_registry.wasm --source-account dm_admin --network testnet
# ...repeat per contract, then stellar contract asset deploy --asset "DMU:$(stellar keys address dm_admin)" ...
# establish trustlines: stellar tx new change-trust --source-account dm_device --network testnet --line "DMU:$ADMIN"
# initialize each contract via `stellar contract invoke --id <ID> --source-account dm_admin --network testnet -- initialize ...`
#   (submission-validator's initialize now also takes --epoch_seconds and --auto_slash_threshold)
```

See each contract's README for the exact `initialize` argument lists.
