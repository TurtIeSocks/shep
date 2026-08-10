//! Simulates the position of a consumer who depends on `shep-client` alone
//! and never added `futures-util` to their own manifest: this file has no
//! `use futures_util::...` anywhere in it, and none of its other imports
//! bring `StreamExt` into scope either. `stream.next()` below resolves only
//! because [`shep_client::EventStream::next`] is an inherent method — if
//! that method were ever removed, this file would fail to compile (`next`
//! not found) rather than silently keep passing, since nothing here has a
//! trait in scope to fall back to.

#![cfg(unix)]

use std::time::Duration;

use shep_client::testing::fake_client_with_push;
use shep_core::protocol::BusEvent;

/// Same guard `event_stream.rs` uses: a broken implementation fails with a
/// named assertion instead of hanging the test run.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn next_resolves_with_no_futures_util_import_in_scope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    daemon
        .push(BusEvent::LogOut {
            id: 1,
            line: "hello".into(),
        })
        .await;

    let event = tokio::time::timeout(EVENT_TIMEOUT, stream.next())
        .await
        .expect("a pushed event must arrive, not hang")
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        BusEvent::LogOut {
            id: 1,
            line: "hello".into(),
        }
    );
}
