//! Own test binary: the process-default rustls provider is process-global, so
//! this must not share a process with anything that installs one.
use cja::observability::posthog::SyncClient;

/// The sync client carries its own ring config. Building it must not install
/// a process-wide rustls provider over the app's own choice.
#[test]
fn building_the_sync_client_installs_no_process_default_provider() {
    let _client = SyncClient::new().unwrap();
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}
