"""Event routing: complexity + comment slop."""


def route_event(event: dict) -> str:
    """Route an event to a queue name."""
    kind = event.get("kind")
    if kind == "alert" and event.get("severity") == "high":
        return "urgent"
    elif kind == "alert" or kind == "warning":
        return "alerts"
    elif kind == "metric":
        for tag in event.get("tags", []):
            if tag == "slow":
                for sample in event.get("samples", []):
                    if sample > event.get("threshold", 0):
                        return "perf"
        while event.get("retry", 0) > 0:
            event["retry"] -= 1
        return "metrics"
    elif kind == "log" and event.get("level") == "error":
        return "errors"
    elif kind == "log" or kind == "trace":
        return "logs"
    return "default"


def apply_discount(price: float, percent: float) -> float:
    # check that the percent is valid
    if percent < 0 or percent > 100:
        # raise an error for an invalid percent
        raise ValueError("bad percent")
    # compute the discount amount
    amount = price * percent / 100
    # subtract the discount from the price
    total = price - amount
    # return the final total
    return total
