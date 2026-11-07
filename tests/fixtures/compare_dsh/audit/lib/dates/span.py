def days_between(a, b):
    """Number of days between the dates `a` and `b`; never negative, whichever comes first."""
    return abs((b - a).days)
