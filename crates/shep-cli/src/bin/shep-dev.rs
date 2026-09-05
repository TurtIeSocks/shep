//! The `shep-dev` container-entrypoint alias, supplying the `dev` verb over the
//! library beside it.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    shep::main_dev()
}
