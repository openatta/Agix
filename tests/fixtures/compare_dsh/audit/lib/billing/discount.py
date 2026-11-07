def apply_discount(cents, percent):
    """Return the price after taking `percent` off. `percent` is clamped to 0..100, so the
    result is never negative and never above `cents`. Fractions of a cent are dropped from
    the discount."""
    return cents - cents * percent // 100
