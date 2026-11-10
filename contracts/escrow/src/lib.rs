//! Netting escrow.
//!
//! Holds sell-side tokens for orders and settles bilateral fills between
//! opposite-direction orders (X sells A for B, Y sells B for A). The contract,
//! not the backend, derives the rate (from a SEP-40 oracle), computes the
//! amounts and fees, and enforces every sender's signed `min_receive`. The
//! settler role can only choose *which* orders to pair and *how much* of A to
//! move; it can never redirect funds or push a sender below their floor.
//!
//! Mirrors `engine/netting/engine.py`: integer math, 7-decimal fixed point,
//! fees rounded up, floors checked by exact cross-multiplication.
#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, token, Address, Env, Symbol, Vec,
};

pub const SCALE: i128 = 10_000_000;
pub const BPS: i128 = 10_000;

const DAY: u32 = 17_280; // ledgers per day (~5s)
const BUMP_THRESHOLD: u32 = 30 * DAY;
const BUMP_TO: u32 = 60 * DAY;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    NotFound = 1,
    NotOpen = 2,
    PairMismatch = 3,
    Expired = 4,
    BadAmount = 5,
    LimitNotMet = 6,
    OraclePrice = 7,
    FullNotMet = 8,
    Overflow = 9,
    BadConfig = 10,
}

