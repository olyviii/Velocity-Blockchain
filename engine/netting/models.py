"""Data model for the netting engine.

Everything is USDC, 1:1. Amounts are integers in stroops (7 decimals), the
unit the token uses on-chain. Floats are never used for money.

An order sends USDC from one region to another (e.g. "CA" -> "IN"). Two orders
flowing in opposite directions can be netted against each other.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

SCALE = 10_000_000  # 7 decimal places: one USDC
BPS = 10_000        # basis points denominator


class Mode(str, Enum):
    INSTANT = "INSTANT"  # no waiting: one matching pass, then release immediately
    PARTIAL = "PARTIAL"  # match whatever is available, wait for the rest
    FULL = "FULL"        # match only if the whole order can be matched at once


class Fallback(str, Enum):
    SEND_REMAINDER = "SEND_REMAINDER"  # at the deadline, send the rest across to the recipient
    REFUND = "REFUND"                  # at the deadline, return the rest to the sender


class Status(str, Enum):
    OPEN = "OPEN"
    FILLED = "FILLED"        # fully matched
    RELEASED = "RELEASED"    # remainder sent to the recipient at the deadline
    CANCELLED = "CANCELLED"  # remainder refunded to the sender


@dataclass
class Order:
    """A funded order: `amount` USDC is locked, to be paid to `recipient` in `to_region`."""

    id: str
    owner: str
    recipient: str
    from_region: str
    to_region: str
    amount: int
    created_at: int
    max_wait: int
    mode: Mode = Mode.PARTIAL
    fallback: Fallback = Fallback.SEND_REMAINDER
    remaining: int = -1
    status: Status = Status.OPEN

    def __post_init__(self) -> None:
        if self.from_region == self.to_region:
            raise ValueError("from_region and to_region must differ")
        if self.amount <= 0:
            raise ValueError("amount must be positive")
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
    """One match of `amount` between X (a -> b) and Y (b -> a).

    X's deposit pays Y's recipient and Y's deposit pays X's recipient, each
    `amount - fee`; the treasury gets `2 * fee`. Conservation is exact:
        2 * amount == x_receives + y_receives + 2 * fee
    """

    x_order: str
    y_order: str
    amount: int
    fee: int  # per side

    @property
    def x_receives(self) -> int:
        """What X's recipient gets."""
        return self.amount - self.fee

    @property
    def y_receives(self) -> int:
        """What Y's recipient gets."""
        return self.amount - self.fee


@dataclass(frozen=True)
class FallbackAction:
    order_id: str
    action: Fallback
    to: str      # recipient (SEND_REMAINDER) or owner (REFUND)
    amount: int  # unmatched remainder
