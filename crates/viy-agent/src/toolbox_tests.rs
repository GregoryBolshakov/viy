//! rpctl tests/test_toolbox.py: ManifestTests, IndexTests, SearchTests
//! and CliTests (the chat's and the sandbox's are in
//! `toolbox_chat_tests.rs`), and the oracle fuzz of the pure functions.

use std::os::unix::fs::PermissionsExt;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;

pub(crate) fn manifest(
    name: &str,
    summary: &str,
    use_when: &str,
    tags: &str,
    author: &str,
    status: &str,
    created: &str,
) -> String {
    format!(
        "---\nname: {name}\nsummary: {summary}\nuse_when: {use_when}\nusage: {name} FILE \
         [--json]\nexamples:\n  - {name} report.pdf --json\ntags: [{tags}]\nauthor: {author}\n\
         status: {status}\ncreated: {created}\n---\n\nFull usage for {name}.\n"
    )
}

/// rp's `make_tool`, its defaults overridden by `over` (summary,
/// use_when, tags, author, status, created, script, executable).
pub(crate) fn make_tool(root: &Path, name: &str, over: &[(&str, &str)]) -> PathBuf {
    let get = |k: &str, d: &'static str| {
        over.iter()
            .find(|(key, _)| *key == k)
            .map_or(d.to_string(), |(_, v)| (*v).to_string())
    };
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("TOOL.md"),
        manifest(
            name,
            &get("summary", "Does a thing"),
            &get("use_when", "you need the thing"),
            &get("tags", "thing"),
            &get("author", "claude"),
            &get("status", "tested"),
            &get("created", "2026-07-08"),
        ),
    )
    .unwrap();
    let exe = d.join(name);
    let script = get("script", "");
    let script = if script.is_empty() {
        format!("#!/bin/sh\necho {name} ran with \"$@\"\n")
    } else {
        script
    };
    std::fs::write(&exe, script).unwrap();
    if get("executable", "1") == "1" {
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    d
}

pub(crate) struct Base {
    pub(crate) root: ScratchDir,
    pub(crate) dirs: Dirs,
}

impl Base {
    pub(crate) fn new() -> Self {
        let root = ScratchDir::new("toolbox").unwrap();
        let dirs = Dirs {
            global: root.path().join("data").join("toolbox"),
            project: root.path().join("proj").join(".viy").join("toolbox"),
        };
        std::fs::create_dir_all(&dirs.global).unwrap();
        std::fs::create_dir_all(&dirs.project).unwrap();
        Self { root, dirs }
    }

    fn rewrite(&self, d: &Path, from: &str, to: &str) {
        let text = std::fs::read_to_string(d.join("TOOL.md")).unwrap();
        std::fs::write(d.join("TOOL.md"), text.replace(from, to)).unwrap();
    }
}

fn texts(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.text.clone()).collect()
}

// ---- ManifestTests ----------------------------------------------------------------

#[test]
fn parses_scalars_quotes_and_both_list_forms() {
    let (meta, body) = parse_manifest(
        "---\nname: x-y\nsummary: \"Quoted: yes\"\ntags: [a, 'b c']\n\
         examples:\n  - x-y one\n  - x-y two\ndeps: requests\n---\n\nBody here\n",
    )
    .unwrap();
    assert_eq!(meta["summary"], Field::Text("Quoted: yes".into()));
    assert_eq!(meta["tags"], Field::List(vec!["a".into(), "b c".into()]));
    assert_eq!(
        meta["examples"],
        Field::List(vec!["x-y one".into(), "x-y two".into()])
    );
    assert_eq!(meta["deps"], Field::List(vec!["requests".into()]));
    assert_eq!(body, "Body here");
}

#[test]
fn refuses_a_file_without_frontmatter() {
    assert!(parse_manifest("# just markdown\n").is_err());
    assert!(parse_manifest("---\nname: x\n").is_err());
}

#[test]
fn a_good_tool_has_no_problems() {
    let b = Base::new();
    let t = b
        .dirs
        .load(&make_tool(&b.dirs.global, "pdf-tables", &[]), Scope::Global)
        .unwrap();
    assert_eq!(t.problems, Vec::<String>::new());
    assert_eq!(t.status(), "tested");
}

