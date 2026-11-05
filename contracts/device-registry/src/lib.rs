#![no_std]

//! Device Registry
//!
//! Lets a physical sensor (identified by its own Stellar keypair) register
//! itself on-chain, declaring who owns it, what kind of readings it
//! produces, and a coarse location. Downstream contracts (submission
//! validation, payouts) check in here before trusting a device's data.

use soroban_sdk::{contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol};

#[contracttype]
#[derive(Clone)]
pub struct Device {
    pub owner: Address,
    pub device_type: Symbol,
    pub lat: i32,
    pub lon: i32,
    pub registered_at: u64,
    pub active: bool,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Device(Address),
    /// Number of open (unresolved) disputes raised against this device.
    /// Incremented by \`raise_dispute\`, decremented (or reset) by
    /// \`resolve_dispute\`. Stored in instance storage so it's cheap to read.
    OpenDisputeCount(Address),
    /// Cumulative count of disputes upheld against this device across its
    /// lifetime. Never decremented — used for reputation tracking.
    UpheldDisputeCount(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    DeviceAlreadyRegistered = 3,
    DeviceNotFound = 4,
    NotOwnerOrAdmin = 5,
    /// A new dispute was raised but the device already has one open.
    /// The existing dispute must be resolved before a new one is accepted.
    /// This prevents griefing through dispute flooding.
    DisputeAlreadyOpen = 6,
    /// Attempted to resolve a dispute but no open dispute exists for the device.
    NoOpenDispute = 7,
}

/// Emitted when a device is successfully registered.
/// Topics: ["reg_dev", device_address]
/// Data: device_type
#[contractevent]
#[derive(Clone)]
pub struct RegDev {
    #[topic]
    pub device: Address,
    pub device_type: Symbol,
}

/// Emitted when a device is deactivated (by owner, admin, or dispute resolution).
/// Topics: ["deactivated", device_address]
/// Data: reason
#[contractevent]
#[derive(Clone)]
pub struct DeviceDeactivated {
    #[topic]
    pub device: Address,
    pub reason: Symbol,
}

/// Emitted when a new dispute is raised against a device.
/// Topics: ["dispute_raised", device_address]
/// Data: (disputer, evidence_hash, open_dispute_count)
#[contractevent]
#[derive(Clone)]
pub struct DisputeRaised {
    #[topic]
    pub device: Address,
    pub disputer: Address,
    pub evidence_hash: u64,
    pub open_dispute_count: u32,
}

/// Emitted when the admin resolves a dispute.
/// Topics: ["dispute_resolved", device_address]
/// Data: (upheld, upheld_total, device_active)
#[contractevent]
#[derive(Clone)]
pub struct DisputeResolved {
    #[topic]
    pub device: Address,
    pub upheld: bool,
    pub upheld_total: u32,
    pub device_active: bool,
}

#[contract]
pub struct DeviceRegistry;

#[contractimpl]
impl DeviceRegistry {
    pub fn initialize(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// A device registers itself — it must sign this call with its own
    /// keypair, proving it controls the address it will later sign
    /// readings with. `lat`/`lon` are coarse degrees * 1e4 (fixed-point,
    /// since Soroban has no floats), used by the validator to bucket
    /// nearby devices for outlier comparison.
    pub fn register_device(
        env: Env,
        device: Address,
        owner: Address,
        device_type: Symbol,
        lat: i32,
        lon: i32,
    ) -> Result<(), Error> {
        device.require_auth();

        let key = DataKey::Device(device.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::DeviceAlreadyRegistered);
        }

        let record = Device {
            owner,
            device_type,
            lat,
            lon,
            registered_at: env.ledger().timestamp(),
            active: true,
        };
        env.storage().persistent().set(&key, &record);
        // Extend the TTL to ~1 year of ledger slots (5 seconds per ledger,
        // 365*24*3600/5 = 6_307_200 ledgers). Without this call a long-lived
        // device entry could be archived by the Soroban fee-based archival
        // system once the default TTL expires, causing \`is_active\` to return
        // false for a device that is still operational.
        env.storage().persistent().extend_ttl(&key, 6_307_200, 6_307_200);
        RegDev {
            device,
            device_type: record.device_type.clone(),
        }
        .publish(&env);
        Ok(())
    }

    /// The device's owner (or the registry admin) can deactivate it —
    /// e.g. decommissioned hardware or a device caught submitting bad data.
    /// Emits a `DeviceDeactivated` event with reason "owner" or "admin".
    pub fn deactivate_device(env: Env, device: Address, caller: Address) -> Result<(), Error> {
        caller.require_auth();

        let key = DataKey::Device(device.clone());
        let mut record: Device = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::DeviceNotFound)?;

        let admin: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        let admin = admin.ok_or(Error::NotInitialized)?;

        let reason = if caller == admin {
            Symbol::new(&env, "admin")
        } else if caller == record.owner {
            Symbol::new(&env, "owner")
        } else {
            return Err(Error::NotOwnerOrAdmin);
        };

        record.active = false;
        env.storage().persistent().set(&key, &record);

        DeviceDeactivated { device, reason }.publish(&env);
        Ok(())
    }

