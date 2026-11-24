pub mod fake_lsp;
pub mod idle_fixture;

pub fn child_pid_for_test(client: &super::client::LspClient) -> u32 {
    client.child_pid()
}
