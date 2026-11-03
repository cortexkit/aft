package demo;

public class Greeter {
    private String name;

    public Greeter(String name) {
        this.name = name;
    }

    public String greet(String who) {
        return "Hi " + who;
    }
}

interface Named {
    String name();
}

enum Color { RED, GREEN }

record Point(int x, int y) {}
