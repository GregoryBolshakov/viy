use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::fixtures;

use super::*;
use crate::model::ModelAction;

/// rpctl's text as viy says it.
fn as_viy(text: &str) -> String {
    text.replace("rp ", "viy ").replace("(.rpctl/)", "(.viy/)")
}

/// rpctl's help where viy's says something else on purpose.
fn viy_help(subcommand: &str, rpctl: &str) -> String {
    match subcommand {
        // rp's migrate copied an old in-repo .env (kb d043).
        "migrate" => "copy rp's config and model profiles to viy's (rp's stay in place)".into(),
        _ => as_viy(rpctl),
    }
}

fn text(s: Option<&clap::builder::StyledStr>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.to_string()))
}

/// One clap argument as the fixture describes an argparse action.
fn as_argparse(arg: &clap::Arg) -> Value {
    let flag = matches!(arg.get_action(), clap::ArgAction::SetTrue);
    let mut flags = Vec::new();
    if let Some(short) = arg.get_short() {
        flags.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        flags.push(format!("--{long}"));
    }
    let optional_value = arg.get_num_args().is_some_and(|n| n.min_values() == 0);
    let nargs = if flag {
        json!(0)
    } else if matches!(arg.get_action(), clap::ArgAction::Append) {
        json!("*")
    } else if arg.is_positional() && !arg.is_required_set() || optional_value {
        json!("?")
    } else {
        Value::Null
    };
    let default = if flag {
        json!(false)
    } else {
        json!(
            arg.get_default_values()
                .first()
                .map(|v| v.to_string_lossy().into_owned())
        )
    };
    let choices: Vec<String> = if flag {
        Vec::new()
    } else {
        arg.get_possible_values()
            .iter()
            .map(|v| v.get_name().to_string())
            .collect()
    };
    json!({
        "flags": flags,
        "nargs": nargs,
        "flag": flag,
        "default": default,
        "choices": if choices.is_empty() { Value::Null } else { json!(choices) },
        "required": arg.is_required_set(),
        "hidden": arg.is_hide_set(),
        "help": text(arg.get_help()),
    })
}

/// The fixture's view of an argparse action, as viy words it. The value
/// type and `-r`'s const (clap has no getter for it) are checked by the
/// parse cases instead; `required` on a `*` positional is argparse's
/// bookkeeping (it accepts none).
fn expected(subcommand: &str, action: &Value) -> Value {
    let mut action = action.clone();
    let map = action.as_object_mut().unwrap();
    map.remove("type");
    map.remove("const");
    if map["nargs"] == "*" {
        map["required"] = json!(false);
    }
    if let Some(help) = map["help"].as_str() {
        map["help"] = json!(viy_help(subcommand, help));
    }
    action
}

#[test]
fn subcommands_and_flags_are_rps() {
    let fixture = fixtures::load("cli/flags").unwrap();
    let mut cmd = command();
    cmd.build();
    let (program, subcommands) = fixture.cases.split_first().unwrap();
    assert_eq!(program["name"], "");
    assert_eq!(
        cmd.get_about().unwrap().to_string(),
        program["description"]
            .as_str()
            .unwrap()
            .replace("rp - ", "viy - ")
    );
    // The hidden ones are viy's own plumbing (`capture-boot`).
    let names: Vec<&str> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(clap::Command::get_name)
        .collect();
    let rp_names: Vec<&str> = subcommands
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, rp_names);
    assert_eq!(names.len(), 19);
    for (sub, rp) in cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .zip(subcommands)
    {
        let name = sub.get_name();
        assert_eq!(
            sub.get_about().unwrap().to_string(),
            viy_help(name, rp["help"].as_str().unwrap()),
            "{name}"
        );
        assert_eq!(
            text(sub.get_after_help()),
            json!(rp["description"].as_str().map(as_viy)),
            "{name}"
        );
        let args: Vec<Value> = sub
            .get_arguments()
            .filter(|a| !matches!(a.get_action(), clap::ArgAction::Help))
            .map(as_argparse)
            .collect();
        let rp_args: Vec<Value> = rp["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| expected(name, a))
            .collect();
        assert_eq!(args, rp_args, "{name}");
    }
}

fn word(value: &impl clap::ValueEnum) -> String {
    value
        .to_possible_value()
        .map(|p| p.get_name().to_string())
        .unwrap_or_default()
}

