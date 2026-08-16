//! The `shep` binary. Everything it does lives in the library beside it.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    shep::main()
}
