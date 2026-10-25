//! Gateway-aware error taxonomy.
//!
//! Every gateway reply is classified by HTTP status first, then by the exact
//! wrapper bodies (`{"error":"..."}`) or the Bifrost envelope's `type` field.
//! The classification is shared: the convenience functions and raw app agents
//! surface identical variants, because [`Error::from_prompt_error`] is the
//! single classifier both go through.
//!
//! # Cause chains
//!
//! Every variant that wraps a rig failure carries the original
//! [`PromptError`] as its `source`. On the direct provider path the chain
//! holds rig's typed `ProviderError`; on the relayed agent path (the serve
//! adapter) it holds the `ErrorReport`'s `message` plus `source_chain`
//! strings. Either way `source()` keeps walking to the underlying error.

use std::fmt;

use rig_agent::completion::{PromptError, StructuredOutputError};
use rig_core::error::ProviderError;

use crate::transport::{CONNECT_MARKER, TIMEOUT_MARKER};

const IDENTITY_403_BODY: &str = "forbidden: caller identity could not be determined";
const ADMIN_403_BODY: &str = "admin only";

/// The result type for cja-llm.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything cja-llm can fail with, classified from the gateway's contract.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Gateway `403 {"error":"forbidden: caller identity could not be
    /// determined"}`: tailnet `WhoIs` failed, so no upstream call was made.
    /// Check the tailnet policy grant that lets this node reach `llm:443`
    /// (e.g. a grant to `tag:llm` on port 443).
    #[error(
        "gateway could not identify this node's tailnet identity; check the tailnet policy grant that lets this node reach llm:443 (e.g. a grant to tag:llm on port 443): {body}"
    )]
    IdentityRejected {
        /// The gateway's reply body.
        body: String,
        #[source]
        source: Box<PromptError>,
    },
    /// Gateway HTTP 402 (Bifrost governance `policy_budget_exceeded`), matched
    /// on status, never a body string. Non-retryable: it fires once accrued
    /// spend reaches the limit, so the crossing request completed and this one
    /// was refused.
    #[error("monthly budget exceeded for this app's tailnet identity (HTTP 402)")]
    BudgetExceeded {
        /// The gateway's reply body.
        body: String,
        #[source]
        source: Box<PromptError>,
    },
    /// Gateway `403 {"error":"admin only"}` — a programming error (wrong
    /// route); cja-llm only uses `POST /v1/chat/completions`.
    #[error("admin-only gateway route; cja-llm only uses POST /v1/chat/completions: {body}")]
    AdminOnly {
        /// The gateway's reply body.
        body: String,
        #[source]
        source: Box<PromptError>,
    },
    /// Any other provider or gateway reply: Bifrost's error envelope with a
    /// provider status, a stale virtual key after its one retry, or an
    /// unexpected status.
    #[error("upstream provider error (HTTP {status})")]
    Upstream {
        /// The reply's HTTP status.
        status: u16,
        /// The reply body.
        body: String,
        #[source]
        source: Box<PromptError>,
    },
    /// The total deadline (non-streaming request or response head) or the
    /// inter-chunk idle timeout elapsed. The attempt may still have been
    /// billed upstream — the gateway runs inference to completion even when
    /// the client gives up — so it is never retried.
    #[error("gateway request timed out; the gateway may still have billed the attempt")]
    Timeout {
        #[source]
        source: Box<PromptError>,
    },
    /// Dial, TLS, or handshake failure or a connect timeout: nothing was
    /// sent. Retryable within the retry policy. A node without the tailnet
    /// policy grant to `llm:443` fails here, not with a 403.
    #[error(
        "could not reach the gateway; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    Connect {
        #[source]
        source: Box<PromptError>,
    },
    /// Structured output failed to parse; `raw` is the model's output text.
    #[error("structured output did not parse")]
    SchemaParse {
        /// The model's output text that failed to parse.
        raw: String,
        #[source]
        error: serde_json::Error,
    },
    /// The model returned no accepted content for a typed extraction.
    #[error("the model returned no structured output")]
    EmptyStructuredOutput,
    /// Anything else rig reported (max turns, memory, decode, ...). The chain
    /// is preserved.
    ///
    /// Deliberately not `#[from]`: the hand-written [`From<PromptError>`]
    /// classifies gateway replies first, so `?` in app agent code still gets
    /// the gateway variants.
    #[error(transparent)]
    Rig { source: Box<PromptError> },
    /// Invalid cja-llm configuration.
    #[error("invalid cja-llm configuration: {0}")]
    Config(String),
}

