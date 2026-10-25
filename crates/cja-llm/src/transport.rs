//! The shared HTTP transport to the LLM gateway.
//!
//! `GatewayHttp` implements rig's [`HttpClientExt`] over a `setec::Dialer`:
//! the host network by default, or an in-process tailnet node. Everything the
//! crate sends goes through it — staged retries, timeouts, tracing, and the
//! two-layer stripping of key-shaped headers.

/// Marker prefix on [`TransportError::Connect`] variants. The classifier in
/// `error.rs` matches this substring in rig's relayed error text to classify
/// connect-phase failures; keep it stable.
pub(crate) const CONNECT_MARKER: &str = "cja-llm transport connect:";

/// Marker prefix on [`TransportError::Timeout`]. Like [`CONNECT_MARKER`], the
/// classifier depends on this exact string.
pub(crate) const TIMEOUT_MARKER: &str = "cja-llm transport timeout:";

/// Response-less transport failures, wrapped into
/// `rig_core::http_client::Error::Instance` for rig.
///
/// The `Display` strings are load-bearing: they start with the marker
/// constants and ride verbatim into rig's error text on both paths rig can
/// surface them (`PromptError::Provider` via `ProviderError::Http`, and
/// `PromptError::Report` via the serve adapter, whose `message` is the
/// source's `Display`). The classifier in `error.rs` matches the markers
/// there, so never reword these casually.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// The connect phase (dial, TLS, HTTP handshake) exceeded its budget.
    /// Nothing was sent.
    #[error(
        "cja-llm transport connect: timed out after {0:?} before the request was sent; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    ConnectTimeout(std::time::Duration),
    /// The connect phase failed outright. Nothing was sent.
    #[error(
        "cja-llm transport connect: failed before the request was sent: {source}; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    Connect {
        #[source]
        source: std::io::Error,
    },
    /// The request deadline elapsed after the request was queued to the
    /// connection. The gateway drains and bills such attempts upstream, so
    /// this is never retried.
    #[error(
        "cja-llm transport timeout: request exceeded its deadline after the request was sent ({0:?} budget; the gateway may still bill the attempt)"
    )]
    Timeout(std::time::Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classifier's only signal on the relayed agent path: pin the exact
    /// marker prefixes so a reword is caught as a test failure, not a silent
    /// misclassification.
    #[test]
    fn transport_error_displays_start_with_their_markers() {
        let connect_timeout = TransportError::ConnectTimeout(std::time::Duration::from_secs(10));
        assert!(connect_timeout.to_string().starts_with(CONNECT_MARKER));

        let connect = TransportError::Connect {
            source: std::io::Error::other("refused"),
        };
        assert!(connect.to_string().starts_with(CONNECT_MARKER));

        let timeout = TransportError::Timeout(std::time::Duration::from_secs(120));
        assert!(timeout.to_string().starts_with(TIMEOUT_MARKER));
    }
}
