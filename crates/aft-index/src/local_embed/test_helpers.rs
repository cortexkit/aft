    static NATIVE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn native_test() -> std::sync::MutexGuard<'static, ()> {
        NATIVE_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_for_sessions(expected: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while super::session_memory().sessions != expected && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(super::session_memory().sessions, expected);
    }
