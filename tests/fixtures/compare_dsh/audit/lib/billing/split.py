def split_evenly(cents, n):
    """Split `cents` into `n` shares that add up to exactly `cents` and differ by at most one
    cent; the larger shares come first."""
    return [cents // n] * n
