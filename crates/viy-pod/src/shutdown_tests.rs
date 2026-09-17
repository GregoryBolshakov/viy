//! rpctl's test_shutdown.py (timeout parsing, finding and closing chats,
//! the order of the steps when it fires), and the countdown, the state
//! file and the command, against scripted chats and processes. Nothing is
//! powered off and no pod is touched: every command goes to a FakeExec.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use chrono::NaiveDate;
use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::exec::named_process;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;
use crate::notice::Report;

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, mo, d)
        .and_then(|day| day.and_hms_opt(h, mi, 0))
        .unwrap()
}

fn argv(args: &[&str]) -> Vec<Vec<u8>> {
    args.iter().map(|a| a.as_bytes().to_vec()).collect()
}

// ----------------------------------------------------------- ParseWhenTests

#[test]
fn durations() {
    let now = at(2026, 6, 28, 22, 0);
    assert_eq!(parse_when("90", now), Ok(5400.0));
    assert_eq!(parse_when("90m", now), Ok(5400.0));
    assert_eq!(parse_when("1h30m", now), Ok(5400.0));
    assert_eq!(parse_when("45s", now), Ok(45.0));
    assert_eq!(parse_when(" 1.5H ", now), Ok(5400.0));
    assert_eq!(parse_when("1d", now), Ok(86400.0));
    for bad in ["", "abc", "1x", "1h junk", "25:00", "1h 30m", "-5m"] {
        assert!(parse_when(bad, now).is_err(), "{bad:?}");
    }
    assert_eq!(
        parse_when("1h junk", now),
        Err("not a duration: '1h junk' (try 90m, 1h30m or 23:30)".into())
    );
    assert_eq!(
        parse_when("25:00", now),
        Err("not a clock time: 25:00".into())
    );
}

#[test]
fn clock_time_is_the_next_occurrence() {
    let now = at(2026, 6, 28, 22, 0);
    assert_eq!(parse_when("23:30", now), Ok(5400.0));
    assert_eq!(parse_when("21:00", now), Ok(23.0 * 3600.0));
    assert_eq!(parse_when("22:00", now), Ok(24.0 * 3600.0));
}

#[test]
fn only_ascii_digits_count() {
    // Python's \d and float() take any script's digits; a typo in another
    // script is not a time.
    let now = at(2026, 6, 28, 22, 0);
    assert!(parse_when("٣m", now).is_err());
    assert!(parse_when("٢٣:٣٠", now).is_err());
}

#[test]
fn fmt_shows_hours_minutes_or_seconds() {
    assert_eq!(fmt(5400.0), "1h30m");
    assert_eq!(fmt(3599.9), "59m59s");
    assert_eq!(fmt(61.0), "1m01s");
    assert_eq!(fmt(7.8), "7s");
    assert_eq!(fmt(-3.0), "0s");
}

// ------------------------------------------------------- ChatDetectionTests

#[test]
fn only_the_chat_subcommand() {
    assert!(is_chat(&argv(&["python", "/x/bin/rp", "chat"])));
    assert!(is_chat(&argv(&["python", "-m", "rpctl", "chat", "-c"])));
    assert!(!is_chat(&argv(&["python", "/x/bin/rp", "ask", "chat"])));
    assert!(!is_chat(&argv(&["python", "-m", "rpctl", "shutdown"])));
    assert!(!is_chat(&argv(&["vim", "chat"])));
}

#[test]
fn viy_chats_too() {
    assert!(is_chat(&argv(&["/home/u/.cargo/bin/viy", "chat", "-r"])));
    assert!(is_chat(&argv(&["viy", "--x", "chat"])));
    assert!(!is_chat(&argv(&["viy", "shutdown", "--run-at", "1"])));
    assert!(!is_chat(&argv(&["/x/viyx", "chat"])));
}

// ---------------------------------------------------------- CloseChatsTests

/// A stand-in chat: `sh` with `<dir>/<tool> <sub> <out>` in its command
/// line. As `chat` it writes "saved" to `out` and exits on SIGTERM; as
/// anything else it ignores SIGTERM.
struct FakeChat(std::process::Child);

