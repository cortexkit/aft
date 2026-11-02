//! Checks for reads the Occipital display server (`reserved:occipital`) makes
//! for an app. The daemon stamps each such route with the session scope it
//! acts for, and a read is admitted only against that stamped session;
//! reading any other session needs that session's own authenticated route.
use subc_protocol::ErrorBody;

pub(crate) const OCCIPITAL_PRINCIPAL: &str = "reserved:occipital";

pub(crate) fn refusal(code: &str) -> ErrorBody {
    ErrorBody::new(code, super::operations::fixed_message(code))
}

/// There are no provider reads in the shipped views. Tests supply operation
/// policies to check read-only access to the stamped session and to ensure a
/// repeated call key replays the same answer rather than executing again.
pub(super) fn operation_policy() -> Option<serde_json::Value> {
    #[cfg(test)]
    {
        return TEST_POLICY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(test)]
static TEST_POLICY: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);
#[cfg(test)]
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) struct TestPolicyGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
}
#[cfg(test)]
impl Drop for TestPolicyGuard {
    fn drop(&mut self) {
        *TEST_POLICY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
#[cfg(test)]
pub(crate) fn set_test_manifest_for_test(bytes: Option<Vec<u8>>) -> TestPolicyGuard {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *TEST_POLICY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        bytes.map(|bytes| serde_json::from_slice(&bytes).expect("carrier operation fixture"));
    TestPolicyGuard { _serial: serial }
}
