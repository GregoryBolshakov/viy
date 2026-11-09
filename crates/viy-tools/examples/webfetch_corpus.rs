//! The corpus check of fetch_url's extractor (flags F10, kb d017, d116, WP 5.2):
//! pages read once and kept, then made text by viy ([`webfetch::page`])
//! and by rpctl's `direct()` through the oracle, and compared.
//!
//! ```text
//! cargo run -q -p viy-tools --features oracle --example webfetch_corpus -- fetch DIR < urls
//! cargo run -q -p viy-tools --features oracle --example webfetch_corpus -- evals DIR
//! cargo run -q -p viy-tools --features oracle --example webfetch_corpus -- compare DIR
//! ```
//!
//! `fetch` reads each URL on stdin with netguard (the system's lookup and
//! TLS, d021's User-Agent) and keeps `N.json` (URL, final URL, status,
//! headers) and `N.body`. `evals` writes rpctl's injection eval pages the
//! same way. `compare` prints one line per page and a summary: the same
//! text when whitespace is set aside, the same title and links, and the
//! words one shows that the other does not. The pages are other people's,
//! so DIR belongs under target/, never in the repository.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use serde_json::Value;
use serde_json::json;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;
use viy_tools::netguard;
use viy_tools::netguard::Options;
use viy_tools::netguard::Response;
use viy_tools::webfetch;

type Fail = Box<dyn std::error::Error>;

fn save(dir: &Path, name: &str, meta: &Value, body: &[u8]) -> Result<(), Fail> {
    fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(meta)?,
    )?;
    fs::write(dir.join(format!("{name}.body")), body)?;
    Ok(())
}

async fn fetch(dir: &Path) -> Result<(), Fail> {
    let mut urls = String::new();
    std::io::stdin().read_to_string(&mut urls)?;
    for (n, url) in urls.split_whitespace().enumerate() {
        let got = netguard::get(
            url,
            &Options::default(),
            &netguard::SystemResolver,
            &netguard::Direct,
        )
        .await;
        match got {
            Ok(r) => {
                println!("{n:02} {} {} bytes {url}", r.status, r.body.len());
                let meta = json!({
                    "url": url, "final": r.url, "status": r.status,
                    "headers": r.headers, "truncated": r.truncated,
                });
                save(dir, &format!("{n:02}"), &meta, &r.body)?;
            }
            Err(e) => println!("{n:02} failed: {e} {url}"),
        }
    }
    Ok(())
}

fn evals(dir: &Path) -> Result<(), Fail> {
    let mut oracle = Oracle::spawn()?;
    let Outcome::Returned(got) = oracle
        .call("webfetch.eval_pages", json!([]), json!({}))?
        .outcome
    else {
        return Err("the oracle has no eval pages".into());
    };
    let url = got["url"].as_str().unwrap_or_default();
    for (name, html) in got["pages"].as_object().into_iter().flatten() {
        let meta = json!({
            "url": url, "final": url, "status": 200,
            "headers": {"content-type": "text/html; charset=utf-8"}, "truncated": false,
        });
        save(
            dir,
            &format!("eval-{name}"),
            &meta,
            html.as_str().unwrap_or_default().as_bytes(),
        )?;
    }
    Ok(())
}

/// Text as words, whitespace set aside.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// The words of `a` that `b` lacks, by count.
fn extra<'a>(a: &[&'a str], b: &[&str]) -> Vec<&'a str> {
    let mut have: BTreeMap<&str, usize> = BTreeMap::new();
    for w in b {
        *have.entry(w).or_default() += 1;
    }
    a.iter()
        .filter(|w| match have.get_mut(**w) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .copied()
        .collect()
}

