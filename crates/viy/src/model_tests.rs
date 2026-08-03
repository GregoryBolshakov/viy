use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_config::Layers;
use viy_config::Project;
use viy_testkit::config_tree::ConfigTree;
use viy_testkit::config_tree::case_env;
use viy_testkit::config_tree::case_files;
use viy_testkit::fixtures;

use super::*;

fn paths(tree: &ConfigTree) -> Paths {
    let config_home = tree.config_home();
    Paths::from_vars(tree.home(), move |name| {
        (name == "XDG_CONFIG_HOME").then(|| config_home.clone().into_os_string())
    })
}

/// argparse's view of `rp model ARGV`.
fn parse(argv: &[Value]) -> ModelArgs {
    let words: Vec<&str> = argv.iter().map(|a| a.as_str().unwrap()).collect();
    let action = match words[0] {
        "list" => ModelAction::List,
        "show" => ModelAction::Show,
        "use" => ModelAction::Use,
        "add" => ModelAction::Add,
        other => panic!("no such action in the fixture: {other}"),
    };
    let rest: Vec<&str> = words[1..]
        .iter()
        .copied()
        .filter(|w| *w != "--force")
        .collect();
    ModelArgs {
        action,
        name: rest.first().map(ToString::to_string),
        force: words.contains(&"--force"),
    }
}

/// rpctl's text as viy says it: viy's command name and config directory.
fn as_viy(text: &str, tree: &ConfigTree) -> String {
    text.replace("rp model", "viy model")
        .replace("(rp down)", "(viy down)")
        .replace("&& rp up", "&& viy up")
        .replace("$T/config/rpctl/", "$T/config/viy/")
        .replace("$T", &tree.root().to_string_lossy())
}

#[test]
fn viy_model_does_what_rp_model_does() {
    let fixture = fixtures::load("config/model").unwrap();
    assert_eq!(fixture.cases.len(), 21);
    for marker in [".viy", ".rpctl"] {
        for case in &fixture.cases {
            let name = case["name"].as_str().unwrap();
            let tree = ConfigTree::new(case, marker).unwrap();
            let paths = paths(&tree);
            let env = case_env(case);
            let project = Project::find(&tree.project()).unwrap();
            let cfg = Layers::new(&paths, &project)
                .load(|key| env.get(key).cloned())
                .unwrap();
            let mut out = Vec::new();
            let result = cmd_model(
                &cfg,
                &paths,
                &parse(case["argv"].as_array().unwrap()),
                &mut out,
            );
            let (exit, stderr) = match result {
                Ok(()) => (0, String::new()),
                Err(msg) => (1, format!("{msg}\n")),
            };
            assert_eq!(exit, case["exit"].as_i64().unwrap(), "{name}");
            let stdout = String::from_utf8(out).unwrap();
            assert_eq!(
                stdout,
                as_viy(case["stdout"].as_str().unwrap(), &tree),
                "{name}"
            );
            assert_eq!(
                stderr,
                as_viy(case["stderr"].as_str().unwrap(), &tree),
                "{name}"
            );
            assert_eq!(
                tree.files().unwrap(),
                case_files(case),
                "{name} under {marker}"
            );
        }
    }
}
