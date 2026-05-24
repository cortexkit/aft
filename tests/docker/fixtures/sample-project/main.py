"""Sample Python project for testing aft outline."""


def greet(name: str) -> str:
    """Return a greeting message."""
    return f"Hello, {name}!"


class UserService:
    """Handles user operations."""

    def __init__(self, db_url: str):
        self.db_url = db_url

    def get_user(self, user_id: int) -> dict:
        """Fetch a user by ID."""
        return {"id": user_id, "name": "test"}


if __name__ == "__main__":
    print(greet("world"))
