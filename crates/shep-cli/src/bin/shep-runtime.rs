//! The `shep-runtime` container-entrypoint alias. Everything it does lives in
//! the library beside it — this binary supplies the `runtime` verb.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    shep_cli::main_runtime()
}
