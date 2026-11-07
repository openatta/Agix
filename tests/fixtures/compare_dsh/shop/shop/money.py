"""Money is kept in integer cents everywhere; these convert at the edges."""


def dollars(cents):
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    return "%s%d.%02d" % (sign, cents // 100, cents % 100)


def parse_dollars(text):
    text = text.strip()
    if "." in text:
        whole, frac = text.split(".", 1)
        frac = (frac + "00")[:2]
    else:
        whole, frac = text, "00"
    return int(whole) * 100 + int(frac)


def percent_of(cents, percent):
    return cents * percent // 100
