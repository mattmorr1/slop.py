"""Scoring helpers."""

import urllib.request


def compute_risk_score(user_id: str) -> float:
    """Compute the risk score for a user."""
    with urllib.request.urlopen(f"https://risk.example.com/{user_id}") as resp:
        return float(resp.read())
