//! Explicit product analytics declarations. Ordinary tracing events never enter capture.
use serde::Serialize;
use serde_json::{Map, Value, json};
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

#[derive(Clone)]
struct Sender {
    tx: mpsc::Sender<Value>,
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
pub fn submit(name: &str, id: Option<&str>, mut properties: Map<String, Value>) {
    let sender = registry().read().unwrap().clone();
    let Some(sender) = sender else { return };
    properties.insert("service.name".into(), sender.service.into());
    let mut event = json!({
        "event": name,
        "uuid": Uuid::now_v7(),
        "distinct_id": id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned),
        "timestamp": jiff::Timestamp::now().to_string(),
        "properties": properties,
    });
    if id.is_none() {
        event["options"] = json!({"process_person_profile": false});
    }
    if sender.tx.try_send(event).is_err() {
        tracing::warn!(target: "posthog", "capture queue full or closed");
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
    pub(crate) async fn shutdown(self) {
        registry().write().unwrap().take();
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(3), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

pub(crate) fn start(
    host: &str,
    token: &str,
    service: &str,
) -> color_eyre::Result<Option<CaptureShutdownHandle>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        println!("Skipping PostHog capture: no Tokio runtime");
        return Ok(None);
    };
    let client = reqwest_012::Client::builder()
        .use_preconfigured_tls(crate::tls::ring_client_config()?)
        .timeout(Duration::from_secs(2))
        .build()?;
    let (tx, rx) = mpsc::channel(MAX_CAPTURE_QUEUE);
    let url = format!("{host}/i/v1/analytics/events");
    let bearer = format!("Bearer {token}");
    let task = runtime.spawn(worker(rx, client, url, bearer));
    *registry().write().unwrap() = Some(Sender {
        tx,
        service: service.to_owned(),
    });
    Ok(Some(CaptureShutdownHandle { task }))
}

async fn worker(
    mut rx: mpsc::Receiver<Value>,
    client: reqwest_012::Client,
    url: String,
    bearer: String,
) {
    let mut batch = Vec::new();
    loop {
        let next = if batch.is_empty() {
            rx.recv().await
        } else {
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .ok()
                .flatten()
        };
        let received = next.is_some();
        if let Some(event) = next
            && serde_json::to_vec(&event).is_ok_and(|v| v.len() < MAX_EVENT_BYTES)
        {
            batch.push(event);
        }
        if batch.len() < 100 && received {
            continue;
        }
        if !batch.is_empty() {
            let events = std::mem::take(&mut batch);
            for chunk in events.chunks(8) {
                // eight individually capped events stay below 9 MB
                let body =
                    json!({"created_at": jiff::Timestamp::now().to_string(), "batch": chunk});
                let Ok(bytes) = serde_json::to_vec(&body) else {
                    continue;
                };
                if bytes.len() >= MAX_BODY_BYTES {
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
                    Ok(mut response) if response.status().is_success() => {
                        let mut body = Vec::new();
                        while let Ok(Some(chunk)) = response.chunk().await {
                            if body.len() + chunk.len() > 65536 {
                                break;
                            }
                            body.extend_from_slice(&chunk);
                        }
                        if serde_json::from_slice::<Value>(&body).is_ok_and(|v| {
                            v.get("rejected")
                                .and_then(Value::as_u64)
                                .is_some_and(|n| n > 0)
                        }) {
                            tracing::warn!(target: "posthog", "capture rejected events");
                        }
                    }
                    _ => tracing::warn!(target: "posthog", "capture request failed"),
                }
            }
        }
        if !received && rx.is_closed() {
            break;
        }
    }
}

#[macro_export]
macro_rules! product_event {
    (name: $name:expr, user_id: $user:expr, message: $message:literal, properties: { $($fields:tt)* }) => {{
        let __id: Option<&dyn std::fmt::Display> = $user;
        let __id = $crate::__private::identity(__id);
        let mut __properties = $crate::__private::serde_json::Map::new();
        let mut __capture_ok = true;
        $crate::__product_fields! { @parse [$name] [$message] [__id] [__properties] [__capture_ok] [] $($fields)* }
    }};
}
#[doc(hidden)]
#[macro_export]
macro_rules! __product_fields {
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*]) => {{
        if let Some(ref id) = $id {
            $crate::__private::tracing::info!(event_type = $name, user_id = %id, $($trace)* $msg);
        } else {
            $crate::__private::tracing::info!(event_type = $name, $($trace)* $msg);
        }
        if $ok { $crate::__private::submit($name, $id.as_deref(), $props); }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = % $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        $props.insert(stringify!($key).into(), $crate::__private::serde_json::Value::String(format!("{}", $key)));
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = %$key,] $($($rest)*)? }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = ? $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        $props.insert(stringify!($key).into(), $crate::__private::serde_json::Value::String(format!("{:?}", $key)));
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = ?$key,] $($($rest)*)? }
    }};
    (@parse [$name:expr] [$msg:literal] [$id:ident] [$props:ident] [$ok:ident] [$($trace:tt)*] $key:ident = $value:expr $(, $($rest:tt)*)?) => {{
        let $key = $value;
        if let Some(value) = $crate::__private::bare(&$key) { $props.insert(stringify!($key).into(), value); } else { $ok = false; }
        $crate::__product_fields! { @parse [$name] [$msg] [$id] [$props] [$ok] [$($trace)* $key = $key,] $($($rest)*)? }
    }};
}
