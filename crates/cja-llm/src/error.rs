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
//! Every variant that wraps a rig failure keeps the original
//! [`PromptError`], but never prints it: the `source()` of every error is a
//! [`SanitizedError`] whose `Display` carries only content-free text, and
//! whose own `source()` returns `None`. A chain-walking reporter (`color_eyre`
//! `{:?}`, eyre/anyhow `{:#}`) therefore sees only sanitized text — the
//! original `PromptError`'s `Display` prints the reply body
//! (`ProviderResponseError: status ..: {body}`), a serde error's prints the
//! offending value fragment, and `MaxTurns`'s `Debug` prints the whole
//! conversation. Reach the originals programmatically through
//! [`Error::prompt_error`] and [`Error::serde_error`]; the `raw` and `body`
//! fields carry content directly.

use std::fmt;

use rig_agent::completion::{PromptError, StructuredOutputError};
use rig_core::error::{ErrorKind, ProviderError};

use crate::transport::{CONNECT_MARKER, TIMEOUT_MARKER};

const IDENTITY_403_BODY: &str = "forbidden: caller identity could not be determined";
const ADMIN_403_BODY: &str = "admin only";

/// The fixed summary carried by the sanitized source of an [`Error::Rig`].
/// The original rig error's text is withheld entirely: on the relayed
/// (`ErrorReport`) path its message is an arbitrary string that can embed
/// reply bodies or serde value fragments with no error object left to walk,
/// so no redaction of it can be proven content-free.
const RIG_SOURCE_SUMMARY: &str = "the original error's text is withheld because it may \
     contain model output or reply bodies; reach it via Error::prompt_error()";

/// The result type for cja-llm.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The `source()` of every [`Error`]: holds the original failure but never
/// prints it.
///
/// `Display` and `Debug` print a content-free summary only (the transport
/// markers, redacted statuses, a serde error's category and position), and
/// `source()` returns `None`, so nothing below it can reach a reporter.
/// The original error stays programmatically reachable through
/// [`Error::prompt_error`] and [`Error::serde_error`].
pub struct SanitizedError {
    summary: String,
    original: Original,
}

enum Original {
    Prompt(Box<PromptError>),
    Serde(serde_json::Error),
}

impl SanitizedError {
    /// Wraps a rig failure, summarizing its `Display` with `redactions`
    /// (the classified reply body) replaced by byte-length markers and any
    /// serde error in the chain replaced by its sanitized form.
    pub(crate) fn from_prompt(error: PromptError, redactions: &[&str]) -> Self {
        let summary = sanitize_prompt_display(&error, redactions);
        Self {
            summary,
            original: Original::Prompt(Box::new(error)),
        }
    }

    /// Wraps a rig failure whose `Display` cannot be proven content-free
    /// (see [`RIG_SOURCE_SUMMARY`]).
    pub(crate) fn opaque_prompt(error: PromptError) -> Self {
        Self {
            summary: RIG_SOURCE_SUMMARY.to_owned(),
            original: Original::Prompt(Box::new(error)),
        }
    }

    /// Wraps a [`serde_json::Error`] whose `Display` prints the offending
    /// value; the summary carries its category and position only.
    pub(crate) fn from_serde(error: serde_json::Error) -> Self {
        let summary = serde_summary(&error);
        Self {
            summary,
            original: Original::Serde(error),
        }
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }

    pub(crate) fn prompt_error(&self) -> Option<&PromptError> {
        match &self.original {
            Original::Prompt(error) => Some(error),
            Original::Serde(_) => None,
        }
    }

    pub(crate) fn serde_error(&self) -> Option<&serde_json::Error> {
        match &self.original {
            Original::Serde(error) => Some(error),
            Original::Prompt(_) => None,
        }
    }
}

impl fmt::Display for SanitizedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)
    }
}

impl fmt::Debug for SanitizedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.summary)
    }
}

