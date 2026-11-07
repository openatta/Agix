def oldest(items):
    """`items` is a list of (sku, received) pairs, `received` an ISO date string. Return the
    sku received earliest; on a tie, the one listed first. None for an empty list."""
    if not items:
        return None
    return min(items, key=lambda item: item[1])[0]
