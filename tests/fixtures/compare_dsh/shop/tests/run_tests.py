import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from shop.cart import Cart  # noqa: E402
from shop.checkout import checkout  # noqa: E402
from shop.money import dollars, parse_dollars  # noqa: E402
from shop.shipping import shipping_cents  # noqa: E402

failures = 0


def check(name, cond):
    global failures
    if not cond:
        failures += 1
        print("FAIL", name)


check("dollars", dollars(5) == "0.05" and dollars(123456) == "1234.56")
check("parse_dollars", parse_dollars("12.3") == 1230 and parse_dollars("7") == 700)

cart = Cart()
cart.add("FD-302", 2)
r = checkout(cart)
check("plain subtotal", r.subtotal_cents == 900)
check("plain tax", r.tax_cents == 72)
check("plain shipping", r.shipping_cents == 499)
check("plain total", r.total_cents == 900 + 72 + 499)

cart = Cart()
cart.add("TY-402", 1)
r = checkout(cart, "save10")
check("save10 discount", r.discount_cents == 229)

cart = Cart()
cart.add("BK-103", 1)
check("flat5 below minimum", checkout(cart, "FLAT5").discount_cents == 0)
cart.add("BK-103", 1)
check("flat5 at minimum", checkout(cart, "FLAT5").discount_cents == 500)

check("shipping first kg", shipping_cents(1000, 100) == 499)
check("shipping started kg", shipping_cents(1001, 100) == 649)
check("shipping free", shipping_cents(9000, 5000) == 0)

print("OK" if failures == 0 else "%d failed" % failures)
sys.exit(1 if failures else 0)
