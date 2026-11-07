"""Product search for the storefront."""

from .catalog import PRODUCTS


def tokens(text):
    return [t for t in "".join(c.lower() if c.isalnum() else " " for c in text).split() if t]


def search(query, category=None):
    wanted = set(tokens(query))
    hits = []
    for product in PRODUCTS.values():
        if category and product.category != category:
            continue
        score = len(wanted & set(tokens(product.name)))
        if score:
            hits.append((-score, product.name, product))
    return [p for _, _, p in sorted(hits)]
