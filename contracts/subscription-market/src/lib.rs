#![no_std]

//! Subscription Market
//!
//! Data consumers (apps, researchers) pay for access to the DriftMesh
//! feed here. Payment goes straight into `payment-streamer`'s own token
//! balance — this contract never custodies funds itself — closing the
//! loop: subscribers fund the pool, devices claim out of it.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env};

#[contracttype]
pub enum DataKey {
    Admin,
    PaymentToken,
    RewardPool,
    PricePerPeriod,
    PeriodSeconds,
    Expiry(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    InvalidPeriods = 3,
}

#[contract]
pub struct SubscriptionMarket;

#[contractimpl]
impl SubscriptionMarket {
    pub fn initialize(
        env: Env,
        admin: Address,
        payment_token: Address,
        reward_pool: Address,
        price_per_period: i128,
        period_seconds: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::PaymentToken, &payment_token);
        env.storage().instance().set(&DataKey::RewardPool, &reward_pool);
        env.storage().instance().set(&DataKey::PricePerPeriod, &price_per_period);
        env.storage().instance().set(&DataKey::PeriodSeconds, &period_seconds);
        Ok(())
    }

    /// Pay for `periods` of access. Extends any existing, still-active
    /// subscription rather than overwriting it. Returns the new expiry
    /// (ledger timestamp, seconds).
    pub fn subscribe(env: Env, subscriber: Address, periods: u32) -> Result<u64, Error> {
        subscriber.require_auth();
        if periods == 0 {
            return Err(Error::InvalidPeriods);
        }

        let price: i128 = env
            .storage()
            .instance()
            .get(&DataKey::PricePerPeriod)
            .ok_or(Error::NotInitialized)?;
        let period_seconds: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PeriodSeconds)
            .ok_or(Error::NotInitialized)?;
        let payment_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentToken)
            .ok_or(Error::NotInitialized)?;
        let reward_pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::RewardPool)
            .ok_or(Error::NotInitialized)?;

        let amount = price * periods as i128;
        let token_client = token::Client::new(&env, &payment_token);
        token_client.transfer(&subscriber, &reward_pool, &amount);

        let now = env.ledger().timestamp();
        let expiry_key = DataKey::Expiry(subscriber.clone());
        let current_expiry: u64 = env.storage().persistent().get(&expiry_key).unwrap_or(0);
        let base = if current_expiry > now { current_expiry } else { now };
        let new_expiry = base + period_seconds * periods as u64;

        env.storage().persistent().set(&expiry_key, &new_expiry);
        env.events()
            .publish((symbol_short!("subscribe"), subscriber), (periods, new_expiry));

        Ok(new_expiry)
    }

    pub fn is_active(env: Env, subscriber: Address) -> bool {
        let expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::Expiry(subscriber))
            .unwrap_or(0);
        expiry > env.ledger().timestamp()
    }

    pub fn get_expiry(env: Env, subscriber: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::Expiry(subscriber))
            .unwrap_or(0)
    }

    pub fn set_price(env: Env, admin: Address, price: i128) -> Result<(), Error> {
        admin.require_auth();
        let stored_admin: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if stored_admin.as_ref() != Some(&admin) {
            return Err(Error::NotInitialized);
        }
        env.storage().instance().set(&DataKey::PricePerPeriod, &price);
        Ok(())
    }
}
