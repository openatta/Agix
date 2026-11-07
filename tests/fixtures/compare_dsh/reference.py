"""The intended pricing, written independently of the code under test.

`python3 reference.py <pristine shop dir> <cart.csv> [CODE]` prints the
receipt `python3 -m shop` should print for that cart and coupon.
"""

import sys

sys.path.insert(0, sys.argv[1])

from shop.catalog import PRODUCTS  # noqa: E402


def dollars(c):
    return "%d.%02d" % (c // 100, c % 100)


lines = []
for raw in open(sys.argv[2]):
    raw = raw.strip()
    if raw and not raw.startswith("#"):
        sku, qty = raw.split(",")
        lines.append((PRODUCTS[sku.strip()], int(qty)))
code = sys.argv[3].upper() if len(sys.argv) > 3 else None

subtotal = sum(p.price_cents * q for p, q in lines)
books = sum(p.price_cents * q for p, q in lines if p.category == "books")
weight = sum(p.weight_g * q for p, q in lines)
discount = {
    None: 0,
    "SAVE10": subtotal * 10 // 100,
    "FLAT5": 500 if subtotal >= 2000 else 0,
    "BOOKS20": books * 20 // 100,
    "FREESHIP": 0,
}[code]
goods = subtotal - discount
tax = goods * 8 // 100
if code == "FREESHIP" or goods >= 5000:
    shipping = 0
else:
    shipping = 499 + 150 * max(0, (weight - 1) // 1000)
for name, c in [
    ("subtotal", subtotal),
    ("discount", discount),
    ("tax", tax),
    ("shipping", shipping),
    ("total", goods + tax + shipping),
]:
    print(name, dollars(c))
