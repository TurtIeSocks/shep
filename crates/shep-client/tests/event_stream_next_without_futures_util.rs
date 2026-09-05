//! Has no `futures_util` import, and nothing else brings `StreamExt`
//! into scope. `stream.next()` resolves only because
//! [`shep_client::EventStream::next`] is an inherent method. If that
//! method were removed, this fails to compile instead of silently
//! passing.

use std::time::Duration;

use shep_client::testing::fake_client_with_push;
use shep_core::protocol::BusEvent;

/// Same guard `event_stream.rs` uses: a broken implementation fails with a
/// named assertion instead of hanging the test run.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn next_resolves_with_no_futures_util_import_in_scope() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
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
