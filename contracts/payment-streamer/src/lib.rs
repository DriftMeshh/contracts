#![no_std]

//! Payment Streamer
//!
//! Pays devices in `reward_token` (a stablecoin SAC) a *proportional share*
//! of an epoch's pool, rather than a flat per-reading rate. Each device's
//! payout for an epoch is:
//!
//! ```text
//! payout = epoch_pool * device_count / total_count
//! ```
//!
//! where `total_count` is the number of valid readings counted across
//! *all* devices in that epoch (tracked by `submission-validator`) and
//! `epoch_pool` is a one-time snapshot of this contract's token balance,
//! taken the first time anyone claims for that epoch after it closes.
//!
//! Because every claimant for an epoch is paid against the same fixed
//! `(epoch_pool, total_count)` pair, payout depends only on a device's
//! own submission count — never on claim order. The old model paid
//! `count * reward_rate` capped by the live balance, so the pool could
//! drain and late claimants got a partial payout while early claimants got
//! the full flat rate; the proportional model removes that unfairness.
//!
//! An epoch's total is only final once its time window has closed — see
//! `submission-validator::is_epoch_final` (Wave-style cycle-end
//! timeboxing). `claim` rejects any epoch that isn't final yet, so payouts
//! are never computed against a total that later readings could still
//! change. The contract never custodies funds through a separate "fund"
//! call — `subscription-market` pays straight into this contract's own
//! token balance, so the pool is simply whatever balance is here.

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, token, Address, Env,
};

/// Calling interface for `submission-validator` — see the matching note in
/// that crate for why this isn't a direct dependency on its implementation.
#[contractclient(name = "SubmissionValidatorClient")]
pub trait SubmissionValidatorInterface {
    fn get_submission_count(env: Env, device: Address, epoch: u32) -> u32;
    /// Total accepted readings across all devices in an epoch.
    fn get_total_submissions(env: Env, epoch: u32) -> u32;
    /// True once the epoch's window has closed and its total is final.
    fn is_epoch_final(env: Env, epoch: u32) -> bool;
}

#[contracttype]
pub enum DataKey {
    Admin,
    Validator,
    RewardToken,
    Claimed(Address, u32),
    /// One-time snapshot of this contract's token balance, taken the first
    /// time anyone claims for a finalized epoch. All claimants for that
    /// epoch are then paid against this fixed pool, so payout is
    /// independent of claim order.
    EpochPoolSnapshot(u32),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    AlreadyClaimed = 3,
    NothingToClaim = 4,
    /// The epoch's window hasn't closed yet, so its total submission count
    /// isn't final and payouts can't be computed fairly. Claim again after
    /// the epoch ends.
    EpochNotFinal = 5,
}

/// Emitted when a device successfully claims its payout for an epoch.
/// Topics: ["claimed", device_address]
/// Data: (epoch, payout)
#[contractevent]
#[derive(Clone)]
pub struct Claimed {
    #[topic]
    pub device: Address,
    pub epoch: u32,
    pub payout: i128,
}

#[contract]
pub struct PaymentStreamer;