    /// Any address may raise a dispute against a registered device by supplying
    /// a 64-bit hash of off-chain evidence (e.g. truncated SHA-256 of a URL +
    /// device + epoch). At most one open dispute is allowed per device at a
    /// time to prevent griefing through dispute flooding — a disputer must
    /// wait for the admin to resolve the current dispute before raising another.
    /// The disputer's address is recorded on-chain (in the event) creating
    /// accountability for false disputes.
    pub fn raise_dispute(
        env: Env,
        disputer: Address,
        device: Address,
        evidence_hash: u64,
    ) -> Result<u32, Error> {
        disputer.require_auth();

        // Verify the device exists.
        if !env.storage().persistent().has(&DataKey::Device(device.clone())) {
            return Err(Error::DeviceNotFound);
        }

        // Enforce one-open-dispute-at-a-time to prevent griefing.
        let open_key = DataKey::OpenDisputeCount(device.clone());
        let open: u32 = env.storage().instance().get(&open_key).unwrap_or(0);
        if open > 0 {
            return Err(Error::DisputeAlreadyOpen);
        }

        let new_open = open + 1;
        env.storage().instance().set(&open_key, &new_open);

        DisputeRaised {
            device,
            disputer,
            evidence_hash,
            open_dispute_count: new_open,
        }
        .publish(&env);

        Ok(new_open)
    }

    /// Only the admin can resolve a dispute. If \`upheld\` is true the device is
    /// immediately deactivated (slashed); if false it is left active (dismissed).
    /// Either way the open dispute count is cleared so a new dispute can be
    /// raised in the future.
    pub fn resolve_dispute(
        env: Env,
        admin_caller: Address,
        device: Address,
        upheld: bool,
    ) -> Result<(), Error> {
        admin_caller.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        if admin_caller != stored_admin {
            return Err(Error::NotOwnerOrAdmin);
        }

        // Guard: there must be an open dispute to resolve.
        let open_key = DataKey::OpenDisputeCount(device.clone());
        let open: u32 = env.storage().instance().get(&open_key).unwrap_or(0);
        if open == 0 {
            return Err(Error::NoOpenDispute);
        }

        // Clear the open dispute.
        env.storage().instance().set(&open_key, &0u32);

        let device_active: bool;

        if upheld {
            // Update the lifetime upheld counter.
            let upheld_key = DataKey::UpheldDisputeCount(device.clone());
            let prev_upheld: u32 = env.storage().instance().get(&upheld_key).unwrap_or(0);
            let new_upheld = prev_upheld + 1;
            env.storage().instance().set(&upheld_key, &new_upheld);

            // Deactivate the device (slash it).
            let dev_key = DataKey::Device(device.clone());
            if let Some(mut record) = env.storage().persistent().get::<DataKey, Device>(&dev_key) {
                record.active = false;
                env.storage().persistent().set(&dev_key, &record);
            }
            device_active = false;

            DeviceDeactivated {
                device: device.clone(),
                reason: Symbol::new(&env, "dispute"),
            }
            .publish(&env);

            DisputeResolved {
                device,
                upheld: true,
                upheld_total: new_upheld,
                device_active: false,
            }
            .publish(&env);
        } else {
            // Dismissed — device stays active, upheld count unchanged.
            let upheld_total: u32 = env
                .storage()
                .instance()
                .get(&DataKey::UpheldDisputeCount(device.clone()))
                .unwrap_or(0);

            let dev_key = DataKey::Device(device.clone());
            device_active = env
                .storage()
                .persistent()
                .get::<DataKey, Device>(&dev_key)
                .map(|d| d.active)
                .unwrap_or(false);

            DisputeResolved {
                device,
                upheld: false,
                upheld_total,
                device_active,
            }
            .publish(&env);
        }

        Ok(())
    }

    /// The registry admin can re-enable a previously deactivated device —
    /// for example, after a dispute has been resolved in the device's favour,
    /// or after a hardware replacement has been verified off-chain.
    /// Only the admin can reactivate (not the owner) so that an owner cannot
    /// unilaterally undo an admin-initiated or dispute-driven deactivation.
    pub fn reactivate_device(env: Env, device: Address, admin_caller: Address) -> Result<(), Error> {
        admin_caller.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;

        if admin_caller != stored_admin {
            return Err(Error::NotOwnerOrAdmin);
        }

        let key = DataKey::Device(device.clone());
        let mut record: Device = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::DeviceNotFound)?;

        record.active = true;
        env.storage().persistent().set(&key, &record);
        Ok(())
    }

    pub fn get_device(env: Env, device: Address) -> Result<Device, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Device(device))
            .ok_or(Error::DeviceNotFound)
    }

    /// Returns (open_disputes, lifetime_upheld_disputes) for a device.
    /// Both return 0 if no disputes have ever been raised against the device.
    pub fn get_dispute_counts(env: Env, device: Address) -> (u32, u32) {
        let open: u32 = env
            .storage()
            .instance()
            .get(&DataKey::OpenDisputeCount(device.clone()))
            .unwrap_or(0);
        let upheld: u32 = env
            .storage()
            .instance()
            .get(&DataKey::UpheldDisputeCount(device))
            .unwrap_or(0);
        (open, upheld)
    }

    /// Cheap existence+status check other contracts cross-call before
    /// trusting a device's submission.
    pub fn is_active(env: Env, device: Address) -> bool {
        let record: Option<Device> = env.storage().persistent().get(&DataKey::Device(device));
        record.map(|d| d.active).unwrap_or(false)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn register_and_deactivate() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(DeviceRegistry, ());
        let client = DeviceRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let device = Address::generate(&env);
        let owner = Address::generate(&env);
        let device_type = Symbol::new(&env, "weather");

        client.register_device(&device, &owner, &device_type, &404_500, &-736_900);
        assert!(client.is_active(&device));

        client.deactivate_device(&device, &owner);
        assert!(!client.is_active(&device));
    }

    #[test]
    fn double_registration_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(DeviceRegistry, ());
        let client = DeviceRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let device = Address::generate(&env);
        let owner = Address::generate(&env);
        let device_type = Symbol::new(&env, "weather");

        client.register_device(&device, &owner, &device_type, &0, &0);
        let result = client.try_register_device(&device, &owner, &device_type, &0, &0);
        assert_eq!(result, Err(Ok(Error::DeviceAlreadyRegistered)));
    }
}
