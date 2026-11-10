//! Two-way USDC netting escrow.
//!
//! Senders lock USDC for a recipient in another region (e.g. CA -> IN) and
//! choose how long to wait. While an order is open, the settler can pair it
//! with orders flowing the opposite way (IN -> CA). A matched amount pays both
//! recipients straight away: X's deposit covers Y's recipient, who is in X's
//! region, and the other way round, so that part of each payment can be paid
//! out from local liquidity and never has to cross the border. Whatever is
//! still unmatched at the deadline goes to the recipient (the cross-border leg)
//! or back to the sender, as the sender chose when depositing.
//!
//! One token, 1:1: no oracle, no exchange rate, no DEX. The settler only
//! chooses *which* orders to pair and *how much*; every payout goes to an
//! address the sender signed, and the fee is fixed at deployment.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    Address, Env, Symbol, Vec,
};

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
    CorridorMismatch = 3,
    Expired = 4,
    BadAmount = 5,
    NotExpired = 6,
    FullNotMet = 7,
    Overflow = 8,
    BadConfig = 9,
}

// ---- storage -------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum Key {
    Config,
    NextId,
    Order(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub settler: Address,
    pub treasury: Address,
    pub token: Address, // USDC
    pub fee_bps: u32,   // charged on matched amounts only, per side
}

