import re


def count_words(text):
    """Number of words in `text`, a word being a maximal run of ASCII letters, ASCII digits
    and apostrophes."""
    return len(re.findall(r"[A-Za-z0-9']+", text))
