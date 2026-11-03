macro_rules! runtime_methods {
    () => {
        async fn poll_session(&self, id: &str) {
            if self.handle_refusal(id).await {
                return;
            }
        }

        async fn handle_refusal(&self, id: &str) -> bool {
            id.is_empty()
        }
    };
}

pub struct Runtime;

impl Runtime {
    runtime_methods!();
}
