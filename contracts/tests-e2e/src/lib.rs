//! End-to-end integration test: the full DriftMesh loop, all four
//! contracts wired together exactly as they would be on-chain.
//!
//! device registers → submits readings → consumer subscribes (funding
//! the pool) → device claims its payout → balances check out.

#[cfg(test)]
mod e2e {
    use device_registry::{DeviceRegistry, DeviceRegistryClient};
    use payment_streamer::{PaymentStreamer, PaymentStreamerClient};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{token, Address, Env, Symbol};
    use submission_validator::{SubmissionValidator, SubmissionValidatorClient};
    use subscription_market::{SubscriptionMarket, SubscriptionMarketClient};

    #[test]
    fn full_loop_register_submit_subscribe_claim() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);

        // -- stablecoin both sides pay/get paid in --
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token_addr = sac.address();
        let token_admin = token::StellarAssetClient::new(&env, &token_addr);
        let token = token::Client::new(&env, &token_addr);

        // -- device-registry --
        let registry_id = env.register(DeviceRegistry, ());
        let registry = DeviceRegistryClient::new(&env, &registry_id);
        registry.initialize(&admin);

        let device = Address::generate(&env);
        let device_owner = Address::generate(&env);
        let device_type = Symbol::new(&env, "weather");
        registry.register_device(&device, &device_owner, &device_type, &404_500, &-736_900);
        assert!(registry.is_active(&device));

        // -- submission-validator --
        let validator_id = env.register(SubmissionValidator, ());
        let validator = SubmissionValidatorClient::new(&env, &validator_id);
        validator.initialize(&admin, &registry_id, &2000);

        let epoch = 1u32;
        let region = 42u32;
        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2500));
        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2550));
        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2600));
        assert_eq!(validator.get_submission_count(&device, &epoch), 3);

        // A wild outlier in the same epoch should be rejected and not
        // inflate the device's payable count.
        let accepted = validator.submit_reading(&device, &device_type, &region, &epoch, &999_999);
        assert!(!accepted);
        assert_eq!(validator.get_submission_count(&device, &epoch), 3);

        // -- payment-streamer --
        let reward_rate: i128 = 25;
        let streamer_id = env.register(PaymentStreamer, ());
        let streamer = PaymentStreamerClient::new(&env, &streamer_id);
        streamer.initialize(&admin, &validator_id, &token_addr, &reward_rate);
        assert_eq!(streamer.pool_balance(), 0);

        // -- subscription-market --
        let price_per_period: i128 = 500;
        let period_seconds: u64 = 86_400;
        let market_id = env.register(SubscriptionMarket, ());
        let market = SubscriptionMarketClient::new(&env, &market_id);
        market.initialize(&admin, &token_addr, &streamer_id, &price_per_period, &period_seconds);

        let consumer = Address::generate(&env);
        token_admin.mint(&consumer, &10_000);

        // Subscriber pays for 2 periods — funds should land directly in
        // the streamer's own balance, with no separate "fund" step.
        market.subscribe(&consumer, &2);
        assert!(market.is_active(&consumer));
        assert_eq!(token.balance(&consumer), 10_000 - price_per_period * 2);
        assert_eq!(streamer.pool_balance(), price_per_period * 2);

        // -- device claims its payout for the epoch --
        let expected_payout = 3 * reward_rate; // 3 valid readings, outlier excluded
        let payout = streamer.claim(&device, &epoch);
        assert_eq!(payout, expected_payout);
        assert_eq!(token.balance(&device), expected_payout);
        assert_eq!(streamer.pool_balance(), price_per_period * 2 - expected_payout);

        // Claiming the same epoch again is rejected.
        let second_claim = streamer.try_claim(&device, &epoch);
        assert!(second_claim.is_err());
    }
}
