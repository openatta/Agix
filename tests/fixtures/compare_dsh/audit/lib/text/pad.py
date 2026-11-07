def center(s, width, fill="*"):
    """Center `s` in a field of `width` characters padded with `fill`. When the padding is
    odd, the extra character goes on the right. `s` longer than `width` comes back
    unchanged."""
    if len(s) >= width:
        return s
    left = (width - len(s)) // 2
    return fill * left + s + fill * (width - len(s) - left)
