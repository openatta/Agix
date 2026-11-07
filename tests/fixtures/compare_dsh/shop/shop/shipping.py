"""What shipping an order costs."""

FREE_OVER_CENTS = 5000
FIRST_KG_CENTS = 499
EXTRA_KG_CENTS = 150


def shipping_cents(weight_g, goods_cents):
    if goods_cents >= FREE_OVER_CENTS:
        return 0
    extra_kg = max(0, (weight_g - 1) // 1000)
    return FIRST_KG_CENTS + EXTRA_KG_CENTS * extra_kg
