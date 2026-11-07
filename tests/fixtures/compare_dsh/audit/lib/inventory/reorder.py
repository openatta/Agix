def needs_reorder(on_hand, reserved, threshold):
    """True when the stock that is actually available (`on_hand` minus `reserved`) is at or
    below `threshold`."""
    return on_hand - reserved <= threshold
