#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, token, Address, Env};

struct Setup<'a> {
    // Held so the Env (and the clients borrowing it) stay alive for the test.
    #[allow(dead_code)]
    env: Env,
    provider: Address,
    subscriber: Address,
    platform: Address,
    token_addr: Address,
    token: token::Client<'a>,
    sac: token::StellarAssetClient<'a>,
    client: SlaEscrowClient<'a>,
    amount: i128,
    threshold: u32,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let provider = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let platform = Address::generate(&env);

    // A test USDC-like token (Stellar Asset Contract).
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token_addr = sac.address();
    let token = token::Client::new(&env, &token_addr);
    let sac_admin = token::StellarAssetClient::new(&env, &token_addr);

    let amount: i128 = 1_000_000; // 0.1 USDC (7 decimals)
    let threshold: u32 = 3;

    // Fund the provider so they can deposit the guarantee.
    sac_admin.mint(&provider, &amount);

    let contract_id = env.register(SlaEscrow, ());
    let client = SlaEscrowClient::new(&env, &contract_id);
    client.init(
        &provider,
        &subscriber,
        &platform,
        &token_addr,
        &amount,
        &threshold,
    );

    Setup {
        env,
        provider,
        subscriber,
        platform,
        token_addr,
        token,
        sac: sac_admin,
        client,
        amount,
        threshold,
    }
}

#[test]
fn releases_to_subscriber_after_threshold_failures() {
    let s = setup();

    s.client.fund();
    assert_eq!(s.token.balance(&s.provider), 0);
    assert_eq!(s.token.balance(&s.client.address), s.amount);
    assert_eq!(s.client.status(), Status::Locked);

    // Fail up to (but not reaching) the threshold — still locked.
    for i in 1..s.threshold {
        let f = s.client.report_failure();
        assert_eq!(f, i);
        assert_eq!(s.client.status(), Status::Locked);
    }
    // The failure that hits the threshold trips it.
    let f = s.client.report_failure();
    assert_eq!(f, s.threshold);
    assert_eq!(s.client.status(), Status::UnderThreat);

    s.client.release();
    assert_eq!(s.client.status(), Status::Disbursed);
    assert_eq!(s.token.balance(&s.subscriber), s.amount);
    assert_eq!(s.token.balance(&s.client.address), 0);
}

#[test]
fn success_resets_the_failure_streak() {
    let s = setup();
    s.client.fund();

    s.client.report_failure();
    s.client.report_failure();
    assert_eq!(s.client.failures(), 2);

    s.client.report_success();
    assert_eq!(s.client.failures(), 0);
    assert_eq!(s.client.status(), Status::Locked);

    // A recovered streak means the threshold is never reached here.
    s.client.report_failure();
    assert_eq!(s.client.status(), Status::Locked);
}

#[test]
fn under_threat_recovers_on_success() {
    let s = setup();
    s.client.fund();
    for _ in 0..s.threshold {
        s.client.report_failure();
    }
    assert_eq!(s.client.status(), Status::UnderThreat);

    s.client.report_success();
    assert_eq!(s.client.status(), Status::Locked);
    assert_eq!(s.client.failures(), 0);
}

#[test]
fn provider_can_refund_while_healthy() {
    let s = setup();
    s.client.fund();
    assert_eq!(s.token.balance(&s.provider), 0);

    s.client.refund();
    assert_eq!(s.client.status(), Status::Refunded);
    assert_eq!(s.token.balance(&s.provider), s.amount);
    assert_eq!(s.token.balance(&s.client.address), 0);
}

#[test]
fn cannot_release_before_threshold() {
    let s = setup();
    s.client.fund();
    let res = s.client.try_release();
    assert_eq!(res, Err(Ok(Error::NotUnderThreat)));
    assert_eq!(s.client.status(), Status::Locked);
}

#[test]
fn cannot_refund_when_under_threat() {
    let s = setup();
    s.client.fund();
    for _ in 0..s.threshold {
        s.client.report_failure();
    }
    let res = s.client.try_refund();
    assert_eq!(res, Err(Ok(Error::NotHealthy)));
}

#[test]
fn cannot_double_init() {
    let s = setup();
    let res = s.client.try_init(
        &s.provider,
        &s.subscriber,
        &s.platform,
        &s.token_addr,
        &s.amount,
        &s.threshold,
    );
    assert_eq!(res, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn cannot_fund_twice() {
    let s = setup();
    s.client.fund();
    // Re-minting so a second fund could theoretically move tokens.
    s.sac.mint(&s.provider, &s.amount);
    let res = s.client.try_fund();
    assert_eq!(res, Err(Ok(Error::AlreadyFunded)));
}
