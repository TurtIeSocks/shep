//! The `shep-runtime` container-entrypoint alias, supplying the `runtime` verb
//! over the library beside it.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    shep::main_runtime()
}
