"""Coupon codes and the discount each one gives."""

from .money import percent_of


class UnknownCoupon(Exception):
    pass


def discount_cents(cart, code):
    if code is None:
        return 0
    code = code.upper()
    if code == "SAVE10":
        return percent_of(cart.subtotal_cents(), 10)
    if code == "FLAT5":
        return 500 if cart.subtotal_cents() >= 2000 else 0
    if code == "BOOKS20":
        return percent_of(cart.subtotal_cents(), 20)
    raise UnknownCoupon(code)
