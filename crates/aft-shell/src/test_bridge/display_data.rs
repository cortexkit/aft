pub fn with_test_reader<T>(reader: Option<&str>, run: impl FnOnce() -> T) -> T {
    crate::late_result_bus::with_test_reader(reader, run)
}

pub mod late_result_bus {
    pub fn hooks() -> Option<&'static crate::late_result_bus::Hooks> {
        crate::late_result_bus::hooks()
    }

    pub async fn card_wake(body: &[u8], msg_id: &str) -> Result<(), &'static str> {
        crate::late_result_bus::readers::card_wake(body, msg_id).await
    }

    pub mod readers {
        pub struct TestAttachment(crate::late_result_bus::readers::TestAttachment);

        impl TestAttachment {
            pub fn new(id: &str, attached: bool) -> Self {
                Self(crate::late_result_bus::readers::TestAttachment::new(
                    id, attached,
                ))
            }

            pub fn state(&self) -> &tokio::sync::watch::Sender<bool> {
                &self.0.state
            }
        }
    }

    pub mod status {
        pub use crate::late_result_bus::status::test_writer::put_session;

        pub fn test_value(session: &str) -> Option<serde_json::Value> {
            crate::late_result_bus::status::test_value(session)
        }
    }

    pub mod display {
        pub use crate::late_result_bus::display::ready::ReadyPublisher;
        use crate::late_result_bus::{self, DisplayPublish, Hooks};

        pub async fn announce(
            hooks: Option<&Hooks>,
            id: String,
            pull_bytes: Vec<u8>,
            publish: DisplayPublish<'_>,
        ) -> Result<(), &'static str> {
            late_result_bus::display::announce(hooks, id, pull_bytes, publish).await
        }

        pub fn body(id: &str) -> Result<Vec<u8>, &'static str> {
            late_result_bus::display::body(id)
        }

        pub async fn publish_one(publisher: &(impl ReadyPublisher + Sync), hooks: Option<&Hooks>) {
            late_result_bus::display::publish_one(publisher, hooks).await
        }
    }

    pub mod views {
        pub use crate::late_result_bus::views::{Publication, RecordingWriter, Writer};

        pub fn key(pin: &str) -> String {
            crate::late_result_bus::views::key(pin)
        }
    }
}
