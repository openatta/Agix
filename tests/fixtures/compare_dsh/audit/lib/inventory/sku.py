import re


def is_valid_sku(s):
    """True for a well-formed SKU: two upper-case letters, a dash, three digits, e.g.
    'BK-101'. Nothing before or after."""
    return re.fullmatch(r"[A-Z]{2}-[0-9]{3}", s) is not None
