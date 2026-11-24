#[allow(unused_imports)]
pub(crate) use aft_base::secret_proxy::tests::*;

// Client::spawn re-execs secret_proxy::tests::client_child in its caller's harness.
#[test]
fn client_child() {
    aft_base::secret_proxy::tests::client_child();
}
