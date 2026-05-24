#[derive(Debug)]
pub struct User {
    pub name: String,
    pub age: u32,
}

pub struct Config {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Serialize)]
pub enum Status {
    Active,
    Inactive,
}
