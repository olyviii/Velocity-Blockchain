#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Env,
};

const U: i128 = SCALE; // one whole unit in stroops

// Minimal SEP-40 oracle: just enough surface for the escrow to read prices.
#[contract]
pub struct MockOracle;

#[contractimpl]
impl MockOracle {
    pub fn set_price(env: Env, asset: OracleAsset, price: i128, timestamp: u64) {
        env.storage().persistent().set(&asset, &PriceData { price, timestamp });
    }
    pub fn lastprice(env: Env, asset: OracleAsset) -> Option<PriceData> {
        env.storage().persistent().get(&asset)
    }
}

struct T {
    env: Env,
    escrow: NettingEscrowClient<'static>,
    usdc: Address,
    eurc: Address,
    alice: Address,
    bob: Address,
    r_alice: Address, // alice's recipient (receives EURC)
    r_bob: Address,   // bob's recipient (receives USDC)
    settler: Address,
    treasury: Address,
    oracle: MockOracleClient<'static>,
}

fn bal(t: &T, token: &Address, who: &Address) -> i128 {
    TokenClient::new(&t.env, token).balance(who)
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let issuer = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(issuer.clone()).address();
    let eurc = env.register_stellar_asset_contract_v2(issuer).address();
    let (alice, bob) = (Address::generate(&env), Address::generate(&env));
    StellarAssetClient::new(&env, &usdc).mint(&alice, &(1_000 * U));
    StellarAssetClient::new(&env, &eurc).mint(&bob, &(1_000 * U));

    // USDC = 1.0000000, EURC = 1.0869565 (in USD): 1 USDC buys 0.92 EURC
    let oracle_id = env.register(MockOracle, ());
    let oracle = MockOracleClient::new(&env, &oracle_id);
    oracle.set_price(&OracleAsset::Stellar(usdc.clone()), &10_000_000, &1_000);
    oracle.set_price(&OracleAsset::Stellar(eurc.clone()), &10_869_565, &1_000);

    let (admin, settler, treasury) =
        (Address::generate(&env), Address::generate(&env), Address::generate(&env));
    let id = env.register(
        NettingEscrow,
        (admin, settler.clone(), treasury.clone(), oracle_id.clone(), 10u32, 300u64),
    );
    T {
        escrow: NettingEscrowClient::new(&env, &id),
        r_alice: Address::generate(&env),
        r_bob: Address::generate(&env),
        env,
        usdc,
        eurc,
        alice,
        bob,
        settler,
        treasury,
        oracle,
    }
}

fn alice_order(t: &T, amount: i128, min_receive: i128, full: bool) -> u64 {
    t.escrow.deposit(&t.alice, &t.r_alice, &t.usdc, &t.eurc, &amount, &min_receive, &100_000, &full)
}

fn bob_order(t: &T, amount: i128, min_receive: i128, full: bool) -> u64 {
    t.escrow.deposit(&t.bob, &t.r_bob, &t.eurc, &t.usdc, &amount, &min_receive, &100_000, &full)
}

#[test]
fn rate_comes_from_oracle() {
    let t = setup();
    assert_eq!(t.escrow.quote_rate(&t.usdc, &t.eurc), 9_200_000);
}

#[test]
fn deposit_locks_funds() {
    let t = setup();
    let id = alice_order(&t, 100 * U, 91 * U, false);
    assert_eq!(bal(&t, &t.usdc, &t.alice), 900 * U);
    assert_eq!(bal(&t, &t.usdc, &t.escrow.address), 100 * U);
    let o = t.escrow.get_order(&id);
    assert_eq!((o.amount, o.remaining, o.status), (100 * U, 100 * U, Status::Open));
}

#[test]
fn full_swap_settles_exactly_and_conserves_value() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 91 * U, false);
    let y = bob_order(&t, 92 * U, 99 * U, false);
    t.escrow.settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);

    // 10 bps fee on each side, taken from what is received
    assert_eq!(bal(&t, &t.eurc, &t.r_alice), 92 * U - 920_000); // 91.908
    assert_eq!(bal(&t, &t.usdc, &t.r_bob), 100 * U - 1_000_000); // 99.9
    assert_eq!(bal(&t, &t.usdc, &t.treasury), 1_000_000);
    assert_eq!(bal(&t, &t.eurc, &t.treasury), 920_000);
    // escrow holds nothing: every stroop went to a recipient or the treasury
    assert_eq!(bal(&t, &t.usdc, &t.escrow.address), 0);
    assert_eq!(bal(&t, &t.eurc, &t.escrow.address), 0);
    assert_eq!(t.escrow.get_order(&x).status, Status::Filled);
    assert_eq!(t.escrow.get_order(&y).status, Status::Filled);
}

#[test]
fn only_the_settler_is_authorized_for_settlement() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, false);
    let y = bob_order(&t, 92 * U, 0, false);
    t.escrow.settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);
    let auths = t.env.auths();
    assert_eq!(auths[0].0, t.settler);
}

