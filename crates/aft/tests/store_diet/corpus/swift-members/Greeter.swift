struct Greeter {
    let name: String

    func greet(_ who: String) -> String {
        return "Hi \(who)"
    }
}

class Service {
    func run() {}
}

protocol Named {
    func name() -> String
}

enum Color { case red, green }

typealias Name = String
