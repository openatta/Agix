"""Order confirmation messages."""

from .money import dollars

TEMPLATE = """Thanks for your order!

Items:     {subtotal}
Discount: -{discount}
Tax:       {tax}
Shipping:  {shipping}
Total:     {total}
"""


def confirmation(receipt):
    return TEMPLATE.format(
        subtotal=dollars(receipt.subtotal_cents),
        discount=dollars(receipt.discount_cents),
        tax=dollars(receipt.tax_cents),
        shipping=dollars(receipt.shipping_cents),
        total=dollars(receipt.total_cents),
    )


def subject(order_id):
    return "Your order %s" % order_id
