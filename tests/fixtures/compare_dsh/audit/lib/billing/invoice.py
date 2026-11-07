def invoice_number(year, seq):
    """Return the invoice number for sequence `seq` of `year`: 'INV-<year>-<seq>', with
    `seq` zero-padded to five digits."""
    return f"INV-{year}-{seq:05d}"