impl std::error::Error for SanitizedError {
    // Deliberately `None` (the default): the original below must never be
    // reachable through the source chain.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// A serde error rendered without its value fragment: category, line, and
/// column only.
fn serde_summary(error: &serde_json::Error) -> String {
    format!(
        "{:?} error at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

/// A rig failure's `Display` with every known content-bearing string
/// redacted: the reply body (exact string) and any serde error reachable by
/// walking the source chain, whose `Display` prints the offending value.
fn sanitize_prompt_display(error: &PromptError, redactions: &[&str]) -> String {
    let mut text = error.to_string();
    for redaction in redactions.iter().filter(|redaction| !redaction.is_empty()) {
        text = text.replace(*redaction, &format!("<redacted {} bytes>", redaction.len()));
    }
    let mut source = std::error::Error::source(error);
    while let Some(link) = source {
        if let Some(serde) = link.downcast_ref::<serde_json::Error>() {
            let display = serde.to_string();
            if !display.is_empty() {
                text = text.replace(&display, &serde_summary(serde));
            }
        }
        source = link.source();
    }
    text
}

/// Everything cja-llm can fail with, classified from the gateway's contract.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Gateway `403 {"error":"forbidden: caller identity could not be
    /// determined"}`: tailnet `WhoIs` failed, so no upstream call was made.
    /// Check the tailnet policy grant that lets this node reach `llm:443`
    /// (e.g. a grant to `tag:llm` on port 443).
    ///
    /// `body` is deliberately not interpolated here: the classifier only
    /// checks the fixed `error` field, so any other field in the reply
    /// body (server-controlled text) must not reach `Display`.
    #[error(
        "gateway could not identify this node's tailnet identity; check the tailnet policy grant that lets this node reach llm:443 (e.g. a grant to tag:llm on port 443)"
    )]
    IdentityRejected {
        /// The gateway's reply body.
        body: String,
        #[source]
        source: SanitizedError,
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
        source: SanitizedError,
    },
    /// Gateway `403 {"error":"admin only"}` — a programming error (wrong
    /// route); cja-llm only uses `POST /v1/chat/completions`.
    ///
    /// `body` is deliberately not interpolated here: the classifier only
    /// checks the fixed `error` field, so any other field in the reply
    /// body (server-controlled text) must not reach `Display`.
    #[error("admin-only gateway route; cja-llm only uses POST /v1/chat/completions")]
    AdminOnly {
        /// The gateway's reply body.
        body: String,
        #[source]
        source: SanitizedError,
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
        source: SanitizedError,
    },
    /// The total deadline (non-streaming request or response head) or the
    /// inter-chunk idle timeout elapsed. The attempt may still have been
    /// billed upstream — the gateway runs inference to completion even when
    /// the client gives up — so it is never retried.
    #[error("gateway request timed out; the gateway may still have billed the attempt")]
    Timeout {
        #[source]
        source: SanitizedError,
    },
    /// Dial, TLS, or handshake failure or a connect timeout: nothing was
    /// sent. Retryable within the retry policy. A node without the tailnet
    /// policy grant to `llm:443` fails here, not with a 403.
    #[error(
        "could not reach the gateway; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    Connect {
        #[source]
        source: SanitizedError,
    },
    /// Structured output failed to parse; `raw` is the model's output text.
    #[error("structured output did not parse")]
    SchemaParse {
        /// The model's output text that failed to parse.
        raw: String,
        #[source]
        error: SanitizedError,
    },
    /// The model returned no accepted content for a typed extraction.
    #[error("the model returned no structured output")]
    EmptyStructuredOutput,
    /// Anything else rig reported (max turns, memory, decode, ...). The
    /// original is preserved and reachable through [`Error::prompt_error`].
    ///
    /// Deliberately not `#[from]`: the hand-written [`From<PromptError>`]
    /// classifies gateway replies first, so `?` in app agent code still gets
    /// the gateway variants.
    #[error(
        "rig reported an error; the original is preserved and available via Error::prompt_error()"
    )]
    Rig {
        #[source]
        source: SanitizedError,
    },
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
/// byte lengths only, and every source prints through its sanitized
/// summary — the original [`PromptError`]'s `Debug` would print the whole
/// conversation (`MaxTurns`), its `Display` the reply body
/// (`ProviderResponseError: status ..: {body}`), and a serde error's either
/// form the offending value fragment. Apps return `cja_llm::Result` from
/// `main`, whose failure path prints `Error: {:?}`, and every cja app walks
/// the source chain through `color_eyre`, so a derived `Debug` or a
/// content-bearing source would leak model output and reply bodies into
/// logs.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// A content-bearing string printed as its byte length only.
        struct Redacted<'a>(&'a str);

        impl fmt::Debug for Redacted<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "<{} bytes>", self.0.len())
            }
        }

        match self {
            Self::IdentityRejected { body, source } => f
                .debug_struct("IdentityRejected")
                .field("body", &Redacted(body))
                .field("source", source)
                .finish(),
            Self::BudgetExceeded { body, source } => f
                .debug_struct("BudgetExceeded")
                .field("body", &Redacted(body))
                .field("source", source)
                .finish(),
            Self::AdminOnly { body, source } => f
                .debug_struct("AdminOnly")
                .field("body", &Redacted(body))
                .field("source", source)
                .finish(),
            Self::Upstream {
                status,
                body,
                source,
            } => f
                .debug_struct("Upstream")
                .field("status", status)
                .field("body", &Redacted(body))
                .field("source", source)
                .finish(),
            Self::Timeout { source } => f.debug_struct("Timeout").field("source", source).finish(),
            Self::Connect { source } => f.debug_struct("Connect").field("source", source).finish(),
            Self::SchemaParse { raw, error } => f
                .debug_struct("SchemaParse")
                .field("raw", &Redacted(raw))
                // The serde error prints as category plus line and column
                // only — never its value fragment.
                .field("error", &error.summary())
                .finish(),
            Self::EmptyStructuredOutput => f.write_str("EmptyStructuredOutput"),
            Self::Rig { source } => f.debug_struct("Rig").field("source", source).finish(),
            Self::Config(message) => f.debug_tuple("Config").field(message).finish(),
        }
    }
}

