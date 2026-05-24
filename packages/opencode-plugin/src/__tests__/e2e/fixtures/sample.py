DEFAULT_NAME = "world"
MAX_COUNT = 3


def greet(name: str) -> str:
    return f"Hello, {name}"


def combine(parts: list[str]) -> str:
    return "-".join(parts)


class SampleTool:
    def upper(self, value: str) -> str:
        return value.upper()

    def lower(self, value: str) -> str:
        return value.lower()