/// argparse's namespace for a parsed command line: rpctl's `dest` names,
/// values as the fixture writes them (numbers as strings).
fn namespace(cli: &Cli) -> Value {
    let mut ns = match &cli.command {
        Command::Init | Command::Migrate | Command::Status | Command::Stop | Command::Down => {
            json!({})
        }
        Command::Up { timeout } => json!({"timeout": timeout.to_string()}),
        Command::Boots(a) => {
            json!({"action": word(&a.action), "boot": a.boot, "n": a.n.to_string()})
        }
        Command::Logs { follow } => json!({"follow": follow}),
        Command::Reap { install, quiet } => json!({"install": install, "quiet": quiet}),
        Command::Shutdown(a) => json!({
            "when": a.when, "no_poweroff": a.no_poweroff, "foreground": a.foreground,
            "status": a.status, "cancel": a.cancel, "run_at": a.run_at,
        }),
        Command::Tunnel(a) => json!({
            "background": a.background, "stop": a.stop, "status": a.status,
            "forget_key": a.forget_key,
        }),
        Command::Chat(a) => json!({
            "url": a.url, "system": a.system, "cont": a.cont, "resume": a.resume,
        }),
        Command::Chats(a) => json!({"words": a.words, "all": a.all}),
        Command::Ask(a) => json!({
            "prompt": a.prompt, "url": a.url, "system": a.system, "think": a.think,
            "save": a.save,
        }),
        Command::Trace(a) => json!({"what": a.what, "n": a.n.map(|n| n.to_string())}),
        Command::Toolbox(a) => json!({
            "toolbox_action": word(&a.action), "arg": a.arg, "glob": a.global,
        }),
        Command::Model(a) => json!({
            "model_action": word(&a.action), "name": a.name, "force": a.force,
        }),
        Command::Backup { samples } => json!({"samples": samples.to_string()}),
        Command::Volume(a) => {
            json!({"volume_action": word(&a.action), "volume_id": a.volume_id})
        }
        // viy's own; rp has nothing to compare it with.
        Command::CaptureBoot { boot_id, pod_id } => json!({"boot_id": boot_id, "pod_id": pod_id}),
    };
    ns["cmd"] = json!(cli.command.name());
    ns
}

/// Where viy's parser and argparse part on purpose: values argparse takes
/// and the command then fails on are refused at parse time.
fn deviation(argv: &[&str]) -> Option<i32> {
    match argv {
        // rp: int("abc") raises ValueError in cmd_up, a traceback.
        ["up", "--timeout", "abc"] => Some(2),
        // rp lists `rows[-n:]` with n = -3; a count is not negative.
        ["boots", "-n", "-3"] => Some(2),
        // argparse fills a `*` positional once, so words after an option
        // are refused; clap collects them all.
        ["chats", "foo", "-a", "bar"] => Some(0),
        _ => None,
    }
}

#[test]
fn command_lines_parse_as_rps() {
    let fixture = fixtures::load("cli/parse").unwrap();
    assert_eq!(fixture.cases.len(), 100);
    for case in &fixture.cases {
        let argv: Vec<&str> = case["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let parsed = parse(argv.iter().map(OsString::from));
        let exit = match &parsed {
            Ok(_) => 0,
            Err(e) => e.exit_code(),
        };
        if let Some(viy_exit) = deviation(&argv) {
            assert_eq!(exit, viy_exit, "{argv:?}");
            continue;
        }
        assert_eq!(i64::from(exit), case["exit"].as_i64().unwrap(), "{argv:?}");
        if let Ok(cli) = parsed {
            assert_eq!(namespace(&cli), case["args"], "{argv:?}");
        }
    }
}

#[test]
fn words_after_an_option_are_still_the_query() {
    let cli = parse(["chats", "foo", "-a", "bar"].map(OsString::from)).unwrap();
    assert_eq!(
        namespace(&cli),
        json!({"cmd": "chats", "words": ["foo", "bar"], "all": true})
    );
}

#[test]
fn model_args_reach_cmd_model() {
    let cli = parse(["model", "add", "beta", "--force"].map(OsString::from)).unwrap();
    let Command::Model(args) = cli.command else {
        panic!("{:?}", cli.command);
    };
    assert_eq!(
        args,
        ModelArgs {
            action: ModelAction::Add,
            name: Some("beta".into()),
            force: true,
        }
    );
}
