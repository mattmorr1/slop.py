"""Reporting: half of an agent-introduced import cycle."""

from services.notify import send_notification


def build_report(rows: list) -> str:
    """Render rows into a report body."""
    body = "\n".join(str(row) for row in rows)
    send_notification("report ready")
    return body
