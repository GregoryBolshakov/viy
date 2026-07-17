#!/usr/bin/env python3
"""Differential fixtures: rpctl's outputs at the pinned commit, for Rust tests.

    python3 tools/fixtures.py write [AREA/NAME ...]   (re)generate tests/fixtures/
    python3 tools/fixtures.py check [AREA/NAME ...]   regenerate in memory; fail on
                                                     any difference from the files

Each fixture is tests/fixtures/<area>/<name>.json: a header (the rpctl commit
from tests/rpctl-ref.txt, the generator) and a list of cases, each with its
inputs and rpctl's output. Rust tests replay them with no Python present
(viy_testkit::fixtures). Regenerating after a re-pin is how rpctl changes
reach viy: `check` fails until `write` is run and the diff reviewed.

Pure functions go through the oracle (tools/oracle.py). Generators for code
that reads files, such as config layering, run each case in its own
`python3 -I` process with HOME, XDG_* and the working directory in a fresh
temporary tree and a minimal environment, so module-level paths are computed
from the case and nothing reaches Gregory's real config. All inputs are
synthetic. Implementation plan 3.3.
"""
import json
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "tests" / "fixtures"


def snapshot():
    out = subprocess.run([str(ROOT / "tools" / "refsnap.sh")], cwd=ROOT, check=True,
                         capture_output=True, text=True).stdout.strip()
    return (ROOT / out).resolve()


def pinned_commit():
    for line in (ROOT / "tests" / "rpctl-ref.txt").read_text().splitlines():
        if line.startswith("commit:"):
            return line.split(":", 1)[1].strip()
    raise SystemExit("tests/rpctl-ref.txt has no commit line")


# ----------------------------------------------------------- config/layering

# One case: the text of each config layer by scope, the environment, and where
# the command runs. Scopes, not rpctl's paths, so viy can replay a case at its
# own paths (~/.config/viy/, .viy/). `project_marker` is the marker directory
# the case creates ("" for none); `cwd` is relative to the project root.
LAYERING_CASES = [
    dict(name="defaults_only"),
    dict(name="global_file_syntax",
         global_config=(
             "# a comment\n"
             "\n"
             "POD_NAME=from-global\n"
             "  MAX_TOKENS = 8192  \n"
             'SERVED_MODEL_NAME="double-quoted"\n'
             "VLLM_IMAGE='single-quoted'\n"
             "GPU_TYPE_IDS=\n"
             "no equals sign here\n"
             "export TUNNEL=0\n"
             "TOOLS_MAX_OUTPUT=12000\r\n"
             "NEW_KEY_NOT_IN_DEFAULTS=kept\n"
             "SEARCH_BACKEND=a=b=c\n"
             "MODEL_REPO=  Ключ-модели 😀  \n"
             "ENDPOINT_MODEL=unit-sep\x1f\n"
             "POD_NAME=second-wins\n"
         )),
    dict(name="profile_overrides_and_clears",
         global_config=(
             "MODEL_PROFILE=other\n"
             "NETWORK_VOLUME_ID=vol-global\n"
             "POD_NAME=global-pod\n"
             "MAX_TOKENS=4096\n"
         ),
         profiles={"other": (
             "POD_NAME=profile-pod\n"
             "NETWORK_VOLUME_ID=\n"
             "MAX_TOKENS=\n"
             "CHAT_MODE=\n"
             "SERVED_MODEL_NAME=profile-model\n"
         ), "unused": "POD_NAME=never-read\n"}),
    dict(name="profile_named_but_missing",
         global_config="MODEL_PROFILE=absent\nPOD_NAME=global-pod\n"),
    dict(name="profile_selected_only_from_global",
         global_config="POD_NAME=global-pod\n",
         profiles={"p": "POD_NAME=profile-pod\n"},
         project_config="MODEL_PROFILE=p\n",
         env={"MODEL_PROFILE": "p"}),
    dict(name="project_overrides_plain_keys",
         global_config="POD_NAME=global-pod\nMAX_TOKENS=4096\n",
         project_config="POD_NAME=project-pod\nMAX_TOKENS=\nTEMPERATURE=0.7\n"),
    dict(name="project_cannot_loosen",
         global_config="CONSULT_ENABLED=0\nCONSULT_MODE=advise\n",
         project_config=(
             "CHAT_MODE=auto-yes\n"
             "SANDBOX_NET=Host\n"
             "SANDBOX_NET_ALLOW=evil.example\n"
             "CONSULT_CLAUDE_BIN=/tmp/fake-claude\n"
             "CONSULT_PERMISSION_MODE=default\n"
             "CONSULT_ROOT=/\n"
             "CONSULT_ENABLED=1\n"
             "CONSULT_APPROVAL=never\n"
             "CONSULT_MODE=act\n"
         )),
    dict(name="project_may_tighten",
         global_config="CHAT_MODE=auto-yes\nCONSULT_APPROVAL=auto\nSANDBOX_NET=host\n",
         project_config=(
             "CHAT_MODE=normal\n"
             "SANDBOX_NET=proxy\n"
             "CONSULT_ENABLED=0\n"
             "CONSULT_APPROVAL=ask\n"
             "CONSULT_MODE=advise\n"
         )),
    dict(name="project_chat_mode_case_is_not_normalised",
         global_config="CHAT_MODE=auto-yes\n",
         project_config="CHAT_MODE=Normal\n"),
    dict(name="environment_wins",
         global_config="POD_NAME=global-pod\nNEW_KEY_NOT_IN_DEFAULTS=from-file\n",
         project_config="POD_NAME=project-pod\nCHAT_MODE=normal\n",
         env={"POD_NAME": "env-pod", "MAX_TOKENS": "", "CHAT_MODE": "auto-yes",
              "SANDBOX_NET": "host", "NEW_KEY_NOT_IN_DEFAULTS": "from-env",
              "KEY_ONLY_IN_ENV": "ignored"}),
    dict(name="marker_in_an_ancestor",
         project_config="POD_NAME=ancestor-pod\n", cwd="src/deep"),
    dict(name="no_marker",
         global_config="POD_NAME=global-pod\n", project_marker="", cwd="src"),
]

