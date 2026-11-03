<?php
namespace Demo;

interface Named {
    public function name(): string;
}

trait Logs {
    public function log(string $msg): void {}
}

class Greeter {
    private string $name;

    public function greet(string $who): string {
        return "Hi $who";
    }
}

function helper(): void {}

enum Color { case Red; case Green; }