#[test]
fn problems_keep_a_tool_out() {
    let b = Base::new();
    make_tool(&b.dirs.global, "noexec", &[("executable", "0")]);
    let d = make_tool(&b.dirs.global, "renamed", &[]);
    b.rewrite(&d, "name: renamed", "name: other");
    make_tool(&b.dirs.global, "ls", &[]); // already a command here
    let d = make_tool(&b.dirs.global, "nousage", &[]);
    b.rewrite(&d, "usage: nousage FILE [--json]\n", "");
    let probs: BTreeMap<String, String> = b
        .dirs
        .scan_all()
        .into_iter()
        .map(|t| (t.name.clone(), t.problems.join(" ")))
        .collect();
    assert!(probs["noexec"].contains("not executable"), "{probs:?}");
    assert!(probs["renamed"].contains("does not match"), "{probs:?}");
    assert!(probs["ls"].contains("already a command"), "{probs:?}");
    assert!(probs["nousage"].contains("missing usage"), "{probs:?}");
    assert_eq!(b.dirs.scan(), Vec::new());
    assert_eq!(cmd_check(&b.dirs).1, 1);
}

#[test]
fn project_overrides_global_and_comes_first() {
    let b = Base::new();
    let g = &b.dirs.global;
    make_tool(
        g,
        "same",
        &[("summary", "global one"), ("created", "2026-06-18")],
    );
    make_tool(g, "old-global", &[("created", "2026-01-01")]);
    make_tool(g, "new-global", &[("created", "2026-07-07")]);
    make_tool(
        &b.dirs.project,
        "same",
        &[("summary", "project one"), ("created", "2026-05-19")],
    );
    let tools = b.dirs.scan();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["same", "new-global", "old-global"]);
    let out = texts(&cmd_list(&b.dirs));
    assert_eq!(out.iter().filter(|o| o.contains("overridden")).count(), 1);
    assert_eq!(tools[0].text("summary"), "project one");
    assert_eq!(b.dirs.get("same").unwrap().scope, Scope::Project);
}

// ---- IndexTests -------------------------------------------------------------------

#[test]
fn index_lines_are_one_line_and_mark_status() {
    let b = Base::new();
    make_tool(
        &b.dirs.global,
        "csv-profile",
        &[
            ("summary", "Profile  a\tCSV\x1b[31m"),
            ("status", "untested"),
        ],
    );
    let broken = make_tool(&b.dirs.global, "two-lines", &[]);
    b.rewrite(
        &broken,
        "summary: Does a thing",
        "summary: Does a thing\nand a stray line",
    );
    let text = render_index(&b.dirs.scan(), INDEX_MAX_DEFAULT).unwrap();
    assert!(text.contains("# Toolbox"));
    assert!(!text.contains("two-lines"));
    let line = text
        .lines()
        .find(|l| l.starts_with("- csv-profile"))
        .unwrap();
    assert!(line.contains("Profile a CSV[31m"), "{line}");
    assert!(!line.contains('\x1b'));
    assert!(
        line.contains("Use when: you need the thing. Usage: `csv-profile FILE [--json]`"),
        "{line}"
    );
    assert!(line.ends_with("(untested)"));
}

#[test]
fn index_is_capped_and_points_to_find_tool() {
    let b = Base::new();
    for i in 0..5 {
        make_tool(&b.dirs.global, &format!("tool-{i}"), &[]);
    }
    let text = render_index(&b.dirs.scan(), 3).unwrap();
    assert_eq!(text.lines().filter(|l| l.starts_with("- tool-")).count(), 3);
    assert!(text.contains("2 more: find_tool"));
}

#[test]
fn empty_toolbox_has_no_index() {
    assert_eq!(render_index(&[], INDEX_MAX_DEFAULT), None);
}

// ---- SearchTests ------------------------------------------------------------------

