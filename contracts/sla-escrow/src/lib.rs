#![no_std]

//! # API Safety Net — SLA Escrow
//!
//! Parametric SLA escrow for API providers, on Soroban/Stellar.
//!
//! A **provider** locks a USDC guarantee in this escrow for a **subscriber**.
//! A trusted **platform** oracle reports health checks. If the API fails
//! `threshold` consecutive checks, the escrow is released to the subscriber.
//! While the API stays healthy the provider can reclaim (refund) the guarantee.
//!
//! State machine:
//! ```text
//!   Locked ──report_failure×threshold──▶ UnderThreat ──release──▶ Disbursed
//!     │  ▲                                    │
//!     │  └────────── report_success ──────────┘
//!     └────────────── refund ───────────────▶ Refunded
//! ```

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Locked = 0,
    UnderThreat = 1,
    Disbursed = 2,
    Refunded = 3,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub provider: Address,
    pub subscriber: Address,
    pub platform: Address,
    pub token: Address,
    pub amount: i128,
    pub threshold: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    Status,
    Failures,
    Funded,
}

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InvalidThreshold = 4,
    AlreadyFunded = 5,
    NotFunded = 6,
    NotUnderThreat = 7,
    NotHealthy = 8,
    Settled = 9,
}

#[contract]
pub struct SlaEscrow;

#[contractimpl]
impl SlaEscrow {
    /// Configure the escrow. Callable once.
    pub fn init(
        env: Env,
        provider: Address,
        subscriber: Address,
        platform: Address,
        token: Address,
        amount: i128,
        threshold: u32,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if threshold == 0 {
            return Err(Error::InvalidThreshold);
        }
        let config = Config {
            provider,
            subscriber,
            platform,
            token,
            amount,
            threshold,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage()
            .instance()
            .set(&DataKey::Status, &Status::Locked);
        env.storage().instance().set(&DataKey::Failures, &0u32);
        env.storage().instance().set(&DataKey::Funded, &false);
        Ok(())
    }

    /// Provider deposits the guarantee into the escrow. Requires provider auth.
    pub fn fund(env: Env) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        if Self::is_funded(&env) {
            return Err(Error::AlreadyFunded);
        }
        config.provider.require_auth();
        let this = env.current_contract_address();
        let client = token::Client::new(&env, &config.token);
        client.transfer(&config.provider, &this, &config.amount);
        env.storage().instance().set(&DataKey::Funded, &true);
        Ok(())
    }

    /// Platform oracle records a failed health check. Returns the new failure
    /// count. When it reaches `threshold`, the escrow enters `UnderThreat`.
    pub fn report_failure(env: Env) -> Result<u32, Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        Self::assert_active(&env)?;

        let mut failures: u32 = env
            .storage()
            .instance()
            .get(&DataKey::Failures)
            .unwrap_or(0);
        failures = failures.saturating_add(1);
        env.storage().instance().set(&DataKey::Failures, &failures);

        if failures >= config.threshold {
            env.storage()
                .instance()
                .set(&DataKey::Status, &Status::UnderThreat);
        }
        Ok(failures)
    }

    /// Platform oracle records a successful health check: resets the failure
    /// streak and (if it had tripped) returns the escrow to `Locked`.
    pub fn report_success(env: Env) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        Self::assert_active(&env)?;

        env.storage().instance().set(&DataKey::Failures, &0u32);
        if Self::status(env.clone()) == Status::UnderThreat {
            env.storage()
                .instance()
                .set(&DataKey::Status, &Status::Locked);
        }
        Ok(())
    }

    /// Release the guarantee to the subscriber. Only valid once `UnderThreat`.
    /// Callable by the platform oracle.
    pub fn release(env: Env) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        if !Self::is_funded(&env) {
            return Err(Error::NotFunded);
        }
        if Self::status(env.clone()) != Status::UnderThreat {
            return Err(Error::NotUnderThreat);
        }
        let this = env.current_contract_address();
        let client = token::Client::new(&env, &config.token);
        client.transfer(&this, &config.subscriber, &config.amount);
        env.storage()
            .instance()
            .set(&DataKey::Status, &Status::Disbursed);
        Ok(())
    }

    /// Provider reclaims the guarantee while the API is healthy (`Locked`).
    pub fn refund(env: Env) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.provider.require_auth();
        if !Self::is_funded(&env) {
            return Err(Error::NotFunded);
        }
        if Self::status(env.clone()) != Status::Locked {
            return Err(Error::NotHealthy);
        }
        let this = env.current_contract_address();
        let client = token::Client::new(&env, &config.token);
        client.transfer(&this, &config.provider, &config.amount);
        env.storage()
            .instance()
            .set(&DataKey::Status, &Status::Refunded);
        Ok(())
    }

    // ── views ────────────────────────────────────────────────────────────

    pub fn config(env: Env) -> Config {
        Self::load_config(&env).unwrap()
    }

    pub fn status(env: Env) -> Status {
        env.storage()
            .instance()
            .get(&DataKey::Status)
            .unwrap_or(Status::Locked)
    }

    pub fn failures(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Failures)
            .unwrap_or(0)
    }

    pub fn funded(env: Env) -> bool {
        Self::is_funded(&env)
    }

    // ── internals ────────────────────────────────────────────────────────

    fn is_funded(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Funded)
            .unwrap_or(false)
    }

    fn load_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }

    /// Only `Locked`/`UnderThreat` accept oracle updates; settled escrows are final.
    fn assert_active(env: &Env) -> Result<(), Error> {
        match Self::status(env.clone()) {
            Status::Disbursed | Status::Refunded => Err(Error::Settled),
            _ => Ok(()),
        }
    }
}

mod test;
