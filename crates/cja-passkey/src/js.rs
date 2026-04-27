pub fn passkey_js() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/passkey-client.js"))
}

pub async fn passkey_js_handler() -> (http::HeaderMap, &'static str) {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=3600"),
    );
    (headers, passkey_js())
}
