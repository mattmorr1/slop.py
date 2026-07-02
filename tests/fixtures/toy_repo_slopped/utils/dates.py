"""Date parsing helpers. Pure."""

from datetime import datetime


def parse_date(raw: str) -> datetime:
    """Parse an ISO-8601 date string."""
    cleaned = raw.strip()
    if cleaned.endswith("Z"):
        cleaned = cleaned[:-1] + "+00:00"
    return datetime.fromisoformat(cleaned)


def format_date(value: datetime) -> str:
    """Format a datetime as an ISO-8601 date string."""
    return value.isoformat()
