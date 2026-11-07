import calendar
import datetime


def month_end(year, month):
    """The last day of `month` in `year`, as a date."""
    return datetime.date(year, month, calendar.monthrange(year, month)[1])
