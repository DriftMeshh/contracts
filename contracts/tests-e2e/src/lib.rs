//! End-to-end integration test: the full DriftMesh loop, all four
//! contracts wired together exactly as they would be on-chain.
//!
//! device registers → submits readings → consumer subscribes (funding
//! the pool) → epoch closes → device claims its proportional payout →
//! balances check out.

#[cfg(test)]
mod e2e {
    use device_registry::{DeviceRegistry, DeviceRegistryClient};
    use payment_streamer::{PaymentStreamer, PaymentStreamerClient};
    use soroban_sdk::testutils::{Address as _, Ledger};
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

        // -- submission-validator (time-boxed epochs) --
        let epoch = 1u32;
        let epoch_seconds: u64 = 86_400; // one day
        let region = 42u32;
        let validator_id = env.register(SubmissionValidator, ());
        let validator = SubmissionValidatorClient::new(&env, &validator_id);
        validator.initialize(&admin, &registry_id, &2000, &epoch_seconds);

        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2500));
        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2550));
        assert!(validator.submit_reading(&device, &device_type, &region, &epoch, &2600));
        assert_eq!(validator.get_submission_count(&device, &epoch), 3);

        // A wild outlier in the same epoch should be rejected and not
        // inflate the device's payable count.
        let accepted = validator.submit_reading(&device, &device_type, &region, &epoch, &999_999);
        assert!(!accepted);
        assert_eq!(validator.get_submission_count(&device, &epoch), 3);
        assert_eq!(validator.get_total_submissions(&epoch), 3);

        // -- payment-streamer (proportional payouts, no flat rate) --
        let streamer_id = env.register(PaymentStreamer, ());
        let streamer = PaymentStreamerClient::new(&env, &streamer_id);
        streamer.initialize(&admin, &validator_id, &token_addr);
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

        // The epoch must close before its total is final and payouts can be
        // computed. Advance the ledger to the epoch's end.
        let epoch_end = validator.epoch_end(&epoch);
        env.ledger().with_mut(|li| li.timestamp = epoch_end);
        assert!(validator.is_epoch_final(&epoch));

        // -- device claims its proportional payout for the epoch --
        // One device holds all 3 of the epoch's submissions, so its share is
        // 3/3 = the whole pool that was snapshotted at first claim.
        let expected_payout = price_per_period * 2;
        let payout = streamer.claim(&device, &epoch);
        assert_eq!(payout, expected_payout);
        assert_eq!(streamer.epoch_pool(&epoch), expected_payout);
        assert_eq!(token.balance(&device), expected_payout);
        assert_eq!(streamer.pool_balance(), price_per_period * 2 - expected_payout);

        // Claiming the same epoch again is rejected.
        let second_claim = streamer.try_claim(&device, &epoch);
        assert!(second_claim.is_err());
    }
}
