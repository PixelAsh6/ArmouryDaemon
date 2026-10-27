//! The process-wide HTTP client, moved here from `catalog.rs` (Phase C1): the console
//! binary has no catalogue machinery of its own, but the band's thumbnails and the cover
//! art helpers share this client.

use std::sync::OnceLock;

use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig};

/// One agent, built once and shared by every fetch — the catalogue, the cover art and
/// the console band's thumbnails: it validates TLS against the **Windows certificate
/// store** — so a machine with a corporate CA, or one whose TLS is intercepted by a
/// proxy, can still reach the catalogue and the art — it reads the user's WinINet proxy
/// settings, and it keeps one connection pool for the whole process.
///
/// The agent is built on the first fetch and not at startup: the daemon's idle path never
/// touches the network, and the platform verifier opens the certificate store when the
/// first TLS connection is made.
///
/// Every fetch goes through this agent. The build carries no other root store, so a request
/// that went anywhere else could not be validated at all.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = Config::builder()
            .tls_config(tls)
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .build();
        ureq::Agent::new_with_config(config)
    })
}