fn compare(dir: &Path) -> Result<(), Fail> {
    let mut oracle = Oracle::spawn()?;
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
        .collect();
    names.sort();
    let (mut same, mut differ, mut not_text) = (0, 0, 0);
    for name in names {
        let meta: Value = serde_json::from_slice(&fs::read(dir.join(format!("{name}.json")))?)?;
        let body = fs::read(dir.join(format!("{name}.body")))?;
        let headers = meta["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        let resp = Response {
            status: u16::try_from(meta["status"].as_u64().unwrap_or(200)).unwrap_or(200),
            headers,
            body: body.clone(),
            url: meta["final"].as_str().unwrap_or_default().to_string(),
            redirects: Vec::new(),
            truncated: meta["truncated"].as_bool().unwrap_or(false),
        };
        let url = meta["url"].as_str().unwrap_or_default();
        let started = std::time::Instant::now();
        let mine = webfetch::page(url, &resp);
        let took = started.elapsed();
        if took.as_millis() >= 20 {
            eprintln!("{name}: {} bytes made text in {took:.0?}", body.len());
        }
        let bytes = json!({"$bytes": base64::engine::general_purpose::STANDARD.encode(&body)});
        let theirs = oracle
            .call(
                "webfetch.text",
                json!([bytes, resp.headers, resp.url]),
                json!({}),
            )?
            .outcome;
        let (mine, theirs) = match (mine, theirs) {
            (Ok(doc), Outcome::Returned(t)) => (doc, t["$tuple"].clone()),
            (Err(e), Outcome::Raised { message, .. }) => {
                println!("{name} both refuse: {e} | {message}");
                not_text += 1;
                continue;
            }
            (mine, theirs) => {
                println!("{name} DIFFER: {mine:?} | {theirs:?}");
                differ += 1;
                continue;
            }
        };
        let text = theirs[2].as_str().unwrap_or_default();
        let (a, b) = (words(&mine.text), words(text));
        let links: Vec<Value> = mine
            .links
            .iter()
            .map(|l| l.clone().map_or(Value::Null, Value::String))
            .collect();
        let title_same = words(&mine.title) == words(theirs[1].as_str().unwrap_or_default());
        let links_same = Value::Array(links) == theirs[3];
        let kind_same = mine.kind.as_str() == theirs[0].as_str().unwrap_or_default();
        if a == b && title_same && links_same && kind_same {
            same += 1;
            let exact = if mine.text == text {
                "exact"
            } else {
                "whitespace differs"
            };
            println!(
                "{name} same ({}, {} words, {} links, {exact}) {url}",
                mine.kind.as_str(),
                a.len(),
                mine.links.len()
            );
            continue;
        }
        differ += 1;
        if !links_same {
            let theirs_links = theirs[3].as_array().cloned().unwrap_or_default();
            let n = mine.links.len().max(theirs_links.len());
            let at = (0..n).find(|&i| {
                mine.links
                    .get(i)
                    .map(|l| l.clone().map_or(Value::Null, Value::String))
                    != theirs_links.get(i).cloned()
            });
            if let Some(i) = at {
                println!(
                    "  links {} vs {}, first differ at [{}]: viy {:?} rpctl {}",
                    mine.links.len(),
                    theirs_links.len(),
                    i + 1,
                    mine.links.get(i),
                    theirs_links.get(i).cloned().unwrap_or(Value::Null)
                );
            }
        }
        let only_viy = extra(&a, &b);
        let only_rpctl = extra(&b, &a);
        println!(
            "{name} DIFFER kind {} title {title_same} links {links_same}: {} words, {} only in viy {:?}, {} only in rpctl {:?} {url}",
            if kind_same { "same" } else { "differs" },
            a.len(),
            only_viy.len(),
            &only_viy[..only_viy.len().min(12)],
            only_rpctl.len(),
            &only_rpctl[..only_rpctl.len().min(12)],
        );
    }
    println!("{same} the same, {differ} differ, {not_text} not text in both");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Fail> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cmd, dir] = args.as_slice() else {
        return Err("usage: webfetch_corpus fetch|evals|compare DIR".into());
    };
    let dir = Path::new(dir);
    fs::create_dir_all(dir)?;
    match cmd.as_str() {
        "fetch" => fetch(dir).await,
        "evals" => evals(dir),
        "compare" => compare(dir),
        other => Err(format!("unknown command {other}").into()),
    }
}
