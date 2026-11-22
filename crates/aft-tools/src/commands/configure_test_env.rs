mod tests {
    #[test]
    fn swept_test_mutexes_do_not_bare_unwrap_locks() {
        fn without_whitespace(source: &str) -> String {
            source
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        }

        let test_env = without_whitespace(include_str!("../../../aft-base/src/test_env.rs"));
        let configure = without_whitespace(include_str!("configure.rs"));

        assert!(test_env.contains("lock_test_mutex(process_env_mutex())"));
        let process_env_bare_unwrap = ["process_env_mutex()", ".lock()", ".unwrap()"].concat();
        assert!(!test_env.contains(&process_env_bare_unwrap));
        assert!(!configure.contains(&process_env_bare_unwrap));
    }
}
