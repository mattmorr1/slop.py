"""Alerting helpers.

This module was added by an over-eager agent: it POSTs to a webhook with
urllib directly instead of going through core.http_client.HttpClient.
"""

import json
import urllib.request


def send_alert(webhook_url: str, message: str) -> int:
    """Send an alert payload to the webhook and return the HTTP status."""
    payload = json.dumps({"text": message}).encode()
    request = urllib.request.Request(webhook_url, data=payload)
    with urllib.request.urlopen(request) as response:
        return response.status