LAYERING_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import config
json.dump(config.load(), sys.stdout)
"""


def layering_case(snap, case):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp)
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        cfgdir = tmp / "config" / "rpctl"
        if case.get("global_config") is not None:
            cfgdir.mkdir(parents=True, exist_ok=True)
            (cfgdir / "config").write_bytes(case["global_config"].encode())
        for pname, text in case.get("profiles", {}).items():
            (cfgdir / "profiles").mkdir(parents=True, exist_ok=True)
            (cfgdir / "profiles" / f"{pname}.env").write_bytes(text.encode())
        project = tmp / "project"
        project.mkdir()
        marker = case.get("project_marker", ".rpctl")
        if marker:
            (project / marker).mkdir()
        if case.get("project_config") is not None:
            (project / marker / "config").write_bytes(case["project_config"].encode())
        cwd = project / case.get("cwd", "")
        cwd.mkdir(parents=True, exist_ok=True)
        env.update(case.get("env", {}))
        out = subprocess.run([sys.executable, "-I", "-c", LAYERING_RUNNER, str(snap)],
                             cwd=cwd, env=env, check=True, capture_output=True, text=True)
        return json.loads(out.stdout)


def gen_config_layering(snap):
    cases = []
    for case in LAYERING_CASES:
        inputs = {
            "name": case["name"],
            "global_config": case.get("global_config"),
            "profiles": case.get("profiles", {}),
            "project_config": case.get("project_config"),
            "project_marker": case.get("project_marker", ".rpctl"),
            "cwd": case.get("cwd", ""),
            "env": case.get("env", {}),
        }
        cases.append({**inputs, "config": layering_case(snap, case)})
    return {
        "about": "rpctl config.load() for synthetic layers: DEFAULTS < global "
                 "< MODEL_PROFILE profile < project < environment",
        "cases": cases,
    }


GENERATORS = {
    "config/layering": gen_config_layering,
}

# ------------------------------------------------------------------ driver


def render(name, body):
    doc = {"rpctl_commit": pinned_commit(), "generator": f"tools/fixtures.py {name}",
           **body}
    return json.dumps(doc, indent=1, ensure_ascii=False) + "\n"


def main(argv):
    if not argv or argv[0] not in ("write", "check"):
        print(__doc__.strip().split("\n\n")[1], file=sys.stderr)
        return 2
    names = argv[1:] or sorted(GENERATORS)
    unknown = [n for n in names if n not in GENERATORS]
    if unknown:
        print(f"unknown fixture: {', '.join(unknown)}; known: {', '.join(GENERATORS)}",
              file=sys.stderr)
        return 2
    snap = snapshot()
    failed = 0
    for name in names:
        path = FIXTURES / f"{name}.json"
        text = render(name, GENERATORS[name](snap))
        if argv[0] == "write":
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            print(f"wrote {path.relative_to(ROOT)}")
        elif not path.exists() or path.read_text() != text:
            print(f"{path.relative_to(ROOT)} differs from rpctl at the pin "
                  f"(run: python3 tools/fixtures.py write {name})")
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
