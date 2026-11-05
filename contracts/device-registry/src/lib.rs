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