impl From<PromptError> for Error {
    fn from(err: PromptError) -> Self {
        Self::from_prompt_error(err)
    }
}

/// [`fmt::Debug`] never prints content. The `raw`/`body` fields print as
/// byte lengths only, and every boxed [`PromptError`] source prints through
/// its `Display` with this variant's own content redacted out of it —
/// `PromptError::MaxTurns`'s derived `Debug` would print the whole
/// conversation, and a provider-response `PromptError`'s `Display` prints
/// the reply body (`ProviderResponseError: status ..: {body}`). Apps
/// return `cja_llm::Result` from `main`, whose failure path prints
/// `Error: {:?}`, so a derived `Debug` would leak model output and reply
/// bodies into logs.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// A content-bearing string printed as its byte length only.
        struct Redacted<'a>(&'a str);

        impl fmt::Debug for Redacted<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "<{} bytes>", self.0.len())
            }
        }

        /// The source rendered through its `Display` (never its `Debug`),
        /// with `redact`'s strings replaced by byte-length markers so a
        /// provider-response body cannot ride along in the chain.
        fn source_field(source: &PromptError, redact: &[&str]) -> String {
            let mut text = source.to_string();
            for secret in redact {
                if !secret.is_empty() {
                    text = text.replace(secret, &format!("<redacted {} bytes>", secret.len()));
                }
            }
            text
        }

        match self {
            Self::IdentityRejected { body, source } => f
                .debug_struct("IdentityRejected")
                .field("body", &Redacted(body))
                .field("source", &source_field(source, &[body.as_str()]))
                .finish(),
            Self::BudgetExceeded { body, source } => f
                .debug_struct("BudgetExceeded")
                .field("body", &Redacted(body))
                .field("source", &source_field(source, &[body.as_str()]))
                .finish(),
            Self::AdminOnly { body, source } => f
                .debug_struct("AdminOnly")
                .field("body", &Redacted(body))
                .field("source", &source_field(source, &[body.as_str()]))
                .finish(),
            Self::Upstream {
                status,
                body,
                source,
            } => f
                .debug_struct("Upstream")
                .field("status", status)
                .field("body", &Redacted(body))
                .field("source", &source_field(source, &[body.as_str()]))
                .finish(),
            Self::Timeout { source } => f
                .debug_struct("Timeout")
                .field("source", &source_field(source, &[]))
                .finish(),
            Self::Connect { source } => f
                .debug_struct("Connect")
                .field("source", &source_field(source, &[]))
                .finish(),
            Self::SchemaParse { raw, error } => f
                .debug_struct("SchemaParse")
                .field("raw", &Redacted(raw))
                // The serde error carries only a message plus line/column.
                .field("error", error)
                .finish(),
            Self::EmptyStructuredOutput => f.write_str("EmptyStructuredOutput"),
            Self::Rig { source } => f
                .debug_struct("Rig")
                .field("source", &source_field(source, &[]))
                .finish(),
            Self::Config(message) => f.debug_tuple("Config").field(message).finish(),
        }
    }
}

impl From<StructuredOutputError> for Error {
    fn from(err: StructuredOutputError) -> Self {
        match err {
            StructuredOutputError::Deserialization { output, error } => {
                Self::SchemaParse { raw: output, error }
            }
            StructuredOutputError::EmptyResponse => Self::EmptyStructuredOutput,
            StructuredOutputError::Prompt(prompt) => Self::from_prompt_error(prompt),
        }
    }
}

