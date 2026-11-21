#![allow(dead_code)]

extern crate self as aft;

mod lsp {
    pub mod jsonrpc {
        include!("../../aft-engine/src/lsp/jsonrpc.rs");
    }
    pub mod transport {
        include!("../../aft-engine/src/lsp/transport.rs");
    }
}

// Keep formatting of the shared fixture owned by the AFT test crate.
#[rustfmt::skip]
#[path = "../../aft/tests/lsp/fake_server.rs"]
mod server;

fn main() -> std::io::Result<()> {
    server::main()
}