impl From<StructuredOutputError> for Error {
    fn from(err: StructuredOutputError) -> Self {
        match err {
            StructuredOutputError::Deserialization { output, error } => Self::SchemaParse {
                raw: output,
                error: SanitizedError::from_serde(error),
            },
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
    /// stable marker strings — but only through a structural gate: the
    /// error must be an HTTP-transport failure
    /// ([`ErrorKind::Http`] on both the `Provider` and relayed `Report`
    /// paths), because everywhere else rig's `Display` text is
    /// model-controlled and a marker inside it (e.g. a model-emitted tool
    /// name) must not masquerade as a transport failure.
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
                    source: SanitizedError::from_prompt(err, &[body.as_str()]),
                    body,
                },
                // A Bifrost envelope (stale virtual key) must win over the
                // gateway wrapper's 403 strings.
                401 | 403 if matches!(body_type, Some("access_not_found" | "access_blocked")) => {
                    Self::Upstream {
                        status,
                        source: SanitizedError::from_prompt(err, &[body.as_str()]),
                        body,
                    }
                }
                403 if body_error == Some(IDENTITY_403_BODY) => Self::IdentityRejected {
                    source: SanitizedError::from_prompt(err, &[body.as_str()]),
                    body,
                },
                403 if body_error == Some(ADMIN_403_BODY) => Self::AdminOnly {
                    source: SanitizedError::from_prompt(err, &[body.as_str()]),
                    body,
                },
                _ => Self::Upstream {
                    status,
                    source: SanitizedError::from_prompt(err, &[body.as_str()]),
                    body,
                },
            };
        }
        // Structural gate: the transport markers may only classify a
        // genuine HTTP-transport failure — `PromptError::Provider(e)`
        // whose `e.kind() == ErrorKind::Http` (rig-core-0.45.0
        // `src/error.rs:457-459`) or `PromptError::Report(r)` whose
        // `r.kind == ErrorKind::Http` (public field, `error.rs:107`).
        // Everywhere else rig's `Display` text is model-controlled —
        // `PromptError::UnknownToolCall` interpolates the model-emitted
        // `tool_name` (`rig-agent-0.45.0/src/run/response.rs:286-299`) —
        // so an unguarded `contains(marker)` would let a prompt-injected
        // tool name masquerade as a transport failure and carry its text
        // into the source chain and reporters.
        let is_http_transport = match &err {
            PromptError::Provider(provider) => provider.kind() == ErrorKind::Http,
            PromptError::Report(report) => report.kind == ErrorKind::Http,
            _ => false,
        };
        let text = err.to_string();
        if is_http_transport && text.contains(CONNECT_MARKER) {
            Self::Connect {
                source: SanitizedError::from_prompt(err, &[]),
            }
        } else if is_http_transport && text.contains(TIMEOUT_MARKER) {
            Self::Timeout {
                source: SanitizedError::from_prompt(err, &[]),
            }
        } else {
            Self::Rig {
                source: SanitizedError::opaque_prompt(err),
            }
        }
    }

    /// Classifies a raw [`ProviderError`] the same way a [`PromptError`]
    /// surfaces it, for callers holding the provider error directly.
    pub fn from_provider_error(err: ProviderError) -> Self {
        Self::from_prompt_error(PromptError::from(err))
    }

    /// The original rig [`PromptError`] this error was classified from,
    /// when it wraps one ([`Error::IdentityRejected`],
    /// [`Error::BudgetExceeded`], [`Error::AdminOnly`], [`Error::Upstream`],
    /// [`Error::Timeout`], [`Error::Connect`], [`Error::Rig`]).
    ///
    /// Its `Debug` and its `Display` can print model output, reply bodies,
    /// and serde value fragments — never log it, only inspect it.
    pub fn prompt_error(&self) -> Option<&PromptError> {
        match self {
            Self::IdentityRejected { source, .. }
            | Self::BudgetExceeded { source, .. }
            | Self::AdminOnly { source, .. }
            | Self::Upstream { source, .. }
            | Self::Timeout { source }
            | Self::Connect { source }
            | Self::Rig { source } => source.prompt_error(),
            Self::SchemaParse { .. } | Self::EmptyStructuredOutput | Self::Config(_) => None,
        }
    }

    /// The original [`serde_json::Error`] behind a [`Error::SchemaParse`],
    /// whose `Display` and `Debug` print the offending value fragment —
    /// never log it, only inspect it.
    pub fn serde_error(&self) -> Option<&serde_json::Error> {
        match self {
            Self::SchemaParse { error, .. } => error.serde_error(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rig_agent::completion::PromptError;
    use rig_core::error::{ErrorKind, ErrorReport, ProviderError};
    use rig_core::http_client;
    use serde::Deserialize;
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
    fn marker_text_in_model_output_is_not_classified_as_transport() {
        // A model-emitted tool name carrying the transport marker must NOT
        // classify as Connect/Timeout: `UnknownToolCall`'s Display
        // interpolates the model-controlled `tool_name`
        // (rig-agent-0.45.0 src/run/response.rs:286-299), and only a
        // structural HTTP-transport failure (`ErrorKind::Http`) may hit
        // the marker check. Without the gate this error would surface as
        // `Timeout` whose sanitized source keeps the marker-bearing
        // Display — model output in the chain and in color_eyre reports.
        for marker in [TIMEOUT_MARKER, CONNECT_MARKER] {
            let err = PromptError::UnknownToolCall {
                tool_name: format!("{marker} {DEBUG_SENTINEL}"),
                available_tools: vec!["add".to_owned()],
                allowed_tools: vec!["add".to_owned()],
                chat_history: Vec::new(),
            };
            let error = Error::from_prompt_error(err);
            assert!(matches!(error, Error::Rig { .. }), "{error:?}");
            assert!(error.prompt_error().is_some());
            assert_chain_is_sanitized(&error);
            assert_color_eyre_sanitized(error);
        }
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

    #[test]
    fn sanitized_source_renders_serde_errors_without_fragments() {
        let summary = SanitizedError::from_serde(mismatch_error(DEBUG_SENTINEL))
            .summary()
            .to_owned();
        assert!(summary.contains("Data error at line"), "{summary}");
        assert!(!summary.contains(DEBUG_SENTINEL), "{summary}");
    }

    #[test]
    fn sanitized_prompt_display_redacts_the_known_body() {
        let err = provider_with_response(500, DEBUG_SENTINEL);
        let text = sanitize_prompt_display(&err, &[DEBUG_SENTINEL]);
        assert!(!text.contains(DEBUG_SENTINEL), "{text}");
        assert!(text.contains("<redacted"), "{text}");
    }

    #[test]
    fn sanitized_prompt_display_redacts_chain_serde_errors() {
        // A ProviderError::Json's Display embeds the serde error's value
        // fragment; walking the chain finds the serde error and replaces it.
        let err = PromptError::Provider(ProviderError::Json(Arc::new(mismatch_error(
            DEBUG_SENTINEL,
        ))));
        let text = sanitize_prompt_display(&err, &[]);
        assert!(!text.contains(DEBUG_SENTINEL), "{text}");
        assert!(text.contains("Data error at line"), "{text}");
    }

    const DEBUG_SENTINEL: &str = "SENTINEL-DEBUG-LEAK-6f1c";

    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "the type exists only to produce a real mismatch")]
    struct Mismatch {
        n: u32,
    }

    /// A real serde type-mismatch error whose Display carries the sentinel
    /// as the offending value: `invalid type: string "SENTINEL...", expected
    /// u32 at line 1 column N`.
    fn mismatch_error(sentinel: &str) -> serde_json::Error {
        serde_json::from_str::<Mismatch>(&format!(r#"{{"n":"{sentinel}"}}"#)).unwrap_err()
    }

    /// No `Display` or `Debug` of the error or of any link of its
    /// `source()` chain contains the sentinel, and `color_eyre`'s report —
    /// which walks exactly that chain — does not either. Every case uses a
    /// realistic construction: `SchemaParse` from a real type mismatch,
    /// `Upstream` from a real provider reply, `Rig` wrapping `MaxTurns`
    /// (whose `Debug` prints the whole conversation), and `Rig` wrapping a
    /// `ProviderError::Json` from the same type mismatch.
    ///
    /// Mutation-checked: restoring `#[source] source: Box<PromptError>` on
    /// `Upstream` fails this test (the chain-walk link's `Display` prints
    /// the reply body). See the commit that introduced it.
    /// No `Display` or `Debug` of any link of the error's `source()` chain
    /// contains the sentinel.
    fn assert_chain_is_sanitized(error: &Error) {
        let mut current: Option<&dyn std::error::Error> = Some(error);
        while let Some(link) = current {
            assert!(
                !link.to_string().contains(DEBUG_SENTINEL),
                "Display leaked: {link}"
            );
            assert!(
                !format!("{link:?}").contains(DEBUG_SENTINEL),
                "Debug leaked: {link:?}"
            );
            current = link.source();
        }
    }

    /// `color_eyre`'s report — which walks the source chain — is sentinel-free.
    fn assert_color_eyre_sanitized(error: Error) {
        assert!(
            !format!("{:?}", color_eyre::Report::from(error)).contains(DEBUG_SENTINEL),
            "color_eyre leaked"
        );
    }

    #[test]
    fn debug_and_display_never_print_content() {
        // SchemaParse: raw = the model's output text, error = a real type
        // mismatch whose Display carries the sentinel value fragment.
        let output = format!(r#"{{"n":"{DEBUG_SENTINEL}"}}"#);
        let error = Error::from(StructuredOutputError::Deserialization {
            output,
            error: mismatch_error(DEBUG_SENTINEL),
        });
        let Error::SchemaParse { raw, .. } = &error else {
            panic!("expected SchemaParse");
        };
        assert!(
            raw.contains(DEBUG_SENTINEL),
            "the field still carries the text"
        );
        assert!(
            error
                .serde_error()
                .is_some_and(|e| e.to_string().contains(DEBUG_SENTINEL)),
            "the accessor still returns the original serde error"
        );
        assert_chain_is_sanitized(&error);
        assert!(
            !format!("{:?}", color_eyre::Report::from(error)).contains(DEBUG_SENTINEL),
            "color_eyre leaked"
        );

        // Upstream: body = the reply body, and the original PromptError's
        // Display prints that same body (ProviderResponseError: status ..:
        // {body}) — so the sanitized source is load-bearing, not decorative.
        let error = Error::from_prompt_error(provider_with_response(500, DEBUG_SENTINEL));
        let Error::Upstream {
            status: 500, body, ..
        } = &error
        else {
            panic!("expected Upstream, got {error:?}");
        };
        assert_eq!(body, DEBUG_SENTINEL, "the field still carries the body");
        assert!(
            error
                .prompt_error()
                .is_some_and(|p| p.to_string().contains(DEBUG_SENTINEL)),
            "the accessor still returns the original PromptError"
        );
        assert_chain_is_sanitized(&error);
        assert!(
            !format!("{:?}", color_eyre::Report::from(error)).contains(DEBUG_SENTINEL),
            "color_eyre leaked"
        );

        // Rig wrapping MaxTurns: the original's derived Debug would print
        // the whole conversation (chat_history and prompt).
        let inner = PromptError::MaxTurns {
            max_turns: 8,
            chat_history: vec![rig_core::completion::Message::user(DEBUG_SENTINEL)],
            prompt: rig_core::completion::Message::user(DEBUG_SENTINEL),
        };
        let error = Error::from_prompt_error(inner);
        assert!(matches!(error, Error::Rig { .. }), "{error:?}");
        assert!(
            error.prompt_error().is_some(),
            "the original PromptError stays reachable"
        );
        assert_chain_is_sanitized(&error);
        assert!(
            !format!("{:?}", color_eyre::Report::from(error)).contains(DEBUG_SENTINEL),
            "color_eyre leaked"
        );

        // Rig wrapping a ProviderError::Json from the same type mismatch:
        // its Display is `JsonError: {serde error}` and would print the
        // value fragment.
        let inner = PromptError::Provider(ProviderError::Json(Arc::new(mismatch_error(
            DEBUG_SENTINEL,
        ))));
        let error = Error::from_prompt_error(inner);
        assert!(matches!(error, Error::Rig { .. }), "{error:?}");
        assert!(
            error
                .prompt_error()
                .is_some_and(|p| p.to_string().contains(DEBUG_SENTINEL)),
            "the accessor still returns the original PromptError"
        );
        assert_chain_is_sanitized(&error);
        assert!(
            !format!("{:?}", color_eyre::Report::from(error)).contains(DEBUG_SENTINEL),
            "color_eyre leaked"
        );
    }

    /// `IdentityRejected` and `AdminOnly` classify from a real 403 whose
    /// body carries an extra `detail` field beyond the fixed `error`
    /// string, and neither the body nor the detail reaches `Display`,
    /// `Debug`, the source chain, or a `color_eyre` report. The body stays
    /// in the field.
    #[test]
    fn identity_and_admin_only_display_never_print_the_body() {
        // IdentityRejected.
        let error = Error::from_prompt_error(provider_with_response(
            403,
            &format!(
                "{{\"error\":\"forbidden: caller identity could not be determined\",\"detail\":\"{DEBUG_SENTINEL}\"}}"
            ),
        ));
        let Error::IdentityRejected { body, .. } = &error else {
            panic!("expected IdentityRejected, got {error:?}");
        };
        assert!(
            body.contains(DEBUG_SENTINEL),
            "the field still carries the body"
        );
        assert!(
            error.prompt_error().is_some(),
            "the original PromptError stays reachable"
        );
        assert!(
            !error.to_string().contains("determined"),
            "the body's error field must not reach Display: {error}"
        );
        assert!(!error.to_string().contains(DEBUG_SENTINEL), "{error}");
        assert_chain_is_sanitized(&error);
        assert_color_eyre_sanitized(error);

        // AdminOnly.
        let error = Error::from_prompt_error(provider_with_response(
            403,
            &format!("{{\"error\":\"admin only\",\"detail\":\"{DEBUG_SENTINEL}\"}}"),
        ));
        let Error::AdminOnly { body, .. } = &error else {
            panic!("expected AdminOnly, got {error:?}");
        };
        assert!(
            body.contains(DEBUG_SENTINEL),
            "the field still carries the body"
        );
        assert!(
            error.prompt_error().is_some(),
            "the original PromptError stays reachable"
        );
        assert!(
            !error.to_string().contains("admin only"),
            "the body's error field must not reach Display: {error}"
        );
        assert!(!error.to_string().contains(DEBUG_SENTINEL), "{error}");
        assert_chain_is_sanitized(&error);
        assert_color_eyre_sanitized(error);
    }
}
