"""`python3 -m shop <cart.csv> [--coupon CODE]`: print a cart's receipt."""

import argparse
import sys

from .cart import UnknownSku, load_cart
from .checkout import checkout
from .coupons import UnknownCoupon
from .money import dollars


def main(argv=None):
    parser = argparse.ArgumentParser(prog="shop")
    parser.add_argument("cart")
    parser.add_argument("--coupon")
    args = parser.parse_args(argv)
    try:
        cart = load_cart(args.cart)
        receipt = checkout(cart, args.coupon)
    except UnknownSku as e:
        print("unknown sku %s" % e, file=sys.stderr)
        return 2
    except UnknownCoupon as e:
        print("unknown coupon %s" % e, file=sys.stderr)
        return 2
    print("subtotal", dollars(receipt.subtotal_cents))
    print("discount", dollars(receipt.discount_cents))
    print("tax", dollars(receipt.tax_cents))
    print("shipping", dollars(receipt.shipping_cents))
    print("total", dollars(receipt.total_cents))
    return 0


if __name__ == "__main__":
    sys.exit(main())
