use std::sync::OnceLock;

use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;

static PLATFORM_VERIFIER_CONFIG: OnceLock<Result<ClientConfig, String>> = OnceLock::new();

/// Return the process-wide platform-verifier TLS configuration.
///
/// The configuration is initialized only when the first HTTPS-capable client is
/// built. Cloning it for reqwest is cheap because the verifier and its shared
/// state are reference counted; importantly, native trust decisions remain
/// connection-time work instead of per-client root-store enumeration.
pub(crate) fn client_config() -> Result<ClientConfig, String> {
    PLATFORM_VERIFIER_CONFIG
        .get_or_init(|| {
            // Reqwest enables rustls' ring provider, but does not install a
            // process default provider for callers that supply their own config.
            let _ = rustls::crypto::ring::default_provider().install_default();
            ClientConfig::with_platform_verifier()
                .map_err(|error| format!("failed to create platform TLS verifier: {error}"))
        })
        .clone()
}

static BLOCKING_CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();

/// Share reqwest's blocking runtime across roots. Deadlines belong to requests,
/// not clients, so one root's budget cannot change another root's connection pool.
pub(crate) fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    BLOCKING_CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(5))
                .use_preconfigured_tls(client_config()?)
                .build()
                .map_err(|error| format!("failed to configure HTTP client: {error}"))
        })
        .clone()
}
