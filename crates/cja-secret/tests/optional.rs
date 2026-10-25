use std::time::Duration;

use cja_secret::{Error, FakeSetec, Secret, SecretStore, Secrets, SetecConfig, setec};
use serde_json::json;
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
