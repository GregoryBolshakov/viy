use super::*;

// rpctl tests/test_secret_files.py NameTests, ported.

const SECRET: [&str; 25] = [
    ".env",
    ".env.prod",
    "secrets.json",
    "secret.yaml",
    "app-secrets.yml",
    "db_secret.toml",
    "secrets",
    "secret",
    "id_rsa",
    "id_ed25519",
    "identity",
    "server.pem",
    "tls.key",
    "store.p12",
    "keystore.jks",
    "vault.kdbx",
    "credentials",
    "credentials.json",
    ".netrc",
    ".pgpass",
    ".npmrc",
    ".git-credentials",
    ".secrets.json",
    "PRIVATE.PEM",
    "Id_Rsa",
];
const NOT_SECRET: [&str; 15] = [
    ".env.example",
    ".env.sample",
    ".env.template",
    "id_rsa.pub",
    "id_ed25519.pub",
    "README.md",
    "main.py",
    "config.json",
    "package.json",
    "notsecrets.md",
    "secretsanta.py",
    "secretary.json",
    "secrets.md",
    "data.txt",
    "public.pub",
];

#[test]
fn secret_names() {
    for n in SECRET {
        assert!(is_secret_name(n), "{n}");
        assert!(secret_name_reason(n).is_some(), "{n}");
    }
}

#[test]
fn non_secret_names() {
    for n in NOT_SECRET {
        assert!(!is_secret_name(n), "{n}");
    }
}

#[test]
fn reasons_are_specific() {
    let reason = |n| secret_name_reason(n).unwrap_or_default();
    assert!(reason(".env").contains("env file"));
    assert!(reason("x.pem").contains("private key"));
    assert!(reason("credentials").contains("credentials"));
    assert!(reason("secrets.json").contains("secrets"));
}

#[test]
fn pub_key_is_never_secret() {
    assert_eq!(secret_name_reason("deploy.pem.pub"), None);
    assert_eq!(secret_name_reason("id_rsa.pub"), None);
}

/// Python's `$` matches before a final newline, so rpctl calls this one
/// secret too.
#[test]
fn a_final_newline_keeps_a_secret_name_secret() {
    assert!(is_secret_name("secrets.json\n"));
}

// From the WP 8.2 review: the stem rule is written twice, as `secret_stem`
// here and as `SECRET_STEM_PATTERN` for a lent server's `find -iregex`
// (remote.rs); they must agree on every name.
#[test]
fn the_servers_stem_pattern_is_the_local_rule() {
    let pattern = regex::Regex::new(&format!(r"(?i)\A{SECRET_STEM_PATTERN}\z")).unwrap();
    let mut names = Vec::new();
    for prefix in ["", "app-", "db_", "x.", "my", "-", "a.b-"] {
        for stem in ["secret", "secrets", "secretz", "Secrets", "secre"] {
            for ext in SECRET_STEM_EXTENSIONS
                .iter()
                .copied()
                .chain(["json5", "jsn", "", "yamll", "y"])
            {
                names.push(format!("{prefix}{stem}.{ext}"));
            }
        }
    }
    for name in names {
        let low = name.to_lowercase();
        assert_eq!(pattern.is_match(&name), secret_stem(&low), "{name}");
    }
}
