use std::time::Duration;

use cja_secret::{Error, Secret, SecretStore, Secrets, SetecConfig, setec};
use reqwest::Url;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

#[derive(Clone, Debug, Secrets)]
struct AppSecrets {
    #[secret("attic/app/api-key")]
    api_key: Secret<String>,
    #[secret("attic/app/signing")]
    signing: Secret<setec::SecretBytes>,
}

fn fake() -> SecretStore {
    SecretStore::fake([
        ("attic/app/api-key", b"key-v1".as_slice()),
        ("attic/app/signing", b"\x00\xffsig".as_slice()),
    ])
}

#[test]
fn derive_lists_names_in_field_order() {
    assert_eq!(
        AppSecrets::NAMES,
        ["attic/app/api-key", "attic/app/signing"]
    );
}

#[tokio::test]
async fn load_fetches_every_declared_secret() {
    let secrets: AppSecrets = fake().load().await.unwrap();

    assert_eq!(*secrets.api_key.current(), "key-v1");
    assert_eq!(secrets.api_key.version(), 1);
    assert_eq!(secrets.signing.current().expose(), b"\x00\xffsig");
}

#[tokio::test]
async fn debug_never_prints_values() {
    let secrets: AppSecrets = fake().load().await.unwrap();
    let debug = format!("{secrets:?}");

    assert!(!debug.contains("key-v1"), "{debug}");
    assert!(debug.contains("attic/app/api-key"), "{debug}");
}

