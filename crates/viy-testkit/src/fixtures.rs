//! The fixture loader: rpctl's outputs at the pinned commit, written by
//! `tools/fixtures.py` to `tests/fixtures/<area>/<name>.json`, read back
//! with no Python present (implementation plan 3.3).
//!
//! A fixture generated from another rpctl commit than the one in
//! `tests/rpctl-ref.txt` is refused: after a re-pin, the fixtures are
//! regenerated before any test may trust them.

use std::io;
use std::path::PathBuf;

use serde_json::Value;

use crate::repo::pinned_commit;
use crate::repo::repo_root;

/// One fixture file.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// `<area>/<name>`, e.g. `config/layering`.
    pub name: String,
    /// The full rpctl commit it was generated from.
    pub rpctl_commit: String,
    /// The generator's own description of what the cases are.
    pub about: String,
    /// The cases, as the generator wrote them.
    pub cases: Vec<Value>,
}

impl Fixture {
    /// The case called `name`.
    pub fn case(&self, name: &str) -> Option<&Value> {
        self.cases
            .iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
    }
}

/// Loads `tests/fixtures/<name>.json`, refusing one generated from a commit
/// other than the pinned one.
pub fn load(name: &str) -> io::Result<Fixture> {
    let text = std::fs::read_to_string(path(name))?;
    let mut doc: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let field = |doc: &Value, key: &str| {
        doc.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| io::Error::other(format!("fixture {name}: no {key:?}")))
    };
    let rpctl_commit = field(&doc, "rpctl_commit")?;
    let pinned = pinned_commit()
        .ok_or_else(|| io::Error::other("tests/rpctl-ref.txt has no commit line"))?;
    if rpctl_commit != pinned {
        return Err(io::Error::other(format!(
            "fixture {name} is from rpctl {rpctl_commit}, the pin is {pinned}: \
             run python3 tools/fixtures.py write {name}"
        )));
    }
    let about = field(&doc, "about")?;
    let cases = match doc.get_mut("cases").map(Value::take) {
        Some(Value::Array(cases)) => cases,
        _ => return Err(io::Error::other(format!("fixture {name}: no cases"))),
    };
    Ok(Fixture {
        name: name.to_string(),
        rpctl_commit,
        about,
        cases,
    })
}

/// Every fixture name under `tests/fixtures/`, sorted.
pub fn all() -> io::Result<Vec<String>> {
    let base = repo_root().join("tests").join("fixtures");
    let mut names = Vec::new();
    for area in std::fs::read_dir(&base)? {
        let area = area?;
        if !area.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(area.path())? {
            let file = file?.path();
            if file.extension().is_some_and(|e| e == "json")
                && let (Some(a), Some(stem)) = (
                    area.file_name().to_str(),
                    file.file_stem().and_then(|s| s.to_str()),
                )
            {
                names.push(format!("{a}/{stem}"));
            }
        }
    }
    names.sort();
    Ok(names)
}

fn path(name: &str) -> PathBuf {
    repo_root()
        .join("tests")
        .join("fixtures")
        .join(format!("{name}.json"))
}

#[cfg(test)]
#[path = "fixtures_tests.rs"]
mod tests;
