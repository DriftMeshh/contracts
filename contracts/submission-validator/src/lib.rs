#![no_std]

//! Submission Validator
//!
//! Accepts signed sensor readings, checks the submitting device is
//! registered and active (cross-contract call into `device-registry`),
//! and runs a lightweight outlier check against a rolling regional
//! average before counting the reading toward that device's payable
//! submissions for the epoch.
//!
//! Epochs are time-boxed (Stellar Wave–style cycle-end timeboxing).
//! `initialize` records `epoch_zero` (the ledger timestamp at init) and an
//! `epoch_seconds` length; epoch `e` covers
//! `[epoch_zero + e*epoch_seconds, epoch_zero + (e+1)*epoch_seconds)`.
//! Once `ledger.timestamp() >= epoch_end(e)` the epoch is "final":
//! `submit_reading` rejects further readings for it (`Error::EpochClosed`),
//! which freezes that epoch's **total** submission count, and
//! `is_epoch_final(e)` returns true. Downstream (`payment-streamer`)
//! uses that finality gate so payouts are computed against a total that
//! can no longer change — see the proportional-share payout model there.
//!
//! The outlier check is deliberately simple for v1 — it is NOT a
//! consensus mechanism. It stops a single device's lie from being wildly
//! out of line with its neighbours, nothing more. Hardening this
//! (stake-weighted scoring, slashing, multi-device consensus) is
//! explicitly left as follow-up Wave issues — see README.

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol,
};

/// Calling interface for `device-registry` — deliberately just the one
/// method this contract needs, not a dependency on the registry's actual
/// implementation crate. Keeps the two contracts independently deployable
/// (and avoids a wasm symbol collision between two separately-compiled
/// `#[contract]` implementations that happen to share function names).
#[contractclient(name = "DeviceRegistryClient")]
pub trait DeviceRegistryInterface {
    fn is_active(env: Env, device: Address) -> bool;
}

#[contracttype]
pub enum DataKey {
    Admin,
    Registry,
    ToleranceBps,
    RegionAgg(u32, Symbol),
    SubmissionCount(Address, u32),
    /// Running count of *all* accepted readings across every device in an
    /// epoch — the denominator for proportional payouts. Frozen once the
    /// epoch is final because `submit_reading` stops accepting for it.
    TotalSubmissions(u32),
    /// Length of one epoch in ledger seconds (set at `initialize`).
    EpochSeconds,
    /// Ledger timestamp at which epoch 0 started (set at `initialize`).
    EpochZero,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    DeviceNotActive = 3,
    /// The epoch's time window has already closed — its total submission
    /// count is final, so late readings can't be allowed to change it.
    EpochClosed = 4,
}

/// Emitted when a reading is submitted (accepted or rejected by outlier check).
/// Topics: ["reading", device_address]
/// Data: (region_code, epoch, value, accepted)
#[contractevent]
#[derive(Clone)]
pub struct Reading {
    #[topic]
    pub device: Address,
    pub region_code: u32,
    pub epoch: u32,
    pub value: i32,
    pub accepted: bool,
}

#[contract]
pub struct SubmissionValidator;

/// Compute the exclusive upper bound of epoch `e`'s window from storage:
/// `epoch_zero + (e + 1) * epoch_seconds`. Shared by the public `epoch_end`
/// view and the internal finality checks so they can never disagree. A free
/// helper (not a contract function) so internal callers pass `&Env` rather
/// than the by-value `Env` contract functions receive.
fn compute_epoch_end(env: &Env, epoch: u32) -> u64 {
    let epoch_seconds: u64 = env
        .storage()
        .instance()
        .get(&DataKey::EpochSeconds)
        .unwrap_or(0);
    let epoch_zero: u64 = env
        .storage()
        .instance()
        .get(&DataKey::EpochZero)
        .unwrap_or(0);
    epoch_zero + (epoch as u64 + 1) * epoch_seconds
}

