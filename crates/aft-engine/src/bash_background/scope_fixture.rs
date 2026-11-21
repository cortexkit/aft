use super::*;

pub struct StopOnDrop(String);

impl StopOnDrop {
    pub fn new(task: String) -> Self {
        Self(task)
    }
}
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Ok(Some(scope)) = crate::bash_background::task_scope::lookup(&self.0) {
            let _ = scope.stop();
        }
    }
}

pub fn require_manager() {
    crate::bash_background::task_scope::probe_user_manager(Duration::from_secs(1))
        .expect("requires a systemd user manager");
}

pub fn scope_stopped(events: &Path) -> bool {
    match std::fs::read_to_string(events) {
        Ok(text) => text.lines().any(|line| line == "populated 0"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => panic!("cannot read cgroup.events: {error}"),
    }
}

pub fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "task scope condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
