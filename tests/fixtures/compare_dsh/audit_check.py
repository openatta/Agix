"""Hidden checker for the `audit` comparison case: every function in the audited
library against its own docstring. Run as `python3 audit_check.py <project>`; prints
one line per broken contract and exits non-zero if there is any."""

import datetime
import importlib
import sys

sys.path.insert(0, sys.argv[1])
D = datetime.date
failures = []


def check(module, cases):
    try:
        mod = importlib.import_module("lib." + module)
    except Exception as e:
        failures.append(f"{module}: import failed: {e!r}")
        return
    for name, call in cases:
        try:
            ok = call(mod)
        except Exception as e:
            ok = False
            name = f"{name} raised {e!r}"
        if not ok:
            failures.append(f"{module}: {name}")


def raises(fn, exc):
    try:
        fn()
    except exc:
        return True
    return False


check("billing.tax", [
    ("rounds half up", lambda m: m.add_tax(1050, 10) == 1155 and m.add_tax(150, 1) == 152),
    ("zero rate", lambda m: m.add_tax(999, 0) == 999),
])
check("billing.discount", [
    ("plain percent", lambda m: m.apply_discount(1000, 25) == 750),
    ("drops cent fractions", lambda m: m.apply_discount(999, 10) == 900),
    ("clamped above 100", lambda m: m.apply_discount(1000, 150) == 0),
    ("clamped below 0", lambda m: m.apply_discount(1000, -20) == 1000),
])
check("billing.invoice", [
    ("padded", lambda m: m.invoice_number(2026, 42) == "INV-2026-00042"),
])
check("billing.split", [
    ("sums and differs by one", lambda m: m.split_evenly(1000, 3) == [334, 333, 333]),
    ("exact", lambda m: m.split_evenly(900, 3) == [300, 300, 300]),
    ("more shares than cents", lambda m: m.split_evenly(2, 4) == [1, 1, 0, 0]),
])
check("billing.currency", [
    ("thousands", lambda m: m.format_cents(123456) == "$1,234.56"),
    ("negative", lambda m: m.format_cents(-100) == "-$1.00"),
    ("small", lambda m: m.format_cents(5) == "$0.05"),
])
check("billing.late_fee", [
    ("grace", lambda m: m.late_fee(3) == 0),
    ("after grace", lambda m: m.late_fee(4) == 600),
    ("cap", lambda m: m.late_fee(100) == 3000),
])

check("inventory.reorder", [
    ("at threshold", lambda m: m.needs_reorder(10, 4, 6) is True),
    ("above", lambda m: m.needs_reorder(10, 3, 6) is False),
])
check("inventory.bins", [
    ("1 to 10", lambda m: all(1 <= m.bin_for(f"BK-{n:03d}") <= 10 for n in range(200))),
    ("formula", lambda m: m.bin_for("AB-100") == sum(map(ord, "AB-100")) % 10 + 1),
])
check("inventory.units", [
    ("kg", lambda m: m.to_grams(1.5, "kg") == 1500),
    ("lb", lambda m: m.to_grams(2, "lb") == 907),
    ("unknown unit", lambda m: raises(lambda: m.to_grams(1, "oz"), ValueError)),
])
check("inventory.merge", [
    ("sums", lambda m: m.merge_counts({"a": 1, "b": 2}, {"b": 3, "c": 4}) == {"a": 1, "b": 5, "c": 4}),
    ("no mutation", lambda m: (lambda a, b: (m.merge_counts(a, b), a == {"a": 1} and b == {"a": 2})[1])({"a": 1}, {"a": 2})),
])
check("inventory.sku", [
    ("valid", lambda m: m.is_valid_sku("BK-101")),
    ("invalid", lambda m: not any(m.is_valid_sku(s) for s in ["bk-101", "BK101", "BK-1011", " BK-101"])),
])
check("inventory.aging", [
    ("earliest", lambda m: m.oldest([("A", "2026-03-01"), ("B", "2025-12-31")]) == "B"),
    ("tie goes first", lambda m: m.oldest([("A", "2026-01-01"), ("B", "2026-01-01")]) == "A"),
    ("empty", lambda m: m.oldest([]) is None),
])

check("text.slug", [
    ("runs", lambda m: m.slugify("Hello,  World") == "hello-world"),
    ("no leading or trailing dash", lambda m: m.slugify("  -- Rust & Go! ") == "rust-go"),
])
check("text.truncate", [
    ("short unchanged", lambda m: m.truncate("abc", 3) == "abc"),
    ("exactly n", lambda m: m.truncate("abcdef", 4) == "abc…" and len(m.truncate("abcdef", 4)) == 4),
])
check("text.initials", [
    ("words", lambda m: m.initials("ada  lovelace") == "AL"),
])
check("text.words", [
    ("apostrophes", lambda m: m.count_words("It's 2 o'clock -- isn't it?") == 5),
])
check("text.title", [
    ("small words", lambda m: m.title_case("the lord OF the rings") == "The Lord of the Rings"),
])
check("text.pad", [
    ("odd padding right", lambda m: m.center("ab", 5) == "*ab**"),
    ("too long", lambda m: m.center("abcdef", 3) == "abcdef"),
])

check("dates.leap", [
    ("rules", lambda m: [m.is_leap(y) for y in (1900, 2000, 2024, 2026)] == [False, True, True, False]),
])
check("dates.weekend", [
    ("sat sun mon", lambda m: [m.is_weekend(D(2026, 11, d)) for d in (7, 8, 9)] == [True, True, False]),
])
check("dates.age", [
    ("before birthday", lambda m: m.age_on(D(1990, 6, 15), D(2026, 6, 14)) == 35),
    ("on birthday", lambda m: m.age_on(D(1990, 6, 15), D(2026, 6, 15)) == 36),
    ("leap day, non-leap year", lambda m: m.age_on(D(2004, 2, 29), D(2026, 2, 28)) == 21 and m.age_on(D(2004, 2, 29), D(2026, 3, 1)) == 22),
])
check("dates.quarter", [
    ("bounds", lambda m: [m.quarter(D(2026, mo, 1)) for mo in (1, 3, 4, 12)] == [1, 1, 2, 4]),
])
check("dates.span", [
    ("either order", lambda m: m.days_between(D(2026, 1, 10), D(2026, 1, 1)) == 9),
])
check("dates.month_end", [
    ("february", lambda m: m.month_end(2024, 2) == D(2024, 2, 29) and m.month_end(2026, 2) == D(2026, 2, 28)),
])

for f in failures:
    print(f)
sys.exit(1 if failures else 0)
