use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::json;
use setec::{
    Client, Connection, DialFuture, Dialer, Error, SecretInfo, StatusCode, TcpDialer, Url,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

async fn setup() -> (MockServer, Client) {
    let server = MockServer::start().await;
    let client = Client::new(Url::parse(&server.uri()).unwrap(), Arc::new(TcpDialer)).unwrap();
    (server, client)
}

fn api(method_name: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!("/api/{method_name}")))
        .and(header("Sec-X-Tailscale-No-Browsers", "setec"))
}

#[tokio::test]
async fn get_sends_name_and_decodes_base64_value() {
    let (server, client) = setup().await;
    api("get")
        .and(body_json(
            json!({"Name": "attic/app/key", "Version": 0, "UpdateIfChanged": false}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": "aGVsbG8=", "Version": 3})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let secret = client.get("attic/app/key").await.unwrap();

    assert_eq!(secret.value.expose(), b"hello");
    assert_eq!(secret.version, 3);
}

#[tokio::test]
async fn get_if_changed_returns_none_on_304() {
    let (server, client) = setup().await;
    api("get")
        .and(body_json(
            json!({"Name": "n", "Version": 3, "UpdateIfChanged": true}),
        ))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    assert!(client.get_if_changed("n", 3).await.unwrap().is_none());
}

#[tokio::test]
async fn get_if_changed_returns_new_version() {
    let (server, client) = setup().await;
    api("get")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": "bmV3", "Version": 4})),
        )
        .mount(&server)
        .await;

    let secret = client.get_if_changed("n", 3).await.unwrap().unwrap();
    assert_eq!(secret.value.expose(), b"new");
    assert_eq!(secret.version, 4);
}

#[tokio::test]
async fn status_codes_map_to_typed_errors() {
    let cases = [
        (404, "not found"),
        (403, "access denied"),
        (412, "version claimed"),
        (400, "bad name"),
        (500, "boom"),
        (502, "bad gateway"),
    ];
    for (status, body) in cases {
        let (server, client) = setup().await;
        api("get")
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;

        let err = client.get("n").await.unwrap_err();
        match (status, &err) {
            (404, Error::NotFound) | (403, Error::AccessDenied) | (412, Error::VersionClaimed) => {
                assert!(!err.is_retryable());
            }
            (400, Error::InvalidRequest { body: b }) => {
                assert_eq!(b, "bad name");
                assert!(!err.is_retryable());
            }
            (500 | 502, Error::Server { status: s, body: b }) => {
                assert_eq!(s.as_u16(), status);
                assert_eq!(b, body);
                assert!(err.is_retryable());
            }
            _ => panic!("HTTP {status} mapped to {err:?}"),
        }
    }
}

#[tokio::test]
async fn unreachable_server_is_retryable_transport_error() {
    // wiremock pools servers, so a dropped MockServer keeps answering. Bind
    // and release a port instead.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let client = Client::new(url, Arc::new(TcpDialer)).unwrap();

    let err = client.get("n").await.unwrap_err();

    assert!(matches!(err, Error::Transport(_)), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn invalid_base64_is_a_decode_error() {
    let (server, client) = setup().await;
    api("get")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": "not base64!", "Version": 1})),
        )
        .mount(&server)
        .await;

    let err = client.get("n").await.unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn secret_bytes_debug_is_redacted() {
    let (server, client) = setup().await;
    api("get")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": "aGVsbG8=", "Version": 1})),
        )
        .mount(&server)
        .await;

    let secret = client.get("n").await.unwrap();
    let debug = format!("{secret:?}");

    assert!(!debug.contains("hello"), "{debug}");
    assert!(debug.contains("REDACTED"), "{debug}");
}

#[tokio::test]
async fn info_and_list_parse_metadata() {
    let (server, client) = setup().await;
    let info = json!({"Name": "n", "Versions": [1, 2], "ActiveVersion": 2, "LastAccess": "2026-10-24T00:00:00Z"});
    api("info")
        .and(body_json(json!({"Name": "n"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(&info))
        .mount(&server)
        .await;
    api("list")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([info])))
        .mount(&server)
        .await;

    let expected = SecretInfo {
        name: "n".into(),
        versions: vec![1, 2],
        active_version: 2,
    };
    assert_eq!(client.info("n").await.unwrap(), expected);
    assert_eq!(client.list().await.unwrap(), vec![expected]);
}

#[tokio::test]
async fn writes_encode_values_and_parse_responses() {
    let (server, client) = setup().await;
    api("put")
        .and(body_json(json!({"Name": "n", "Value": "aGVsbG8="})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(5)))
        .expect(1)
        .mount(&server)
        .await;
    api("create-version")
        .and(body_json(
            json!({"Name": "n", "Version": 9, "Value": "aGk="}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    api("activate")
        .and(body_json(json!({"Name": "n", "Version": 5})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    api("delete-version")
        .and(body_json(json!({"Name": "n", "Version": 4})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    api("delete")
        .and(body_json(json!({"Name": "n"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(null)))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(client.put("n", b"hello").await.unwrap(), 5);
    client.create_version("n", 9, b"hi").await.unwrap();
    client.activate("n", 5).await.unwrap();
    client.delete_version("n", 4).await.unwrap();
    client.delete("n").await.unwrap();
}

#[tokio::test]
async fn missing_header_is_rejected_by_mock_so_header_is_always_sent() {
    // Guard against the header matcher being vacuous: a request without the
    // header must not match `api()`.
    let (server, _client) = setup().await;
    api("get")
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let status = reqwest::Client::new()
        .post(format!("{}/api/get", server.uri()))
        .json(&json!({"Name": "n"}))
        .send()
        .await
        .unwrap()
        .status();

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_with_nothing_visible_is_empty_not_an_error() {
    // Observed against a live setec: Go encodes the empty result as `null`.
    let (server, client) = setup().await;
    api("list")
        .respond_with(ResponseTemplate::new(200).set_body_string("null"))
        .mount(&server)
        .await;

    assert_eq!(client.list().await.unwrap(), vec![]);
}

/// Sends every dial to a fixed address, the way an embedded tailnet node
/// resolves `setec.<tailnet>.ts.net` to a peer itself.
struct FixedDialer {
    addr: std::net::SocketAddr,
    dials: AtomicUsize,
}

impl Dialer for FixedDialer {
    fn dial<'a>(&'a self, _host: &'a str, _port: u16) -> DialFuture<'a> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect(self.addr).await?;
            Ok(Box::new(stream) as Box<dyn Connection>)
        })
    }
}

#[tokio::test]
async fn requests_go_through_the_dialer_with_the_base_url_host() {
    let server = MockServer::start().await;
    let dialer = Arc::new(FixedDialer {
        addr: *server.address(),
        dials: AtomicUsize::new(0),
    });
    let client = Client::new(
        Url::parse("http://setec.example.ts.net").unwrap(),
        dialer.clone(),
    )
    .unwrap();
    api("get")
        .and(header("host", "setec.example.ts.net"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"Value": "aGk=", "Version": 1})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let secret = client.get("n").await.unwrap();

    assert_eq!(secret.value.expose(), b"hi");
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 1);
}

#[test]
fn non_http_urls_are_rejected() {
    let err = Client::new(
        Url::parse("ftp://setec.example").unwrap(),
        Arc::new(TcpDialer),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidUrl(_)), "{err:?}");
}
