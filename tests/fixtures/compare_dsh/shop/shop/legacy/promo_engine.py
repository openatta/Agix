"""The original promotion engine, replaced by `shop.coupons`.

Old orders in the archive were priced with these rules, so `shop.reports`
still uses them to re-price archived receipts. Do not use for new orders.
"""

from ..money import percent_of

PROMOTIONS = {
    "WELCOME": {"kind": "percent", "value": 15, "category": None},
    "BOOKS20": {"kind": "percent", "value": 20, "category": "books"},
    "KITCHEN5": {"kind": "flat", "value": 500, "category": "kitchen"},
    "SHIPFREE": {"kind": "shipping", "value": 0, "category": None},
}


def promotion(code):
    return PROMOTIONS.get(code.upper())


def legacy_discount_cents(lines, code):
    promo = promotion(code)
    if promo is None:
        return 0
    eligible = sum(
        price * qty
        for (category, price, qty) in lines
        if promo["category"] is None or promo["category"] == category
    )
    if promo["kind"] == "percent":
        return percent_of(eligible, promo["value"])
    if promo["kind"] == "flat":
        return min(promo["value"], eligible)
    return 0


def legacy_ships_free(code):
    promo = promotion(code)
    return promo is not None and promo["kind"] == "shipping"
