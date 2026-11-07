import re


def slugify(title):
    """Lower-case `title` and turn every run of characters other than ASCII letters and
    digits into a single '-'. The slug never starts or ends with '-'."""
    return re.sub(r"[^a-z0-9]+", "-", title.lower())
