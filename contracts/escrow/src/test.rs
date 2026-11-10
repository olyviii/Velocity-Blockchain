#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Env,
};

const U: i128 = 10_000_000; // one USDC in stroops

struct T {
    env: Env,
    escrow: NettingEscrowClient<'static>,
    usdc: Address,
    alice: Address, // in CA, sending to r_alice in IN
    carol: Address, // in IN, sending to r_carol in CA
    r_alice: Address,
    r_carol: Address,
    settler: Address,
    treasury: Address,
}

fn bal(t: &T, who: &Address) -> i128 {
    TokenClient::new(&t.env, &t.usdc).balance(who)
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let usdc = env.register_stellar_asset_contract_v2(Address::generate(&env)).address();
    let (alice, carol) = (Address::generate(&env), Address::generate(&env));
    StellarAssetClient::new(&env, &usdc).mint(&alice, &(1_000 * U));
    StellarAssetClient::new(&env, &usdc).mint(&carol, &(1_000 * U));

    let (admin, settler, treasury) =
        (Address::generate(&env), Address::generate(&env), Address::generate(&env));
    let id = env.register(
        NettingEscrow,
        (admin, settler.clone(), treasury.clone(), usdc.clone(), 10u32),
    );
    T {
        escrow: NettingEscrowClient::new(&env, &id),
        r_alice: Address::generate(&env),
        r_carol: Address::generate(&env),
        env,
        usdc,
        alice,
        carol,
        settler,
        treasury,
    }
}

const DEADLINE: u64 = 1_000 + 6 * 3600; // a six-hour wait

fn ca_to_in(t: &T, amount: i128, full: bool, on_expiry: OnExpiry) -> u64 {
    t.escrow.deposit(
        &t.alice, &t.r_alice, &symbol_short!("CA"), &symbol_short!("IN"),
        &amount, &DEADLINE, &full, &on_expiry,
    )
}

fn in_to_ca(t: &T, amount: i128, full: bool) -> u64 {
    t.escrow.deposit(
        &t.carol, &t.r_carol, &symbol_short!("IN"), &symbol_short!("CA"),
        &amount, &DEADLINE, &full, &OnExpiry::SendRemainder,
    )
}

fn fill(t: &T, x: u64, y: u64, amount: i128) {
    t.escrow.settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount }]);
}

#[test]
fn deposit_locks_funds() {
    let t = setup();
    let id = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    assert_eq!(bal(&t, &t.alice), 900 * U);
    assert_eq!(bal(&t, &t.escrow.address), 100 * U);
    let o = t.escrow.get_order(&id);
    assert_eq!((o.amount, o.remaining, o.status), (100 * U, 100 * U, Status::Open));
}

#[test]
fn equal_opposite_orders_net_out_completely() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 100 * U, false);
    fill(&t, x, y, 100 * U);

    // 10 bps on each side
    assert_eq!(bal(&t, &t.r_alice), 100 * U - 1_000_000);
    assert_eq!(bal(&t, &t.r_carol), 100 * U - 1_000_000);
    assert_eq!(bal(&t, &t.treasury), 2_000_000);
    assert_eq!(bal(&t, &t.escrow.address), 0);
    assert_eq!(t.escrow.get_order(&x).status, Status::Filled);
    assert_eq!(t.escrow.get_order(&y).status, Status::Filled);
}

#[test]
fn partial_match_pays_now_and_remainder_crosses_at_deadline() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 60 * U, false);
    fill(&t, x, y, 60 * U);

    // matched part arrives straight away
    assert_eq!(bal(&t, &t.r_alice), 60 * U - 600_000);
    assert_eq!(t.escrow.get_order(&x).remaining, 40 * U);

    // too early to release
    assert_eq!(t.escrow.try_release(&x), Err(Ok(Error::NotExpired)));

    t.env.ledger().set_timestamp(DEADLINE);
    assert_eq!(t.escrow.release(&x), 40 * U);
    assert_eq!(bal(&t, &t.r_alice), 100 * U - 600_000); // fee only on the matched part
    assert_eq!(t.escrow.get_order(&x).status, Status::Released);
    assert_eq!(bal(&t, &t.escrow.address), 0);
}

#[test]
fn refund_on_expiry_returns_remainder_to_sender() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::Refund);
    t.env.ledger().set_timestamp(DEADLINE);
    t.escrow.release(&x);
    assert_eq!(bal(&t, &t.alice), 1_000 * U);
    assert_eq!(bal(&t, &t.r_alice), 0);
    assert_eq!(t.escrow.get_order(&x).status, Status::Cancelled);
}

