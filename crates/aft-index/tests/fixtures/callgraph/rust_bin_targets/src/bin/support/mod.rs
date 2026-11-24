pub mod admin_client;
pub mod route_client;

pub fn run() {
    admin_client::first(&[]);
}
