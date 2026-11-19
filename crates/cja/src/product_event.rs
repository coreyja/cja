//! Explicit product analytics declarations. Ordinary tracing events never enter capture.
use color_eyre::eyre::WrapErr as _;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fmt,
    sync::{OnceLock, RwLock},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

const MAX_CAPTURE_QUEUE: usize = 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_BODY_BYTES: usize = 9 * 1024 * 1024;
static DROPPED: AtomicU64 = AtomicU64::new(0);
fn dropped(reason: &str) {
    let count = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if count.is_power_of_two() {
        tracing::warn!(target: "posthog", count, reason, "capture events dropped");
    }
}

#[derive(Serialize)]
struct CaptureEvent {
    event: String,
    uuid: Uuid,
    distinct_id: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<CaptureOptions>,
    properties: Map<String, Value>,
}
#[derive(Serialize)]
struct CaptureOptions {
    process_person_profile: bool,
}

#[derive(Clone)]
struct Sender {
    tx: mpsc::Sender<CaptureEvent>,
    service: String,
}
static SENDER: OnceLock<RwLock<Option<Sender>>> = OnceLock::new();
fn registry() -> &'static RwLock<Option<Sender>> {
    SENDER.get_or_init(|| RwLock::new(None))
}

#[doc(hidden)]
pub fn identity(id: Option<&dyn fmt::Display>) -> Option<String> {
    id.map(ToString::to_string)
}
#[doc(hidden)]
pub fn bare<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value)
        .map_err(|_| tracing::warn!(target: "posthog", "product property serialization failed"))
        .ok()
}
#[doc(hidden)]
pub fn enabled() -> bool {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}
#[doc(hidden)]
pub fn submit(name: &str, id: Option<&str>, mut properties: Map<String, Value>) {
    let sender = registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(sender) = sender else { return };
    properties.insert("service.name".into(), sender.service.into());
    let event = CaptureEvent {
        event: name.to_owned(),
        uuid: Uuid::now_v7(),
        distinct_id: id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned),
        timestamp: jiff::Timestamp::now().to_string(),
        options: id.is_none().then_some(CaptureOptions {
            process_person_profile: false,
        }),
        properties,
    };
    if sender.tx.try_send(event).is_err() {
        dropped("queue full or closed");
    }
}

pub(crate) struct CaptureShutdownHandle {
    task: JoinHandle<()>,
}
impl fmt::Debug for CaptureShutdownHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaptureShutdownHandle")
    }
}
impl CaptureShutdownHandle {
    pub(crate) async fn shutdown(self, budget: Duration) {
        registry()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut task = self.task;
        if tokio::time::timeout(budget, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

pub(crate) fn start(
    host: &str,
    token: &str,
    service: &str,
) -> color_eyre::Result<Option<CaptureShutdownHandle>> {
    start_with_capacity(host, token, service, MAX_CAPTURE_QUEUE)
}

fn start_with_capacity(
    host: &str,
    token: &str,
    service: &str,
    capacity: usize,
) -> color_eyre::Result<Option<CaptureShutdownHandle>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        println!("Skipping PostHog capture: no Tokio runtime");
        return Ok(None);
    };
    let client = reqwest_012::Client::builder()
        .use_preconfigured_tls(
            crate::tls::ring_client_config().wrap_err("building the PostHog capture TLS config")?,
        )
        .timeout(Duration::from_secs(2))
        .build()?;
    let (tx, rx) = mpsc::channel(capacity);
    let url = format!("{host}/i/v1/analytics/events");
    let bearer = format!("Bearer {token}");
    let task = runtime.spawn(worker(rx, client, url, bearer));
    *registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Sender {
        tx,
        service: service.to_owned(),
    });
    Ok(Some(CaptureShutdownHandle { task }))
}