/// What happens to the unmatched part when the wait runs out.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnExpiry {
    SendRemainder, // send it across to the recipient
    Refund,        // return it to the sender
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Open,
    Filled,    // fully matched
    Released,  // remainder sent to the recipient at the deadline
    Cancelled, // remainder refunded to the sender
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Order {
    pub owner: Address,
    pub recipient: Address,
    pub from_region: Symbol, // where the sender is, e.g. "CA"
    pub to_region: Symbol,   // where the recipient is, e.g. "IN"
    pub amount: i128,
    pub remaining: i128,
    pub deadline: u64, // last second the order may be matched
    pub full: bool,    // all-or-nothing within one settle_batch
    pub on_expiry: OnExpiry,
    pub status: Status,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FillReq {
    pub x_id: u64,
    pub y_id: u64, // must flow the opposite way to x
    pub amount: i128,
}

// ---- events --------------------------------------------------------------

#[contractevent]
pub struct Deposited {
    #[topic]
    pub order_id: u64,
    pub owner: Address,
    pub from_region: Symbol,
    pub to_region: Symbol,
    pub amount: i128,
}

#[contractevent]
pub struct Matched {
    #[topic]
    pub x_id: u64,
    #[topic]
    pub y_id: u64,
    pub amount: i128,
    pub fee: i128, // per side
}

#[contractevent]
pub struct Released {
    #[topic]
    pub order_id: u64,
    pub to: Address,
    pub amount: i128,
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

/// Fee on one side of a match, rounded up so rounding never shorts the treasury.
fn fee_for(amount: i128, fee_bps: u32) -> Result<i128, Error> {
    let n = amount.checked_mul(fee_bps as i128).ok_or(Error::Overflow)?;
    Ok((n + BPS - 1) / BPS)
}

fn settle_one(env: &Env, cfg: &Config, f: &FillReq) -> Result<(), Error> {
    if f.amount <= 0 || f.x_id == f.y_id {
        return Err(Error::BadAmount);
    }
    let mut x = load_order(env, f.x_id)?;
    let mut y = load_order(env, f.y_id)?;
    if x.status != Status::Open || y.status != Status::Open {
        return Err(Error::NotOpen);
    }
    if x.from_region != y.to_region || x.to_region != y.from_region {
        return Err(Error::CorridorMismatch);
    }
    let now = env.ledger().timestamp();
    if now > x.deadline || now > y.deadline {
        return Err(Error::Expired);
    }
    let m = f.amount;
    if m > x.remaining || m > y.remaining {
        return Err(Error::BadAmount);
    }
    let fee = fee_for(m, cfg.fee_bps)?;
    let paid = m - fee;
    if paid <= 0 {
        return Err(Error::BadAmount);
    }

    x.remaining -= m;
    y.remaining -= m;
    if x.remaining == 0 {
        x.status = Status::Filled;
    }
    if y.remaining == 0 {
        y.status = Status::Filled;
    }
    save_order(env, f.x_id, &x);
    save_order(env, f.y_id, &y);

    let me = env.current_contract_address();
    let usdc = token::Client::new(env, &cfg.token);
    usdc.transfer(&me, &y.recipient, &paid); // funded by x, paid in x's region
    usdc.transfer(&me, &x.recipient, &paid); // funded by y, paid in y's region
    if fee > 0 {
        usdc.transfer(&me, &cfg.treasury, &(fee * 2));
    }
    Matched { x_id: f.x_id, y_id: f.y_id, amount: m, fee }.publish(env);
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
        token: Address,
        fee_bps: u32,
    ) {
        if fee_bps >= BPS as u32 {
            panic_with_error!(&env, Error::BadConfig);
        }
        env.storage()
            .instance()
            .set(&Key::Config, &Config { admin, settler, treasury, token, fee_bps });
        env.storage().instance().set(&Key::NextId, &0u64);
    }

    /// Lock `amount` USDC for `recipient` and open an order. A `deadline` equal
    /// to the current time means "instant": match against what is open now,
    /// then release. Returns the order id.
    pub fn deposit(
        env: Env,
        owner: Address,
        recipient: Address,
        from_region: Symbol,
        to_region: Symbol,
        amount: i128,
        deadline: u64,
        full: bool,
        on_expiry: OnExpiry,
    ) -> Result<u64, Error> {
        owner.require_auth();
        if amount <= 0 || from_region == to_region {
            return Err(Error::BadAmount);
        }
        if deadline < env.ledger().timestamp() {
            return Err(Error::Expired);
        }
        let cfg = config(&env);
        token::Client::new(&env, &cfg.token).transfer(
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
                from_region: from_region.clone(),
                to_region: to_region.clone(),
                amount,
                remaining: amount,
                deadline,
                full,
                on_expiry,
                status: Status::Open,
            },
        );
        Deposited { order_id: id, owner, from_region, to_region, amount }.publish(&env);
        Ok(id)
    }

    /// Settle matches atomically. Only the settler may call; every match is
    /// re-validated here. If any match or any `full` constraint fails, the whole
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

    /// Once the deadline is reached, anyone may close the order: the unmatched
    /// remainder goes to the recipient or back to the owner, as the owner chose.
    /// Permissionless so funds never depend on the backend staying up; the
    /// destination is fixed by the order, so the caller cannot redirect it.
    pub fn release(env: Env, order_id: u64) -> Result<i128, Error> {
        let mut o = load_order(&env, order_id)?;
        if o.status != Status::Open {
            return Err(Error::NotOpen);
        }
        if env.ledger().timestamp() < o.deadline {
            return Err(Error::NotExpired);
        }
        let amount = o.remaining;
        let to = match o.on_expiry {
            OnExpiry::SendRemainder => {
                o.status = Status::Released;
                o.recipient.clone()
            }
            OnExpiry::Refund => {
                o.status = Status::Cancelled;
                o.owner.clone()
            }
        };
        o.remaining = 0;
        save_order(&env, order_id, &o);
        token::Client::new(&env, &config(&env).token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );
        match o.status {
            Status::Released => Released { order_id, to, amount }.publish(&env),
            _ => Cancelled { order_id, refunded: amount }.publish(&env),
        }
        Ok(amount)
    }

    /// Owner takes back whatever is still unmatched, at any time.
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
        token::Client::new(&env, &config(&env).token).transfer(
            &env.current_contract_address(),
            &o.owner,
            &refund,
        );
        Cancelled { order_id, refunded: refund }.publish(&env);
        Ok(refund)
    }

    pub fn get_order(env: Env, order_id: u64) -> Result<Order, Error> {
        load_order(&env, order_id)
    }

    pub fn get_config(env: Env) -> Config {
        config(&env)
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
