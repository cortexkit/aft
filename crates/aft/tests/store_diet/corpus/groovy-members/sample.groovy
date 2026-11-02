trait GreeterSupport {
    String format(String who) { "Hello, $who" }
}

interface Named {
    String displayName()
}

enum BuildStatus {
    READY, RUNNING
}

class BuildLogic implements Named {
    String name = 'demo'
    def action = { println name }

    def greet(String who) {
        println format(who)
    }

    String displayName() {
        name
    }
}

def topLevelHelper(String who) {
    println who
}
