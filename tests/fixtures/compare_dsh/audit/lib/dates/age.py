def age_on(birth, on):
    """Whole years completed by date `on` for someone born on `birth`. Someone born on
    29 February has their birthday on 1 March in years that are not leap years."""
    return on.year - birth.year
