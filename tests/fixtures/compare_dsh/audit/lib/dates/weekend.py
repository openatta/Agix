def is_weekend(d):
    """True when the date `d` falls on a Saturday or a Sunday."""
    return d.weekday() >= 5