// ---- SEP-40 oracle interface (consumer side) -----------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OracleAsset {
    Stellar(Address),
    Other(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

#[contractclient(name = "OracleClient")]
pub trait PriceFeed {
    fn lastprice(env: Env, asset: OracleAsset) -> Option<PriceData>;
}

// ---- storage -------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum Key {
    Config,
    NextId,
    OracleAsset(Address),
    Order(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub settler: Address,
    pub treasury: Address,
    pub oracle: Address,
    pub fee_bps: u32,
    pub max_oracle_age: u64, // seconds
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Open,
    Filled,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Order {
    pub owner: Address,
    pub recipient: Address,
    pub sell_token: Address,
    pub buy_token: Address,
    pub amount: i128,
    pub remaining: i128,
    pub min_receive: i128, // floor for the whole order, in buy_token stroops
    pub deadline: u64,
    pub full: bool, // all-or-nothing within one settle_batch
    pub status: Status,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FillReq {
    pub x_id: u64, // sells A, buys B
    pub y_id: u64, // sells B, buys A
    pub a_amount: i128,
}

// ---- events --------------------------------------------------------------

#[contractevent]
pub struct Deposited {
    #[topic]
    pub order_id: u64,
    pub owner: Address,
    pub amount: i128,
}

#[contractevent]
pub struct Filled {
    #[topic]
    pub x_id: u64,
    #[topic]
    pub y_id: u64,
    pub a_amount: i128,
    pub b_amount: i128,
}

#[contractevent]
pub struct Cancelled {
    #[topic]
    pub order_id: u64,
    pub refunded: i128,
}

// ---- helpers (free functions: not part of the contract interface) --------

fn config(env: &Env) -> Config {
    env.storage().instance().get(&Key::Config).unwrap()
}

fn load_order(env: &Env, id: u64) -> Result<Order, Error> {
    env.storage().persistent().get(&Key::Order(id)).ok_or(Error::NotFound)
}

fn save_order(env: &Env, id: u64, order: &Order) {
    let key = Key::Order(id);
    env.storage().persistent().set(&key, order);
    env.storage().persistent().extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

fn ceil_div(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

fn oracle_price(env: &Env, cfg: &Config, token: &Address) -> Result<i128, Error> {
    let asset: OracleAsset = env
        .storage()
        .instance()
        .get(&Key::OracleAsset(token.clone()))
        .unwrap_or(OracleAsset::Stellar(token.clone()));
    let p = OracleClient::new(env, &cfg.oracle)
        .lastprice(&asset)
        .ok_or(Error::OraclePrice)?;
    let now = env.ledger().timestamp();
    if p.price <= 0 || (now > p.timestamp && now - p.timestamp > cfg.max_oracle_age) {
        return Err(Error::OraclePrice);
    }
    Ok(p.price)
}

/// B per A, scaled by SCALE, from two oracle prices quoted in the same base.
fn rate(env: &Env, cfg: &Config, sell: &Address, buy: &Address) -> Result<i128, Error> {
    let pa = oracle_price(env, cfg, sell)?;
    let pb = oracle_price(env, cfg, buy)?;
    pa.checked_mul(SCALE).ok_or(Error::Overflow).map(|n| n / pb)
}

fn settle_one(env: &Env, cfg: &Config, f: &FillReq) -> Result<(), Error> {
    if f.a_amount <= 0 {
        return Err(Error::BadAmount);
    }
    let mut x = load_order(env, f.x_id)?;
    let mut y = load_order(env, f.y_id)?;
    if x.status != Status::Open || y.status != Status::Open {
        return Err(Error::NotOpen);
    }
    if x.sell_token != y.buy_token || x.buy_token != y.sell_token {
        return Err(Error::PairMismatch);
    }
    let now = env.ledger().timestamp();
    if now > x.deadline || now > y.deadline {
        return Err(Error::Expired);
    }
    if f.a_amount > x.remaining {
        return Err(Error::BadAmount);
    }

    let r = rate(env, cfg, &x.sell_token, &x.buy_token)?;
    let a = f.a_amount;
    let b = a.checked_mul(r).ok_or(Error::Overflow)? / SCALE;
    if b <= 0 || b > y.remaining {
        return Err(Error::BadAmount);
    }

    let fee = cfg.fee_bps as i128;
    let fee_a = ceil_div(a.checked_mul(fee).ok_or(Error::Overflow)?, BPS);
    let fee_b = ceil_div(b.checked_mul(fee).ok_or(Error::Overflow)?, BPS);
    let x_recv = b - fee_b; // x receives B
    let y_recv = a - fee_a; // y receives A
    if x_recv <= 0 || y_recv <= 0 {
        return Err(Error::BadAmount);
    }

    // Signed floors, pro rata to what each side sells in this fill.
    let x_ok = x_recv.checked_mul(x.amount).ok_or(Error::Overflow)?
        >= x.min_receive.checked_mul(a).ok_or(Error::Overflow)?;
    let y_ok = y_recv.checked_mul(y.amount).ok_or(Error::Overflow)?
        >= y.min_receive.checked_mul(b).ok_or(Error::Overflow)?;
    if !x_ok || !y_ok {
        return Err(Error::LimitNotMet);
    }

    x.remaining -= a;
    y.remaining -= b;
    if x.remaining == 0 {
        x.status = Status::Filled;
    }
    if y.remaining == 0 {
        y.status = Status::Filled;
    }

    let me = env.current_contract_address();
    let token_a = token::Client::new(env, &x.sell_token);
    let token_b = token::Client::new(env, &x.buy_token);
    token_a.transfer(&me, &y.recipient, &y_recv);
    token_b.transfer(&me, &x.recipient, &x_recv);
    if fee_a > 0 {
        token_a.transfer(&me, &cfg.treasury, &fee_a);
    }
    if fee_b > 0 {
        token_b.transfer(&me, &cfg.treasury, &fee_b);
    }

    save_order(env, f.x_id, &x);
    save_order(env, f.y_id, &y);
    Filled { x_id: f.x_id, y_id: f.y_id, a_amount: a, b_amount: b }.publish(env);
    Ok(())
}

// ---- contract ------------------------------------------------------------

#[contract]
pub struct NettingEscrow;

#[contractimpl]
impl NettingEscrow {
    pub fn __constructor(
        env: Env,
        admin: Address,
        settler: Address,
        treasury: Address,
        oracle: Address,
        fee_bps: u32,
        max_oracle_age: u64,
    ) {
        if fee_bps >= BPS as u32 {
            panic_with_error!(&env, Error::BadConfig);
        }
        env.storage().instance().set(
            &Key::Config,
            &Config { admin, settler, treasury, oracle, fee_bps, max_oracle_age },
        );
        env.storage().instance().set(&Key::NextId, &0u64);
    }

    /// Lock `amount` of `sell_token` and open an order. Returns the order id.
    pub fn deposit(
        env: Env,
        owner: Address,
        recipient: Address,
        sell_token: Address,
        buy_token: Address,
        amount: i128,
        min_receive: i128,
        deadline: u64,
        full: bool,
    ) -> Result<u64, Error> {
        owner.require_auth();
        if amount <= 0 || min_receive < 0 || sell_token == buy_token {
            return Err(Error::BadAmount);
        }
        if deadline <= env.ledger().timestamp() {
            return Err(Error::Expired);
        }
        token::Client::new(&env, &sell_token).transfer(
            &owner,
            &env.current_contract_address(),
            &amount,
        );

        let id: u64 = env.storage().instance().get(&Key::NextId).unwrap_or(0);
        env.storage().instance().set(&Key::NextId, &(id + 1));
        env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
        save_order(
            &env,
            id,
            &Order {
                owner: owner.clone(),
                recipient,
                sell_token,
                buy_token,
                amount,
                remaining: amount,
                min_receive,
                deadline,
                full,
                status: Status::Open,
            },
        );
        Deposited { order_id: id, owner, amount }.publish(&env);
        Ok(id)
    }

    /// Settle fills atomically. Only the settler may call; every fill is
    /// re-validated here. If any fill or any `full` constraint fails, the whole
    /// batch reverts.
    pub fn settle_batch(env: Env, fills: Vec<FillReq>) -> Result<(), Error> {
        let cfg = config(&env);
        cfg.settler.require_auth();

        let mut touched: Vec<u64> = Vec::new(&env);
        for f in fills.iter() {
            settle_one(&env, &cfg, &f)?;
            touched.push_back(f.x_id);
            touched.push_back(f.y_id);
        }
        for id in touched.iter() {
            let o = load_order(&env, id)?;
            if o.full && o.remaining > 0 {
                return Err(Error::FullNotMet);
            }
        }
        Ok(())
    }

    /// Owner withdraws whatever is still unmatched. Used for refunds and for
    /// the DEX fallback (the app then routes the funds with a path payment).
    pub fn cancel(env: Env, order_id: u64) -> Result<i128, Error> {
        let mut o = load_order(&env, order_id)?;
        o.owner.require_auth();
        if o.status != Status::Open {
            return Err(Error::NotOpen);
        }
        let refund = o.remaining;
        o.remaining = 0;
        o.status = Status::Cancelled;
        save_order(&env, order_id, &o);
        if refund > 0 {
            token::Client::new(&env, &o.sell_token).transfer(
                &env.current_contract_address(),
                &o.owner,
                &refund,
            );
        }
        Cancelled { order_id, refunded: refund }.publish(&env);
        Ok(refund)
    }

    pub fn get_order(env: Env, order_id: u64) -> Result<Order, Error> {
        load_order(&env, order_id)
    }

    pub fn get_config(env: Env) -> Config {
        config(&env)
    }

    /// Current oracle-derived rate (buy per sell, scaled by 1e7).
    pub fn quote_rate(env: Env, sell: Address, buy: Address) -> Result<i128, Error> {
        rate(&env, &config(&env), &sell, &buy)
    }

    /// Map a token to the oracle's own asset identifier (e.g. `Other("EUR")`).
    /// Defaults to `Stellar(token)`.
    pub fn set_oracle_asset(env: Env, token: Address, asset: OracleAsset) {
        config(&env).admin.require_auth();
        env.storage().instance().set(&Key::OracleAsset(token), &asset);
    }

    pub fn set_settler(env: Env, settler: Address) {
        let mut cfg = config(&env);
        cfg.admin.require_auth();
        cfg.settler = settler;
        env.storage().instance().set(&Key::Config, &cfg);
    }
}

#[cfg(test)]
mod test;
