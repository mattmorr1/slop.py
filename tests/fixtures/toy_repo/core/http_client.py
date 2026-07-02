"""Sanctioned network channel: all HTTP goes through HttpClient."""

import urllib.request


class HttpClient:
    """Thin wrapper over urllib with retries."""

    def __init__(self, base_url: str, retries: int = 3):
        self.base_url = base_url
        self.retries = retries

    def get(self, path: str) -> bytes:
        """Fetch a path relative to base_url."""
        url = self.base_url + path
        last_error = None
        for _ in range(self.retries):
            try:
                with urllib.request.urlopen(url) as response:
                    return response.read()
            except OSError as exc:
                last_error = exc
        raise last_error
