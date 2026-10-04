pub fn product() {}
#[cfg(test)]
mod checks {
    struct Helper;
    enum Mode { First, Second }
    fn helper() {}
    #[test]
    fn first() {}
    mod nested {
        #[test]
        fn second() {}
    }
}
pub fn after() {}