#[contractimpl]
impl PaymentStreamer {
    pub fn initialize(
        env: Env,
        admin: Address,
        validator: Address,
        reward_token: Address,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Validator, &validator);
        env.storage().instance().set(&DataKey::RewardToken, &reward_token);
        Ok(())
    }

    /// A device claims its proportional share of an epoch's pool. The epoch
    /// must be final (its total is frozen), and each device can claim once
    /// per epoch. The first claim for a finalized epoch snapshots the pool;
    /// `payout = epoch_pool * device_count / total`, with the live balance
    /// used only as a defensive cap that never triggers in normal flow.
    pub fn claim(env: Env, device: Address, epoch: u32) -> Result<i128, Error> {
        device.require_auth();

        let claimed_key = DataKey::Claimed(device.clone(), epoch);
        if env.storage().temporary().has(&claimed_key) {
            return Err(Error::AlreadyClaimed);
        }

        let validator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Validator)
            .ok_or(Error::NotInitialized)?;
        let validator_client = SubmissionValidatorClient::new(&env, &validator);

        // The epoch must be closed before its total submission count is
        // final — otherwise early claimants would be paid against a total
        // that later readings could still inflate.
        if !validator_client.is_epoch_final(&epoch) {
            return Err(Error::EpochNotFinal);
        }

        let device_count = validator_client.get_submission_count(&device, &epoch);
        if device_count == 0 {
            return Err(Error::NothingToClaim);
        }
        let total = validator_client.get_total_submissions(&epoch);
        // Defensive: device_count > 0 implies total > 0, but guard the
        // division regardless.
        if total == 0 {
            return Err(Error::NothingToClaim);
        }

        let reward_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::RewardToken)
            .ok_or(Error::NotInitialized)?;
        let token_client = token::Client::new(&env, &reward_token);

        // Snapshot the pool the first time anyone claims for a finalized
        // epoch. From then on every claimant is paid against this same
        // snapshot, so payout is a pure function of `device_count` — not
        // of claim order.
        let snapshot_key = DataKey::EpochPoolSnapshot(epoch);
        let pool: i128 = match env.storage().instance().get(&snapshot_key) {
            Some(p) => p,
            None => {
                let p = token_client.balance(&env.current_contract_address());
                env.storage().instance().set(&snapshot_key, &p);
                p
            }
        };

        let share = pool * (device_count as i128) / (total as i128);
        let available = token_client.balance(&env.current_contract_address());
        // Cap by the live balance as a safety net only. In normal flow the
        // sum of all proportional shares never exceeds the snapshot (each is
        // a floor of its fraction of the pool), so this never trims a
        // rightful payout — it just prevents a revert if the balance were
        // somehow short.
        let payout = if share > available { available } else { share };

        if payout <= 0 {
            return Err(Error::NothingToClaim);
        }

        token_client.transfer(&env.current_contract_address(), &device, &payout);
        env.storage().temporary().set(&claimed_key, &true);
        env.storage().temporary().extend_ttl(&claimed_key, 120_960, 120_960);

        Claimed {
            device,
            epoch,
            payout,
        }
        .publish(&env);

        Ok(payout)
    }

    /// How much reward token this contract currently holds — i.e. what's
    /// available to be paid out (across all not-yet-claimed epochs).
    pub fn pool_balance(env: Env) -> i128 {
        let reward_token: Option<Address> = env.storage().instance().get(&DataKey::RewardToken);
        match reward_token {
            Some(token_addr) => {
                let token_client = token::Client::new(&env, &token_addr);
                token_client.balance(&env.current_contract_address())
            }
            None => 0,
        }
    }

    /// The one-time pool snapshot for an epoch (0 if no one has claimed for
    /// it yet). Useful for inspecting what a finalized epoch will split.
    pub fn epoch_pool(env: Env, epoch: u32) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::EpochPoolSnapshot(epoch))
            .unwrap_or(0)
    }

    pub fn has_claimed(env: Env, device: Address, epoch: u32) -> bool {
        env.storage().temporary().has(&DataKey::Claimed(device, epoch))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    const EPOCH_SECONDS: u64 = 100;
    const EPOCH: u32 = 1;
    const REGION: u32 = 42;

    fn create_token<'a>(
        env: &Env,
        admin: &Address,
    ) -> (Address, token::StellarAssetClient<'a>, token::Client<'a>) {
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let address = sac.address();
        (
            address.clone(),
            token::StellarAssetClient::new(env, &address),
            token::Client::new(env, &address),
        )
    }

    /// Wires up device-registry + submission-validator (with epoch
    /// timeboxing) + payment-streamer and registers one device that has
    /// submitted `readings` valid readings for `EPOCH`.
    fn wire(
        env: &Env,
    ) -> (
        submission_validator::SubmissionValidatorClient<'static>,
        Address, // registry_id
        PaymentStreamerClient<'static>,
        Address, // streamer_id (to mint the pool into)
        token::StellarAssetClient<'static>,
        token::Client<'static>,
    ) {
        let admin = Address::generate(env);

        let registry_id = env.register(device_registry::DeviceRegistry, ());
        let registry_client = device_registry::DeviceRegistryClient::new(env, &registry_id);
        registry_client.initialize(&admin);

        let validator_id = env.register(submission_validator::SubmissionValidator, ());
        let validator_client = submission_validator::SubmissionValidatorClient::new(env, &validator_id);
        validator_client.initialize(&admin, &registry_id, &2000, &EPOCH_SECONDS);

        let (token_addr, token_admin_client, token_client) = create_token(env, &admin);

        let streamer_id = env.register(PaymentStreamer, ());
        let streamer_client = PaymentStreamerClient::new(env, &streamer_id);
        streamer_client.initialize(&admin, &validator_id, &token_addr);

        (
            validator_client,
            registry_id,
            streamer_client,
            streamer_id,
            token_admin_client,
            token_client,
        )
    }

    /// Register a device and submit `readings` accepted readings (all 2500,
    /// within tolerance) for `EPOCH` in `REGION`.
    fn register_device(
        env: &Env,
        validator: &submission_validator::SubmissionValidatorClient,
        registry_id: &Address,
        readings: u32,
    ) -> Address {
        let device = Address::generate(env);
        let owner = Address::generate(env);
        let device_type = Symbol::new(env, "weather");
        let registry = device_registry::DeviceRegistryClient::new(env, registry_id);
        registry.register_device(&device, &owner, &device_type, &0, &0);
        for _ in 0..readings {
            validator.submit_reading(&device, &device_type, &REGION, &EPOCH, &2500);
        }
        device
    }

    /// Advance the ledger timestamp to the close of `EPOCH`, finalizing it.
    fn finalize_epoch(env: &Env, validator: &submission_validator::SubmissionValidatorClient) {
        let end = validator.epoch_end(&EPOCH);
        env.ledger().with_mut(|li| li.timestamp = end);
    }

    #[test]
    fn single_device_claims_whole_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, token) = wire(&env);
        let device = register_device(&env, &validator, &registry_id, 3);

        let pool: i128 = 1_000;
        token_admin.mint(&streamer_id, &pool);
        assert_eq!(streamer.pool_balance(), pool);

        // Before the epoch closes its total isn't final — claiming is
        // rejected so payouts can't be computed against a moving total.
        let r = streamer.try_claim(&device, &EPOCH);
        assert_eq!(r, Err(Ok(Error::EpochNotFinal)));

        finalize_epoch(&env, &validator);
        assert!(validator.is_epoch_final(&EPOCH));
        assert_eq!(validator.get_total_submissions(&EPOCH), 3);

        // One device holds all 3 readings → its share is 3/3 = the whole pool.
        let payout = streamer.claim(&device, &EPOCH);
        assert_eq!(payout, pool);
        assert_eq!(streamer.epoch_pool(&EPOCH), pool);
        assert_eq!(token.balance(&device), pool);
        assert_eq!(streamer.pool_balance(), 0);
    }

    #[test]
    fn payouts_split_proportionally() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, token) = wire(&env);
        let a = register_device(&env, &validator, &registry_id, 2);
        let b = register_device(&env, &validator, &registry_id, 1);

        let pool: i128 = 300;
        token_admin.mint(&streamer_id, &pool);
        finalize_epoch(&env, &validator);
        assert_eq!(validator.get_total_submissions(&EPOCH), 3);

        // A has 2/3 of the submissions, B has 1/3.
        assert_eq!(streamer.claim(&a, &EPOCH), 200);
        assert_eq!(streamer.claim(&b, &EPOCH), 100);
        assert_eq!(token.balance(&a), 200);
        assert_eq!(token.balance(&b), 100);
        assert_eq!(streamer.pool_balance(), 0);
    }

    #[test]
    fn payouts_independent_of_claim_order() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, token) = wire(&env);
        let a = register_device(&env, &validator, &registry_id, 2);
        let b = register_device(&env, &validator, &registry_id, 1);

        token_admin.mint(&streamer_id, &300);
        finalize_epoch(&env, &validator);

        // Claiming in the reverse order yields identical per-device
        // payouts — proof that payout doesn't depend on claim order.
        assert_eq!(streamer.claim(&b, &EPOCH), 100);
        assert_eq!(streamer.claim(&a, &EPOCH), 200);
        assert_eq!(token.balance(&a), 200);
        assert_eq!(token.balance(&b), 100);
    }

    #[test]
    fn double_claim_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, _token) = wire(&env);
        let device = register_device(&env, &validator, &registry_id, 2);

        token_admin.mint(&streamer_id, &1_000);
        finalize_epoch(&env, &validator);

        streamer.claim(&device, &EPOCH);
        let r = streamer.try_claim(&device, &EPOCH);
        assert_eq!(r, Err(Ok(Error::AlreadyClaimed)));
    }

    #[test]
    fn claim_with_no_submissions_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, _token) = wire(&env);
        // Register the device but submit no readings.
        let device = register_device(&env, &validator, &registry_id, 0);

        token_admin.mint(&streamer_id, &1_000);
        finalize_epoch(&env, &validator);

        let r = streamer.try_claim(&device, &EPOCH);
        assert_eq!(r, Err(Ok(Error::NothingToClaim)));
    }

    #[test]
    fn claim_before_epoch_final_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (validator, registry_id, streamer, streamer_id, token_admin, _token) = wire(&env);
        let device = register_device(&env, &validator, &registry_id, 2);

        token_admin.mint(&streamer_id, &1_000);
        // Do NOT finalize the epoch.
        assert!(!validator.is_epoch_final(&EPOCH));

        let r = streamer.try_claim(&device, &EPOCH);
        assert_eq!(r, Err(Ok(Error::EpochNotFinal)));
    }
}
