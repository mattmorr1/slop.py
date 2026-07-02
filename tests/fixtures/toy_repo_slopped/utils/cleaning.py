"""Row cleaning utilities: agent-planted duplication."""


def clean_rows(rows: list) -> list:
    out = []
    for row in rows:
        value = row.strip().lower()
        if value and value not in out:
            out.append(value)
    return out


def normalize_rows(rows: list) -> list:
    # normalize each row of the input
    out = []
    for row in rows:
        value = row.strip().lower()
        # skip empties and duplicates
        if value and value not in out:
            out.append(value)
    return out


def scale_rows(rows: list) -> list:
    acc = []
    for item in rows:
        cleaned = item.strip().upper()
        if cleaned and cleaned not in acc:
            acc.append(cleaned)
    return acc