impl Error {
    /// Classifies a rig [`PromptError`] into the gateway-aware taxonomy.
    ///
    /// Status first: a preserved provider reply decides by HTTP status, with
    /// the Bifrost body `type` checked before the gateway wrapper's `error`
    /// strings so a Bifrost envelope is never mistaken for the wrapper's
    /// identity 403. Response-less failures classify by the transport's
    /// stable marker strings, which ride verbatim into rig's error text on
    /// both the `Provider` and relayed `Report` paths.
    pub fn from_prompt_error(err: PromptError) -> Self {
        if let Some(status) = err.provider_response_status() {
            let status = status.as_u16();
            let body = err.provider_response_body().unwrap_or_default().to_owned();
            let json = err.provider_response_json().ok().flatten();
            let body_type = json
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(serde_json::Value::as_str);
            let body_error = json
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(serde_json::Value::as_str);
            return match status {
                402 => Self::BudgetExceeded {
                    body,
                    source: Box::new(err),
                },
                // A Bifrost envelope (stale virtual key) must win over the
                // gateway wrapper's 403 strings.
                401 | 403 if matches!(body_type, Some("access_not_found" | "access_blocked")) => {
                    Self::Upstream {
                        status,
                        body,
                        source: Box::new(err),
                    }
                }
                403 if body_error == Some(IDENTITY_403_BODY) => Self::IdentityRejected {
                    body,
                    source: Box::new(err),
                },
                403 if body_error == Some(ADMIN_403_BODY) => Self::AdminOnly {
                    body,
                    source: Box::new(err),
                },
                _ => Self::Upstream {
                    status,
                    body,
                    source: Box::new(err),
                },
            };
        }
        let text = err.to_string();
        if text.contains(CONNECT_MARKER) {
            Self::Connect {
                source: Box::new(err),
            }
        } else if text.contains(TIMEOUT_MARKER) {
            Self::Timeout {
                source: Box::new(err),
            }
        } else {
            Self::Rig {
                source: Box::new(err),
            }
        }
    }

