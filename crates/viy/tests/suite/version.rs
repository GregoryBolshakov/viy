use std::process::Command;

#[test]
fn binary_prints_its_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_viy"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("viy {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn binary_exits_2_on_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_viy"))
        .arg("bogus")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
