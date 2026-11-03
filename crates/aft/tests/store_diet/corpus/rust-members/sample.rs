/// Sample Rust module for tree-sitter symbol extraction tests.

pub fn public_function(x: i32) -> i32 {
    x + 1
}

fn private_function() {
    // not exported
}

pub struct MyStruct {
    pub field: String,
}

pub enum Color {
    Red,
    Green,
    Blue,
}

pub trait Drawable {
    fn draw(&self);
    fn area(&self) -> f64;
}

impl MyStruct {
    pub fn new(field: String) -> Self {
        Self { field }
    }

    fn helper(&self) -> usize {
        self.field.len()
    }
}

impl Drawable for MyStruct {
    fn draw(&self) {
        println!("drawing {}", self.field);
    }

    fn area(&self) -> f64 {
        0.0
    }
}
