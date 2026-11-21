use super::*;

#[derive(Clone)]
pub struct TestPartners {
    pub(crate) seal_failure: Option<Refusal>,
    pub(crate) expect_placed: Option<&'static str>,
    pub(crate) run_placed: Option<&'static str>,
    pub(crate) reserved_send_placed: bool,
    pub(crate) bindings: Arc<dyn Fn(&str, &str) -> Result<Value, Refusal> + Send + Sync>,
    pub(crate) reserve: Arc<
        dyn Fn(
                &str,
                &str,
                &ScopeStamp,
                Value,
            ) -> Result<super::super::partner::ReservedBotRequest, Refusal>
            + Send
            + Sync,
    >,
    pub(crate) before_final_gate: Option<Arc<dyn Fn() + Send + Sync>>,
}