impl FakeChat {
    fn spawn(dir: &Path, tool: &str, sub: &str) -> Self {
        let script = if sub == "chat" {
            r#"trap 'echo saved > "$2"; exit 0' TERM; while :; do sleep 0.1; done"#
        } else {
            r#"trap '' TERM; while :; do sleep 0.1; done"#
        };
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg(dir.join(tool))
            .arg(sub)
            .arg(dir.join(format!("{tool}-{sub}.out")))
            .spawn()
            .unwrap();
        Self(child)
    }

    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for FakeChat {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn finds_chats_saves_them_and_kills_stuck_ones() {
    let dir = ScratchDir::new("shutdown-chats").unwrap();
    let mut rp_chat = FakeChat::spawn(dir.path(), "rp", "chat");
    let mut viy_chat = FakeChat::spawn(dir.path(), "viy", "chat");
    let mut other = FakeChat::spawn(dir.path(), "rp", "ask");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let found = find_chats();
    assert!(found.contains(&rp_chat.id()), "{found:?}");
    assert!(found.contains(&viy_chat.id()), "{found:?}");
    assert!(!found.contains(&other.id()), "{found:?}");
    let pids = [rp_chat.id(), viy_chat.id(), other.id()];
    let done = close_chats(&pids, Duration::from_secs(2)).await;
    for chat in [&mut rp_chat, &mut viy_chat, &mut other] {
        chat.0.wait().unwrap();
    }
    assert_eq!(
        done,
        Closed {
            closed: vec![rp_chat.id(), viy_chat.id()],
            killed: vec![other.id()],
            failed: vec![],
        }
    );
    for tool in ["rp", "viy"] {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("{tool}-chat.out"))).unwrap(),
            "saved\n"
        );
    }
}

#[tokio::test]
async fn a_pid_not_ours_is_a_failure_and_a_gone_one_is_closed() {
    // pid 1 is init's: SIGTERM is refused.
    let done = close_chats(&[1], Duration::from_millis(10)).await;
    assert_eq!(done.failed, [1]);
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let done = close_chats(&[pid], Duration::from_millis(10)).await;
    assert_eq!(done.closed, [pid]);
}

// ------------------------------------------------------------------ the host

/// The chats a test says are open; what was closed, in order.
#[derive(Default)]
struct Chatter {
    open: Vec<u32>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Chats for Chatter {
    fn find(&self) -> Vec<u32> {
        self.open.clone()
    }

    fn close<'a>(&'a self, pids: &'a [u32], _grace: Duration) -> BoxFuture<'a, Closed> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push("chats".into());
        Box::pin(async move {
            Closed {
                closed: pids.to_vec(),
                ..Closed::default()
            }
        })
    }
}

/// An environment variable by name.
type Var = Box<dyn Fn(&str) -> Option<OsString> + Send + Sync>;

/// This machine for a test: a scratch home, a `bin` directory on PATH with
/// the programs `installed` (empty files, never run: the FakeExec answers),
/// and the environment in `vars`.
struct Machine {
    _dir: ScratchDir,
    paths: Paths,
    exec: FakeExec,
    time: FakeTime,
    chats: Chatter,
    vars: Arc<Mutex<BTreeMap<String, OsString>>>,
    var: Var,
}

impl Machine {
    fn new(installed: &[&str], replies: impl IntoIterator<Item = Run>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new("shutdown").unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for program in installed {
            let file = bin.join(program);
            fs::write(&file, "").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let vars = Arc::new(Mutex::new(BTreeMap::from([(
            "PATH".to_string(),
            bin.into_os_string(),
        )])));
        let read = Arc::clone(&vars);
        Self {
            paths: Paths::under(dir.path()),
            _dir: dir,
            exec: FakeExec::new(replies),
            time: FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 17, 21, 0, 0).unwrap()),
            chats: Chatter::default(),
            vars,
            var: Box::new(move |name| read.lock().unwrap().get(name).cloned()),
        }
    }

    fn host(&self) -> Host<'_> {
        Host {
            paths: &self.paths,
            exec: &self.exec,
            time: &self.time,
            chats: &self.chats,
            var: &*self.var,
        }
    }

    fn set_var(&self, name: &str, value: &str) {
        self.vars.lock().unwrap().insert(name.into(), value.into());
    }

    fn calls(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.chats.calls)
    }

    fn sent(&self) -> Vec<Vec<String>> {
        self.exec
            .sent()
            .iter()
            .map(viy_exec::Command::argv)
            .collect()
    }
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// The log's lines without their time stamps.
fn lines(out: &[u8]) -> Vec<String> {
    text(out)
        .lines()
        .map(|l| l.get(20..).unwrap_or(l).to_string())
        .collect()
}

