def late_fee(days_late):
    """Late fee in cents. Nothing within the three-day grace period; after it, $5 plus $1
    for every day past the grace period, capped at $30."""
    if days_late <= 3:
        return 0
    return min(500 + 100 * (days_late - 3), 3000)
