"""Root attribution for bounded daemon index-event records."""

import hashlib
import re


def index_event_root_matches(logged_root: str, root: str) -> bool:
    """Match full roots or hashed abbreviations without conflating sibling worktrees."""
    normalized = root.replace("\\", "/")
    sanitized = "".join(
        ch if "!" <= ch <= "~" and ch != "=" else "_" for ch in normalized
    ) or "-"
    if logged_root == sanitized:
        return True
    digest = hashlib.sha256(sanitized.encode()).hexdigest()[:8]
    match = re.fullmatch(r"\.\.\./([0-9a-f]{8})(?:/(.*))?", logged_root)
    return bool(
        match
        and match.group(1) == digest
        and (match.group(2) is None or sanitized.endswith(match.group(2)))
    )
