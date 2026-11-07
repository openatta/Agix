"""A cart: products and quantities, read from a cart file."""

from . import catalog


class UnknownSku(Exception):
    pass


class Line:
    def __init__(self, product, qty):
        self.product = product
        self.qty = qty

    def total_cents(self):
        return self.product.price_cents * self.qty

    def weight_g(self):
        return self.product.weight_g * self.qty


class Cart:
    def __init__(self):
        self._lines = {}

    def add(self, sku, qty):
        product = catalog.find(sku)
        if product is None:
            raise UnknownSku(sku)
        if sku in self._lines:
            self._lines[sku].qty += qty
        else:
            self._lines[sku] = Line(product, qty)

    def lines(self):
        return list(self._lines.values())

    def subtotal_cents(self):
        return sum(line.total_cents() for line in self._lines.values())

    def category_subtotal_cents(self, category):
        return sum(
            line.total_cents()
            for line in self._lines.values()
            if line.product.category == category
        )

    def weight_g(self):
        return sum(line.weight_g() for line in self._lines.values())


def load_cart(path):
    cart = Cart()
    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            sku, qty = line.split(",")
            cart.add(sku.strip(), int(qty))
    return cart
