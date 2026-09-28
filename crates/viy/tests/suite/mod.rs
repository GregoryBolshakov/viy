// cfg(test) so its helpers may unwrap, as test code (clippy.toml).
#[cfg(test)]
mod chat_pty;
#[cfg(test)]
mod ui_pty;
mod version;
