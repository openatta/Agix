"""Sales reports over archived receipts."""

import json
from collections import defaultdict

from .legacy.promo_engine import legacy_discount_cents
from .money import dollars


def load_archive(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def revenue_by_day(orders):
    days = defaultdict(int)
    for order in orders:
        days[order["date"]] += order["total_cents"]
    return dict(sorted(days.items()))


def top_products(orders, n=5):
    counts = defaultdict(int)
    for order in orders:
        for line in order["lines"]:
            counts[line["sku"]] += line["qty"]
    return sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:n]


def repriced_discount(order):
    lines = [(l["category"], l["price_cents"], l["qty"]) for l in order["lines"]]
    return legacy_discount_cents(lines, order.get("coupon") or "")


def format_daily(orders):
    return ["%s %s" % (day, dollars(c)) for day, c in revenue_by_day(orders).items()]
