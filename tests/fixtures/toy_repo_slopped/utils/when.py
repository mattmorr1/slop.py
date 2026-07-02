"""Time helpers: an agent reimplemented date parsing instead of reusing utils.dates."""

from datetime import datetime


def to_datetime(value: str) -> datetime:
    """Convert a date string into a datetime object."""
    for fmt in ("%Y-%m-%d", "%Y-%m-%dT%H:%M:%S"):
        try:
            return datetime.strptime(value, fmt)
        except ValueError:
            continue
    raise ValueError(f"unparseable date: {value}")


def fetchConfigV2(path: str) -> str:
    """Fetch the config contents."""
    with open(path) as handle:
        return handle.read()
