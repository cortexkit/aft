"""Sample Python module for tree-sitter symbol extraction tests."""


def top_level_function(x: int, y: int) -> int:
    """A top-level function."""
    return x + y


class MyClass:
    """A sample class with methods."""

    def __init__(self, name: str):
        self.name = name

    def instance_method(self) -> str:
        return self.name


@staticmethod
def decorated_function():
    """A decorated top-level function."""
    pass


class OuterClass:
    """A class containing a nested class."""

    class InnerClass:
        def inner_method(self):
            pass

    def outer_method(self):
        pass