// -------------------------------------------------------------- what fails

fn busctl(data: Value) -> Run {
    Run::stdout(json!({"type": "s", "data": data}).to_string())
}

#[tokio::test]
async fn poweroff_problems_ask_logind() {
    let none = Machine::new(&[], []);
    assert_eq!(
        poweroff_problems(&none.host()).await,
        ["systemctl is not available, so this machine can't be powered off"]
    );
    let no_busctl = Machine::new(&["systemctl"], []);
    assert_eq!(
        poweroff_problems(&no_busctl.host()).await,
        ["could not ask logind whether this user may power off; the poweroff may fail"]
    );
    let cases = [
        (
            vec![busctl(json!(["challenge"]))],
            vec![
                "powering off needs a password here (polkit), which nobody will be there to type: \
             the machine will stay on",
            ],
        ),
        (
            vec![busctl(json!(["na"]))],
            vec!["this user may not power off the machine (logind: na)"],
        ),
        (
            vec![Run::exit(1, "", "no bus")],
            vec!["could not ask logind whether this user may power off; the poweroff may fail"],
        ),
        (
            vec![
                busctl(json!(["yes"])),
                busctl(json!([[
                    ["shutdown:sleep", "Updater", "installing", "block", 0, 7],
                    ["sleep", "Player", "playing", "block", 1000, 8],
                    ["shutdown", "Backup", "copying", "delay", 1000, 9],
                ]])),
            ],
            vec![
                "Updater blocks shutdown (installing); the poweroff will be refused while it \
                 holds that lock",
            ],
        ),
        (vec![busctl(json!(["yes"])), Run::exit(1, "", "")], vec![]),
    ];
    for (replies, want) in cases {
        let machine = Machine::new(&["systemctl", "busctl"], replies);
        assert_eq!(poweroff_problems(&machine.host()).await, want);
    }
    let machine = Machine::new(
        &["systemctl", "busctl"],
        [busctl(json!(["yes"])), busctl(json!([[]]))],
    );
    poweroff_problems(&machine.host()).await;
    assert_eq!(
        machine.sent()[0],
        [
            "busctl",
            "--json=short",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "CanPowerOff"
        ]
    );
}

#[test]
fn down_problems_by_backend() {
    let mut cfg = Config::defaults();
    cfg.set("RUNPOD_API_KEY", "rpa_SYNTHETICKEY0000");
    assert!(down_problems(&cfg).is_empty());
    cfg.set("RUNPOD_API_KEY", "");
    assert_eq!(
        down_problems(&cfg),
        [
            "RUNPOD_API_KEY is not set, so viy down can't reach RunPod; the pod would be left to \
          its own idle watchdog"
        ]
    );
    cfg.set("BACKEND", "OpenAI");
    assert_eq!(
        down_problems(&cfg),
        ["viy down does nothing for the 'openai' backend; that step is skipped"]
    );
    cfg.set("BACKEND", "azure");
    assert_eq!(
        down_problems(&cfg),
        ["unknown BACKEND 'azure'; supported: local, openai, runpod"]
    );
}

#[test]
fn notify_problems_need_notify_send_and_a_session() {
    let none = Machine::new(&[], []);
    assert_eq!(
        notify_problems(&none.host()),
        ["notify-send is missing: no desktop warnings before the shutdown"]
    );
    let machine = Machine::new(&["notify-send"], []);
    assert_eq!(
        notify_problems(&machine.host()),
        ["no desktop session in this environment: no desktop warnings"]
    );
    machine.set_var("DISPLAY", "");
    assert_eq!(notify_problems(&machine.host()).len(), 1);
    machine.set_var("WAYLAND_DISPLAY", "wayland-0");
    assert!(notify_problems(&machine.host()).is_empty());
}

// ---------------------------------------------------------------- FireTests