#[tokio::test]
async fn missing_secret_fails_load() {
    let store = SecretStore::fake([("attic/app/api-key", b"k".as_slice())]);

    let err = store.load::<AppSecrets>().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Setec {
                name: "attic/app/signing",
                source: setec::Error::NotFound
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn unparseable_secret_fails_load() {
    let store = SecretStore::fake([
        ("attic/app/api-key", b"\xff\xfe".as_slice()),
        ("attic/app/signing", b"s".as_slice()),
    ]);

    let err = store.load::<AppSecrets>().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Parse {
                name: "attic/app/api-key",
                version: 1,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn refresh_delivers_rotated_value_to_every_clone_and_watcher() {
    let store = fake();
    let secrets: AppSecrets = store.load().await.unwrap();
    let clone = secrets.clone();
    let mut watcher = secrets.api_key.watch();

    store.rotate("attic/app/api-key", b"key-v2");
    store.refresh().await.unwrap();

    assert!(watcher.has_changed().unwrap());
    assert_eq!(**watcher.borrow_and_update(), "key-v2");
    assert_eq!(*clone.api_key.current(), "key-v2");
    assert_eq!(clone.api_key.version(), 2);
}

#[tokio::test]
async fn unchanged_refresh_does_not_wake_watchers() {
    let store = fake();
    let secrets: AppSecrets = store.load().await.unwrap();
    let watcher = secrets.api_key.watch();

    store.refresh().await.unwrap();

    assert!(!watcher.has_changed().unwrap());
    assert_eq!(secrets.api_key.version(), 1);
}

#[tokio::test]
async fn unparseable_rotation_keeps_last_good_value() {
    let store = fake();
    let secrets: AppSecrets = store.load().await.unwrap();

    store.rotate("attic/app/api-key", b"\xff\xfe");
    let err = store.refresh().await.unwrap_err();

    let Error::Refresh(failures) = err else {
        panic!("expected Refresh, got {err:?}");
    };
    assert!(
        matches!(
            failures.as_slice(),
            [Error::Parse {
                name: "attic/app/api-key",
                version: 2,
                ..
            }]
        ),
        "{failures:?}"
    );
    assert_eq!(*secrets.api_key.current(), "key-v1");
    assert_eq!(secrets.api_key.version(), 1);
}

#[tokio::test(start_paused = true)]
async fn refresh_until_polls_on_interval_and_stops_on_cancel() {
    let store = fake();
    let secrets: AppSecrets = store.load().await.unwrap();
    let mut watcher = secrets.api_key.watch();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(store.clone().refresh_until(shutdown.clone()));
    tokio::task::yield_now().await; // let the refresher arm its interval at t=0

    store.rotate("attic/app/api-key", b"key-v2");
    tokio::time::advance(Duration::from_mins(30)).await;
    assert!(
        !watcher.has_changed().unwrap(),
        "polled before the interval"
    );

    tokio::time::timeout(Duration::from_mins(31), watcher.changed())
        .await
        .expect("refresher never delivered the rotation")
        .unwrap();
    assert_eq!(*secrets.api_key.current(), "key-v2");

    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn last_refreshed_tracks_the_oldest_check() {
    let store = fake();
    assert!(store.last_refreshed().is_none());
    let _secrets: AppSecrets = store.load().await.unwrap();
    let loaded = store.last_refreshed().unwrap();

    tokio::time::advance(Duration::from_secs(10)).await;
    store.refresh().await.unwrap();

    assert_eq!(
        store.last_refreshed().unwrap() - loaded,
        Duration::from_secs(10)
    );
}

// --- Against a fake setec HTTP server ---------------------------------------

#[derive(Debug, Secrets)]
struct OneSecret {
    #[secret("attic/app/token")]
    token: Secret<String>,
}

fn setec_store(server: &MockServer, boot_timeout: Duration) -> SecretStore {
    let mut config = SetecConfig::new(Url::parse(&server.uri()).unwrap());
    config.boot_timeout = boot_timeout;
    SecretStore::connect(config).unwrap()
}

fn value(v: &str, version: u32) -> ResponseTemplate {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(v);
    ResponseTemplate::new(200).set_body_json(json!({"Value": encoded, "Version": version}))
}

#[tokio::test]
async fn access_denied_at_boot_fails_without_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let err = setec_store(&server, Duration::from_secs(30))
        .load::<OneSecret>()
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Setec {
                source: setec::Error::AccessDenied,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn transient_failure_at_boot_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .respond_with(value("t1", 7))
        .expect(1)
        .mount(&server)
        .await;

    let secrets: OneSecret = setec_store(&server, Duration::from_secs(30))
        .load()
        .await
        .unwrap();

    assert_eq!(*secrets.token.current(), "t1");
    assert_eq!(secrets.token.version(), 7);
}

#[tokio::test]
async fn persistent_outage_at_boot_times_out_with_last_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    let err = setec_store(&server, Duration::from_millis(600))
        .load::<OneSecret>()
        .await
        .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        matches!(
            err,
            Error::BootTimeout {
                name: "attic/app/token",
                last: Some(setec::Error::Server { .. }),
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn refresh_sends_conditional_get_with_known_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(json!({"UpdateIfChanged": false})))
        .respond_with(value("t1", 7))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(
            json!({"Version": 7, "UpdateIfChanged": true}),
        ))
        .respond_with(ResponseTemplate::new(304))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(
            json!({"Version": 7, "UpdateIfChanged": true}),
        ))
        .respond_with(value("t2", 8))
        .expect(1)
        .mount(&server)
        .await;

    let store = setec_store(&server, Duration::from_secs(30));
    let secrets: OneSecret = store.load().await.unwrap();

    store.refresh().await.unwrap();
    assert_eq!(*secrets.token.current(), "t1");

    store.refresh().await.unwrap();
    assert_eq!(*secrets.token.current(), "t2");
    assert_eq!(secrets.token.version(), 8);
}

#[tokio::test]
async fn refresh_failure_keeps_value_and_reports_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(json!({"UpdateIfChanged": false})))
        .respond_with(value("t1", 7))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(json!({"UpdateIfChanged": true})))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let store = setec_store(&server, Duration::from_secs(30));
    let secrets: OneSecret = store.load().await.unwrap();

    let err = store.refresh().await.unwrap_err();

    assert!(
        matches!(
            &err,
            Error::Refresh(f) if matches!(
                f.as_slice(),
                [Error::Setec { source: setec::Error::AccessDenied, .. }]
            )
        ),
        "{err:?}"
    );
    assert_eq!(*secrets.token.current(), "t1");
}
