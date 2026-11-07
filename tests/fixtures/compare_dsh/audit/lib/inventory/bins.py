def bin_for(sku):
    """Return the storage bin for `sku`. Bins are numbered 1 to 10: the bin is the sum of the
    character codes of the SKU, modulo 10, plus one."""
    return sum(ord(c) for c in sku) % 10
