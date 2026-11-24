pub mod bash_background {
    pub use crate::bash_background::spawn;
    pub use aft_shell::test_bridge::bash_background::*;
    #[cfg(unix)]
    pub mod push_projection_fixture {
        pub use crate::shell_test_fixtures::push_projection::{busy_root, BusyRoot};
    }
}

pub mod bash_rewrite {
    pub use aft_shell::test_bridge::bash_rewrite::*;
    pub mod dispatch {
        use crate::bash_rewrite::dispatch::{self, RewriteDispatch};
        use crate::context::AppContext;
        use crate::host_tool_names::HostToolNames;
        use crate::protocol::Response;

        pub fn count_route_records_for_prefix(prefix: &str) -> usize {
            dispatch::count_route_records_for_prefix(prefix)
        }

        pub fn dispatch_for_request_with_handlers(
            command: &str,
            request_id: &str,
            session_id: Option<&str>,
            host_tool_names: &HostToolNames,
            ctx: &AppContext,
            handlers: Option<&RewriteDispatch>,
        ) -> Option<Response> {
            dispatch::dispatch_for_request_with_handlers(
                command,
                request_id,
                session_id,
                host_tool_names,
                ctx,
                handlers,
            )
        }
    }
}
