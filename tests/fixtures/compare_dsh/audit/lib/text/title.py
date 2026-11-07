SMALL = {"a", "an", "the", "of", "and"}


def title_case(s):
    """Capitalize every word of `s` except 'a', 'an', 'the', 'of' and 'and', which stay
    lower-case unless they are the first word. All other letters are lower-cased. `s` is
    always words separated by single spaces; nothing else is promised for other input."""
    words = s.lower().split(" ")
    return " ".join(
        w if i > 0 and w in SMALL else w[:1].upper() + w[1:] for i, w in enumerate(words)
    )
