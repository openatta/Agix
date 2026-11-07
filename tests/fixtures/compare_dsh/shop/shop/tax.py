"""Sales tax."""

from .money import percent_of

RATE_PERCENT = 8


def tax_cents(taxable_cents):
    return percent_of(taxable_cents, RATE_PERCENT)
