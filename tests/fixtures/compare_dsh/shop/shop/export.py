"""Writes receipts out for the accounting system."""

import csv
import json

from .money import dollars

FIELDS = ["order_id", "subtotal", "discount", "tax", "shipping", "total"]


def receipt_row(order_id, receipt):
    return {
        "order_id": order_id,
        "subtotal": dollars(receipt.subtotal_cents),
        "discount": dollars(receipt.discount_cents),
        "tax": dollars(receipt.tax_cents),
        "shipping": dollars(receipt.shipping_cents),
        "total": dollars(receipt.total_cents),
    }


def write_csv(path, rows):
    with open(path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=FIELDS)
        writer.writeheader()
        writer.writerows(rows)


def write_json(path, rows):
    with open(path, "w") as f:
        json.dump(rows, f, indent=2)
