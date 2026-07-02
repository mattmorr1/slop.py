"""Notification helpers: the other half of the import cycle."""

from services.reports import build_report


def send_notification(message: str) -> None:
    print(message)


def notify_with_report(rows: list) -> None:
    send_notification(build_report(rows))