fn search_base() -> Base {
    let b = Base::new();
    let g = &b.dirs.global;
    make_tool(
        g,
        "pdf-tables",
        &[
            ("summary", "Extract tables from a text PDF"),
            ("use_when", "a PDF has tables you need as data"),
            ("tags", "pdf, table, csv"),
        ],
    );
    make_tool(
        g,
        "html-text",
        &[
            ("summary", "Turn an HTML page into plain text"),
            ("use_when", "you saved a web page"),
            ("tags", "html, text"),
        ],
    );
    make_tool(
        g,
        "csv-profile",
        &[
            ("summary", "Column types and counts of a CSV"),
            ("use_when", "you get a new CSV file"),
            ("tags", "csv, stats"),
        ],
    );
    b
}

#[test]
fn ranks_by_meaning_words_with_word_forms() {
    let b = search_base();
    let tools = b.dirs.scan();
    assert_eq!(
        search("parsing tables out of pdfs", &tools, 5)[0].1.name,
        "pdf-tables"
    );
    assert_eq!(search("html", &tools, 5)[0].1.name, "html-text");
    assert!(search("quantum chemistry", &tools, 5).is_empty());
}

#[test]
fn find_tool_by_name_query_and_listing() {
    let b = search_base();
    let tools = b.dirs.scan();
    let args = |v: Value| v.as_object().unwrap().clone();
    let r = run_find(&args(json!({"name": "pdf-tables"})), &tools);
    assert_eq!(r["tool"]["name"], "pdf-tables");
    assert!(r["tool"]["doc"].as_str().unwrap().contains("Full usage"));
    assert!(
        r["tool"]["path"]
            .as_str()
            .unwrap()
            .ends_with("pdf-tables/pdf-tables")
    );
    let r = run_find(&args(json!({"name": "pdf-table"})), &tools);
    assert!(
        r["did_you_mean"]
            .as_array()
            .unwrap()
            .contains(&json!("pdf-tables"))
    );
    let r = run_find(&args(json!({"query": "profile a csv"})), &tools);
    assert_eq!(r["matches"][0]["name"], "csv-profile");
    let r = run_find(&args(json!({"query": "zebra"})), &tools);
    assert_eq!(r["matches"], json!([]));
    assert_eq!(r["note"], "no tool fits 'zebra'; 3 tool(s) in the toolbox");
    let r = run_find(&Map::new(), &tools);
    assert_eq!(r["tools"].as_array().unwrap().len(), 3);
}

#[test]
fn used_in_finds_command_words_only() {
    let b = search_base();
    let tools = b.dirs.scan();
    let names =
        |c: &str| -> Vec<String> { used_in(c, &tools).iter().map(|t| t.name.clone()).collect() };
    assert_eq!(
        names("cd data && pdf-tables a.pdf --json | head"),
        ["pdf-tables"]
    );
    assert_eq!(
        names("FOO=1 env time csv-profile x.csv; html-text p.html"),
        ["csv-profile", "html-text"]
    );
    assert_eq!(names("cat pdf-tables/TOOL.md"), Vec::<String>::new());
    assert_eq!(names("echo $(html-text page.html)"), ["html-text"]);
}

// ---- CliTests ---------------------------------------------------------------------

#[test]
fn add_validates_copies_and_refuses_an_existing_name() {
    let b = Base::new();
    let src = make_tool(&b.root.path().join("src"), "json-query", &[]);
    std::fs::create_dir(src.join("__pycache__")).unwrap();
    assert_eq!(cmd_add(&b.dirs, &src, Scope::Global, true).1, 0);
    assert!(
        b.dirs
            .global
            .join("json-query")
            .join("json-query")
            .is_file()
    );
    assert!(
        !b.dirs
            .global
            .join("json-query")
            .join("__pycache__")
            .exists()
    );
    assert_eq!(cmd_add(&b.dirs, &src, Scope::Global, true).1, 1);
    let bad = make_tool(
        &b.root.path().join("src"),
        "bad-one",
        &[("executable", "0")],
    );
    assert_eq!(cmd_add(&b.dirs, &bad, Scope::Project, true).1, 1);
    assert!(!b.dirs.project.join("bad-one").exists());
    // and the project scope needs the project's folder
    let (said, code) = cmd_add(&b.dirs, &src, Scope::Project, false);
    assert_eq!(code, 1);
    assert_eq!(
        texts(&said),
        ["no .viy/ here: run viy init, or use --global"]
    );
}