#[test]
fn release_is_permissionless_and_needs_no_signature() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    t.env.ledger().set_timestamp(DEADLINE);
    t.env.set_auths(&[]); // nobody signs
    assert_eq!(t.escrow.release(&x), 100 * U);
    assert_eq!(bal(&t, &t.r_alice), 100 * U);
}

#[test]
fn instant_order_can_be_matched_then_released_in_the_same_second() {
    let t = setup();
    let y = in_to_ca(&t, 30 * U, false);
    let x = t.escrow.deposit(
        &t.alice, &t.r_alice, &symbol_short!("CA"), &symbol_short!("IN"),
        &(100 * U), &1_000, &false, &OnExpiry::SendRemainder,
    );
    fill(&t, x, y, 30 * U);
    assert_eq!(t.escrow.release(&x), 70 * U);
    assert_eq!(bal(&t, &t.r_alice), 100 * U - 300_000);
}

#[test]
fn only_the_settler_is_authorized_for_settlement() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 100 * U, false);
    fill(&t, x, y, 100 * U);
    assert_eq!(t.env.auths()[0].0, t.settler);
}

#[test]
fn same_direction_orders_cannot_be_paired() {
    let t = setup();
    let x1 = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let x2 = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x1, y_id: x2, amount: U }]);
    assert_eq!(r, Err(Ok(Error::CorridorMismatch)));
}

#[test]
fn different_corridor_cannot_be_paired() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = t.escrow.deposit(
        &t.carol, &t.r_carol, &symbol_short!("MX"), &symbol_short!("CA"),
        &(100 * U), &DEADLINE, &false, &OnExpiry::SendRemainder,
    );
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: U }]);
    assert_eq!(r, Err(Ok(Error::CorridorMismatch)));
}

#[test]
fn cannot_match_more_than_either_side_has() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 10 * U, false);
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: 11 * U }]);
    assert_eq!(r, Err(Ok(Error::BadAmount)));
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: 0 }]);
    assert_eq!(r, Err(Ok(Error::BadAmount)));
}

#[test]
fn expired_orders_cannot_be_matched() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 100 * U, false);
    t.env.ledger().set_timestamp(DEADLINE + 1);
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: U }]);
    assert_eq!(r, Err(Ok(Error::Expired)));
}

#[test]
fn full_order_must_be_completed_within_the_batch_or_everything_reverts() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, true, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 20 * U, false);
    let r = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: 20 * U }]);
    assert_eq!(r, Err(Ok(Error::FullNotMet)));
    assert_eq!(t.escrow.get_order(&x).remaining, 100 * U);
    assert_eq!(bal(&t, &t.r_alice), 0);
}

#[test]
fn full_order_can_be_filled_by_two_counterparties_in_one_batch() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, true, OnExpiry::SendRemainder);
    let y1 = in_to_ca(&t, 50 * U, false);
    let y2 = in_to_ca(&t, 50 * U, false);
    t.escrow.settle_batch(&vec![
        &t.env,
        FillReq { x_id: x, y_id: y1, amount: 50 * U },
        FillReq { x_id: x, y_id: y2, amount: 50 * U },
    ]);
    assert_eq!(t.escrow.get_order(&x).status, Status::Filled);
}

#[test]
fn owner_can_cancel_remainder_before_deadline() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 50 * U, false);
    fill(&t, x, y, 50 * U);
    assert_eq!(t.escrow.cancel(&x), 50 * U);
    assert_eq!(bal(&t, &t.alice), 950 * U);
}

#[test]
fn closed_orders_cannot_be_reused() {
    let t = setup();
    let x = ca_to_in(&t, 100 * U, false, OnExpiry::SendRemainder);
    let y = in_to_ca(&t, 100 * U, false);
    fill(&t, x, y, 100 * U);
    let again = t.escrow.try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, amount: U }]);
    assert_eq!(again, Err(Ok(Error::NotOpen)));
    assert_eq!(t.escrow.try_cancel(&x), Err(Ok(Error::NotOpen)));
    assert_eq!(t.escrow.try_release(&x), Err(Ok(Error::NotOpen)));
}

#[test]
fn deposit_rejects_bad_input() {
    let t = setup();
    let ca = symbol_short!("CA");
    let same = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &ca, &ca, &U, &DEADLINE, &false, &OnExpiry::Refund,
    );
    assert_eq!(same, Err(Ok(Error::BadAmount)));
    let zero = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &ca, &symbol_short!("IN"), &0, &DEADLINE, &false, &OnExpiry::Refund,
    );
    assert_eq!(zero, Err(Ok(Error::BadAmount)));
    let past = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &ca, &symbol_short!("IN"), &U, &999, &false, &OnExpiry::Refund,
    );
    assert_eq!(past, Err(Ok(Error::Expired)));
}
