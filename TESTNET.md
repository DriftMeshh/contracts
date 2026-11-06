# Live testnet deployment

Redeployed 2026-11-06 (fresh contract IDs — see below) to pick up the
`#[contractevent]` migration and the proportional-payout/epoch-finality
model, and exercised through a full end-to-end flow again — not just
deployed and left idle. This is the evidence that the contracts and the
`indexer` repo actually work together against a real network, not only
in the in-process test environment. There is no upgrade mechanism in any
of these contracts, so adopting new contract code means new addresses
and a fresh on-chain state, not an in-place update — the previous
deployment's IDs are no longer live.

## Contract IDs (Stellar testnet)

| Contract | ID |
|---|---|
| `device-registry` | `CAZUUT5WRULDICF67IFHUWJZZTGP7BOCPJNIIGE7GDZHBAS7ZNZTESLB` |
| `submission-validator` | `CDTYY76UHKKSL5LDFGPEDO7DCU4LOR4S3LPHQCEHDA3UYPZDT3P5T3U2` |
| `payment-streamer` | `CBWUNOBVIFEYKDXVNXSIIYK3WF6FSP6JUKNUAYVVRCVIRO2RP7ZUEILB` |
| `subscription-market` | `CBSPQORJFRKPYP2OASYWYMBLY7UMIRBQRWOWBRRSQDTNW2B5JORYBYOG` |
| DMU test token (SAC-wrapped classic asset, issuer = `dm_admin`), reused from the previous deployment | `CAQKKB7WWC52HAWZUVGPQNSRMG5YKVOH7BW5D6N5BASAMTIKWU6QXM6X` |

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

1. Registered two devices, `dm_device` and `dm_device2` (each signed by
   its own key), with `device-registry` — confirmed `is_active` returns
   `true` for both.
2. Submitted readings to `submission-validator` for the then-current
   epoch (24): 3 accepted readings from `dm_device`, 2 accepted from
   `dm_device2` — `get_submission_count` returned `3` and `2`
   respectively, `get_total_submissions` returned `5`.
3. `dm_consumer` subscribed for 2 periods (500 DMU/period) via
   `subscription-market` — confirmed the full `1000` DMU landed directly
   in `payment-streamer`'s balance (`pool_balance` → `"10000000000"`
   stroops), with no separate funding step.
4. Waited for `is_epoch_final(24)` to become `true` (the epoch's real
   1-hour window closing), then called `claim` for both devices —
   **the new `payout = epoch_pool * device_count / total_count` formula
   is exactly what was paid out**: `dm_device` received
   `10000000000 * 3/5 = 6000000000` stroops (600 DMU), `dm_device2`
   received `10000000000 * 2/5 = 4000000000` stroops (400 DMU), matching
   both the `claimed` event and the devices' actual token balances
   afterward. The pool was snapshotted once on the first claim, not
   re-read on the second, confirmed by the two payouts summing to
   exactly the pre-claim pool balance.
5. Hit a genuine prerequisite along the way, not a contract bug:
   `dm_device2`'s claim initially failed with
   `"trustline entry is missing for account"` — a classic-Stellar
   asset (DMU, SAC-wrapped) requires the receiving account to hold a
   trustline before it can receive a transfer. Fixed with
   `stellar tx new change-trust --source-account dm_device2 --network
   testnet --line "DMU:<issuer>"`, then the claim succeeded. Any new
   device identity needs this one-time step before its first claim.
6. `indexer`'s REST API, polling real testnet RPC, correctly reflected
   both claims within one poll cycle: `GET
   /devices/<dm_device>/claims` and `GET /devices/<dm_device2>/claims`
   returned the exact `6000000000`/`4000000000` payouts and matching
   `tx_hash`es — confirming the `#[contractevent]` map-shaped event
   parsing fix (`indexer`'s `parse.ts`) works against this contract
   code's real on-chain event encoding.

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
#   (submission-validator's initialize now also takes --epoch_seconds)
```

See each contract's README for the exact `initialize` argument lists.
