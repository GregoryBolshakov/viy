//! The printing and the commands that need no RunPod. The pod commands
//! themselves are tested in viy-pod against a scripted RunPod; none runs
//! here through `run`, whose config could hold a real key.

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_pod::notice::Clock;
use viy_testkit::exec::FakeExec;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn printed(terminal: Terminal, reports: Vec<Report>) -> String {
    let mut out = Vec::new();
    let exec = FakeExec::new([]);
    {
        let printer = Printer::new(&mut out, terminal, &exec);
        for r in reports {
            printer.report(r);
        }
        printer.close();
    }
    String::from_utf8(out).unwrap()
}

fn reports() -> Vec<Report> {
    vec![
        Report::Notice(Notice::new(Tone::Dim, "Tunnel mode")),
        Report::Clock(Clock {
            elapsed: 3,
            label: "waiting for sshd",
            state: "starting".into(),
        }),
        Report::Notice(Notice::new(Tone::Error, "sshd never came up")),
        Report::Passing("  confirming nothing comes back... 10s ".into()),
        Report::Alert(Alert {
            title: "viy up FAILED".into(),
            body: "billing".into(),
            urgent: true,
        }),
        Report::Notice(Notice::new(Tone::Plain, "done")),
    ]
}

#[test]
fn a_terminal_gets_colour_the_redrawn_lines_and_the_bell() {
    let terminal = Terminal {
        tty: true,
        err_tty: false,
        desktop: false,
        ..Terminal::default()
    };
    assert_eq!(
        printed(terminal, reports()),
        "\x1b[2mTunnel mode\x1b[0m\n\
         \r  00:03 waiting for sshd (starting)   \x1b[K\
         \n\x1b[31msshd never came up\x1b[0m\n\
         \r  confirming nothing comes back... 10s \x1b[K\
         \x07\
         \r\x1b[Kdone\x1b[0m\n"
    );
}

#[test]
fn a_pipe_gets_plain_lines() {
    assert_eq!(
        printed(Terminal::default(), reports()),
        "Tunnel mode\n  00:03 waiting for sshd (starting)\nsshd never came up\ndone\n"
    );
}

#[tokio::test]
async fn an_alert_reaches_the_desktop_when_there_is_one() {
    let mut out = Vec::new();
    let exec = FakeExec::new([viy_testkit::exec::Reply::StandIn(viy_exec::Command::new(
        "true",
    ))]);
    let terminal = Terminal {
        desktop: true,
        ..Terminal::default()
    };
    Printer::new(&mut out, terminal, &exec).report(Report::Alert(Alert {
        title: "viy up: vLLM is serving".into(),
        body: "ready in 12s".into(),
        urgent: false,
    }));
    assert_eq!(
        exec.sent()[0].argv(),
        [
            "notify-send",
            "-a",
            "viy",
            "-u",
            "normal",
            "viy up: vLLM is serving",
            "ready in 12s"
        ]
    );
}

#[test]
fn only_the_runpod_backend_so_far() {
    let mut cfg = Config::defaults();
    assert!(runpod_backend(&cfg, "up").is_ok());
    cfg.set("BACKEND", "Local");
    assert_eq!(
        runpod_backend(&cfg, "up").unwrap_err().message.unwrap(),
        "viy up: the local backend is not ported yet (ROADMAP phase 2)"
    );
    cfg.set("BACKEND", "azure");
    assert_eq!(
        runpod_backend(&cfg, "status").unwrap_err().message.unwrap(),
        "unknown BACKEND 'azure'; supported: local, openai, runpod"
    );
}

