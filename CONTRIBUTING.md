# Contributing to DriftMesh — contracts

This repo is scoped for [Stellar Wave](https://www.drips.network/wave/stellar) —
issue complexity here follows Wave's own point system, so a contributor
coming from Wave finds exactly what they expect.

## Before you start

1. Read the [README](README.md) for the architecture and why each
   contract is split the way it is.
2. Read [TESTNET.md](TESTNET.md) to see the contracts actually working
   end to end on live testnet — the same flow your change should keep
   working.
3. Set up the toolchain and confirm the existing suite passes before
   touching anything:

   ```bash
   rustup target add wasm32v1-none
   cargo test --workspace   # 13 tests should pass
   stellar contract build   # should produce 4 wasm files with no errors
   ```

## Issue complexity (matches Stellar Wave's point system exactly)

| Tier | Points | What belongs here |
|---|---|---|
| Trivial | 100 | Small, clearly bounded changes with obvious acceptance criteria — a doc fix, a view-only helper function, a missing test case for existing logic. |
| Medium | 150 | Standard features or logic touching multiple parts of a contract — new storage field + migration, a new entrypoint, TTL/archival handling. |
| High | 200 | Complex engineering — new economic logic (e.g. proportional payouts), cross-contract composability changes, security-sensitive work like outlier/slashing logic. |

See each contract's README section ("Suggested Stellar Wave issues") for
concrete, already-scoped examples at each tier.

## What a good PR looks like here

- **One contract, one concern.** `device-registry`, `submission-validator`,
  `payment-streamer`, and `subscription-market` are deliberately
  decoupled (see the `#[contractclient]` notes at the top of
  `submission-validator`/`payment-streamer`) — a PR touching one
  shouldn't need to touch another's implementation.
- **Tests are not optional.** Every contract here has unit tests and the
  `tests-e2e` crate has a full cross-contract integration test. A PR
  that changes behavior without a test covering it will be asked to add
  one before merge — this isn't Wave-specific policy, it's how this repo
  has been built from the first commit.
- **Run `stellar contract build` before opening the PR**, not just
  `cargo test`. Unit tests run against Soroban's in-process environment;
  the build step catches real deployment issues (we hit a wasm symbol
  collision this way during initial scaffolding — see the
  `#[contractclient]` notes for what that was and why the fix matters).
- **PR description includes `Closes #<issue>`** and a one-line summary of
  what changed and why — not what the diff says (the diff already says
  that).

## Local testing against a live network

Unit tests never touch a real network. If your change affects
cross-contract calls, token transfers, or anything else that only shows
up under real Soroban RPC behavior, deploy to testnet yourself and
exercise it — see [TESTNET.md](TESTNET.md) for the exact commands used
to deploy and drive the full flow the first time.

## Questions

Open a draft PR early if you're unsure about scope or approach — that's
cheaper for everyone than a large PR that needs to be re-architected in
review.
