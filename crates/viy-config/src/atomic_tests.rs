use std::thread;

use viy_testkit::scratch::ScratchDir;

use super::*;

#[test]
fn threads_writing_one_file_each_replace_it_whole() {
    let dir = ScratchDir::new("atomic").unwrap();
    let file = dir.path().join("config");
    let writers: Vec<_> = (0..8)
        .map(|w| {
            let file = file.clone();
            thread::spawn(move || {
                let text = format!("{w}\n").repeat(4096);
                for _ in 0..50 {
                    write_atomically(&file, &text).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    let text = fs::read_to_string(&file).unwrap();
    let first = text.lines().next().unwrap();
    assert_eq!(text, format!("{first}\n").repeat(4096));
    let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(left.len(), 1);
}
