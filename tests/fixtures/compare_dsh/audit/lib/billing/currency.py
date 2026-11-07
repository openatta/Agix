def format_cents(cents):
    """Format an amount in cents as dollars with two decimals and thousands separators:
    123456 -> '$1,234.56'. A negative amount gets a leading minus: -100 -> '-$1.00'."""
    sign = "-" if cents < 0 else ""
    dollars, rest = divmod(abs(cents), 100)
    return f"{sign}${dollars:,}.{rest:02d}"
