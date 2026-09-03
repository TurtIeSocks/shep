//! A supervised app that answers on the shepherd channel.
//!
//! Run under shep with `channel = true` and `shep trigger <name> gc` reaches
//! the handler below. `tests/real_child.rs` drives this same behavior in a
//! re-exec of its own test binary, not this binary -- see that file's module
//! doc for why -- but this example still needs to build and stay
//! clippy-clean, since it is the copy an app author actually reads.

fn main() {
    let shepherd = shep_channel::serve();
    shepherd.on_action("gc", |params, _name| {
        format!("collected, params={params:?}")
    });
    shepherd.on_shutdown(|| std::process::exit(0));
    shepherd
        .ready()
        .expect("failed to send the readiness message");
    shepherd.metric("rps", 42.0);

    // Park. The reader thread is doing the work; the test kills this process.
    loop {
        std::thread::park();
    }
}