#[test]
fn rm_asks_first() {
    let b = Base::new();
    make_tool(&b.dirs.project, "gone-soon", &[]);
    let mut asked = Vec::new();
    let (_, code) = cmd_rm(&b.dirs, "gone-soon", Scope::Project, &mut |q| {
        asked.push(q.to_string());
        false
    });
    assert_eq!(code, 1);
    assert!(b.dirs.project.join("gone-soon").exists());
    assert_eq!(
        asked,
        [format!(
            "delete {}? [y/N] ",
            b.dirs.project.join("gone-soon").display()
        )]
    );
    assert_eq!(
        cmd_rm(&b.dirs, "gone-soon", Scope::Project, &mut |_| true).1,
        0
    );
    assert!(!b.dirs.project.join("gone-soon").exists());
}

// ---- the oracle ---------------------------------------------------------------------

/// A tool as the oracle's helpers take it: `[name, scope, TOOL.md]`.
#[cfg(feature = "oracle")]
fn tools_of(manifests: &[(String, &str, String)]) -> Vec<Tool> {
    manifests
        .iter()
        .map(|(name, scope, text)| {
            let (meta, body) = parse_manifest(text).unwrap();
            Tool {
                name: name.clone(),
                scope: if *scope == "project" {
                    Scope::Project
                } else {
                    Scope::Global
                },
                dir: PathBuf::from("/tb").join(scope).join(name),
                meta,
                body,
                problems: Vec::new(),
            }
        })
        .collect()
}

/// The manifest checks of [`validate`] that a tool in the index passed.
#[cfg(feature = "oracle")]
fn fields_valid(meta: &Meta) -> bool {
    let text_or_none = |k: &str| matches!(meta.get(k), None | Some(Field::Text(_)));
    let one_of = |k: &str, allowed: &[&str]| match meta.get(k) {
        None => true,
        Some(Field::Text(v)) => v.is_empty() || allowed.contains(&v.as_str()),
        Some(Field::List(l)) => l.is_empty(),
    };
    ["summary", "use_when", "usage"]
        .iter()
        .all(|k| text_or_none(k))
        && one_of("author", &AUTHORS)
        && one_of("status", &STATUSES)
}

#[cfg(feature = "oracle")]
fn meta_json(meta: &Meta) -> Value {
    Value::Object(
        meta.iter()
            .map(|(k, f)| (k.clone(), f.to_value()))
            .collect(),
    )
}

// Pieces for TOOL.md lines and values. Left out, as implementation details
// (kb d029): the line breaks Python's splitlines adds (\r, \x0b, \x0c,
// \x1c-\x1e, \x85,  ,  ), \x1f (whitespace to Python, not to
// Rust), Unicode digits, and characters whose category differs between
// Unicode versions.
#[cfg(feature = "oracle")]
const VALUE: &[&str] = &[
    "a", "pdf", "tables", "parse", "parser", "parsing", "csv", " ", "  ", "\t", ":", ", ", "'",
    "\"", "[", "]", "-", "#", "x-y", "é", "Ж", "😀", "\u{200b}", "\u{ad}", "\x1b[31m", "\x07",
    "\u{e000}", "!", ".", "?", "Use", "text", "HTML", "data", "s", "ss", "2026-10-", "0", "K",
    "\u{212a}",
];

#[cfg(feature = "oracle")]
fn tool_md(g: &mut viy_testkit::fuzz::Gen, name: &str) -> String {
    let keys = [
        "name", "summary", "use_when", "usage", "examples", "tags", "deps", "author", "status",
        "created", "selftest", "Bad", "x",
    ];
    let mut lines = vec!["---".to_string()];
    if g.below(4) > 0 {
        lines.push(format!("name: {name}"));
    }
    for _ in 0..g.below(8) {
        let k = *g.pick(&keys);
        let v = g.string(VALUE, 6);
        lines.push(match g.below(7) {
            0 => format!("{k}:"),
            1 => format!("{k}: [{v}, {}]", g.string(VALUE, 3)),
            2 => format!("  - {v}"),
            3 => format!("\t-{v}"),
            4 => String::new(),
            5 if g.below(4) == 0 => v.trim().to_string(),
            _ => format!("{k}: {v}"),
        });
    }
    if g.below(10) > 0 {
        lines.push(if g.below(2) == 0 { "---" } else { " --- " }.into());
    }
    lines.push(g.string(VALUE, 5));
    if g.below(3) == 0 {
        lines.insert(0, String::new());
    }
    lines.join("\n")
}

