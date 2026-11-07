def quarter(d):
    """The quarter of the year (1 to 4) that the date `d` is in."""
    return (d.month - 1) // 3 + 1
