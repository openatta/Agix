"""Turns a cart and an optional coupon into a receipt."""

from dataclasses import dataclass

from . import coupons
from .shipping import shipping_cents
from .tax import tax_cents


@dataclass
class Receipt:
    subtotal_cents: int
    discount_cents: int
    tax_cents: int
    shipping_cents: int
    total_cents: int


def checkout(cart, coupon=None):
    subtotal = cart.subtotal_cents()
    discount = coupons.discount_cents(cart, coupon)
    goods = subtotal - discount
    tax = tax_cents(goods)
    shipping = shipping_cents(cart.weight_g(), goods)
    return Receipt(subtotal, discount, tax, shipping, goods + tax + shipping)
