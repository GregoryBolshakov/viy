use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn vol(target: &str, source: &str, fstype: &str, ro: bool) -> Volume {
    Volume {
        target: PathBuf::from(target),
        source: source.into(),
        fstype: fstype.into(),
        read_only: ro,
    }
}

// rpctl tests/test_mounts.py ParseTests, ProblemTests, NoteTests, ported.

#[test]
fn mode_word_and_spaces_and_quotes() {
    assert_eq!(
        parse_arg("/media/me/UBUNTU 22_0"),
        ("/media/me/UBUNTU 22_0".into(), false)
    );
    assert_eq!(
        parse_arg("/media/me/USB stick rw"),
        ("/media/me/USB stick".into(), true)
    );
    assert_eq!(
        parse_arg("'/media/me/my rw' ro"),
        ("/media/me/my rw".into(), false)
    );
    assert_eq!(parse_arg("rw"), (String::new(), true));
    assert_eq!(parse_arg(""), (String::new(), false));
}

#[test]
fn mount_table_escapes() {
    assert_eq!(
        unescape(r"/media/me/UBUNTU\04022_0"),
        "/media/me/UBUNTU 22_0"
    );
}

#[test]
fn short_names_resolve_back() {
    let table = [
        vol("/media/me/UBUNTU 22_0", "/dev/sda1", "vfat", false),
        vol("/media/me/Backup", "/dev/sdb1", "ext4", false),
        vol("/media/me/Backup 2", "/dev/sdc1", "ext4", false),
        vol("/media/me/USB", "/dev/sdd1", "vfat", false),
        vol("/mnt/USB", "/dev/sde1", "vfat", false),
    ];
    let home = Path::new("/home/me");
    let short = |p: &str| short_name(Path::new(p), &table, home);
    assert_eq!(short("/media/me/UBUNTU 22_0").as_deref(), Some("ubuntu"));
    assert_eq!(short("/media/me/Backup").as_deref(), Some("backup"));
    assert_eq!(short("/media/me/Backup 2").as_deref(), Some("backup 2"));
    assert_eq!(short("/mnt/USB"), None);
}

#[test]
fn resolve_by_drive_name() {
    let table = [
        vol("/media/me/UBUNTU 22_0", "/dev/sda1", "vfat", false),
        vol("/media/me/Backup", "/dev/sdb1", "ext4", false),
        vol("/media/me/Backup2", "/dev/sdc1", "ext4", false),
        vol("/", "/dev/nvme0n1p2", "ext4", false),
    ];
    let home = Path::new("/home/me");
    assert_eq!(
        resolve("ubuntu", &table, home),
        Ok(PathBuf::from("/media/me/UBUNTU 22_0"))
    );
    assert_eq!(
        resolve("backup", &table, home),
        Ok(PathBuf::from("/media/me/Backup"))
    );
    assert!(
        resolve("back", &table, home)
            .unwrap_err()
            .contains("several")
    );
    assert!(
        resolve("nope", &table, home)
            .unwrap_err()
            .contains("no mounted drive")
    );
}

struct Tree {
    _scratch: ScratchDir,
    root: PathBuf,
    drive: PathBuf,
}

fn tree() -> Tree {
    let scratch = ScratchDir::new("mounts").unwrap();
    let root = scratch.path().to_path_buf();
    let drive = root.join("drive");
    std::fs::create_dir_all(drive.join("docs")).unwrap();
    std::fs::write(drive.join("docs/a.txt"), "hello from the drive\n").unwrap();
    std::fs::write(drive.join(".env"), "KEY=secret\n").unwrap();
    Tree {
        _scratch: scratch,
        root,
        drive,
    }
}

impl Tree {
    fn problem(
        &self,
        path: &Path,
        rw: bool,
        guarded: &[PathBuf],
        table: &[Volume],
    ) -> Option<String> {
        problem(path, rw, guarded, table, &self.root.join("home"))
    }
}

#[test]
fn a_plain_folder_is_fine() {
    let t = tree();
    std::fs::create_dir(t.root.join("home")).unwrap();
    assert_eq!(t.problem(&t.drive, false, &[], &[]), None);
    assert_eq!(t.problem(&t.drive, true, &[], &[]), None);
}

#[test]
fn refusals() {
    let t = tree();
    let home = t.root.join("home");
    std::fs::create_dir_all(home.join(".ssh/keys")).unwrap();
    let p = |path: &Path| t.problem(path, false, &[], &[]).unwrap_or_default();
    assert!(p(Path::new("/")).contains("whole filesystem"));
    assert!(p(&home).contains("home directory"));
    assert!(p(&t.root).contains("home directory"));
    assert!(p(&home.join(".ssh/keys")).contains(".ssh"));
    assert!(p(Path::new("/etc")).contains("system directory"));
    assert!(p(Path::new("/usr/share")).contains("system directory"));
    assert!(p(&t.root.join("missing")).contains("does not exist"));
    assert!(p(&t.drive.join("docs/a.txt")).contains("not a folder"));
    let guarded = |g: &Path, path: &Path| {
        t.problem(path, false, &[g.to_path_buf()], &[])
            .unwrap_or_default()
    };
    assert!(guarded(&t.drive.join("docs"), &t.drive).contains("holds"));
    assert!(guarded(&t.drive, &t.drive.join("docs")).contains("inside"));
}