/// `fire` with chat 11 open and a down that records itself and fails
/// unless `down_ok`; the exit status, the steps in order and the log.
async fn run_fire(
    machine: &Machine,
    down_ok: bool,
    budget: DownBudget,
    poweroff: bool,
) -> (i32, Vec<String>, Vec<u8>) {
    let calls = machine.calls();
    let mut out = Vec::new();
    let code = {
        let log = Log::new(&mut out, &machine.time);
        let down = async |_: &Config| {
            calls.lock().unwrap().push("down".into());
            if down_ok {
                Ok(Down::Done)
            } else {
                Err("1".to_string())
            }
        };
        fire(
            &machine.host(),
            &log,
            &Config::defaults(),
            poweroff,
            budget,
            down,
        )
        .await
    };
    let mut steps = calls.lock().unwrap().clone();
    steps.extend(machine.sent().into_iter().map(|argv| argv[0].clone()));
    (code, steps, out)
}

fn with_chat(mut machine: Machine) -> Machine {
    machine.chats.open = vec![11];
    machine
}

#[tokio::test(start_paused = true)]
async fn chats_then_down_then_poweroff() {
    let machine = with_chat(Machine::new(&[], [Run::stdout("")]));
    let (code, steps, out) = run_fire(&machine, true, DownBudget::default(), true).await;
    assert_eq!(
        (code, steps),
        (0, vec!["chats".into(), "down".into(), "systemctl".into()])
    );
    assert_eq!(
        machine.sent()[0],
        ["systemctl", "poweroff", "--no-ask-password"]
    );
    assert_eq!(
        lines(&out),
        [
            "time is up",
            "closing 1 chat(s): 11",
            "closed and saved: 11",
            "viy down (attempt 1)",
            "viy down finished",
            "warning: systemctl is not available, so this machine can't be powered off",
            "powering off",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn failed_down_still_powers_off() {
    let machine = with_chat(Machine::new(&[], [Run::stdout("")]));
    let budget = DownBudget {
        total: Duration::ZERO,
        ..DownBudget::default()
    };
    let (code, steps, out) = run_fire(&machine, false, budget, true).await;
    assert_eq!(
        (code, steps),
        (0, vec!["chats".into(), "down".into(), "systemctl".into()])
    );
    let log = lines(&out);
    assert!(log.contains(&"viy down failed: 1".to_string()), "{log:?}");
    assert!(
        log.iter().any(|l| l.starts_with(
            "FLAG: viy down could not confirm that no pod is running; \
                                    powering off anyway."
        )),
        "{log:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn down_is_retried_within_its_budget() {
    let machine = with_chat(Machine::new(&[], [Run::stdout("")]));
    let budget = DownBudget {
        total: Duration::from_millis(500),
        retry: Duration::from_millis(200),
        worst: Duration::ZERO,
    };
    let begun = Instant::now();
    let (_, steps, _) = run_fire(&machine, false, budget, true).await;
    assert_eq!(steps.iter().filter(|s| *s == "down").count(), 3);
    assert_eq!(steps.last().unwrap(), "systemctl");
    assert_eq!(begun.elapsed(), Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn refused_poweroff_is_flagged() {
    let machine = with_chat(Machine::new(
        &["notify-send"],
        [Run::exit(1, "", "inhibited"), Run::stdout("")],
    ));
    let (code, steps, out) = run_fire(&machine, true, DownBudget::default(), true).await;
    assert_eq!(
        (code, steps),
        (
            1,
            vec![
                "chats".into(),
                "down".into(),
                "systemctl".into(),
                "notify-send".into()
            ]
        )
    );
    let msg = "the pod is down, but the poweroff was refused: inhibited";
    assert_eq!(lines(&out).last().unwrap(), &format!("FLAG: {msg}"));
    assert_eq!(
        machine.sent()[1],
        [
            "notify-send",
            "-a",
            "viy",
            "-u",
            "critical",
            "viy shutdown",
            msg
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn no_poweroff_leaves_the_machine_on() {
    let machine = Machine::new(&[], []);
    let (code, steps, out) = run_fire(
        &machine,
        false,
        DownBudget {
            total: Duration::ZERO,
            ..DownBudget::default()
        },
        false,
    )
    .await;
    assert_eq!((code, steps), (1, vec!["down".into()]));
    let log = lines(&out);
    assert_eq!(log[1], "no open chats");
    assert_eq!(log.last().unwrap(), "--no-poweroff: leaving the machine on");
}

#[tokio::test(start_paused = true)]
async fn fires_with_the_config_of_firing_time() {
    // viy model use after scheduling: viy down must target the new profile's pod.
    let machine = Machine::new(&[], [Run::stdout("")]);
    let mut old = Config::defaults();
    old.set("POD_NAME", "old");
    let reload = || {
        let mut new = Config::defaults();
        new.set("POD_NAME", "new");
        Ok(new)
    };
    let fired = Mutex::new(Vec::new());
    let down = async |cfg: &Config| {
        let name = cfg.get("POD_NAME").unwrap_or_default().to_string();
        fired.lock().unwrap().push(name);
        Ok(Down::Done)
    };
    let mut out = Vec::new();
    let log = Log::new(&mut out, &machine.time);
    let deadline = regions::now(&machine.time) - 1.0;
    let budget = DownBudget::default();
    let code = run(
        &machine.host(),
        &log,
        old,
        deadline,
        true,
        false,
        budget,
        reload,
        down,
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(*fired.lock().unwrap(), ["new"]);
}

// -------------------------------------------------------------- the countdown

#[tokio::test(start_paused = true)]
async fn warns_ten_minutes_and_one_minute_ahead() {
    let machine = Machine::new(
        &["notify-send", "systemctl"],
        [Run::stdout(""), Run::stdout(""), Run::stdout("")],
    );
    let mut out = Vec::new();
    let log = Log::new(&mut out, &machine.time);
    let deadline = regions::now(&machine.time) + 11.0 * 60.0;
    let begun = Instant::now();
    let code = run(
        &machine.host(),
        &log,
        Config::defaults(),
        deadline,
        true,
        false,
        DownBudget::default(),
        || Err("no config here".into()),
        async |_: &Config| Ok(Down::Nothing("openai")),
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(begun.elapsed(), Duration::from_secs(11 * 60));
    let sent = machine.sent();
    assert_eq!(
        sent[0],
        [
            "notify-send",
            "-a",
            "viy",
            "-u",
            "normal",
            "viy shutdown",
            "In 10m00s: open chats close and this PC powers off. Cancel: viy shutdown --cancel"
        ]
    );
    assert_eq!(sent[1][4], "critical");
    assert_eq!(
        sent[1][6],
        "In 1m00s: open chats close and this PC powers off. Cancel: viy shutdown --cancel"
    );
    assert_eq!(sent[2], ["systemctl", "poweroff", "--no-ask-password"]);
    assert_eq!(
        lines(&out)[..5],
        [
            "could not reread the config, using the one from scheduling: no config here",
            "time is up",
            "no open chats",
            "viy down (attempt 1)",
            "the 'openai' backend has nothing to take down",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn waking_past_the_deadline_gives_a_minute() {
    let machine = Machine::new(
        &["notify-send"],
        [
            Run::stdout(""),
            Run::stdout(""),
            Run::stdout(""),
            Run::stdout(""),
        ],
    );
    let mut out = Vec::new();
    let log = Log::new(&mut out, &machine.time);
    let deadline = regions::now(&machine.time) + 30.0;
    let begun = Instant::now();
    let suspend = async {
        // The lid closes a second in, for ten minutes.
        tokio::time::sleep(Duration::from_secs(1)).await;
        machine.time.advance(Duration::from_secs(600));
    };
    let host = machine.host();
    let ((), code) = tokio::join!(
        suspend,
        run(
            &host,
            &log,
            Config::defaults(),
            deadline,
            false,
            false,
            DownBudget::default(),
            || Ok(Config::defaults()),
            async |_: &Config| Ok(Down::Done),
        )
    );
    assert_eq!(code, 0);
    let notes: Vec<String> = machine.sent().iter().map(|a| a[6].clone()).collect();
    // Under a minute left, both warnings go out at once, as rp's do: the
    // 10-minute one is looked at first.
    let early = "In 30s: open chats close and the pod goes down. Cancel: viy shutdown --cancel";
    assert_eq!(
        notes,
        [
            early,
            early,
            "Woke up past the deadline: chats close, the pod goes down and this PC powers off in \
             60s. Cancel: viy shutdown --cancel",
        ]
    );
    assert_eq!(lines(&out)[0], "woke up past the deadline");
    // 5 s asleep in the loop's first pause, then the minute.
    assert_eq!(begun.elapsed(), Duration::from_secs(65));
}

#[tokio::test(start_paused = true)]
async fn the_foreground_counts_down_every_second() {
    let machine = Machine::new(&[], [Run::stdout("")]);
    let mut out = Vec::new();
    let log = Log::new(&mut out, &machine.time);
    let deadline = regions::now(&machine.time) + 3.0;
    run(
        &machine.host(),
        &log,
        Config::defaults(),
        deadline,
        false,
        true,
        DownBudget::default(),
        || Ok(Config::defaults()),
        async |_: &Config| Ok(Down::Done),
    )
    .await;
    assert!(
        text(&out).starts_with(
            "\r\x1b[Kshutdown in 3s  (Ctrl-C cancels)\r\x1b[Kshutdown in 2s  (Ctrl-C \
             cancels)\r\x1b[Kshutdown in 1s  (Ctrl-C cancels)\n"
        ),
        "{:?}",
        text(&out)
    );
}

// ------------------------------------------------------------ the state file

/// A process whose command line is a scheduled shutdown's.
fn runner() -> viy_testkit::exec::NamedProcess {
    named_process("viy", &["shutdown", "--run-at", "1"]).unwrap()
}

#[test]
fn a_live_runner_is_the_scheduled_shutdown() {
    let machine = Machine::new(&[], []);
    let paths = &machine.paths;
    assert_eq!(Scheduled::live(paths), None);
    let process = runner();
    let scheduled = Scheduled {
        pid: process.id(),
        deadline: 1_784_322_000.5,
        poweroff: false,
    };
    scheduled.save(paths).unwrap();
    assert_eq!(
        fs::read_to_string(paths.shutdown_state_file()).unwrap(),
        format!(
            r#"{{"pid":{},"deadline":1784322000.5,"poweroff":false}}"#,
            process.id()
        )
    );
    assert_eq!(Scheduled::live(paths), Some(scheduled));
    // rp's spelling, poweroff left out as rp's reader allows.
    fs::write(
        paths.shutdown_state_file(),
        format!(r#"{{"pid": {}, "deadline": 5.0}}"#, process.id()),
    )
    .unwrap();
    assert_eq!(Scheduled::live(paths).map(|s| s.poweroff), Some(true));
    // A pid that is not a shutdown: the file goes.
    let other = named_process("viy", &["chat"]).unwrap();
    Scheduled {
        pid: other.id(),
        ..scheduled
    }
    .save(paths)
    .unwrap();
    assert_eq!(Scheduled::live(paths), None);
    assert!(!paths.shutdown_state_file().exists());
}

#[test]
fn clear_state_removes_only_our_own() {
    let machine = Machine::new(&[], []);
    let paths = &machine.paths;
    let theirs = Scheduled {
        pid: 1,
        deadline: 0.0,
        poweroff: true,
    };
    theirs.save(paths).unwrap();
    clear_state(paths);
    assert!(paths.shutdown_state_file().exists());
    Scheduled {
        pid: std::process::id(),
        ..theirs
    }
    .save(paths)
    .unwrap();
    clear_state(paths);
    assert!(!paths.shutdown_state_file().exists());
}

#[derive(Default, Clone)]
struct Told(Arc<Mutex<Vec<(Tone, String)>>>);

impl Told {
    fn reporter(&self) -> impl Fn(Report) + Send + Sync + 'static {
        let told = Arc::clone(&self.0);
        move |r| {
            if let Report::Notice(n) = r {
                told.lock().unwrap().push((n.tone, n.text));
            }
        }
    }

    fn texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }
}

fn clock(at: f64, format: &str) -> String {
    local(at).unwrap().format(format).to_string()
}

#[tokio::test(start_paused = true)]
async fn status_then_cancel() {
    let mut machine = Machine::new(&[], []);
    machine.chats.open = vec![41, 42];
    let told = Told::default();
    status(&machine.host(), &told.reporter());
    assert_eq!(told.texts(), ["no shutdown is scheduled"]);

    let mut process = runner();
    let deadline = regions::now(&machine.time) + 5400.0;
    Scheduled {
        pid: process.id(),
        deadline,
        poweroff: true,
    }
    .save(&machine.paths)
    .unwrap();
    let told = Told::default();
    status(&machine.host(), &told.reporter());
    assert_eq!(
        told.texts(),
        [
            format!(
                "shutdown at {} (in 1h30m): chats close, viy down, then poweroff",
                clock(deadline, "%a %H:%M:%S")
            ),
            format!(
                "pid {}, log {}",
                process.id(),
                machine.paths.shutdown_log_file().display()
            ),
            "open chats right now: 2 (41, 42)".to_string(),
        ]
    );

    let told = Told::default();
    assert!(
        cancel(&machine.paths, false, &told.reporter())
            .await
            .unwrap()
    );
    assert_eq!(
        told.texts(),
        [format!(
            "cancelled the shutdown scheduled for {}",
            clock(deadline, "%H:%M:%S")
        )]
    );
    assert!(!process.wait().unwrap().success());
    assert!(!machine.paths.shutdown_state_file().exists());
    let told = Told::default();
    assert!(
        !cancel(&machine.paths, false, &told.reporter())
            .await
            .unwrap()
    );
    assert_eq!(told.texts(), ["no shutdown is scheduled"]);
}

#[tokio::test(start_paused = true)]
async fn an_rp_shutdown_shows_as_rps() {
    let machine = Machine::new(&[], []);
    let process = named_process("python3", &["-m", "rpctl", "shutdown", "--run-at", "1"]).unwrap();
    Scheduled {
        pid: process.id(),
        deadline: regions::now(&machine.time) + 60.0,
        poweroff: false,
    }
    .save(&machine.paths)
    .unwrap();
    let told = Told::default();
    status(&machine.host(), &told.reporter());
    assert!(
        told.texts()[0].ends_with("(in 1m00s): chats close, rp down (no poweroff)"),
        "{:?}",
        told.texts()
    );
}

// --------------------------------------------------------------- the command

#[tokio::test(start_paused = true)]
async fn schedule_flags_what_would_fail_and_replaces_the_last_one() {
    let mut machine = Machine::new(&[], []);
    machine.chats.open = vec![41];
    let mut process = runner();
    Scheduled {
        pid: process.id(),
        deadline: 0.0,
        poweroff: true,
    }
    .save(&machine.paths)
    .unwrap();
    let told = Told::default();
    let mut cfg = Config::defaults();
    cfg.set("RUNPOD_API_KEY", "rpa_SYNTHETICKEY0000");
    let deadline = schedule(&machine.host(), &cfg, "90m", true, false, &told.reporter())
        .await
        .unwrap();
    assert_eq!(deadline, regions::now(&machine.time) + 5400.0);
    assert!(!process.wait().unwrap().success());
    assert_eq!(
        told.0.lock().unwrap().clone(),
        [
            (
                Tone::Warn,
                "FLAG: systemctl is not available, so this machine can't be powered off".into()
            ),
            (
                Tone::Warn,
                "FLAG: notify-send is missing: no desktop warnings before the shutdown".into()
            ),
            (
                Tone::Dim,
                "replaced the previously scheduled shutdown".into()
            ),
            (
                Tone::Ok,
                format!(
                    "At {} (in 1h30m): close open chats, viy down, power off this PC.",
                    clock(deadline, "%a %H:%M:%S")
                )
            ),
            (
                Tone::Dim,
                "open chats right now: 1. Chats opened later are closed too.".into()
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn schedule_refuses_what_is_not_a_time() {
    let machine = Machine::new(&[], []);
    let error = schedule(
        &machine.host(),
        &Config::defaults(),
        "soon",
        false,
        true,
        &|_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "not a duration: 'soon' (try 90m, 1h30m or 23:30)"
    );
}

#[tokio::test]
async fn the_runner_is_started_detached_and_recorded() {
    let machine = Machine::new(&[], [Run::StandIn(viy_exec::Command::new("true"))]);
    fs::create_dir_all(machine.paths.pod_state_dir()).unwrap();
    let told = Told::default();
    start_runner(
        &machine.host(),
        Path::new("/opt/viy/bin/viy"),
        1_784_322_000.25,
        false,
        &told.reporter(),
    )
    .unwrap();
    assert_eq!(
        machine.sent(),
        [[
            "/opt/viy/bin/viy",
            "shutdown",
            "--run-at",
            "1784322000.25",
            "--no-poweroff"
        ]]
    );
    let state: Value =
        serde_json::from_str(&fs::read_to_string(machine.paths.shutdown_state_file()).unwrap())
            .unwrap();
    assert_eq!(state["deadline"], 1_784_322_000.25);
    assert_eq!(state["poweroff"], false);
    let pid = state["pid"].as_u64().unwrap();
    assert_eq!(
        told.texts(),
        [format!(
            "running in the background (pid {pid}); log: {}\nviy shutdown --status shows it, \
             viy shutdown --cancel stops it.",
            machine.paths.shutdown_log_file().display()
        )]
    );
    assert!(machine.paths.shutdown_log_file().exists());
}

// --------------------------------------------------------------- the oracle

/// rpctl's `parse_when`, `_is_chat` and `_fmt` against viy's on generated
/// inputs (`cargo test -p viy-pod --features oracle`).
#[cfg(feature = "oracle")]
mod oracle {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    use super::*;

    fn pick<'a>(g: &mut Gen, pieces: &[&'a str]) -> &'a str {
        pieces[g.below(pieces.len())]
    }

    /// The same seconds, or the same refusal, for every input in ASCII
    /// (only ASCII digits are digits to viy: `only_ascii_digits_count`).
    #[test]
    fn parse_when_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let pieces = [
            "1", "9", "0", "12", "90", "23", "59", "60", "24", "007", ".", ".5", "1.5", ":", "s",
            "m", "h", "d", "H", "M", " ", "\t", "x", "-", "+", "e",
        ];
        let nows = [
            "2026-06-28T22:00:00",
            "2026-07-17T23:59:30",
            "2025-12-31T00:00:00",
            "2026-02-28T12:34:56",
        ];
        for case in 0..4000 {
            let text: String = (0..1 + g.below(5)).map(|_| pick(&mut g, &pieces)).collect();
            let now_text = pick(&mut g, &nows);
            let now = NaiveDateTime::parse_from_str(now_text, "%Y-%m-%dT%H:%M:%S").unwrap();
            let ours = parse_when(&text, now);
            let theirs = oracle
                .call("shutdown.parse_when", json!([text, now_text]), json!({}))
                .unwrap()
                .outcome;
            let theirs = match theirs {
                Outcome::Returned(v) => Ok(v.as_f64().unwrap()),
                Outcome::Raised { kind, message } if kind == "ValueError" => Err(message),
                other => panic!("{text:?}: {other:?}"),
            };
            assert_eq!(
                ours,
                theirs,
                "case {case}, seed {:#x}: {text:?} at {now_text}",
                seed()
            );
        }
    }

    /// The same answer as rpctl's `_is_chat` for every command line.
    #[test]
    fn is_chat_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let pieces = [
            "rp",
            "/x/rp",
            "/x/bin/rp",
            "rpctl",
            "viy",
            "/x/viy",
            "chat",
            "ask",
            "shutdown",
            "-c",
            "--x",
            "-",
            "",
            "python",
            "-m",
            "xrp",
            "/x/rpx",
            "/x/viyx",
            "chatx",
        ];
        for case in 0..4000 {
            let args: Vec<&str> = (0..g.below(6)).map(|_| pick(&mut g, &pieces)).collect();
            let ours = is_chat(&argv(&args));
            let theirs = oracle
                .call("shutdown.is_chat", json!([args]), json!({}))
                .unwrap()
                .outcome;
            let Outcome::Returned(Value::Bool(theirs)) = theirs else {
                panic!("{args:?}: {theirs:?}")
            };
            assert_eq!(ours, theirs, "case {case}, seed {:#x}: {args:?}", seed());
        }
    }

    #[test]
    fn fmt_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        for _ in 0..2000 {
            let seconds = (g.below(400_000) as f64 - 1000.0) / [1.0, 7.0, 0.5][g.below(3)];
            let theirs = oracle
                .call("shutdown.fmt", json!([seconds]), json!({}))
                .unwrap()
                .outcome;
            assert_eq!(Outcome::Returned(json!(fmt(seconds))), theirs, "{seconds}");
        }
    }
}

/// The CLI starts the runner from synchronous code; tokio's process spawn
/// needs a runtime's reactor all the same, as `viy::shutdown::run` enters.
#[test]
fn the_runner_starts_on_the_system_from_sync_code_in_a_runtime_context() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _context = runtime.enter();
    let machine = Machine::new(&[], []);
    fs::create_dir_all(machine.paths.pod_state_dir()).unwrap();
    let host = Host {
        exec: &viy_exec::SystemExec,
        ..machine.host()
    };
    start_runner(&host, Path::new("/bin/true"), 1.0, true, &|_| {}).unwrap();
    assert!(machine.paths.shutdown_state_file().exists());
}
