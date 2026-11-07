def initials(name):
    """The upper-cased first letter of each whitespace-separated word of `name`:
    'ada  lovelace' -> 'AL'."""
    return "".join(word[0].upper() for word in name.split())
