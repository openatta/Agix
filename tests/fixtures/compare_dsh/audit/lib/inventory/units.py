def to_grams(qty, unit):
    """Convert `qty` in `unit` ('g', 'kg' or 'lb', where 1 lb = 453.592 g) to whole grams,
    rounded with Python's round(). Any other unit raises ValueError."""
    factors = {"g": 1, "kg": 1000, "lb": 453.592}
    if unit not in factors:
        raise ValueError(f"unknown unit: {unit}")
    return round(qty * factors[unit])