async fn worker(
    mut rx: mpsc::Receiver<CaptureEvent>,
    client: reqwest_012::Client,
    url: String,
    bearer: String,
) {
    let mut batch = Vec::new();
    let mut deadline = None;
    loop {
        let (next, channel_drained) = if let Some(batch_deadline) = deadline {
            match tokio::time::timeout_at(batch_deadline, rx.recv()).await {
                Ok(Some(event)) => (Some(event), false),
                Ok(None) => (None, true),
                Err(_) => (None, false),
            }
        } else {
            match rx.recv().await {
                Some(event) => (Some(event), false),
                None => (None, true),
            }
        };
        let received = next.is_some();
        if let Some(event) = next {
            if serde_json::to_vec(&event).is_ok_and(|v| v.len() < MAX_EVENT_BYTES) {
                if batch.is_empty() {
                    deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                }
                batch.push(event);
            } else {
                dropped("event too large or invalid");
            }
        }
        if batch.len() < 100 && received {
            continue;
        }
        if !batch.is_empty() {
            let events = std::mem::take(&mut batch);
            deadline = None;
            let mut chunks: Vec<Vec<CaptureEvent>> = Vec::new();
            let mut chunk = Vec::new();
            let mut size = 128;
            for event in events {
                let event_size = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len() + 1);
                if size + event_size >= MAX_BODY_BYTES && !chunk.is_empty() {
                    chunks.push(std::mem::take(&mut chunk));
                    size = 128;
                }
                size += event_size;
                chunk.push(event);
            }
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
            for chunk in chunks {
                let sent_uuids: Vec<String> =
                    chunk.iter().map(|event| event.uuid.to_string()).collect();
                let body =
                    json!({"created_at": jiff::Timestamp::now().to_string(), "batch": chunk});
                let Ok(bytes) = serde_json::to_vec(&body) else {
                    dropped("batch serialization failed");
                    continue;
                };
                if bytes.len() >= MAX_BODY_BYTES {
                    dropped("batch too large");
                    continue;
                }
                let result = client
                    .post(&url)
                    .header("Authorization", &bearer)
                    .header("User-Agent", format!("cja/{}", env!("CARGO_PKG_VERSION")))
                    .header(
                        "PostHog-Sdk-Info",
                        format!("cja/{}", env!("CARGO_PKG_VERSION")),
                    )
                    .header("PostHog-Request-Id", Uuid::new_v4().to_string())
                    .header("PostHog-Attempt", "1")
                    .header(
                        "PostHog-Request-Timestamp",
                        jiff::Timestamp::now().to_string(),
                    )
                    .header("Content-Type", "application/json")
                    .body(bytes)
                    .send()
                    .await;
                match result {
                    Ok(response) if response.status().is_success() => {
                        match capture_result(response, &sent_uuids).await {
                            CaptureResult::Delivered => {}
                            CaptureResult::Rejected => {
                                tracing::warn!(target: "posthog", "capture rejected events");
                            }
                            CaptureResult::Unreadable => {
                                tracing::warn!(target: "posthog", "capture response unreadable");
                            }
                        }
                    }
                    _ => tracing::warn!(target: "posthog", "capture request failed"),
                }
            }
        }
        if channel_drained {
            break;
        }
    }
}

#[derive(Clone, Copy)]
enum CaptureResult {
    Delivered,
    Rejected,
    Unreadable,
}

async fn capture_result(
    mut response: reqwest_012::Response,
    sent_uuids: &[String],
) -> CaptureResult {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= 65536 => body.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return CaptureResult::Unreadable,
        }
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return CaptureResult::Unreadable;
    };
    let Some(raw_results) = value.get("results") else {
        return CaptureResult::Delivered;
    };
    let Some(results) = raw_results.as_object() else {
        return CaptureResult::Unreadable;
    };
    let rejected = results.iter().any(|(uuid, entry)| {
        sent_uuids.iter().any(|sent| sent == uuid)
            && (matches!(
                entry.get("result").and_then(Value::as_str),
                Some("drop" | "retry")
            ) || entry
                .get("duplicates_dropped")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0))
    });
    if rejected {
        CaptureResult::Rejected
    } else {
        CaptureResult::Delivered
    }
}

#[macro_export]
macro_rules! product_event {
    (name: $name:expr, user_id: $user:expr, message: $message:literal, properties: { $($fields:tt)* }) => {{
        let __name = $name;
        let __id: Option<&dyn std::fmt::Display> = $user;
        let __id = $crate::__private::identity(__id);
        let mut __properties = $crate::__private::serde_json::Map::new();
        let mut __capture_ok = true;
        $crate::__product_fields! { @parse [__name] [$message] [__id] [__properties] [__capture_ok] [] $($fields)* }
    }};
}
#[doc(hidden)]
#[macro_export]
macro_rules! __product_fields {
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] user_id = $($rest:tt)*) => { compile_error!("user_id is a reserved product event field"); };
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] message = $($rest:tt)*) => { compile_error!("message is a reserved product event field"); };
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*]) => {{
        if let Some(ref id) = $id {
            $crate::__private::tracing::info!(event_type = $name, user_id = %id, $($trace)* "{}", $msg);
        } else {
            $crate::__private::tracing::info!(event_type = $name, $($trace)* "{}", $msg);
        }
        if $ok { $crate::__private::submit($name, $id.as_deref(), $props); }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = % $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        if $crate::__private::enabled() { $props.insert(stringify!($key).into(), $crate::__private::serde_json::Value::String(format!("{}", $key))); }
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = %$key,] $($($rest)*)? }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = ? $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        if $crate::__private::enabled() { $props.insert(stringify!($key).into(), $crate::__private::serde_json::Value::String(format!("{:?}", $key))); }
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = ?$key,] $($($rest)*)? }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        if $crate::__private::enabled() {
            if let Some(value) = $crate::__private::bare(&$key) { $props.insert(stringify!($key).into(), value); } else { $ok = false; }
        }
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = $key,] $($($rest)*)? }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn capacity_one_drops_overflow_promptly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _hold = stream;
            let _ = accepted_tx.send(());
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let handle = start_with_capacity(&host, "phc_fixture", "fixture", 1)
            .unwrap()
            .unwrap();
        submit("first", None, Map::new());
        tokio::time::timeout(Duration::from_secs(3), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        submit("queued", None, Map::new());
        let before = DROPPED.load(Ordering::Relaxed);
        let start = std::time::Instant::now();
        submit("overflow", None, Map::new());
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(DROPPED.load(Ordering::Relaxed), before + 1);
        handle.shutdown(Duration::from_millis(100)).await;
        server.abort();
    }

    #[test]
    fn ring_client_does_not_install_global_provider() {
        let tls = crate::tls::ring_client_config().unwrap();
        reqwest_012::Client::builder()
            .use_preconfigured_tls(tls)
            .build()
            .unwrap();
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }
}
