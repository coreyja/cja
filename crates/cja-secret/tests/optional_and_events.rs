use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use cja_secret::{Error, FakeSetec, Secret, SecretStore, Secrets, SetecConfig, setec};
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing_subscriber::{Layer, layer::SubscriberExt as _};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

#[derive(Debug, Secrets)]
struct WithOptional {
    #[secret("arena/required")]
    required: Secret<String>,
    #[secret("arena/optional")]
    optional: Option<Secret<String>>,
}

#[tokio::test]
async fn present_optional_secret_loads_and_rotates() {
    let fake = FakeSetec::new([
        ("arena/required", b"r".as_slice()),
        ("arena/optional", b"o1".as_slice()),
    ]);
    let (secrets, refresher) = fake.store().load::<WithOptional>().await.unwrap();
    let optional = secrets.optional.expect("present secret is Some");
    assert_eq!(*optional.current(), "o1");

    fake.rotate("arena/optional", b"o2");
    refresher.refresh().await.unwrap();

    assert_eq!(*optional.current(), "o2");
    assert_eq!(*secrets.required.current(), "r");
}

#[tokio::test]
async fn missing_optional_secret_is_none() {
    let fake = FakeSetec::new([("arena/required", b"r".as_slice())]);

    let (secrets, refresher) = fake.store().load::<WithOptional>().await.unwrap();

    assert!(secrets.optional.is_none());
    // Absent secrets aren't polled, so refresh has nothing to fail on.
    refresher.refresh().await.unwrap();
}

#[tokio::test]
async fn missing_required_secret_still_fails() {
    let fake = FakeSetec::new([("arena/optional", b"o".as_slice())]);

    let err = fake.store().load::<WithOptional>().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Setec {
                name: "arena/required",
                source: setec::Error::NotFound
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn unparseable_optional_secret_fails_instead_of_disabling() {
    let fake = FakeSetec::new([
        ("arena/required", b"r".as_slice()),
        ("arena/optional", b"\xff\xfe".as_slice()),
    ]);

    let err = fake.store().load::<WithOptional>().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Parse {
                name: "arena/optional",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn denied_optional_secret_fails_instead_of_disabling() {
    let server = MockServer::start().await;
    let encoded = "cg=="; // "r"
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(json!({"Name": "arena/required"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": encoded, "Version": 1})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get"))
        .and(body_partial_json(json!({"Name": "arena/optional"})))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let mut config = SetecConfig::new(setec::Url::parse(&server.uri()).unwrap());
    config.boot_timeout = Duration::from_secs(5);

    let err = SecretStore::connect(config)
        .await
        .unwrap()
        .load::<WithOptional>()
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Setec {
                name: "arena/optional",
                source: setec::Error::AccessDenied
            }
        ),
        "{err:?}"
    );
}

// --- Events -------------------------------------------------------------

/// Records every event that carries an `event_type`, with its fields.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> Layer<S> for Recorder {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        if fields.contains_key("event_type") {
            self.0.lock().unwrap().push(fields);
        }
    }
}

impl Recorder {
    fn of_type(&self, event_type: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == event_type)
            .cloned()
            .collect()
    }
}

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test]
async fn boot_and_rotation_emit_stable_events_without_values() {
    let recorder = Recorder::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));
    let fake = FakeSetec::new([("arena/required", b"value-v1".as_slice())]);

    let (_secrets, refresher) = fake.store().load::<WithOptional>().await.unwrap();
    fake.rotate("arena/required", b"value-v2");
    refresher.refresh().await.unwrap();

    let without_message = |events: Vec<BTreeMap<String, String>>| -> Vec<BTreeMap<String, String>> {
        events
            .into_iter()
            .map(|mut e| {
                e.remove("message");
                e
            })
            .collect()
    };
    assert_eq!(
        without_message(recorder.of_type("secret_loaded")),
        vec![fields(&[
            ("event_type", "secret_loaded"),
            ("secret.name", "arena/required"),
            ("secret.optional", "false"),
            ("secret.version", "1"),
        ])]
    );
    assert_eq!(
        without_message(recorder.of_type("secret_absent")),
        vec![fields(&[
            ("event_type", "secret_absent"),
            ("secret.name", "arena/optional")
        ])]
    );
    assert_eq!(
        without_message(recorder.of_type("secrets_loaded")),
        vec![fields(&[
            ("event_type", "secrets_loaded"),
            ("secrets.absent", "1"),
            ("secrets.loaded", "1"),
            ("setec.transport", "fake"),
        ])]
    );
    assert_eq!(
        without_message(recorder.of_type("secret_rotated")),
        vec![fields(&[
            ("event_type", "secret_rotated"),
            ("secret.from_version", "1"),
            ("secret.name", "arena/required"),
            ("secret.version", "2"),
        ])]
    );

    let everything = format!("{:?}", recorder.0.lock().unwrap());
    assert!(
        !everything.contains("value-v"),
        "a secret value leaked into events: {everything}"
    );
}

#[tokio::test]
async fn refresh_failure_event_carries_the_kept_version() {
    let recorder = Recorder::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));
    let fake = FakeSetec::new([("arena/required", b"r".as_slice())]);
    let (_secrets, refresher) = fake.store().load::<WithOptional>().await.unwrap();

    fake.rotate("arena/required", b"\xff\xfe");
    refresher.refresh().await.unwrap_err();

    let failed = recorder.of_type("secret_refresh_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["secret.name"], "arena/required");
    assert_eq!(failed[0]["secret.version"], "1");
}
