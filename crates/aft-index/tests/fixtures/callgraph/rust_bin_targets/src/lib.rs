pub mod route_client;

pub fn lib_target() {}

#[cfg(test)]
mod tests {
    #[test]
    fn calls_lib_target() {
        super::lib_target();
    }
}
