//! Asks the real tok vault for each named token as viy does, and prints
//! whether it got one and its length, never the value. For WP 1.4's gate:
//! no DENIED line in tok's log after asking for PIN-gated names. Run the
//! built binary directly (`target/debug/examples/vault_check`), since cargo
//! points HOME at the test home.
//!
//!     vault_check NAME ...

use std::process::ExitCode;

use viy_config::vault::Tok;

fn main() -> ExitCode {
    let names: Vec<String> = std::env::args().skip(1).collect();
    if names.is_empty() {
        eprintln!("usage: vault_check NAME ...");
        return ExitCode::from(2);
    }
    let tok = Tok::from_env();
    for name in names {
        match tok.open_token(&name) {
            Ok(value) => println!("{name}: open, {} characters", value.chars().count()),
            Err(why) => println!("{name}: {why}"),
        }
    }
    ExitCode::SUCCESS
}
