# shep-client

The async client for [shep](https://github.com/shep-pm/shep), a process
manager written in Rust. Use this crate to talk to a running shepherd from
your own program instead of shelling out to the `shep` binary.

```rust,ignore
use shep_client::Client;

let mut client = Client::connect(&socket_path).await?;
let flock = client.request(shep_core::protocol::Request::List).await?;
```

What it gives you:

- `Client::connect`, which completes a full handshake rather than a bare
  `connect(2)`. A socket that is bound but not accepting satisfies the latter,
  so only a finished handshake counts as a daemon answering.
- Typed requests and replies, with a deadline variant for callers that cannot
  block forever.
- `subscribe`, which takes topic globs and hands back an `EventStream`. Pulling
  one event needs no `futures-util` dependency of your own, because `next` is
  an inherent method.
- `spawn`, for starting a shepherd that is not running yet and waiting until
  its socket is ready.

Errors are one enum per module, each `#[non_exhaustive]`, so matching on them
survives this crate growing.

shep is pre-release, and anything public here can change before 1.0. The
[CHANGELOG](CHANGELOG.md) records what moved.

## License

MIT OR Apache-2.0, at your option.
