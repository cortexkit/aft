def plain_function():
    return 42


def another_plain():
    x = 1
    return x


@app.route("/users")
def get_users():
    return []


@login_required
@app.route("/admin")
def admin_panel():
    return "admin"


class MyService:
    def process(self):
        pass

    @staticmethod
    def helper():
        return True