#[test]
fn boots_lists_and_shows_the_records() {
    let dir = ScratchDir::new("boots").unwrap();
    let paths = Paths::under(dir.path());
    let list = |args: &BootsArgs| {
        let mut out = Vec::new();
        let result = boots(&paths, args, &mut out);
        (result, String::from_utf8(out).unwrap())
    };
    let args = |action, boot: Option<&str>| BootsArgs {
        action,
        boot: boot.map(str::to_string),
        n: 10,
    };
    let (_, text) = list(&args(BootsAction::List, None));
    assert_eq!(
        text,
        format!(
            "no boots recorded yet ({})\n",
            paths.boots_history_file().display()
        )
    );
    let error = list(&args(BootsAction::Show, None)).0.unwrap_err();
    assert_eq!(
        error.message.unwrap(),
        "no boot records yet; they start with the next viy up"
    );

    let records = Records::new(&paths);
    std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
    let rows = [
        json!({"at": "2026-07-15T08:00:00+00:00", "boot_id": "20260715T080000Z-pod000",
               "outcome": "ready", "total_s": 92.4, "budget_s": 180,
               "stages": {"pull": 5.0, "imports": 18.2, "endpoint": 2.1}, "gpu": "A100"}),
        json!({"at": "2026-07-16T09:30:00+00:00", "boot_id": "20260716T093000Z-pod111",
               "outcome": "failed", "total_s": 400.0, "budget_s": 180, "stages": {"pull": 380.0}}),
    ];
    let history: String = rows.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(paths.boots_history_file(), history).unwrap();
    let mut record = rows[0].clone();
    record["facts"] = json!({"image": "already on host", "layers_downloaded": 0});
    record["host"] = json!({"machine_id": "m1", "same_machine_as_previous_boot": true});
    record["timeline"] = json!([{"t": 0.0, "event": "viy up started"},
                                {"t": 1.25, "event": "ssh reachable", "host": "1.2.3.4", "port": 40122}]);
    record["files"] = json!({"platform_log": "/x/p.log"});
    records.write("20260715T080000Z-pod000", &record).unwrap();

    let (_, text) = list(&args(BootsAction::List, None));
    assert_eq!(
        text,
        "when (UTC)            total       pull   prepare   imports  api_init engine_in   \
         weights   compile    engine       api  endpoint  gpu\n\
         2026-07-15T08:00:00     92           5         -        18         -         -         \
         -         -         -         -         2  A100\n\
         2026-07-16T09:30:00    400x        380         -         -         -         -         \
         -         -         -         -         -  ?\n\
         budget 180s; ! = over budget, x = failed or interrupted. Details: viy boots show \
         [boot_id]\n"
    );
    let (_, text) = list(&args(BootsAction::Show, Some("pod000")));
    let want = [
        "boot 20260715T080000Z-pod000  outcome: ready  total: 92.4s (budget 180s)  gpu: A100",
        "stages:",
        "  GPU allocation + image pull           5.0s",
        "  vLLM CLI imports                     18.2s",
        "  SSH tunnel / local endpoint           2.1s",
        "facts:",
        "  image                              already on host",
        "  layers_downloaded                  0",
        "  machine_id                         m1",
        "  same_machine_as_previous_boot      True",
        "viy up timeline (seconds from start):",
        "      0.0  viy up started",
        "      1.2  ssh reachable  {\"host\":\"1.2.3.4\",\"port\":40122}",
        "files:",
    ];
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[..want.len()], want);
    assert_eq!(lines[want.len() + 1], "  platform_log  /x/p.log");
    // The latest boot's record is missing.
    let error = list(&args(BootsAction::Show, Some("latest")))
        .0
        .unwrap_err();
    assert!(
        error
            .message
            .unwrap()
            .ends_with("20260716T093000Z-pod111.json is missing")
    );
    let error = list(&args(BootsAction::Show, Some("nope"))).0.unwrap_err();
    assert_eq!(error.message.unwrap(), "no boot record matching nope");
}

#[test]
fn a_question_is_asked_only_with_a_terminal_both_ways() {
    let question = Question::yes_no(None, "Try the backup volume in US-MO-1?");
    for terminal in [
        Terminal::default(),
        Terminal {
            tty: true,
            ..Terminal::default()
        },
        Terminal {
            stdin_tty: true,
            ..Terminal::default()
        },
    ] {
        let mut out = Vec::new();
        let exec = FakeExec::new([]);
        let printer = Printer::new(&mut out, terminal, &exec);
        assert!(printer.ask(&question).is_none(), "{terminal:?}");
        drop(printer);
        assert!(out.is_empty());
    }
}
