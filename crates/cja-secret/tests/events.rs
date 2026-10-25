//! Event tests live in their own binary. tracing caches each callsite's
//! interest process-wide: if a test without a subscriber logs first on another
//! thread, the callsite is cached as disabled and a recording subscriber
//! misses it. Every test here installs a subscriber before logging.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use cja_secret::{FakeSetec, Secret, Secrets};
use tracing::field::{Field, Visit};
use tracing_subscriber::{Layer, layer::SubscriberExt as _};

#[derive(Secrets)]
#[expect(dead_code, reason = "loaded only for the events it emits")]
struct WithOptional {
    #[secret("arena/required")]
    required: Secret<String>,
    #[secret("arena/optional")]
    optional: Option<Secret<String>>,
}

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
