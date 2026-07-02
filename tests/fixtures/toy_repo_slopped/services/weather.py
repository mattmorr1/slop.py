"""Weather service: consumes the sanctioned HTTP channel."""

from datetime import datetime

from core.http_client import HttpClient
from utils.dates import parse_date


def fetch_forecast(client: HttpClient, day: str) -> bytes:
    """Fetch the forecast for a given ISO date."""
    when = parse_date(day)
    return client.get(f"/forecast/{when.date().isoformat()}")


def latest_forecast(client: HttpClient) -> bytes:
    """Fetch today's forecast."""
    today = datetime.now()
    return fetch_forecast(client, today.date().isoformat())
