"""Data model for the netting engine.

All amounts are integers in stroops (7 decimals), the same unit a SEP-41 token
or classic Stellar asset uses on-chain. Floats are never used for money.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

SCALE = 10_000_000  # 7 decimal places; also the fixed-point scale of `rate`
BPS = 10_000        # basis points denominator


class Mode(str, Enum):
    INSTANT = "INSTANT"  # no waiting: one matching pass, then fallback immediately
    PARTIAL = "PARTIAL"  # fill whatever is available, wait for the rest
    FULL = "FULL"        # fill only if the whole order can be filled at once


class Fallback(str, Enum):
    ROUTE_DEX = "ROUTE_DEX"  # send the unmatched remainder through a Stellar path payment
    REFUND = "REFUND"        # return the unmatched remainder to the sender


class Status(str, Enum):
    OPEN = "OPEN"
    FILLED = "FILLED"        # nothing left to match (or handed to the DEX router)
    CANCELLED = "CANCELLED"


@dataclass
class Order:
    """A funded order: `amount` of `sell_asset` is locked, sender wants `buy_asset`.

    `min_receive` is the sender's signed floor for the whole order, in `buy_asset`
    stroops (like `destMin` on a path payment). Partial fills must respect it pro rata.
    """

    id: str
    owner: str
    recipient: str
    sell_asset: str
    buy_asset: str
    amount: int
    min_receive: int
    created_at: int
    max_wait: int
    mode: Mode = Mode.PARTIAL
    fallback: Fallback = Fallback.ROUTE_DEX
    remaining: int = -1
    status: Status = Status.OPEN

    def __post_init__(self) -> None:
        if self.sell_asset == self.buy_asset:
            raise ValueError("sell_asset and buy_asset must differ")
        if self.amount <= 0:
            raise ValueError("amount must be positive")
        if self.min_receive < 0:
            raise ValueError("min_receive cannot be negative")
        if self.max_wait < 0:
            raise ValueError("max_wait cannot be negative")
        if self.remaining < 0:
            self.remaining = self.amount

    @property
    def deadline(self) -> int:
        """Last timestamp at which the order may still be matched."""
        return self.created_at + (0 if self.mode is Mode.INSTANT else self.max_wait)


@dataclass(frozen=True)
class Fill:
    """One bilateral match. X sells asset A and buys B; Y sells B and buys A.

    Conservation holds exactly, per asset:
        A: x gives a_amount  == y receives (a_amount - fee_a) + fee_a
        B: y gives b_amount  == x receives (b_amount - fee_b) + fee_b
    """

    x_order: str
    y_order: str
    asset_a: str
    asset_b: str
    a_amount: int
    b_amount: int
    fee_a: int
    fee_b: int
    rate: int  # B per A, scaled by SCALE

    @property
    def x_receives(self) -> int:
        return self.b_amount - self.fee_b

    @property
    def y_receives(self) -> int:
        return self.a_amount - self.fee_a


@dataclass(frozen=True)
class FallbackAction:
    order_id: str
    action: Fallback
    sell_asset: str
    buy_asset: str
    amount: int  # unmatched remainder, in sell_asset stroops
    min_receive: int  # pro-rata floor for the remainder
