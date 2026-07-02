"""Date parsing helpers. Pure."""

from datetime import datetime


def parse_date(raw: str) -> datetime:
    """Parse an ISO-8601 date string."""
    return datetime.fromisoformat(raw)


def format_date(value: datetime) -> str:
    """Format a datetime as an ISO-8601 date string."""
    return value.isoformat()