#[cfg(feature = "oracle")]
#[test]
fn toolbox_matches_rpctl() {
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;
    use viy_testkit::oracle::tuple;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let mut call = |f: &str, args: Value| oracle.call(f, args, json!({})).unwrap().outcome;
    let names = [
        "pdf-tables",
        "html-text",
        "csv-profile",
        "a1",
        "say-hi",
        "x-y",
        "tool-0",
    ];
    // parsed, indexed with tools, searched with hits, used_in and suggest with hits
    let mut seen = [0usize; 5];
    for case in 0..cases(3000) {
        // parse_manifest
        let name = *g.pick(&names);
        let text = tool_md(&mut g, name);
        let at = format!("case {case}, seed {:#x}", seed());
        let want = call("toolbox.parse_manifest", json!([text]));
        match parse_manifest(&text) {
            Ok((meta, body)) => {
                seen[0] += 1;
                assert_eq!(
                    want,
                    Outcome::Returned(tuple(vec![meta_json(&meta), json!(body)])),
                    "parse {at}: {text:?}"
                );
            }
            Err(e) => assert!(
                matches!(&want, Outcome::Raised { message, .. } if *message == e),
                "parse {at}: {text:?}: {e} vs {want:?}"
            ),
        }

        // tools that parse, for the index, search and describe
        let mut manifests = Vec::new();
        for _ in 0..1 + g.below(5) {
            let name = (*g.pick(&names)).to_string();
            let scope = *g.pick(&["global", "project"]);
            let text = tool_md(&mut g, &name);
            // as scan() gives them: the field checks of validate passed
            if parse_manifest(&text).is_ok_and(|(meta, _)| fields_valid(&meta)) {
                manifests.push((name, scope, text));
            }
        }
        let tools = tools_of(&manifests);
        seen[1] += usize::from(!tools.is_empty());
        let wire: Vec<Value> = manifests.iter().map(|(n, s, t)| json!([n, s, t])).collect();
        let limit = g.below(5);
        assert_eq!(
            call("toolbox.index", json!([wire, limit])),
            Outcome::Returned(json!(render_index(&tools, limit))),
            "index {at}: {manifests:?}"
        );
        let full = g.below(2) == 0;
        let described: Vec<Value> = tools.iter().map(|t| describe(t, full)).collect();
        assert_eq!(
            call("toolbox.describe", json!([wire, full])),
            Outcome::Returned(json!(described)),
            "describe {at}: {manifests:?}"
        );
        let query = g.string(VALUE, 6);
        let Outcome::Returned(Value::Array(want)) = call("toolbox.search", json!([wire, query, 5]))
        else {
            panic!("search {at}: no list");
        };
        let got = search(&query, &tools, 5);
        seen[2] += usize::from(!got.is_empty());
        assert_eq!(
            want.len(),
            got.len(),
            "search {at}: {query:?} {manifests:?}"
        );
        for (w, (s, t)) in want.iter().zip(&got) {
            // rpctl sums over a set, in hash order: equal to the last bits
            let ws = w[0].as_f64().unwrap();
            assert!((ws - s).abs() < 1e-9, "search {at}: {ws} vs {s}");
            if w[1] != json!(t.name) {
                let tie = want
                    .iter()
                    .any(|x| x[1] == json!(t.name) && (x[0].as_f64().unwrap() - ws).abs() < 1e-9);
                assert!(tie, "search {at}: {query:?} {want:?} vs {got:?}");
            }
        }

        // search over manifests written as people write them, so most
        // queries hit and the ranking is what is compared
        let pool = [
            "tables",
            "table",
            "parse",
            "parser",
            "parsing",
            "pdf",
            "pdfs",
            "csv",
            "CSV",
            "html",
            "text",
            "page",
            "pages",
            "data",
            "stats",
            "convert",
            "converting",
            "fetch",
            "json",
            "xml",
            "a",
            "of",
            "the",
            "into",
            "plain",
            "Column",
            "columns",
            "class",
            "glass",
        ];
        let phrase = |g: &mut viy_testkit::fuzz::Gen, n: usize| -> String {
            (0..1 + g.below(n))
                .map(|_| *g.pick(&pool))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut written = Vec::new();
        for i in 0..2 + g.below(5) {
            let name = format!("{}-{i}", g.pick(&pool).to_lowercase());
            let text = format!(
                "---\nname: {name}\nsummary: {}\nuse_when: {}\nusage: {name} {}\ntags: [{}, {}]\n\
                 examples:\n  - {name} {}\n---\n",
                phrase(&mut g, 6),
                phrase(&mut g, 6),
                phrase(&mut g, 2),
                phrase(&mut g, 1),
                phrase(&mut g, 1),
                phrase(&mut g, 3),
            );
            written.push((name, "global", text));
        }
        let wtools = tools_of(&written);
        let wwire: Vec<Value> = written.iter().map(|(n, s, t)| json!([n, s, t])).collect();
        let query = phrase(&mut g, 4);
        let Outcome::Returned(Value::Array(want)) =
            call("toolbox.search", json!([wwire, query, 5]))
        else {
            panic!("search {at}: no list");
        };
        let got = search(&query, &wtools, 5);
        seen[2] += usize::from(!got.is_empty());
        assert_eq!(want.len(), got.len(), "search {at}: {query:?} {written:?}");
        for (w, (s, t)) in want.iter().zip(&got) {
            let ws = w[0].as_f64().unwrap();
            assert!((ws - s).abs() < 1e-9, "search {at}: {ws} vs {s}");
            if w[1] != json!(t.name) {
                let tie = want
                    .iter()
                    .any(|x| x[1] == json!(t.name) && (x[0].as_f64().unwrap() - ws).abs() < 1e-9);
                assert!(tie, "search {at}: {query:?} {want:?} vs {got:?}");
            }
        }

        // used_in and suggest
        let kit: Vec<&str> = names.iter().copied().filter(|_| g.below(2) == 0).collect();
        let cmd_pieces = [
            "pdf-tables",
            "html-text",
            "say-hi",
            " ",
            "&&",
            "||",
            "|",
            ";",
            "&",
            "(",
            ")",
            "\n",
            "$(",
            "`",
            "env ",
            "time ",
            "FOO=1 ",
            "-x ",
            "'",
            "\"",
            "/usr/bin/",
            "./",
            "12 ",
            "cat ",
            "x-y",
            "a1",
            "=",
            "é",
        ];
        let cmd = g.string(&cmd_pieces, 10);
        let kit_tools = tools_of(
            &kit.iter()
                .map(|n| ((*n).to_string(), "global", format!("---\nname: {n}\n---\n")))
                .collect::<Vec<_>>(),
        );
        let found: Vec<String> = used_in(&cmd, &kit_tools)
            .iter()
            .map(|t| t.name.clone())
            .collect();
        seen[3] += usize::from(!found.is_empty());
        assert_eq!(
            call("toolbox.used_in", json!([kit, cmd])),
            Outcome::Returned(json!(found)),
            "used_in {at}: {cmd:?} {kit:?}"
        );
        let word = g.string(
            &["pdf", "-", "tables", "t", "x", "html", "a", "1", "é", "say"],
            4,
        );
        seen[4] += usize::from(!suggest(&word, &kit).is_empty());
        assert_eq!(
            call("toolbox.suggest", json!([word, kit])),
            Outcome::Returned(json!(suggest(&word, &kit))),
            "suggest {at}: {word:?} {kit:?}"
        );
    }
    eprintln!("parsed, indexed, searched, used_in, suggest with hits: {seen:?}");
}