    /// Classifies a raw [`ProviderError`] the same way a [`PromptError`]
    /// surfaces it, for callers holding the provider error directly.
    pub fn from_provider_error(err: ProviderError) -> Self {
        Self::from_prompt_error(PromptError::from(err))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rig_agent::completion::PromptError;
    use rig_core::error::{ErrorKind, ErrorReport, ProviderError};
    use rig_core::http_client;
    use serde_json::json;

    use super::*;
    use crate::transport::TransportError;

    fn provider_with_response(status: u16, body: &str) -> PromptError {
        PromptError::Provider(ProviderError::from_http_response(
            http::StatusCode::from_u16(status).unwrap(),
            body,
        ))
    }

    fn report(message: &str) -> PromptError {
        PromptError::Report(ErrorReport {
            kind: ErrorKind::Http,
            retryable: false,
            message: message.to_owned(),
            code: None,
            http_status: None,
            refusal: false,
            source_chain: Vec::new(),
            request_id: None,
            provider_response: None,
        })
    }

    #[test]
    fn status_402_is_budget_exceeded_on_any_body() {
        let error = Error::from_prompt_error(provider_with_response(
            402,
            r#"{"type":"budget_exceeded","error":{"message":"spend cap"}}"#,
        ));
        assert!(matches!(error, Error::BudgetExceeded { .. }), "{error:?}");
    }

    #[test]
    fn identity_403_classifies_with_the_grant_hint() {
        let error = Error::from_prompt_error(provider_with_response(
            403,
            r#"{"error":"forbidden: caller identity could not be determined"}"#,
        ));
        let Error::IdentityRejected { body, .. } = &error else {
            panic!("unexpected {error:?}");
        };
        assert!(error.to_string().contains("tag:llm"), "{}", error);
        assert!(body.contains("identity"), "{body}");
    }

    #[test]
    fn admin_403_is_admin_only() {
        let error =
            Error::from_prompt_error(provider_with_response(403, r#"{"error":"admin only"}"#));
        assert!(matches!(error, Error::AdminOnly { .. }), "{error:?}");
    }

    #[test]
    fn bifrost_envelope_wins_over_the_wrapper_403() {
        // A 403 with a Bifrost `type` (stale virtual key) is Upstream, not
        // IdentityRejected, even though the status matches the wrapper's.
        let error = Error::from_prompt_error(provider_with_response(
            403,
            r#"{"type":"access_blocked","error":{"message":"key blocked"}}"#,
        ));
        let Error::Upstream { status, .. } = &error else {
            panic!("unexpected {error:?}");
        };
        assert_eq!(*status, 403);
    }

    #[test]
    fn stale_virtual_key_401_is_upstream() {
        let error = Error::from_prompt_error(provider_with_response(
            401,
            r#"{"type":"access_not_found","error":{"message":"stale"}}"#,
        ));
        assert!(
            matches!(error, Error::Upstream { status: 401, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn other_statuses_are_upstream_with_status_and_body() {
        let error = Error::from_prompt_error(provider_with_response(500, "boom"));
        let Error::Upstream { status, body, .. } = &error else {
            panic!("unexpected {error:?}");
        };
        assert_eq!(*status, 500);
        assert_eq!(body, "boom");
    }

    #[test]
    fn relayed_report_classifies_by_status_too() {
        // The agent runtime surfaces failures as ErrorReport; a preserved
        // provider response must classify there as well.
        let err = PromptError::Report(ErrorReport {
            kind: ErrorKind::ProviderResponse,
            retryable: false,
            message: "provider reply".to_owned(),
            code: None,
            http_status: Some(402),
            refusal: false,
            source_chain: Vec::new(),
            request_id: None,
            provider_response: Some(
                ProviderError::from_http_response(
                    http::StatusCode::PAYMENT_REQUIRED,
                    r#"{"type":"budget_exceeded"}"#,
                )
                .provider_response()
                .cloned()
                .unwrap(),
            ),
        });
        assert!(matches!(
            Error::from_prompt_error(err),
            Error::BudgetExceeded { .. }
        ),);
    }

    #[test]
    fn timeout_marker_classifies_on_the_report_path() {
        let message = format!(
            "HttpError: Http client error: {}",
            TransportError::Timeout(std::time::Duration::from_secs(120))
        );
        assert!(matches!(
            Error::from_prompt_error(report(&message)),
            Error::Timeout { .. }
        ));
    }

    #[test]
    fn connect_marker_classifies_on_the_report_path() {
        let message = format!(
            "HttpError: Http client error: {}",
            TransportError::Connect {
                source: std::io::Error::other("no route")
            }
        );
        let error = Error::from_prompt_error(report(&message));
        assert!(matches!(error, Error::Connect { .. }), "{error:?}");
    }

    #[test]
    fn timeout_marker_classifies_on_the_provider_path() {
        let err =
            PromptError::Provider(ProviderError::Http(Arc::new(http_client::Error::instance(
                TransportError::Timeout(std::time::Duration::from_secs(120)),
            ))));
        assert!(matches!(
            Error::from_prompt_error(err),
            Error::Timeout { .. }
        ));
    }

    #[test]
    fn other_prompt_errors_fall_through_to_rig() {
        let err = PromptError::MaxTurns {
            max_turns: 8,
            chat_history: Vec::new(),
            prompt: rig_core::completion::Message::user("hi"),
        };
        assert!(matches!(Error::from_prompt_error(err), Error::Rig { .. }));
    }

    #[test]
    fn structured_output_errors_map_to_typed_variants() {
        let deserialization = StructuredOutputError::Deserialization {
            output: "not json".to_owned(),
            error: serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
        };
        let Error::SchemaParse { raw, .. } = Error::from(deserialization) else {
            panic!("expected SchemaParse");
        };
        assert_eq!(raw, "not json");

        assert!(matches!(
            Error::from(StructuredOutputError::EmptyResponse),
            Error::EmptyStructuredOutput
        ));

        let prompt = StructuredOutputError::Prompt(provider_with_response(402, "{}"));
        assert!(matches!(Error::from(prompt), Error::BudgetExceeded { .. }));
    }

    #[test]
    fn question_mark_through_prompt_error_classifies() {
        // `?` on a PromptError goes through From<PromptError>, so a gateway
        // reply still classifies instead of landing in Rig.
        let result: Result<()> = (|| {
            let err: PromptError = provider_with_response(402, r#"{"type":"budget_exceeded"}"#);
            Err(err)?;
            Ok(())
        })();
        assert!(matches!(result, Err(Error::BudgetExceeded { .. })));
    }

    #[test]
    fn from_provider_error_wraps_and_classifies() {
        let provider = ProviderError::from_http_response(
            http::StatusCode::PAYMENT_REQUIRED,
            json!({"type": "budget_exceeded"}).to_string(),
        );
        assert!(matches!(
            Error::from_provider_error(provider),
            Error::BudgetExceeded { .. }
        ));
    }

    const DEBUG_SENTINEL: &str = "SENTINEL-DEBUG-LEAK-6f1c";

    /// `Debug` and `Display` never print content, but the `raw`/`body`
    /// fields still carry it. All three cases use the realistic
    /// constructions: `SchemaParse` from a real parse failure, `Upstream`
    /// from a real provider reply (whose source `Display` prints the body
    /// — so the redaction is load-bearing, not decorative), and `Rig`
    /// wrapping a `MaxTurns` whose history holds the sentinel.
    ///
    /// Mutation-checked: restoring `#[derive(Debug)]` fails this test
    /// (the derived `Debug` prints `raw`, `body`, and the whole
    /// conversation).
    #[test]
    fn debug_and_display_never_print_content() {
        // SchemaParse: raw = the model's output text.
        let error = Error::from(StructuredOutputError::Deserialization {
            output: DEBUG_SENTINEL.to_owned(),
            error: serde_json::from_str::<serde_json::Value>(DEBUG_SENTINEL).unwrap_err(),
        });
        let Error::SchemaParse { raw, .. } = &error else {
            panic!("expected SchemaParse");
        };
        assert_eq!(raw, DEBUG_SENTINEL, "the field still carries the raw text");
        assert!(
            !format!("{error:?}").contains(DEBUG_SENTINEL),
            "Debug leaked: {error:?}"
        );
        assert!(
            !format!("{error}").contains(DEBUG_SENTINEL),
            "Display leaked: {error}"
        );

        // Upstream: body = the reply body, and the source's Display prints
        // that same body (ProviderResponseError: status ..: {body}).
        let error = Error::from_prompt_error(provider_with_response(500, DEBUG_SENTINEL));
        let Error::Upstream {
            status: 500, body, ..
        } = &error
        else {
            panic!("expected Upstream, got {error:?}");
        };
        assert_eq!(body, DEBUG_SENTINEL, "the field still carries the body");
        assert!(
            !format!("{error:?}").contains(DEBUG_SENTINEL),
            "Debug leaked: {error:?}"
        );
        assert!(
            !format!("{error}").contains(DEBUG_SENTINEL),
            "Display leaked: {error}"
        );

        // Rig wrapping MaxTurns: the derived Debug would print the whole
        // conversation (chat_history and prompt); Display does not.
        let inner = PromptError::MaxTurns {
            max_turns: 8,
            chat_history: vec![rig_core::completion::Message::user(DEBUG_SENTINEL)],
            prompt: rig_core::completion::Message::user(DEBUG_SENTINEL),
        };
        let error = Error::Rig {
            source: Box::new(inner),
        };
        assert!(
            !format!("{error:?}").contains(DEBUG_SENTINEL),
            "Debug leaked: {error:?}"
        );
        assert!(
            !format!("{error}").contains(DEBUG_SENTINEL),
            "Display leaked: {error}"
        );
    }
}
