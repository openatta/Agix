def merge_counts(a, b):
    """Return a new dict with the counts of `a` and `b` summed per key. Neither argument is
    modified."""
    out = dict(a)
    for key, n in b.items():
        out[key] = out.get(key, 0) + n
    return out
