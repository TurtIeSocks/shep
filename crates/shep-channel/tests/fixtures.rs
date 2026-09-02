//! The fixture corpus, and the test that keeps it honest.
//!
//! Each case is serialized from this crate's own types and compared byte for
//! byte with the committed file, then read back and compared with the value.
//! Both directions, because the Go, JavaScript and Python libraries are
//! written against these bytes and a drift in either direction would reach
//! them silently.
//!
//! Regenerate with `SHEP_CHANNEL_BLESS=1 cargo test -p shep-channel --test fixtures`.

use std::fs;
use std::path::PathBuf;

use shep_channel::{ChildMessage, ShepherdMessage};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn check(name: &str, encoded: String) {
    let path = fixtures_dir().join(format!("{name}.json"));
    if std::env::var_os("SHEP_CHANNEL_BLESS").is_some() {
        fs::create_dir_all(fixtures_dir()).expect("create fixtures dir");
        fs::write(&path, &encoded).expect("write fixture");
        return;
    }
    let committed = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("{}: {error}. Run with SHEP_CHANNEL_BLESS=1 to create it.", path.display())
    });
    assert_eq!(
        committed, encoded,
        "{} is stale. Three other libraries are written against these bytes.",
        path.display()
    );
}

#[test]
fn child_messages_match_their_fixtures() {
    let cases: Vec<(&str, ChildMessage)> = vec![
        ("child-ready", ChildMessage::Ready),
        ("child-metric", ChildMessage::Metric { name: "rps".into(), value: 42.0 }),
        ("child-action-reply", ChildMessage::ActionReply {
            action: "gc".into(), body: "ok".into(), id: None,
        }),
        ("child-action-reply-id", ChildMessage::ActionReply {
            action: "gc".into(), body: "ok".into(), id: Some(7),
        }),
    ];
    for (name, value) in cases {
        let encoded = serde_json::to_string(&value).expect("encode");
        check(name, encoded.clone());
        let decoded: ChildMessage = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, value, "{name} does not survive a round trip");
    }
}

#[test]
fn shepherd_messages_match_their_fixtures() {
    let cases: Vec<(&str, ShepherdMessage)> = vec![
        ("shepherd-shutdown", ShepherdMessage::Shutdown),
        ("shepherd-action", ShepherdMessage::Action {
            name: "gc".into(), params: None, id: 7,
        }),
        ("shepherd-action-params", ShepherdMessage::Action {
            name: "set-log-level".into(), params: Some("debug".into()), id: 8,
        }),
    ];
    for (name, value) in cases {
        let encoded = serde_json::to_string(&value).expect("encode");
        check(name, encoded.clone());
        let decoded: ShepherdMessage = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, value, "{name} does not survive a round trip");
    }
}

/// fails if an `action-reply` carrying no `id` stops decoding. Every app
/// written before the correlation id existed sends this shape, and the
/// daemon's name-and-order fallback exists for exactly it.
#[test]
fn an_action_reply_without_an_id_still_decodes() {
    let decoded: ChildMessage =
        serde_json::from_str(r#"{"kind":"action-reply","action":"gc","body":"ok"}"#)
            .expect("decode");
    assert_eq!(
        decoded,
        ChildMessage::ActionReply { action: "gc".into(), body: "ok".into(), id: None }
    );
}
