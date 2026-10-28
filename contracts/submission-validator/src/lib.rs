#![no_std]

//! Submission Validator
//!
//! Accepts signed sensor readings, checks the submitting device is
//! registered and active (cross-contract call into `device-registry`),
//! and runs a lightweight outlier check against a rolling regional
//! average before counting the reading toward that device's payable
//! submissions for the epoch.
//!
//! The outlier check is deliberately simple for v1 — it is NOT a
//! consensus mechanism. It stops a single device's lie from being wildly
//! out of line with its neighbours, nothing more. Hardening this
//! (stake-weighted scoring, slashing, multi-device consensus) is
//! explicitly left as follow-up Wave issues — see README.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Symbol,
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
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    DeviceNotActive = 3,
}

#[contract]
pub struct SubmissionValidator;

#[contractimpl]
impl SubmissionValidator {
    /// `tolerance_bps` is how far (in basis points of the current regional
    /// average) a new reading may deviate and still be accepted, e.g.
    /// 2000 = 20%. A minimum absolute floor of 50 (fixed-point units)
    /// applies too, so regions averaging near zero aren't impossibly strict.
    pub fn initialize(env: Env, admin: Address, registry: Address, tolerance_bps: u32) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Registry, &registry);
        env.storage().instance().set(&DataKey::ToleranceBps, &tolerance_bps);
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

        let registry: Option<Address> = env.storage().instance().get(&DataKey::Registry);
        let registry = registry.ok_or(Error::NotInitialized)?;
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

            let count_key = DataKey::SubmissionCount(device.clone(), epoch);
            let current: u32 = env.storage().temporary().get(&count_key).unwrap_or(0);
            env.storage().temporary().set(&count_key, &(current + 1));
            env.storage().temporary().extend_ttl(&count_key, 120_960, 120_960);
        }

        env.events().publish(
            (symbol_short!("reading"), device),
            (region_code, epoch, value, accepted),
        );

        Ok(accepted)
    }

    pub fn get_submission_count(env: Env, device: Address, epoch: u32) -> u32 {
        env.storage()
            .temporary()
            .get(&DataKey::SubmissionCount(device, epoch))
            .unwrap_or(0)
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

    fn setup(env: &Env) -> (SubmissionValidatorClient<'static>, Address) {
        let registry_id = env.register(device_registry::DeviceRegistry, ());
        let registry_client = device_registry::DeviceRegistryClient::new(env, &registry_id);
        let admin = Address::generate(env);
        registry_client.initialize(&admin);

        let validator_id = env.register(SubmissionValidator, ());
        let validator_client = SubmissionValidatorClient::new(env, &validator_id);
        validator_client.initialize(&admin, &registry_id, &2000);

        (validator_client, registry_id)
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
        let registry_client = device_registry::DeviceRegistryClient::new(&env, &registry_id);

        let device = Address::generate(&env);
        let owner = Address::generate(&env);
        let device_type = Symbol::new(&env, "weather");
        registry_client.register_device(&device, &owner, &device_type, &0, &0);

        assert!(validator.submit_reading(&device, &device_type, &1, &7, &2500));
        assert!(validator.submit_reading(&device, &device_type, &1, &7, &2600));
        assert_eq!(validator.get_submission_count(&device, &7), 2);

        // Wildly out of line with the region average should be rejected.
        let accepted = validator.submit_reading(&device, &device_type, &1, &7, &999_999);
        assert!(!accepted);
        assert_eq!(validator.get_submission_count(&device, &7), 2);
    }
}
