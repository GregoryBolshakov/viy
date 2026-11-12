use std::fs;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

#[test]
fn no_record_is_the_empty_one() {
    let dir = ScratchDir::new("cutover").unwrap();
    assert_eq!(Record::load(dir.path()).unwrap(), Record::default());
}

#[test]
fn a_record_reads_back_as_written() {
    let dir = ScratchDir::new("cutover").unwrap();
    let record = Record {
        histories: vec![PathBuf::from("/p/a"), PathBuf::from("/p/b c")],
        pod_state_moved: true,
        finished: Some(1_762_900_000.5),
    };
    record.save(dir.path()).unwrap();
    assert_eq!(Record::load(dir.path()).unwrap(), record);
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(dir.path().join(FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode, 0o600);
    assert_eq!(
        fs::read_to_string(dir.path().join(FILE)).unwrap(),
        "{\"histories\":[\"/p/a\",\"/p/b c\"],\"pod_state\":\"moved\",\"finished\":1762900000.5}\n"
    );
}

#[test]
fn a_damaged_record_is_an_error_not_a_guess() {
    let dir = ScratchDir::new("cutover").unwrap();
    for text in [
        "",
        "[]",
        "{\"pod_state\": \"rp\"}",
        "{\"pod_state\": true}",
        "{\"histories\": \"/p\"}",
        "{\"histories\": [1]}",
        "{\"finished\": \"now\"}",
    ] {
        fs::write(dir.path().join(FILE), text).unwrap();
        let error = Record::load(dir.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{text}");
        assert!(
            error
                .to_string()
                .starts_with(&dir.path().join(FILE).display().to_string()),
            "{error}"
        );
    }
}