#[contractimpl]
impl SubmissionValidator {
    /// `tolerance_bps` is how far (in basis points of the current regional
    /// average) a new reading may deviate and still be accepted, e.g.
    /// 2000 = 20%. A minimum absolute floor of 50 (fixed-point units)
    /// applies too, so regions averaging near zero aren't impossibly strict.
    ///
    /// `epoch_seconds` is the length of one epoch in ledger seconds; it
    /// defines the cycle-end cutoff that freezes an epoch's total.
    pub fn initialize(
        env: Env,
        admin: Address,
        registry: Address,
        tolerance_bps: u32,
        epoch_seconds: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Registry, &registry);
        env.storage().instance().set(&DataKey::ToleranceBps, &tolerance_bps);
        env.storage().instance().set(&DataKey::EpochSeconds, &epoch_seconds);
        env.storage().instance().set(&DataKey::EpochZero, &env.ledger().timestamp());
        Ok(())
    }

    /// `value` is a fixed-point reading (e.g. temperature * 100).
    /// `region_code` is a coarse geohash-style grid cell the caller
    /// computes off-chain from the device's registered lat/lon.
    /// Returns whether the reading was accepted as plausible.
    pub fn submit_reading(
        env: Env,
        device: Address,
        device_type: Symbol,
        region_code: u32,
        epoch: u32,
        value: i32,
    ) -> Result<bool, Error> {
        device.require_auth();

        let registry: Address = env
            .storage()
            .instance()
            .get(&DataKey::Registry)
            .ok_or(Error::NotInitialized)?;

        // Reject readings for an epoch whose window has already closed.
        // Its total submission count is final at that point, so letting a
        // late reading in would change what payouts are computed against.
        if env.ledger().timestamp() >= compute_epoch_end(&env, epoch) {
            return Err(Error::EpochClosed);
        }

        let registry_client = DeviceRegistryClient::new(&env, &registry);
        if !registry_client.is_active(&device) {
            return Err(Error::DeviceNotActive);
        }

        let tolerance_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ToleranceBps)
            .unwrap_or(2000);

        let agg_key = DataKey::RegionAgg(region_code, device_type.clone());
        let agg: Option<(i64, u32)> = env.storage().temporary().get(&agg_key);
        let (sum, count) = agg.unwrap_or((0, 0));

        let accepted = if count == 0 {
            true
        } else {
            let avg = sum / count as i64;
            let deviation = (value as i64 - avg).abs();
            let allowed = ((avg.abs() * tolerance_bps as i64) / 10_000).max(50);
            deviation <= allowed
        };

        if accepted {
            let new_sum = sum + value as i64;
            let new_count = count + 1;
            env.storage().temporary().set(&agg_key, &(new_sum, new_count));
            env.storage().temporary().extend_ttl(&agg_key, 120_960, 120_960);

            // Per-device count (unchanged).
            let count_key = DataKey::SubmissionCount(device.clone(), epoch);
            let current: u32 = env.storage().temporary().get(&count_key).unwrap_or(0);
            env.storage().temporary().set(&count_key, &(current + 1));
            env.storage().temporary().extend_ttl(&count_key, 120_960, 120_960);

            // Per-epoch total across all devices — the proportional-payout
            // denominator.
            let total_key = DataKey::TotalSubmissions(epoch);
            let total: u32 = env.storage().temporary().get(&total_key).unwrap_or(0);
            env.storage().temporary().set(&total_key, &(total + 1));
            env.storage().temporary().extend_ttl(&total_key, 120_960, 120_960);
        }

        Reading {
            device,
            region_code,
            epoch,
            value,
            accepted,
        }
        .publish(&env);

        Ok(accepted)
    }

    pub fn get_submission_count(env: Env, device: Address, epoch: u32) -> u32 {
        env.storage()
            .temporary()
            .get(&DataKey::SubmissionCount(device, epoch))
            .unwrap_or(0)
    }

    /// Total accepted readings across *all* devices in an epoch. Frozen
    /// once the epoch is final (no readings accepted after the window
    /// closes), so this is the stable denominator for proportional payouts.
    pub fn get_total_submissions(env: Env, epoch: u32) -> u32 {
        env.storage()
            .temporary()
            .get(&DataKey::TotalSubmissions(epoch))
            .unwrap_or(0)
    }

    /// True once `ledger.timestamp() >= epoch_end(epoch)` — the epoch's
    /// window has closed and its total submission count is final.
    pub fn is_epoch_final(env: Env, epoch: u32) -> bool {
        env.ledger().timestamp() >= compute_epoch_end(&env, epoch)
    }

    /// Exclusive upper bound of epoch `e`'s window, in ledger seconds:
    /// `epoch_zero + (e + 1) * epoch_seconds`.
    pub fn epoch_end(env: Env, epoch: u32) -> u64 {
        compute_epoch_end(&env, epoch)
    }

    /// Current rolling average for a region+device-type bucket, 0 if none yet.
    pub fn get_region_average(env: Env, region_code: u32, device_type: Symbol) -> i64 {
        let agg: Option<(i64, u32)> = env
            .storage()
            .temporary()
            .get(&DataKey::RegionAgg(region_code, device_type));
        match agg {
            Some((sum, count)) if count > 0 => sum / count as i64,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    const EPOCH_SECONDS: u64 = 100;

    fn setup(env: &Env) -> (SubmissionValidatorClient<'static>, Address) {
        let registry_id = env.register(device_registry::DeviceRegistry, ());
        let registry_client = device_registry::DeviceRegistryClient::new(env, &registry_id);
        let admin = Address::generate(env);
        registry_client.initialize(&admin);

        let validator_id = env.register(SubmissionValidator, ());
        let validator_client = SubmissionValidatorClient::new(env, &validator_id);
        validator_client.initialize(&admin, &registry_id, &2000, &EPOCH_SECONDS);

        (validator_client, registry_id)
    }

    /// Register a device and submit `readings` accepted readings (all 2500,
    /// well within tolerance) for `epoch` in region 1.
    fn register(
        env: &Env,
        validator: &SubmissionValidatorClient,
        registry_id: &Address,
        readings: u32,
        epoch: u32,
    ) -> Address {
        let device = Address::generate(env);
        let owner = Address::generate(env);
        let device_type = Symbol::new(env, "weather");
        let registry = device_registry::DeviceRegistryClient::new(env, registry_id);
        registry.register_device(&device, &owner, &device_type, &0, &0);
        for _ in 0..readings {
            validator.submit_reading(&device, &device_type, &1, &epoch, &2500);
        }
        device
    }

    /// Advance the ledger timestamp to the close of `epoch`.
    fn close_epoch(env: &Env, validator: &SubmissionValidatorClient, epoch: u32) {
        let end = validator.epoch_end(&epoch);
        env.ledger().with_mut(|li| li.timestamp = end);
    }

    #[test]
    fn rejects_unregistered_device() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, _registry) = setup(&env);

        let device = Address::generate(&env);
        let result = validator.try_submit_reading(
            &device,
            &Symbol::new(&env, "weather"),
            &1,
            &1,
            &2500,
        );
        assert_eq!(result, Err(Ok(Error::DeviceNotActive)));
    }

    #[test]
    fn accepts_plausible_readings_and_counts_them() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id) = setup(&env);
        let device = register(&env, &validator, &registry_id, 0, 7);
        let device_type = Symbol::new(&env, "weather");

        assert!(validator.submit_reading(&device, &device_type, &1, &7, &2500));
        assert!(validator.submit_reading(&device, &device_type, &1, &7, &2600));
        assert_eq!(validator.get_submission_count(&device, &7), 2);
        assert_eq!(validator.get_total_submissions(&7), 2);

        // Wildly out of line with the region average should be rejected.
        let accepted = validator.submit_reading(&device, &device_type, &1, &7, &999_999);
        assert!(!accepted);
        assert_eq!(validator.get_submission_count(&device, &7), 2);
        // A rejected reading must not inflate the epoch total either.
        assert_eq!(validator.get_total_submissions(&7), 2);
    }

    #[test]
    fn total_submissions_tracks_across_devices() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id) = setup(&env);
        let a = register(&env, &validator, &registry_id, 2, 1);
        let b = register(&env, &validator, &registry_id, 1, 1);

        assert_eq!(validator.get_submission_count(&a, &1), 2);
        assert_eq!(validator.get_submission_count(&b, &1), 1);
        // The epoch total is the sum across all devices, not just one.
        assert_eq!(validator.get_total_submissions(&1), 3);
    }

    #[test]
    fn epoch_is_not_final_until_window_closes() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, _registry_id) = setup(&env);

        assert!(!validator.is_epoch_final(&1));
        close_epoch(&env, &validator, 1);
        assert!(validator.is_epoch_final(&1));
    }

    #[test]
    fn rejects_readings_for_a_closed_epoch() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id) = setup(&env);
        let device = register(&env, &validator, &registry_id, 1, 1);
        let device_type = Symbol::new(&env, "weather");

        // One accepted reading before the epoch closes.
        assert_eq!(validator.get_total_submissions(&1), 1);

        close_epoch(&env, &validator, 1);

        // A late reading for the now-closed epoch is rejected, so the total
        // stays frozen at 1 — payouts can't be changed after the fact.
        let result = validator.try_submit_reading(&device, &device_type, &1, &1, &2500);
        assert_eq!(result, Err(Ok(Error::EpochClosed)));
        assert_eq!(validator.get_total_submissions(&1), 1);
    }
}
