#![no_std]

//! Payment Streamer
//!
//! Pays devices in `reward_token` (a stablecoin SAC) proportional to the
//! number of valid readings they had counted by `submission-validator`
//! in a given epoch, at a fixed per-reading rate. The contract never
//! custodies funds through a separate "fund" call — `subscription-market`
//! pays straight into this contract's own token balance, so payouts are
//! simply capped by whatever balance is actually here.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

/// Calling interface for `submission-validator` — see the matching note in
/// that crate for why this isn't a direct dependency on its implementation.
#[contractclient(name = "SubmissionValidatorClient")]
pub trait SubmissionValidatorInterface {
    fn get_submission_count(env: Env, device: Address, epoch: u32) -> u32;
}

#[contracttype]
pub enum DataKey {
    Admin,
    Validator,
    RewardToken,
    RewardRate,
    Claimed(Address, u32),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    AlreadyClaimed = 3,
    NothingToClaim = 4,
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
        reward_rate: i128,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Validator, &validator);
        env.storage().instance().set(&DataKey::RewardToken, &reward_token);
        env.storage().instance().set(&DataKey::RewardRate, &reward_rate);
        Ok(())
    }

    pub fn set_reward_rate(env: Env, admin: Address, rate: i128) -> Result<(), Error> {
        admin.require_auth();
        let stored_admin: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if stored_admin.as_ref() != Some(&admin) {
            return Err(Error::NotInitialized);
        }
        env.storage().instance().set(&DataKey::RewardRate, &rate);
        Ok(())
    }

    /// A device claims its payout for an epoch once. Amount is
    /// `submission_count * reward_rate`, capped by whatever the pool
    /// actually holds so one epoch's claims can never drain more than
    /// subscribers have paid in.
    pub fn claim(env: Env, device: Address, epoch: u32) -> Result<i128, Error> {
        device.require_auth();

        let claimed_key = DataKey::Claimed(device.clone(), epoch);
        if env.storage().temporary().has(&claimed_key) {
            return Err(Error::AlreadyClaimed);
        }

        let validator: Option<Address> = env.storage().instance().get(&DataKey::Validator);
        let validator = validator.ok_or(Error::NotInitialized)?;
        let validator_client = SubmissionValidatorClient::new(&env, &validator);
        let count = validator_client.get_submission_count(&device, &epoch);
        if count == 0 {
            return Err(Error::NothingToClaim);
        }

        let rate: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardRate)
            .ok_or(Error::NotInitialized)?;
        let owed = (count as i128) * rate;

        let reward_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::RewardToken)
            .ok_or(Error::NotInitialized)?;
        let token_client = token::Client::new(&env, &reward_token);
        let available = token_client.balance(&env.current_contract_address());
        let payout = if owed > available { available } else { owed };

        if payout <= 0 {
            return Err(Error::NothingToClaim);
        }

        token_client.transfer(&env.current_contract_address(), &device, &payout);
        env.storage().temporary().set(&claimed_key, &true);
        env.storage().temporary().extend_ttl(&claimed_key, 120_960, 120_960);

        env.events()
            .publish((symbol_short!("claimed"), device), (epoch, payout));

        Ok(payout)
    }

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

    pub fn has_claimed(env: Env, device: Address, epoch: u32) -> bool {
        env.storage().temporary().has(&DataKey::Claimed(device, epoch))
    }
}
