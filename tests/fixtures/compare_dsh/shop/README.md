# shop

Checkout for a small online store. `python3 -m shop <cart.csv> [--coupon CODE]`
prints the receipt for a cart.

A cart file has one `sku,qty` per line; blank lines and lines starting with `#`
are skipped.

## Pricing

1. **Subtotal** — price × quantity over every line.
2. **Discount** — at most one coupon per order; codes are case-insensitive.
   - `SAVE10`: 10% off the subtotal, rounded down to the cent.
   - `FLAT5`: $5.00 off when the subtotal is at least $20.00.
   - `BOOKS20`: 20% off the book items, rounded down to the cent.
3. **Tax** — 8% of (subtotal − discount), rounded down.
4. **Shipping** — free when (subtotal − discount) is at least $50.00; otherwise
   $4.99 for the first kilogram plus $1.50 for each started kilogram after it.
5. **Total** — subtotal − discount + tax + shipping.

Run the tests with `python3 tests/run_tests.py`.