#[test]
fn partial_fill_then_owner_cancels_remainder() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 91 * U, false);
    let y = bob_order(&t, 46 * U, 49 * U, false);
    t.escrow.settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 50 * U }]);
    let o = t.escrow.get_order(&x);
    assert_eq!((o.remaining, o.status), (50 * U, Status::Open));

    let refunded = t.escrow.cancel(&x);
    assert_eq!(refunded, 50 * U);
    assert_eq!(bal(&t, &t.usdc, &t.alice), 950 * U); // 1000 - 100 + 50 back
    assert_eq!(t.escrow.get_order(&x).status, Status::Cancelled);
    // the 50 filled USDC left escrow (to bob's recipient + fee), the 50 refunded left too
    assert_eq!(bal(&t, &t.usdc, &t.escrow.address), 0);
    assert_eq!(bal(&t, &t.usdc, &t.r_bob), 50 * U - 500_000); // 49.95 after 10 bps
    assert_eq!(bal(&t, &t.usdc, &t.treasury), 500_000);
}

#[test]
fn sender_floor_is_enforced() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 95 * U, false); // wants 95 EURC, rate gives ~91.9
    let y = bob_order(&t, 92 * U, 0, false);
    let r = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);
    assert_eq!(r, Err(Ok(Error::LimitNotMet)));
    assert_eq!(bal(&t, &t.eurc, &t.r_alice), 0);
}

#[test]
fn same_direction_orders_cannot_be_paired() {
    let t = setup();
    let x1 = alice_order(&t, 100 * U, 0, false);
    let x2 = alice_order(&t, 100 * U, 0, false);
    let r = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x1, y_id: x2, a_amount: 10 * U }]);
    assert_eq!(r, Err(Ok(Error::PairMismatch)));
}

#[test]
fn cannot_fill_more_than_remaining_on_either_side() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, false);
    let y = bob_order(&t, 10 * U, 0, false); // worth ~10.87 USDC
    let too_big_for_y = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 50 * U }]);
    assert_eq!(too_big_for_y, Err(Ok(Error::BadAmount)));
    let too_big_for_x = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 101 * U }]);
    assert_eq!(too_big_for_x, Err(Ok(Error::BadAmount)));
}

#[test]
fn expired_orders_cannot_settle() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, false);
    let y = bob_order(&t, 92 * U, 0, false);
    t.env.ledger().set_timestamp(100_001);
    t.oracle.set_price(&OracleAsset::Stellar(t.usdc.clone()), &10_000_000, &100_001);
    t.oracle.set_price(&OracleAsset::Stellar(t.eurc.clone()), &10_869_565, &100_001);
    let r = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);
    assert_eq!(r, Err(Ok(Error::Expired)));
}

#[test]
fn stale_oracle_blocks_settlement() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, false);
    let y = bob_order(&t, 92 * U, 0, false);
    t.env.ledger().set_timestamp(1_000 + 301); // max age is 300s
    let r = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);
    assert_eq!(r, Err(Ok(Error::OraclePrice)));
}

#[test]
fn full_order_must_be_completed_within_the_batch_or_everything_reverts() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, true); // FULL
    let y = bob_order(&t, 20 * U, 0, false); // only ~21.7 USDC of counterparty
    let r = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 21 * U }]);
    assert_eq!(r, Err(Ok(Error::FullNotMet)));
    // nothing moved and nothing was recorded
    assert_eq!(t.escrow.get_order(&x).remaining, 100 * U);
    assert_eq!(t.escrow.get_order(&y).remaining, 20 * U);
    assert_eq!(bal(&t, &t.eurc, &t.r_alice), 0);
}

#[test]
fn full_order_can_be_filled_by_two_counterparties_in_one_batch() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, true);
    let y1 = bob_order(&t, 46 * U, 0, false);
    let y2 = bob_order(&t, 46 * U, 0, false);
    t.escrow.settle_batch(&vec![
        &t.env,
        FillReq { x_id: x, y_id: y1, a_amount: 50 * U },
        FillReq { x_id: x, y_id: y2, a_amount: 50 * U },
    ]);
    assert_eq!(t.escrow.get_order(&x).status, Status::Filled);
}

#[test]
fn deposit_rejects_bad_input() {
    let t = setup();
    let same = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &t.usdc, &t.usdc, &U, &0, &100_000, &false,
    );
    assert_eq!(same, Err(Ok(Error::BadAmount)));
    let zero = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &t.usdc, &t.eurc, &0, &0, &100_000, &false,
    );
    assert_eq!(zero, Err(Ok(Error::BadAmount)));
    let past = t.escrow.try_deposit(
        &t.alice, &t.r_alice, &t.usdc, &t.eurc, &U, &0, &999, &false,
    );
    assert_eq!(past, Err(Ok(Error::Expired)));
}

#[test]
fn filled_or_cancelled_orders_cannot_be_reused() {
    let t = setup();
    let x = alice_order(&t, 100 * U, 0, false);
    let y = bob_order(&t, 92 * U, 0, false);
    t.escrow.settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: 100 * U }]);
    let again = t
        .escrow
        .try_settle_batch(&vec![&t.env, FillReq { x_id: x, y_id: y, a_amount: U }]);
    assert_eq!(again, Err(Ok(Error::NotOpen)));
    assert_eq!(t.escrow.try_cancel(&x), Err(Ok(Error::NotOpen)));
}
