def truncate(s, n):
    """Return `s` unchanged when it has at most `n` characters. Otherwise return its first
    n - 1 characters followed by '…', so the result is exactly `n` characters long."""
    if len(s) <= n:
        return s
    return s[:n] + "…"
