def add_tax(cents, rate_percent):
    """Return `cents` plus tax at `rate_percent` (an integer), with the tax rounded half up
    to the nearest cent."""
    return cents + (cents * rate_percent + 50) // 100
