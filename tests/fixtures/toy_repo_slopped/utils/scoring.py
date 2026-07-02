"""Scoring helpers."""

import urllib.request


def compute_risk_score(user_id: str) -> float:
    """Compute the risk score for a user."""
    # slop: allow purity-lie — legacy public name, kept for API compat
    with urllib.request.urlopen(f"https://risk.example.com/{user_id}") as resp:
        return float(resp.read())


def parse_config(path: str) -> str:
    """Parse the scoring config file."""
    with open(path) as handle:
        return handle.read()