#[test]
fn rw_refused_on_a_boot_volume_and_a_read_only_drive() {
    let t = tree();
    std::fs::create_dir(t.root.join("home")).unwrap();
    std::fs::create_dir_all(t.drive.join("efi/boot")).unwrap();
    let table = [Volume {
        target: t.drive.clone(),
        source: "/dev/sda1".into(),
        fstype: "vfat".into(),
        read_only: false,
    }];
    assert_eq!(t.problem(&t.drive.join("docs"), false, &[], &table), None);
    let why = t.problem(&t.drive.join("docs"), true, &[], &table).unwrap();
    assert!(why.contains("boot or system volume"), "{why}");
    let ro = [Volume {
        read_only: true,
        fstype: "ext4".into(),
        ..table[0].clone()
    }];
    assert!(
        t.problem(&t.drive, true, &[], &ro)
            .unwrap()
            .contains("mounted read-only")
    );
}

#[test]
fn boot_markers() {
    let t = tree();
    assert_eq!(boot_reason(&t.drive), None);
    std::fs::create_dir_all(t.drive.join("Windows/System32")).unwrap();
    assert!(boot_reason(&t.drive).unwrap().contains("System32"));
}

/// NoteTests.test_note_and_title (the title half is viy-agent's sessions).
#[test]
fn note_and_title() {
    let ms = [Mount::new("/media/me/USB", false)];
    let n = note(&ms, &[]);
    assert!(is_note(&n));
    assert!(n.contains("/media/me/USB (read-only)"));
    assert!(!n.contains("write_file"));
    assert!(note(&[], &[PathBuf::from("/media/me/USB")]).contains("unmounted /media/me/USB"));
    assert!(!is_note("[rp chat: ssh: lent"));
}

#[test]
fn a_scan_finds_the_secret_files() {
    let t = tree();
    std::fs::create_dir_all(t.drive.join("node_modules/x")).unwrap();
    std::fs::write(t.drive.join("node_modules/x/.env"), "").unwrap();
    std::fs::write(t.drive.join("docs/id_rsa"), "").unwrap();
    let m = scan(&t.drive, false, &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert!(scan(&t.drive, false, &std::sync::atomic::AtomicBool::new(true)).is_none());
    let mut names: Vec<_> = m
        .secrets
        .iter()
        .map(|p| p.strip_prefix(&t.drive).unwrap().display().to_string())
        .collect();
    names.sort();
    assert_eq!(names, [".env", "docs/id_rsa"]);
}

/// rpctl's note (model-visible) and parse_arg against viy's
/// (`cargo test -p viy-tools --features oracle`).
#[cfg(feature = "oracle")]
#[test]
fn note_and_parse_arg_match_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let paths = ["/media/me/USB", "/mnt/disk one", "/srv/data", "/media/me/é"];
    for case in 0..cases(500) {
        let ms: Vec<(String, bool)> = (0..g.below(3))
            .map(|_| (g.pick(&paths).to_string(), g.below(2) == 0))
            .collect();
        let gone: Vec<String> = (0..g.below(3))
            .map(|_| g.pick(&paths).to_string())
            .collect();
        let theirs = oracle
            .call("mounts.note", json!([ms, gone]), json!({}))
            .unwrap()
            .outcome
            .to_viy();
        let mounts: Vec<Mount> = ms.iter().map(|(p, rw)| Mount::new(p, *rw)).collect();
        let gone: Vec<PathBuf> = gone.iter().map(PathBuf::from).collect();
        assert_eq!(
            theirs,
            Outcome::Returned(json!(note(&mounts, &gone))),
            "case {case}, seed {:#x}",
            seed()
        );
    }
    let pieces = [
        "/media/me/x",
        " ",
        "  ",
        "rw",
        "ro",
        "'",
        "\"",
        "a b",
        "\t",
        "rwx",
    ];
    for case in 0..cases(3000) {
        let arg = g.string(&pieces, 5);
        let theirs = oracle
            .call("mounts.parse_arg", json!([arg]), json!({}))
            .unwrap()
            .outcome;
        let (a, rw) = parse_arg(&arg);
        assert_eq!(
            theirs,
            Outcome::Returned(json!([a, rw])),
            "case {case}, seed {:#x}: {arg:?}",
            seed()
        );
    }
}

/// A mount point that is not UTF-8 keeps its real path and the other rows
/// (found by the WP 4.5 review: one bad line emptied the table, and with it
/// the rw refusals).
#[test]
fn a_mount_table_with_a_latin1_label() {
    use std::os::unix::ffi::OsStrExt;
    let raw =
        b"/dev/sda1 /media/me/CL\xc9 vfat rw 0 0\n/dev/sdb1 /media/me/USB\\040stick ext4 ro 0 0\n";
    let rows = parse_mount_table(raw);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].target.as_os_str().as_bytes(), b"/media/me/CL\xc9");
    assert_eq!(rows[1].target, PathBuf::from("/media/me/USB stick"));
    assert!(rows[1].read_only);
}

// rpctl's test_secret_files.py WalkTests.test_secret_files_walk_finds_them_and_skips_pub.
#[test]
fn secret_files_walk_finds_them_and_skips_pub() {
    let d = viy_testkit::scratch::ScratchDir::new("secret-walk").unwrap();
    let root = d.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    for n in [
        "secrets.json",
        ".env",
        "sub/id_rsa",
        "sub/id_rsa.pub",
        "app.py",
    ] {
        std::fs::write(root.join(n), "x").unwrap();
    }
    let mut found: Vec<String> = secret_files(root, crate::sandbox::SECRET_LIMIT)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, [".env", "id_rsa", "secrets.json"]);
}

// kb d131: viy's notes are `[viy chat: `; rp's `[rp chat: ` in chats rp
// saved still read as notes, an ssh note of either as not a mount's.
#[test]
fn notes_of_both_names_are_mount_notes() {
    assert!(is_note("[viy chat: the user mounted /srv]"));
    assert!(is_note("[rp chat: the user mounted /srv]"));
    assert!(!is_note("[viy chat: ssh: lent]"));
    assert!(!is_note("[rp chat: ssh: lent]"));
    assert!(!is_note("the user mounted /srv"));
}
