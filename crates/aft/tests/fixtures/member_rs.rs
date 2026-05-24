pub struct Config {
    pub name: String,
    pub value: i32,
}

impl Config {
    pub fn new(name: String, value: i32) -> Self {
        Self { name, value }
    }

    pub fn display(&self) -> String {
        format!("{}: {}", self.name, self.value)
    }
}

struct EmptyStruct {
}
