//! Secret file names (rpctl sandbox.py `secret_name_reason`): the files
//! masked in the shell sandbox, confirmed before the file tools read them,
//! and kept out of the network reviewer. Name only, no file contents; a
//! `.pub` public key is never secret.

/// `.env.*` files that are templates, not secrets.
pub const SECRET_ALLOW: [&str; 3] = [".env.example", ".env.sample", ".env.template"];

/// Exact file names (case-insensitive) that are credentials.
const SECRET_FILES: [&str; 20] = [
    "secret",
    "secrets",
    "credential",
    "credentials",
    "credentials.json",
    ".netrc",
    "_netrc",
    ".pgpass",
    ".htpasswd",
    ".npmrc",
    ".pypirc",
    ".dockercfg",
    ".git-credentials",
    ".boto",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_xmss",
    "identity",
];

/// Extensions (case-insensitive) of private keys, keystores and credential
/// dumps.
const SECRET_SUFFIXES: [&str; 9] = [
    ".pem",
    ".key",
    ".pfx",
    ".p12",
    ".jks",
    ".keystore",
    ".ppk",
    ".kdbx",
    ".keyring",
];

/// Data extensions after a `secret`/`secrets` stem.
const SECRET_STEM_EXTENSIONS: [&str; 9] = [
    "json", "yaml", "yml", "toml", "ini", "txt", "enc", "conf", "cfg",
];

/// Why a file called `name` is treated as a secret, or `None`.
pub fn secret_name_reason(name: &str) -> Option<&'static str> {
    let low = name.to_lowercase();
    if low.ends_with(".pub") {
        return None;
    }
    if name == ".env" || (name.starts_with(".env.") && !SECRET_ALLOW.contains(&name)) {
        return Some("it is an env file that may hold API keys");
    }
    if SECRET_FILES.contains(&low.as_str()) {
        return Some("it is a credentials file");
    }
    if SECRET_SUFFIXES.iter().any(|s| low.ends_with(s)) {
        return Some("it looks like a private key or keystore");
    }
    if secret_stem(&low) {
        return Some("its name says it holds secrets");
    }
    None
}

pub fn is_secret_name(name: &str) -> bool {
    secret_name_reason(name).is_some()
}

/// rpctl's `(.*[-_.])?secrets?\.(json|ya?ml|toml|ini|txt|enc|conf|cfg)$`,
/// matched from the start: a `secret` or `secrets` stem, alone or after a
/// `-`, `_` or `.`, then a data extension. Python's `$` also matches before
/// a final newline, so one is allowed (a name may hold one).
fn secret_stem(low: &str) -> bool {
    let low = low.strip_suffix('\n').unwrap_or(low);
    let Some((stem, ext)) = low.rsplit_once('.') else {
        return false;
    };
    SECRET_STEM_EXTENSIONS.contains(&ext)
        && ["secret", "secrets"].iter().any(|word| {
            stem.strip_suffix(word)
                .is_some_and(|before| before.is_empty() || before.ends_with(['-', '_', '.']))
        })
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
