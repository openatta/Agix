"""The product catalog: what can be sold, at what price, and how heavy it is."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Product:
    sku: str
    name: str
    category: str
    price_cents: int
    weight_g: int


PRODUCTS = {
    p.sku: p
    for p in [
        Product("BK-101", "Cooking for Two", "books", 1899, 650),
        Product("BK-102", "The Bread Book", "books", 2450, 900),
        Product("BK-103", "Pocket Atlas", "books", 1225, 300),
        Product("BK-104", "Garden Year", "books", 3175, 1200),
        Product("KT-201", "Coffee Machine", "kitchen", 8999, 4200),
        Product("KT-202", "Chef Knife", "kitchen", 4550, 350),
        Product("KT-203", "Measuring Cups", "kitchen", 799, 250),
        Product("FD-301", "Espresso Beans 1kg", "food", 1999, 1000),
        Product("FD-302", "Sea Salt", "food", 450, 500),
        Product("FD-303", "Olive Oil", "food", 1350, 1100),
        Product("TY-401", "Wooden Puzzle", "toys", 1500, 400),
        Product("TY-402", "Kite", "toys", 2299, 700),
    ]
}


def find(sku):
    return PRODUCTS.get(sku)


def by_category(category):
    return [p for p in PRODUCTS.values() if p.category == category]
