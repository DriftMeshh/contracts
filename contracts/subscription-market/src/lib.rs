#![no_std]

//! Subscription Market
//!
//! Data consumers (apps, researchers) pay for access to the DriftMesh
//! feed here. Payment goes straight into `payment-streamer`'s own token
//! balance — this contract never custodies funds itself — closing the
//! loop: subscribers fund the pool, devices claim out of it.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, BytesN, ContractExecutable,
    Env,
};

#[contracttype]
pub enum DataKey {
    Admin,
    PaymentToken,
    RewardPool,
    PricePerPeriod,
    PeriodSeconds,
    Expiry(Address),
    FeeBps,
    FeeRecipient,
}

/// Upper bound on the protocol fee, enforced in both `initialize` and
/// `set_fee`. Capped well below 100% so a misconfiguration can't zero out
/// device payouts — the whole point of this contract is funding the device
/// reward pool, not the protocol treasury.
const MAX_FEE_BPS: u32 = 5_000; // 50%

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    InvalidPeriods = 3,
    /// set_price was called with price <= 0. A zero-or-negative price would
    /// let subscribers fund nothing into the pool, breaking the economic loop.
    InvalidPrice = 4,
    /// initialize or set_fee was called with fee_bps above MAX_FEE_BPS.
    InvalidFeeBps = 5,
}

/// Emitted when a subscriber purchases or extends their subscription.
/// Topics: ["subscribe", subscriber_address]
/// Data: (periods, new_expiry, fee_amount)
#[contractevent]
#[derive(Clone)]
pub struct Subscribe {
    #[topic]
    pub subscriber: Address,
    pub periods: u32,
    pub new_expiry: u64,
    /// The protocol fee taken out of this payment, in payment_token's
    /// smallest unit. 0 when no fee is configured.
    pub fee_amount: i128,
}

/// Emitted when the admin updates the subscription price.
/// Topics: ["price_updated"]
/// Data: (old_price, new_price)
#[contractevent]
#[derive(Clone)]
pub struct PriceUpdated {
    pub old_price: i128,
    pub new_price: i128,
}

