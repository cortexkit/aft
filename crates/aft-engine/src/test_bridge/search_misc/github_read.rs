use crate::github_read::{self, GithubReadDeferred, GithubReadEngine, GithubReadLateAnswer};

pub fn stale_thread_note_for_test() -> &'static str {
    github_read::stale_thread_note_for_test()
}

pub trait GithubReadDeferredTestExt {
    fn late_answer_for_test(&self) -> &GithubReadLateAnswer;
}

impl GithubReadDeferredTestExt for GithubReadDeferred {
    fn late_answer_for_test(&self) -> &GithubReadLateAnswer {
        self.late_answer_fixture()
    }
}

pub trait GithubReadEngineTestExt {
    fn flights_empty_for_test(&self) -> bool;
}

impl GithubReadEngineTestExt for GithubReadEngine {
    fn flights_empty_for_test(&self) -> bool {
        self.flights_empty_fixture()
    }
}