/// Emitted when the admin updates the protocol fee configuration.
/// Topics: ["fee_updated"]
/// Data: (old_fee_bps, new_fee_bps, fee_recipient)
#[contractevent]
#[derive(Clone)]
pub struct FeeUpdated {
    pub old_fee_bps: u32,
    pub new_fee_bps: u32,
    pub fee_recipient: Address,
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
        fee_bps: u32,
        fee_recipient: Address,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        if fee_bps > MAX_FEE_BPS {
            return Err(Error::InvalidFeeBps);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::PaymentToken, &payment_token);
        env.storage().instance().set(&DataKey::RewardPool, &reward_pool);
        env.storage().instance().set(&DataKey::PricePerPeriod, &price_per_period);
        env.storage().instance().set(&DataKey::PeriodSeconds, &period_seconds);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::FeeRecipient, &fee_recipient);
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
        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::FeeBps)
            .ok_or(Error::NotInitialized)?;
        let fee_recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeRecipient)
            .ok_or(Error::NotInitialized)?;

        let amount = price * periods as i128;
        // Integer division floors the fee, and the pool gets the exact
        // remainder (not a separately-computed share), so the two
        // transfers always sum to exactly `amount` with no rounding leak.
        let fee_amount = (amount * fee_bps as i128) / 10_000;
        let pool_amount = amount - fee_amount;

        let token_client = token::Client::new(&env, &payment_token);
        if fee_amount > 0 {
            token_client.transfer(&subscriber, &fee_recipient, &fee_amount);
        }
        token_client.transfer(&subscriber, &reward_pool, &pool_amount);

        let now = env.ledger().timestamp();
        let expiry_key = DataKey::Expiry(subscriber.clone());
        let current_expiry: u64 = env.storage().persistent().get(&expiry_key).unwrap_or(0);
        let base = if current_expiry > now { current_expiry } else { now };
        let new_expiry = base + period_seconds * periods as u64;

        env.storage().persistent().set(&expiry_key, &new_expiry);
        // Extend the TTL so an active subscriber's expiry record is never
        // archived before the subscription itself expires. We use ~1 year of
        // ledger slots (6_307_200) as the target, which comfortably covers
        // the maximum sensible subscription length.
        env.storage().persistent().extend_ttl(&expiry_key, 6_307_200, 6_307_200);
        Subscribe {
            subscriber,
            periods,
            new_expiry,
            fee_amount,
        }
        .publish(&env);

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
        if price <= 0 {
            return Err(Error::InvalidPrice);
        }
        let old_price: i128 = env
            .storage()
            .instance()
            .get(&DataKey::PricePerPeriod)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::PricePerPeriod, &price);
        PriceUpdated { old_price, new_price: price }.publish(&env);
        Ok(())
    }

    /// Returns the current price per subscription period.
    /// Returns 0 if the contract has not been initialized.
    pub fn get_price(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::PricePerPeriod)
            .unwrap_or(0)
    }

    /// Updates the protocol fee (basis points, 0-MAX_FEE_BPS) and the
    /// address that receives it. Admin-gated, same pattern as `set_price`.
    pub fn set_fee(
        env: Env,
        admin: Address,
        fee_bps: u32,
        fee_recipient: Address,
    ) -> Result<(), Error> {
        admin.require_auth();
        let stored_admin: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if stored_admin.as_ref() != Some(&admin) {
            return Err(Error::NotInitialized);
        }
        if fee_bps > MAX_FEE_BPS {
            return Err(Error::InvalidFeeBps);
        }
        let old_fee_bps: u32 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &fee_recipient);
        FeeUpdated {
            old_fee_bps,
            new_fee_bps: fee_bps,
            fee_recipient,
        }
        .publish(&env);
        Ok(())
    }

    /// Returns the current protocol fee in basis points.
    /// Returns 0 if the contract has not been initialized.
    pub fn get_fee_bps(env: Env) -> u32 {
        env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0)
    }

    /// Returns the address currently configured to receive the protocol
    /// fee. Returns None if the contract has not been initialized.
    pub fn get_fee_recipient(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::FeeRecipient)
    }

    /// Returns the length of one subscription period, in seconds, as set at
    /// `initialize`. Returns 0 if the contract has not been initialized.
    pub fn get_period_seconds(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PeriodSeconds)
            .unwrap_or(0)
    }

    /// Replaces this contract's executable with the Wasm at `new_wasm_hash`
    /// (already uploaded via `env.deployer().upload_contract_wasm`),
    /// admin-gated the same way `set_price` already is. Storage layout
    /// must stay backward-compatible across an upgrade. The new executable
    /// only takes effect after this invocation finishes.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        admin.require_auth();
        let stored_admin: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if stored_admin.as_ref() != Some(&admin) {
            return Err(Error::NotInitialized);
        }
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash));
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    fn create_token<'a>(env: &Env, admin: &Address) -> (Address, token::StellarAssetClient<'a>, token::Client<'a>) {
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let address = sac.address();
        (
            address.clone(),
            token::StellarAssetClient::new(env, &address),
            token::Client::new(env, &address),
        )
    }

    const PRICE: i128 = 200;
    const PERIOD_SECONDS: u64 = 86_400; // 1 day, for a readable test

    fn setup(env: &Env) -> (SubscriptionMarketClient<'static>, Address, token::Client<'static>, Address) {
        let admin = Address::generate(env);
        let (token_addr, token_admin_client, token_client) = create_token(env, &admin);
        let reward_pool = Address::generate(env); // stand-in for payment-streamer's address
        let fee_recipient = Address::generate(env);

        let market_id = env.register(SubscriptionMarket, ());
        let market_client = SubscriptionMarketClient::new(env, &market_id);
        // fee_bps=0 here so every pre-existing test below keeps seeing the
        // full payment land in reward_pool; the fee-specific tests set up
        // their own non-zero fee explicitly.
        market_client.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &0,
            &fee_recipient,
        );

        let subscriber = Address::generate(env);
        token_admin_client.mint(&subscriber, &10_000);

        (market_client, subscriber, token_client, reward_pool)
    }

    #[test]
    fn subscribe_transfers_payment_straight_to_reward_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, subscriber, token_client, reward_pool) = setup(&env);

        market.subscribe(&subscriber, &3);

        assert_eq!(token_client.balance(&reward_pool), PRICE * 3);
        assert_eq!(token_client.balance(&subscriber), 10_000 - PRICE * 3);
        assert!(market.is_active(&subscriber));
    }

    #[test]
    fn expiry_extends_on_top_of_an_active_subscription() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, subscriber, _token_client, _reward_pool) = setup(&env);

        let first_expiry = market.subscribe(&subscriber, &1);
        let second_expiry = market.subscribe(&subscriber, &1);

        // Still within the first period, so the second purchase should
        // extend from the first expiry, not from "now" — i.e. the two
        // periods stack rather than overlap.
        assert_eq!(second_expiry, first_expiry + PERIOD_SECONDS);
    }

    #[test]
    fn inactive_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, subscriber, _token_client, _reward_pool) = setup(&env);

        market.subscribe(&subscriber, &1);
        assert!(market.is_active(&subscriber));

        env.ledger().with_mut(|li| li.timestamp += PERIOD_SECONDS + 1);
        assert!(!market.is_active(&subscriber));
    }

    #[test]
    fn zero_periods_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, subscriber, _token_client, _reward_pool) = setup(&env);

        let result = market.try_subscribe(&subscriber, &0);
        assert_eq!(result, Err(Ok(Error::InvalidPeriods)));
    }

    #[test]
    fn get_price_returns_current_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, _subscriber, _token_client, _reward_pool) = setup(&env);

        // setup() initializes with PRICE=200.
        assert_eq!(market.get_price(), PRICE);
    }

    #[test]
    fn get_period_seconds_returns_the_initialized_value() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, _subscriber, _token_client, _reward_pool) = setup(&env);

        assert_eq!(market.get_period_seconds(), PERIOD_SECONDS);
    }

    #[test]
    fn non_admin_cannot_upgrade() {
        // Covers the auth-gating half only — see device-registry's
        // non_admin_cannot_upgrade test for why a real successful-upgrade
        // test isn't something a unit test here can produce.
        let env = Env::default();
        env.mock_all_auths();
        let (market, _subscriber, _token_client, _reward_pool) = setup(&env);

        let stranger = Address::generate(&env);
        let fake_wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
        let result = market.try_upgrade(&stranger, &fake_wasm_hash);
        assert_eq!(result, Err(Ok(Error::NotInitialized)));
    }

    #[test]
    fn subscribe_emits_the_exact_subscribe_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::Event as _;

        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let (token_addr, token_admin, _token_client) = create_token(&env, &admin);
        let reward_pool = Address::generate(&env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        let fee_recipient = Address::generate(&env);
        market.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &0,
            &fee_recipient,
        );

        let subscriber = Address::generate(&env);
        token_admin.mint(&subscriber, &10_000);
        let new_expiry = market.subscribe(&subscriber, &2);

        let expected = Subscribe {
            subscriber: subscriber.clone(),
            periods: 2,
            new_expiry,
            fee_amount: 0,
        }
        .to_xdr(&env, &market_id);
        assert_eq!(env.events().all().events().last(), Some(&expected));
    }

    #[test]
    fn get_expiry_for_a_never_subscribed_address_is_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, _subscriber, _token_client, _reward_pool) = setup(&env);

        let never_subscribed = Address::generate(&env);
        assert_eq!(market.get_expiry(&never_subscribed), 0);
        // Consistent with is_active treating expiry 0 as not active.
        assert!(!market.is_active(&never_subscribed));
    }

    #[test]
    fn subscribe_splits_payment_between_fee_recipient_and_reward_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token_addr, token_admin_client, token_client) = create_token(&env, &admin);
        let reward_pool = Address::generate(&env);
        let fee_recipient = Address::generate(&env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        // 1000 bps = 10%.
        market.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &1_000,
            &fee_recipient,
        );

        let subscriber = Address::generate(&env);
        token_admin_client.mint(&subscriber, &10_000);
        market.subscribe(&subscriber, &3);

        let total = PRICE * 3;
        let expected_fee = total * 1_000 / 10_000;
        assert_eq!(token_client.balance(&fee_recipient), expected_fee);
        assert_eq!(token_client.balance(&reward_pool), total - expected_fee);
        // The two transfers must sum to exactly what the subscriber paid —
        // no rounding leak, no double-counting.
        assert_eq!(token_client.balance(&subscriber), 10_000 - total);
    }

    #[test]
    fn fee_amount_floors_and_the_pool_gets_the_exact_remainder() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token_addr, token_admin_client, token_client) = create_token(&env, &admin);
        let reward_pool = Address::generate(&env);
        let fee_recipient = Address::generate(&env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        // price=101, 1 period, 250 bps (2.5%): 101 * 250 / 10_000 = 2 (floored
        // from 2.525), so the pool must get the remaining 99 — not 98 or 99.5.
        market.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &101,
            &PERIOD_SECONDS,
            &250,
            &fee_recipient,
        );

        let subscriber = Address::generate(&env);
        token_admin_client.mint(&subscriber, &1_000);
        market.subscribe(&subscriber, &1);

        assert_eq!(token_client.balance(&fee_recipient), 2);
        assert_eq!(token_client.balance(&reward_pool), 99);
    }

    #[test]
    fn zero_fee_skips_the_fee_transfer_entirely() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, subscriber, token_client, reward_pool) = setup(&env);

        // setup() already initializes with fee_bps=0; confirm the fee
        // recipient's balance stays untouched (never minted, never
        // transferred to) and the full amount lands in reward_pool.
        market.subscribe(&subscriber, &1);

        assert_eq!(token_client.balance(&reward_pool), PRICE);
    }

    #[test]
    fn initialize_rejects_fee_above_the_cap() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token_addr, _token_admin_client, _token_client) = create_token(&env, &admin);
        let reward_pool = Address::generate(&env);
        let fee_recipient = Address::generate(&env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        let result = market.try_initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &(MAX_FEE_BPS + 1),
            &fee_recipient,
        );
        assert_eq!(result, Err(Ok(Error::InvalidFeeBps)));
    }

    /// Builds a market plus its admin address, for the set_fee tests that
    /// need to act as the admin — `setup()` doesn't expose the admin it
    /// generates internally, so these build their own fresh instance.
    fn setup_with_admin(env: &Env) -> (SubscriptionMarketClient<'static>, Address) {
        let admin = Address::generate(env);
        let (token_addr, _token_admin_client, _token_client) = create_token(env, &admin);
        let reward_pool = Address::generate(env);
        let fee_recipient = Address::generate(env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(env, &market_id);
        market.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &0,
            &fee_recipient,
        );
        (market, admin)
    }

    #[test]
    fn set_fee_updates_the_fee_and_emits_the_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, admin) = setup_with_admin(&env);

        let new_recipient = Address::generate(&env);
        market.set_fee(&admin, &1_500, &new_recipient);

        assert_eq!(market.get_fee_bps(), 1_500);
        assert_eq!(market.get_fee_recipient(), Some(new_recipient));
    }

    #[test]
    fn set_fee_rejects_values_above_the_cap() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, admin) = setup_with_admin(&env);

        let new_recipient = Address::generate(&env);
        let result = market.try_set_fee(&admin, &(MAX_FEE_BPS + 1), &new_recipient);
        assert_eq!(result, Err(Ok(Error::InvalidFeeBps)));
    }

    #[test]
    fn non_admin_cannot_set_fee() {
        let env = Env::default();
        env.mock_all_auths();
        let (market, _admin) = setup_with_admin(&env);

        let stranger = Address::generate(&env);
        let new_recipient = Address::generate(&env);
        let result = market.try_set_fee(&stranger, &1_000, &new_recipient);
        assert_eq!(result, Err(Ok(Error::NotInitialized)));
    }

    #[test]
    fn subscribe_event_carries_the_fee_amount() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::Event as _;

        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token_addr, token_admin, _token_client) = create_token(&env, &admin);
        let reward_pool = Address::generate(&env);
        let fee_recipient = Address::generate(&env);

        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        market.initialize(
            &admin,
            &token_addr,
            &reward_pool,
            &PRICE,
            &PERIOD_SECONDS,
            &1_000,
            &fee_recipient,
        );

        let subscriber = Address::generate(&env);
        token_admin.mint(&subscriber, &10_000);
        let new_expiry = market.subscribe(&subscriber, &1);

        let expected = Subscribe {
            subscriber: subscriber.clone(),
            periods: 1,
            new_expiry,
            fee_amount: PRICE * 1_000 / 10_000,
        }
        .to_xdr(&env, &market_id);
        assert_eq!(env.events().all().events().last(), Some(&expected));
    }
}
