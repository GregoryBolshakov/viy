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
import shlex
import shutil
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
    dict(name="profile_selected_by_the_environment",
         global_config="POD_NAME=global-pod\n",
         profiles={"p": "POD_NAME=profile-pod\n"},
         project_config="MODEL_PROFILE=p\n",
         env={"MODEL_PROFILE": "p"}),
    dict(name="profile_not_selected_by_the_project",
         global_config="MODEL_PROFILE=g\nPOD_NAME=global-pod\n",
         profiles={"g": "POD_NAME=g-pod\n", "p": "POD_NAME=p-pod\n"},
         project_config="MODEL_PROFILE=p\n"),
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
    dict(name="project_host_network_in_any_case",
         project_config="SANDBOX_NET=  HoSt  \nNEW_PROJECT_KEY=added\n"),
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


# ------------------------------------------------------------ config/project

# One case: a tree under the case's temporary root T (`dirs`, `files`,
# `symlinks` as link -> target), the directory the command runs in, and the
# XDG variables (absent: set to T/config, T/state, T/data; None: unset; a
# string: that value, "" included). HOME is always T/home. The output is
# rpctl's project root and paths with T written as "$T".
PROJECT_CASES = [
    dict(name="marker_in_cwd", dirs=["project/.rpctl"], cwd="project"),
    dict(name="marker_in_an_ancestor", dirs=["project/.rpctl", "project/src/deep"],
         cwd="project/src/deep"),
    dict(name="nearest_marker_wins",
         dirs=["project/.rpctl", "project/sub/.rpctl", "project/sub/src"],
         cwd="project/sub/src"),
    dict(name="no_marker", dirs=["project/src"], cwd="project/src"),
    dict(name="marker_is_a_file", dirs=["project/src"], files=["project/.rpctl"],
         cwd="project/src"),
    dict(name="marker_is_a_symlink_to_a_directory", dirs=["elsewhere", "project/src"],
         symlinks={"project/.rpctl": "../elsewhere"}, cwd="project/src"),
    dict(name="marker_is_a_dangling_symlink", dirs=["project/src"],
         symlinks={"project/.rpctl": "missing"}, cwd="project/src"),
    dict(name="cwd_through_a_symlink", dirs=["real/.rpctl", "real/src"],
         symlinks={"link": "real"}, cwd="link/src"),
    dict(name="marker_in_home", dirs=["home/.rpctl", "home/work"], cwd="home/work"),
    dict(name="xdg_unset", dirs=["project"], cwd="project",
         xdg={"XDG_CONFIG_HOME": None, "XDG_STATE_HOME": None, "XDG_DATA_HOME": None}),
    dict(name="xdg_empty", dirs=["project"], cwd="project",
         xdg={"XDG_CONFIG_HOME": "", "XDG_STATE_HOME": "", "XDG_DATA_HOME": ""}),
]

PROJECT_PATHS = ("PROJECT_DIR", "USER_CONFIG_DIR", "USER_STATE_DIR", "USER_DATA_DIR",
                 "SESSIONS_DIR", "CONFIG_PATH", "PROFILES_DIR", "STATE_PATH",
                 "TRANSCRIPTS", "HISTORY_PATH")

PROJECT_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import config
out = {k: str(getattr(config, k)) for k in sys.argv[2:]}
out["has_project"] = config.has_project()
json.dump(out, sys.stdout)
"""


def project_case(snap, case):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        (tmp / "home").mkdir()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": str(tmp / "home")}
        xdg = {"XDG_CONFIG_HOME": str(tmp / "config"), "XDG_STATE_HOME": str(tmp / "state"),
               "XDG_DATA_HOME": str(tmp / "data")}
        xdg.update(case.get("xdg", {}))
        env.update({k: v for k, v in xdg.items() if v is not None})
        for d in case.get("dirs", []):
            (tmp / d).mkdir(parents=True, exist_ok=True)
        for f in case.get("files", []):
            (tmp / f).write_text("")
        for link, target in case.get("symlinks", {}).items():
            (tmp / link).symlink_to(target)
        out = subprocess.run([sys.executable, "-I", "-c", PROJECT_RUNNER, str(snap),
                              *PROJECT_PATHS],
                             cwd=tmp / case["cwd"], env=env, check=True,
                             capture_output=True, text=True)
        paths = json.loads(out.stdout)
        return {k: v.replace(str(tmp), "$T") if isinstance(v, str) else v
                for k, v in paths.items()}


def gen_config_project(snap):
    cases = []
    for case in PROJECT_CASES:
        inputs = {
            "name": case["name"],
            "dirs": case.get("dirs", []),
            "files": case.get("files", []),
            "symlinks": case.get("symlinks", {}),
            "cwd": case["cwd"],
            "xdg": case.get("xdg", {}),
        }
        cases.append({**inputs, "paths": project_case(snap, case)})
    return {
        "about": "rpctl config's project root (find_project from the cwd), has_project() "
                 "and module paths for synthetic trees; $T is the case's root, HOME is "
                 "$T/home, XDG_* are $T/config, $T/state, $T/data unless the case says",
        "cases": cases,
    }


# ------------------------------------------------ config/model, config/persist

# One case: the global config, the profiles, the project's .rpctl/config and
# the environment as in config/layering, and what to run. The output is what
# the command printed, its exit status, and every file under rpctl's config
# directory afterwards (path relative to it -> text), with the case's root T
# written as "$T".
MODEL_CASES = [
    dict(name="list_without_a_profiles_dir", argv=["list"]),
    dict(name="list_with_an_empty_profiles_dir", argv=["list"], profiles_dir=True,
         global_config="MODEL_PROFILE=ghost\n"),
    dict(name="list_marks_the_active_one_and_sorts_by_code_point", argv=["list"],
         global_config="MODEL_PROFILE=beta\n",
         profiles={"beta": "MODEL_REPO=org/beta-repo\n",
                   "alpha": "SERVED_MODEL_NAME=alpha-served\nMODEL_REPO=org/alpha\n",
                   "Zeta": "POD_NAME=z\n",
                   "blank-served": "SERVED_MODEL_NAME=\nMODEL_REPO=org/blank\n"},
         other_files={"profiles/README": "not a profile\n"}),
    dict(name="list_with_an_active_profile_that_has_no_file", argv=["list"],
         global_config="MODEL_PROFILE=ghost\n", profiles={"alpha": "MODEL_REPO=a\n"}),
    dict(name="list_ignores_the_project_files_profile", argv=["list"],
         global_config="MODEL_PROFILE=alpha\n", project_config="MODEL_PROFILE=beta\n",
         profiles={"alpha": "MODEL_REPO=a\n", "beta": "MODEL_REPO=b\n"}),
    dict(name="show_the_active_profile_in_profile_key_order", argv=["show"],
         global_config="MODEL_PROFILE=alpha\n",
         profiles={"alpha": "EXTRA_ONE=1\nPOD_NAME=alpha-pod\nBACKEND=runpod\n"
                            "# a comment\nNETWORK_VOLUME_ID=\nEXTRA_TWO=2\n"}),
    dict(name="show_a_named_profile", argv=["show", "beta"],
         global_config="MODEL_PROFILE=alpha\n",
         profiles={"alpha": "MODEL_REPO=a\n", "beta": "MODEL_REPO=b\nMAX_TOKENS=99\n"}),
    dict(name="show_without_an_active_profile", argv=["show"]),
    dict(name="show_a_missing_profile", argv=["show", "ghost"]),
    dict(name="show_an_empty_profile", argv=["show", "empty"], profiles={"empty": ""}),
    dict(name="use_appends_to_the_global_file", argv=["use", "alpha"],
         global_config="# rp config\nRUNPOD_API_KEY=k\n\nPOD_NAME=p", profiles={"alpha": ""}),
    dict(name="use_replaces_the_line_in_place", argv=["use", "beta"],
         global_config="POD_NAME=p\nMODEL_PROFILE=alpha\nTAVILY_API_KEY=t\n",
         profiles={"alpha": "", "beta": "MODEL_REPO=b\n"}),
    dict(name="use_uncomments_a_commented_line", argv=["use", "beta"],
         global_config="POD_NAME=p\n#MODEL_PROFILE=alpha\n",
         profiles={"beta": "MODEL_REPO=b\n"}),
    dict(name="use_keeps_one_of_two_lines", argv=["use", "beta"],
         global_config="MODEL_PROFILE=alpha\nPOD_NAME=p\nMODEL_PROFILE=gamma\n",
         profiles={"beta": "MODEL_REPO=b\n"}),
    dict(name="use_over_a_commented_line_above_the_real_one", argv=["use", "beta"],
         global_config="#MODEL_PROFILE=old\nMODEL_PROFILE=alpha\n",
         profiles={"beta": "MODEL_REPO=b\n"}),
    dict(name="use_creates_the_global_file", argv=["use", "alpha"],
         profiles={"alpha": "MODEL_REPO=a\n"}),
    dict(name="use_a_missing_profile", argv=["use", "ghost"],
         global_config="POD_NAME=p\n", profiles={"alpha": ""}),
    dict(name="use_without_a_name", argv=["use"]),
    dict(name="add_seeds_from_the_effective_config", argv=["add", "new"],
         global_config="POD_NAME=global-pod\nNETWORK_VOLUME_ID=vol-old\nRUNPOD_API_KEY=k\n"
                       "MODEL_REPO=org/global\n",
         env={"MAX_TOKENS": "4096"}),
    dict(name="add_refuses_an_existing_profile", argv=["add", "alpha"],
         profiles={"alpha": "MODEL_REPO=keep\n"}),
    dict(name="add_force_overwrites", argv=["add", "alpha", "--force"],
         global_config="MODEL_PROFILE=alpha\n",
         profiles={"alpha": "MODEL_REPO=org/alpha\nNETWORK_VOLUME_ID=vol-a\nEXTRA=x\n"}),
    dict(name="add_without_a_name", argv=["add"]),
]

MODEL_RUNNER = r"""
import sys
sys.path.insert(0, sys.argv[1])
sys.argv = ["rp", "model", *sys.argv[2:]]
from rpctl.__main__ import main
main()
"""

# persist(cfg, key, value) after load(): _write_kv into the active profile,
# else into the global file.
PERSIST_CASES = [
    dict(name="appends_a_new_key", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="# rp\nPOD_NAME=p\n"),
    dict(name="replaces_in_place_keeping_comments_and_order", key="NETWORK_VOLUME_ID",
         value="vol-new",
         global_config="# rp\nPOD_NAME=p\nNETWORK_VOLUME_ID=vol-old\n\n# tail\nX=1\n"),
    dict(name="uncomments_a_commented_key", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="#NETWORK_VOLUME_ID=vol-old\nPOD_NAME=p\n"),
    dict(name="a_spaced_comment_is_not_the_key", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="# NETWORK_VOLUME_ID=vol-old\n"),
    dict(name="a_spaced_assignment_is_the_key", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="NETWORK_VOLUME_ID = vol-old\n"),
    dict(name="an_indented_line_loses_its_indent", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="  NETWORK_VOLUME_ID=vol-old\n\tPOD_NAME=p\n"),
    dict(name="a_longer_key_is_not_the_key", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="NETWORK_VOLUME_ID_OLD=x\n"),
    dict(name="the_first_of_two_lines_the_second_goes", key="NETWORK_VOLUME_ID",
         value="vol-new", global_config="NETWORK_VOLUME_ID=a\nPOD_NAME=p\nNETWORK_VOLUME_ID=b\n"),
    dict(name="a_commented_line_above_the_real_one", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="#NETWORK_VOLUME_ID=old\nPOD_NAME=p\nNETWORK_VOLUME_ID=b\n"),
    dict(name="crlf_becomes_lf", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="POD_NAME=p\r\nNETWORK_VOLUME_ID=vol-old\r\n"),
    dict(name="no_final_newline", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="POD_NAME=p"),
    dict(name="an_empty_value", key="NETWORK_VOLUME_ID", value="",
         global_config="NETWORK_VOLUME_ID=vol-old\n"),
    dict(name="creates_the_global_file_and_dir", key="NETWORK_VOLUME_ID", value="vol-new"),
    dict(name="the_active_profile_gets_it", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="MODEL_PROFILE=alpha\nNETWORK_VOLUME_ID=vol-global\n",
         profiles={"alpha": "MODEL_REPO=a\nNETWORK_VOLUME_ID=\n"}),
    dict(name="creates_a_missing_active_profile", key="NETWORK_VOLUME_ID", value="vol-new",
         global_config="MODEL_PROFILE=alpha\n"),
    dict(name="the_profile_named_by_the_environment", key="NETWORK_VOLUME_ID",
         value="vol-new", global_config="MODEL_PROFILE=alpha\n",
         profiles={"alpha": "", "other": ""}, env={"MODEL_PROFILE": "other"}),
    dict(name="a_project_file_does_not_name_the_profile", key="NETWORK_VOLUME_ID",
         value="vol-new", global_config="MODEL_PROFILE=alpha\n",
         profiles={"alpha": "", "other": ""}, project_config="MODEL_PROFILE=other\n"),
    dict(name="a_blank_active_profile_means_the_global_file", key="NETWORK_VOLUME_ID",
         value="vol-new", global_config="POD_NAME=p\n", env={"MODEL_PROFILE": "  "}),
]

PERSIST_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import config
cfg = config.load()
config.persist(cfg, sys.argv[2], sys.argv[3])
print(json.dumps(cfg[sys.argv[2]]))
"""


def tree_run(snap, case, runner, args):
    """Runs `runner` in a fresh tree laid out as the case says; returns the
    exit status, the output, and the files under rpctl's config dir."""
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        cfgdir = tmp / "config" / "rpctl"
        files = {}
        if case.get("global_config") is not None:
            files["config"] = case["global_config"]
        for pname, text in case.get("profiles", {}).items():
            files[f"profiles/{pname}.env"] = text
        files.update(case.get("other_files", {}))
        for rel, text in files.items():
            (cfgdir / rel).parent.mkdir(parents=True, exist_ok=True)
            (cfgdir / rel).write_bytes(text.encode())
        if case.get("profiles_dir"):
            (cfgdir / "profiles").mkdir(parents=True, exist_ok=True)
        project = tmp / "project"
        (project / ".rpctl").mkdir(parents=True)
        if case.get("project_config") is not None:
            (project / ".rpctl" / "config").write_bytes(case["project_config"].encode())
        env.update(case.get("env", {}))
        out = subprocess.run([sys.executable, "-I", "-c", runner, str(snap), *args],
                             cwd=project, env=env, capture_output=True)
        after = {str(p.relative_to(cfgdir)): p.read_bytes().decode()
                 for p in sorted(cfgdir.rglob("*")) if p.is_file()}
        t = lambda s: s.replace(str(tmp), "$T")
        return {"exit": out.returncode, "stdout": t(out.stdout.decode()),
                "stderr": t(out.stderr.decode()), "files": after}


def tree_inputs(case):
    return {
        "name": case["name"],
        "global_config": case.get("global_config"),
        "profiles": case.get("profiles", {}),
        "profiles_dir": case.get("profiles_dir", False),
        "other_files": case.get("other_files", {}),
        "project_config": case.get("project_config"),
        "env": case.get("env", {}),
    }


def gen_config_model(snap):
    cases = []
    for case in MODEL_CASES:
        out = tree_run(snap, case, MODEL_RUNNER, case["argv"])
        cases.append({**tree_inputs(case), "argv": case["argv"], **out})
    return {
        "about": "`rp model ARGV` (rpctl __main__.cmd_model after config.load()) in a "
                 "synthetic tree: exit status, stdout, stderr, and the files under "
                 "$T/config/rpctl afterwards; HOME is $T/home, XDG_CONFIG_HOME $T/config, "
                 "the project $T/project with a .rpctl/ marker",
        "cases": cases,
    }


def gen_config_persist(snap):
    cases = []
    for case in PERSIST_CASES:
        out = tree_run(snap, case, PERSIST_RUNNER, [case["key"], case["value"]])
        if out["exit"] != 0:
            raise SystemExit(f"config/persist {case['name']}: {out['stderr']}")
        cases.append({**tree_inputs(case), "key": case["key"], "value": case["value"],
                      "cfg_value": json.loads(out["stdout"]), "files": out["files"]})
    return {
        "about": "rpctl config.persist(cfg, KEY, VALUE) after config.load() in a synthetic "
                 "tree: cfg[KEY] afterwards and the files under $T/config/rpctl",
        "cases": cases,
    }


# ---------------------------------------------------------------- config/init

# `rp init` in $T/project/<cwd>: `project_files` are laid out first (a
# trailing "/" makes a directory), `global_config` under $T/config/rpctl/.
INIT_CASES = [
    dict(name="a_fresh_folder"),
    dict(name="a_fresh_folder_with_a_global_config", global_config="POD_NAME=p\n"),
    dict(name="already_a_project", project_files={".rpctl/": ""}),
    dict(name="a_marker_with_only_a_config_gets_the_rest",
         project_files={".rpctl/config": "CHAT_MODE=normal\n"}),
    dict(name="an_existing_gitignore_is_kept", project_files={".rpctl/.gitignore": "mine\n"}),
    dict(name="a_subfolder_of_a_project_gets_its_own_marker",
         project_files={".rpctl/": "", "sub/": ""}, cwd="sub"),
    dict(name="a_marker_that_is_a_file", project_files={".rpctl": "not a dir\n"}),
]

INIT_RUNNER = r"""
import sys
sys.path.insert(0, sys.argv[1])
sys.argv = ["rp", "init"]
from rpctl.__main__ import main
main()
"""


def init_case(snap, case):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        if case.get("global_config") is not None:
            (tmp / "config" / "rpctl").mkdir()
            (tmp / "config" / "rpctl" / "config").write_bytes(case["global_config"].encode())
        project = tmp / "project"
        project.mkdir()
        for rel, text in case.get("project_files", {}).items():
            if rel.endswith("/"):
                (project / rel).mkdir(parents=True, exist_ok=True)
            else:
                (project / rel).parent.mkdir(parents=True, exist_ok=True)
                (project / rel).write_bytes(text.encode())
        out = subprocess.run([sys.executable, "-I", "-c", INIT_RUNNER, str(snap)],
                             cwd=project / case.get("cwd", ""), env=env, capture_output=True)
        after = {}
        for q in sorted(project.rglob("*")):
            rel = str(q.relative_to(project))
            after[rel + "/" if q.is_dir() else rel] = "" if q.is_dir() else q.read_bytes().decode()
        t = lambda s: s.replace(str(tmp), "$T")
        stderr = t(out.stderr.decode())
        # A traceback is Python's; the case records only its last line.
        if "Traceback" in stderr:
            stderr = stderr.strip().splitlines()[-1] + "\n"
        return {"exit": out.returncode, "stdout": t(out.stdout.decode()),
                "stderr": stderr, "project_files_after": after}


def gen_config_init(snap):
    cases = []
    for case in INIT_CASES:
        inputs = {"name": case["name"], "global_config": case.get("global_config"),
                  "project_files": case.get("project_files", {}), "cwd": case.get("cwd", "")}
        cases.append({**inputs, **init_case(snap, case)})
    return {
        "about": "`rp init` (rpctl __main__.cmd_init) in $T/project/<cwd> after laying out "
                 "project_files (a trailing / is a directory): exit status, stdout, stderr "
                 "(a traceback's last line only), and every path under $T/project "
                 "afterwards; HOME is $T/home, XDG_CONFIG_HOME $T/config",
        "cases": cases,
    }


# ------------------------------------------------------------------ cli/flags

# argv after `rp`. Abbreviated long options, `-n5`, `--opt=value` and `--`
# are argparse's and kept; values that argparse takes and the command later
# rejects (`up --timeout abc`) are here too, so the Rust test names each
# place viy refuses them at parse time instead.
CLI_CASES = [
    [], ["bogus"], ["--help"], ["model", "-h"],
    ["init"], ["init", "x"], ["migrate"],
    ["up"], ["up", "--timeout", "30"], ["up", "--timeout=30"], ["up", "--time", "30"],
    ["up", "--timeout", "abc"],
    ["status"], ["stop"], ["down"], ["down", "now"],
    ["boots"], ["boots", "show"], ["boots", "show", "abc"], ["boots", "list", "-n", "3"],
    ["boots", "-n5"], ["boots", "abc"], ["boots", "show", "a", "b"], ["boots", "-n", "x"],
    ["boots", "-n", "-3"],
    ["logs"], ["logs", "-f"], ["logs", "--follow"],
    ["reap"], ["reap", "--install"], ["reap", "--quiet"], ["reap", "--inst"],
    ["shutdown"], ["shutdown", "90m"], ["shutdown", "--status"], ["shutdown", "--cancel"],
    ["shutdown", "23:30", "--no-poweroff", "-f"], ["shutdown", "--no-p", "1h"],
    ["shutdown", "--run-at", "2026-07-14T23:30:00"], ["shutdown", "1h", "2h"],
    ["tunnel"], ["tunnel", "-b"], ["tunnel", "--background", "--stop"],
    ["tunnel", "--status"], ["tunnel", "--forget-key"], ["tunnel", "--forget"],
    ["tunnel", "--st"],
    ["chat"], ["chat", "-c"], ["chat", "--continue"], ["chat", "--cont"], ["chat", "-r"],
    ["chat", "-r", "foo"], ["chat", "--resume=foo bar"], ["chat", "-r", "-c"],
    ["chat", "--url", "http://127.0.0.1:8000", "--system", "be terse"],
    ["chat", "-c", "extra"],
    ["chats"], ["chats", "foo", "bar"], ["chats", "show", "abc", "-a"],
    ["chats", "-a", "rename", "x", "new", "title"], ["chats", "rm", "x"],
    ["chats", "import"], ["chats", "--all"], ["chats", "foo", "-a", "bar"],
    ["ask"], ["ask", "hi"], ["ask", "hi", "--think", "--save"], ["ask", "a", "b"],
    ["ask", "--url", "u", "--system", "s", "hi"], ["ask", "--", "--not-a-flag"],
    ["trace"], ["trace", "alarms"], ["trace", "-n", "5"], ["trace", "abc123", "-n", "3"],
    ["trace", "a", "b"],
    ["toolbox"], ["toolbox", "list"], ["toolbox", "find", "a", "b"],
    ["toolbox", "add", "dir", "--global"], ["toolbox", "rm", "t", "--global"],
    ["toolbox", "bogus"], ["toolbox", "show", "x"], ["toolbox", "--global"],
    ["model"], ["model", "list"], ["model", "show"], ["model", "use", "beta"],
    ["model", "add", "x", "--force"], ["model", "bogus"], ["model", "use", "a", "b"],
    ["backup"], ["backup", "--samples", "3"], ["backup", "--samples", "x"],
    ["volume"], ["volume", "list"], ["volume", "create"],
    ["volume", "delete", "--volume-id", "v1"], ["volume", "delete"], ["volume", "bogus"],
]

CLI_RUNNER = r"""
import argparse, contextlib, io, json, sys
sys.path.insert(0, sys.argv[1])
from rpctl.__main__ import build_parser

def value(v):
    if v is None or isinstance(v, bool):
        return v
    if isinstance(v, list):
        return [value(x) for x in v]
    return str(v)

parser = build_parser()
sub = next(a for a in parser._actions if isinstance(a, argparse._SubParsersAction))
helps = {c.dest: c.help for c in sub._choices_actions}
table = [{"name": "", "help": None, "description": parser.description, "args": []}]
for name, p in sub.choices.items():
    args = []
    for a in p._actions:
        if isinstance(a, argparse._HelpAction):
            continue
        args.append({
            "flags": a.option_strings,
            "nargs": a.nargs,
            "flag": isinstance(a, argparse._StoreTrueAction),
            "const": value(a.const),
            "default": value(a.default),
            "type": a.type.__name__ if a.type else None,
            "choices": list(a.choices) if a.choices else None,
            "required": a.required,
            "hidden": a.help == argparse.SUPPRESS,
            "help": None if a.help == argparse.SUPPRESS else a.help,
        })
    table.append({"name": name, "help": helps[name], "description": p.description,
                  "args": args})

parses = []
for argv in json.loads(sys.argv[2]):
    out, err = io.StringIO(), io.StringIO()
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            ns = parser.parse_args(argv)
        parses.append({"argv": argv, "exit": 0,
                       "args": {k: value(v) for k, v in vars(ns).items() if k != "fn"}})
    except SystemExit as e:
        parses.append({"argv": argv, "exit": e.code})
print(json.dumps({"flags": table, "parse": parses}))
"""


def cli_run(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([sys.executable, "-I", "-c", CLI_RUNNER, str(snap),
                              json.dumps(CLI_CASES)],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def gen_cli_flags(snap):
    return {
        "about": "rpctl __main__.build_parser(): the program (name \"\") and each "
                 "subcommand in order, with its help, description and arguments (option "
                 "strings, nargs, whether a store_true flag, const, default, type, choices, "
                 "required, hidden, help; values as strings). Help and usage layout are "
                 "argparse's and not recorded (flags.md F31)",
        "cases": cli_run(snap)["flags"],
    }


def gen_cli_parse(snap):
    return {
        "about": "rpctl __main__.build_parser().parse_args(argv): the exit status and, on "
                 "success, the namespace without fn, values as strings",
        "cases": cli_run(snap)["parse"],
    }


# -------------------------------------------------------------------- pod/api

# One case: a call on rpctl's api.RunPod (method name and arguments), and the
# reply its single HTTP request gets: a status with a body, sent in `chunks`
# (default: the whole body at once) and then ended by EOF or `then`
# ("read_timeout": urllib3's read timeout, as a stalled stream raises), or
# `fail` ("connect": a refused connection) instead of any reply. The call
# never reaches the network: a requests transport adapter records what
# would be sent and plays the reply. Keys, ids and bodies are synthetic.
API_KEY = "rpa_SYNTHETICKEY0000"

def _json(value):
    return json.dumps(value)

POD = {"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING",
       "machine": {"gpuDisplayName": "A100 SXM"}, "costPerHr": 1.89}
VOLUME = {"id": "vol222", "name": "qwen-weights", "size": 120, "dataCenterId": "US-MO-1"}
ERROR_JSON = {"error": "pod not found", "status": 404}
LOG_LINES = [
    b'data: {"ts": "2026-07-15T10:00:02Z", "source": "container", "line": "second"}\n',
    b": keepalive\n",
    b'data: {"ts": "2026-07-15T10:00:01Z", "source": "system", "line": "first"}\n',
    b"data: not json\n",
    b"data: [1, 2]\n",
    b'data: {"ts": "2026-07-15T10:00:03Z", "source": "system"}\n',
    b'data:{"source": "container", "line": "no ts, sorts first"}\r\n',
    b'event: log\n',
    b'data: {"ts": "2026-07-15T10:00:04Z", "line": "caf\xc3\xa9 \\u00e9"}\n',
]

API_CASES = [
    # REST: what each call sends, and what it makes of the answer.
    dict(name="list_pods_list", call=["list_pods"], reply=dict(body=_json([POD]))),
    dict(name="list_pods_data_wrapper", call=["list_pods"],
         reply=dict(body=_json({"data": [POD, {"id": "pod333"}]}))),
    dict(name="list_pods_empty_body", call=["list_pods"], reply=dict(body="")),
    dict(name="list_pods_not_json", call=["list_pods"],
         reply=dict(body="<html>ok</html>", content_type="text/html")),
    dict(name="list_pods_null", call=["list_pods"], reply=dict(body="null")),
    dict(name="get_pod", call=["get_pod", "pod111"], reply=dict(body=_json(POD))),
    dict(name="get_pod_404_is_gone", call=["get_pod", "pod111"],
         reply=dict(status=404, body=_json(ERROR_JSON))),
    dict(name="get_pod_empty_body", call=["get_pod", "pod111"], reply=dict(body="")),
    dict(name="create_pod", call=["create_pod", {
        "name": "qwen-pod", "imageName": "vllm/vllm-openai:v0.11.0",
        "gpuTypeIds": ["NVIDIA A100-SXM4-80GB", "NVIDIA H100 80GB HBM3"],
        "gpuCount": 1, "cloudType": "SECURE", "networkVolumeId": "vol222",
        "ports": ["8000/http", "22/tcp"], "env": {"RPCTL_A": "1", "B": ""},
        "dockerStartCmd": ["bash", "-c", "echo 'boot'"], "minCudaVersion": "12.8",
        "supportPublicIp": True, "interruptible": False}],
         reply=dict(body=_json(POD))),
    dict(name="stop_pod", call=["stop_pod", "pod111"], reply=dict(body="")),
    dict(name="start_pod", call=["start_pod", "pod111"],
         reply=dict(body=_json({"id": "pod111", "desiredStatus": "RUNNING"}))),
    dict(name="delete_pod", call=["delete_pod", "pod111"], reply=dict(body="")),
    dict(name="list_volumes", call=["list_volumes"], reply=dict(body=_json([VOLUME]))),
    dict(name="get_volume", call=["get_volume", "vol222"], reply=dict(body=_json(VOLUME))),
    dict(name="create_volume", call=["create_volume", "qwen-weights-backup", 120, "EU-RO-1"],
         reply=dict(body=_json(VOLUME))),
    dict(name="delete_volume", call=["delete_volume", "vol222"], reply=dict(body="")),
    # REST errors: the status decides transient, the body becomes the detail.
    *[dict(name=f"get_volume_http_{code}", call=["get_volume", "vol222"],
           reply=dict(status=code, body=_json({"error": f"synthetic {code}"})))
      for code in (400, 401, 403, 404, 409, 429, 500, 502, 503, 504)],
    dict(name="http_error_nested_json_detail", call=["create_pod", {"name": "x"}],
         reply=dict(status=500, body=_json(
             {"error": "create pod: There are no longer any instances available "
                       "with the requested specifications.",
              "details": {"gpuTypeIds": ["A", "B"], "retry": None, "n": [], "m": {}}}))),
    dict(name="http_error_text_detail", call=["list_pods"],
         reply=dict(status=502, body="<html>502 Bad Gateway</html>",
                    content_type="text/html")),
    dict(name="http_error_long_text_is_cut_at_2000", call=["list_pods"],
         reply=dict(status=503, body="x" * 1990 + "0123456789abcdef",
                    content_type="text/plain; charset=utf-8")),
    dict(name="http_error_json_scalar", call=["list_pods"],
         reply=dict(status=403, body=_json("forbidden"))),
    dict(name="http_error_empty_body", call=["stop_pod", "pod111"],
         reply=dict(status=500, body="")),
    dict(name="transport_failure_is_transient", call=["get_pod", "pod111"],
         reply=dict(fail="connect")),
    dict(name="read_timeout_is_transient", call=["list_volumes"],
         reply=dict(body="", then="read_timeout")),
    # GraphQL.
    dict(name="data_centers", call=["data_centers"], reply=dict(body=_json({"data": {
        "dataCenters": [{"id": "US-MO-1", "location": "United States", "listed": True,
                         "storageSupport": True, "gpuAvailability": [
                             {"gpuTypeId": "NVIDIA A100-SXM4-80GB", "stockStatus": "High",
                              "available": True}]}]}}))),
    dict(name="data_centers_null_data", call=["data_centers"],
         reply=dict(body=_json({"data": None}))),
    dict(name="data_centers_null_list", call=["data_centers"],
         reply=dict(body=_json({"data": {"dataCenters": None}}))),
    dict(name="graphql_errors_at_200", call=["data_centers"],
         reply=dict(body=_json({"errors": [{"message": "Something went wrong",
                                           "path": ["dataCenters"]}], "data": None}))),
    dict(name="graphql_empty_errors_are_no_error", call=["data_centers"],
         reply=dict(body=_json({"errors": [], "data": {"dataCenters": []}}))),
    dict(name="graphql_no_data_key", call=["data_centers"], reply=dict(body=_json({}))),
    dict(name="graphql_http_500_text", call=["data_centers"],
         reply=dict(status=500, body="upstream failed", content_type="text/plain")),
    dict(name="graphql_http_429_json", call=["data_centers"],
         reply=dict(status=429, body=_json({"data": {"dataCenters": []}}))),
    dict(name="graphql_not_json", call=["data_centers"],
         reply=dict(status=200, body="<html></html>", content_type="text/html")),
    dict(name="graphql_transport_failure", call=["data_centers"], reply=dict(fail="connect")),
    dict(name="cuda_stock_with_older", call=[
        "cuda_stock", ["US-MO-1", "EU-RO-1"],
        ["NVIDIA A100-SXM4-80GB", "NVIDIA H100 80GB HBM3"], "13.0", ["12.8", "12.9"], True],
         reply=dict(body=_json({"data": {
             "s0_0f": [{"lowestPrice": {"stockStatus": "High"}}],
             "s0_0o": [{"lowestPrice": {"stockStatus": "Low"}}],
             "s0_1f": [{"lowestPrice": None}],
             "s0_1o": [],
             "s1_0f": [None],
             "s1_0o": None,
             "s1_1f": [{"lowestPrice": {"stockStatus": "Medium"}}]}}))),
    dict(name="cuda_stock_community_no_older", call=[
        "cuda_stock", ["CA-MTL-3"], ["NVIDIA A100 80GB PCIe"], "12.8", [], False],
         reply=dict(body=_json({"data": {"s0_0f": [{"lowestPrice": {"stockStatus": "Low"}}]}}))),
    # The v2 platform log stream.
    dict(name="pod_logs_entries_sorted_by_ts", call=["pod_logs", "pod111", 100, 8],
         reply=dict(chunks=[line.decode() for line in LOG_LINES])),
    dict(name="pod_logs_split_across_chunks", call=["pod_logs", "pod111", 30, 8],
         reply=dict(chunks=['data: {"ts": "1", "li', 'ne": "a"}\ndata: {"ts": "0", "line"',
                            ': "b"}\n', 'data: {"line": "no newline at the end"}'])),
    dict(name="pod_logs_stall_after_entries_returns_them", call=["pod_logs", "pod111", 5000, 8],
         reply=dict(chunks=['data: {"ts": "1", "line": "a"}\n'], then="read_timeout")),
    dict(name="pod_logs_stall_before_entries_is_an_error", call=["pod_logs", "pod111", 100, 8],
         reply=dict(chunks=[": keepalive\n"], then="read_timeout")),
    dict(name="pod_logs_unreachable", call=["pod_logs", "pod111", 100, 8],
         reply=dict(fail="connect")),
    dict(name="pod_logs_http_401", call=["pod_logs", "pod111", 100, 8],
         reply=dict(status=401, chunks=['{"error": "unauthorized"}'])),
    dict(name="pod_logs_http_404", call=["pod_logs", "pod111", 100, 8],
         reply=dict(status=404, chunks=[""])),
]

API_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
import requests, urllib3
from requests.adapters import BaseAdapter
from requests.structures import CaseInsensitiveDict
from rpctl import api

class Raw:
    def __init__(self, chunks, then):
        self.chunks, self.then = chunks, then
    def stream(self, chunk_size, decode_content=True):
        for chunk in self.chunks:
            yield chunk
        if self.then == "read_timeout":
            raise urllib3.exceptions.ReadTimeoutError(None, None, "Read timed out.")
    def close(self):
        pass
    def release_conn(self):
        pass

class Player(BaseAdapter):
    def __init__(self, reply):
        super().__init__()
        self.reply, self.sent = reply, []
    def send(self, request, stream=False, timeout=None, **kw):
        body = request.body
        if isinstance(body, bytes):
            body = body.decode()
        self.sent.append({"method": request.method, "url": request.url,
                          "headers": dict(request.headers), "body": body,
                          "timeout": list(timeout) if isinstance(timeout, tuple) else timeout,
                          "stream": stream})
        if self.reply.get("fail") == "connect":
            raise requests.exceptions.ConnectionError("synthetic: connection refused")
        r = requests.Response()
        r.status_code = self.reply.get("status", 200)
        r.headers = CaseInsensitiveDict(
            {"Content-Type": self.reply.get("content_type", "application/json")})
        chunks = self.reply.get("chunks")
        if chunks is None:
            chunks = [self.reply.get("body", "")]
        r.raw = Raw([c.encode() for c in chunks], self.reply.get("then"))
        r.url, r.request, r.reason = request.url, request, "synthetic"
        r.encoding = requests.utils.get_encoding_from_headers(r.headers)
        return r

def outcome(name, value):
    if name == "cuda_stock":
        return [[dc, gpu, fit, old] for (dc, gpu), (fit, old) in value.items()]
    return value

cases = []
for case in json.loads(sys.argv[2]):
    rp = api.RunPod(sys.argv[3])
    player = Player(case["reply"])
    rp.s.mount("https://", player)
    name, *args = case["call"]
    try:
        result = {"ok": outcome(name, getattr(rp, name)(*args))}
    except api.RunPodError as e:
        result = {"error": str(e), "status_code": e.status_code, "transient": e.transient}
    cases.append({**case, "sent": player.sent, **result})
cases.append({"name": "proxy_url", "call": ["proxy_url", "pod111", "8000"],
              "ok": api.proxy_url("pod111", "8000")})
print(json.dumps(cases))
"""


def rp_python():
    """The command line of the interpreter the installed `rp` runs with
    (pipx's venv): its requests, not the system's, decides the headers rp
    sends."""
    rp = shutil.which("rp")
    if not rp:
        raise SystemExit("pod/api needs the installed rp (its venv's requests)")
    first = pathlib.Path(rp).read_text().splitlines()[0]
    if not first.startswith("#!"):
        raise SystemExit(f"{rp} has no #! line")
    return shlex.split(first[2:])


def gen_pod_api(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([*rp_python(), "-I", "-c", API_RUNNER, str(snap),
                              json.dumps(API_CASES), API_KEY],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl api.RunPod and api.proxy_url: each call with the reply its one "
                 "HTTP request gets (status, body chunks, then EOF or a read timeout; or "
                 "a refused connection), what requests would send (method, URL, headers, "
                 "body, timeout) and the outcome: the value returned (cuda_stock as "
                 "[dc, gpu, fit, old] rows) or RunPodError's text, status_code and "
                 f"transient. Played through a transport adapter, run by the installed "
                 f"rp's interpreter; the key is {API_KEY!r}",
        "cases": json.loads(out.stdout),
    }


# ---------------------------------------------------------------- pod/resolve

# One case: pod.json before (text, or none), config keys over DEFAULTS, the
# call (`resolve_pod`, or `base_url` with `discover`) and the HTTP answers its
# requests get, in order. Out: the requests sent ("METHOD URL"), what rpctl
# printed (`_say`), the pod or URL returned or RunPodError's text, pod.json
# after, and tunnel_only and transport_label read after the call. Played
# through rpctl's own api.RunPod with a transport adapter, the state
# directory in a temporary HOME. Ids and bodies are synthetic; transient
# errors are HTTP statuses, whose text viy shares (pod/api has the rest).
OUR_NAME = "qwen-pod"
RUNNING = {"id": "pod111", "name": OUR_NAME, "desiredStatus": "RUNNING", "costPerHr": 1.89,
           "networkVolumeId": "vol222", "lastStartedAt": "2026-07-15T09:00:00Z"}


def _pod(**kw):
    return {**RUNNING, **kw}


def _ok(body):
    return dict(body=_json(body))


def _state(**kw):
    return json.dumps(kw, indent=2) + "\n"


GONE = dict(status=404, body=_json({"error": "pod not found", "status": 404}))
DOWN = dict(status=503, body="upstream unavailable", content_type="text/plain")
CACHED = _state(pod_id="pod111", name=OUR_NAME, cost_per_hr=1.89, volume_id="vol222",
                created_at=1761640000.5, tunnel_only=True, last_startup_seconds=88.4)
OTHER = {"id": "pod999", "name": "someone-else", "desiredStatus": "RUNNING"}

RESOLVE_CASES = [
    # No cached id: RunPod is searched by POD_NAME.
    dict(name="no_cache_no_pods", call="resolve_pod", replies=[_ok([])]),
    dict(name="no_cache_adopts_by_name", call="resolve_pod",
         replies=[_ok([OTHER, _pod(id="pod333")])]),
    dict(name="no_cache_name_matches_exactly", call="resolve_pod",
         replies=[_ok([_pod(name="Qwen-Pod"), _pod(name="qwen-pod "), _pod(name=None),
                       {"id": "pod444", "desiredStatus": "RUNNING"}])]),
    dict(name="no_cache_terminated_is_not_adopted", call="resolve_pod",
         replies=[_ok([_pod(desiredStatus="TERMINATED")])]),
    dict(name="no_cache_stopped_pod_is_adopted", call="resolve_pod",
         replies=[_ok([_pod(desiredStatus="EXITED")])]),
    dict(name="no_cache_list_unreachable", call="resolve_pod", replies=[DOWN]),
    dict(name="empty_pod_id_is_no_cache", call="resolve_pod", state=_state(pod_id=""),
         replies=[_ok([_pod()])]),
    dict(name="unreadable_state_is_no_cache", call="resolve_pod", state='{"pod_id": ',
         replies=[_ok([_pod()])]),
    # Several pods with our name: RUNNING first, then the latest start
    # (lastStartedAt, else createdAt), ties in RunPod's order.
    dict(name="several_prefers_running_then_latest", call="resolve_pod", replies=[_ok([
        _pod(id="podA", desiredStatus="EXITED", lastStartedAt="2026-07-15T12:00:00Z"),
        _pod(id="podB", lastStartedAt="2026-07-14T08:00:00Z"),
        _pod(id="podC", lastStartedAt="2026-07-15T08:00:00Z"),
        _pod(id="podD", desiredStatus="TERMINATED", lastStartedAt="2026-07-16T00:00:00Z")])]),
    dict(name="several_stopped_falls_back_to_created_at", call="resolve_pod", replies=[_ok([
        _pod(id="podA", desiredStatus="EXITED", lastStartedAt=None,
             createdAt="2026-07-07T00:00:00Z"),
        _pod(id="podB", desiredStatus="EXITED", lastStartedAt="",
             createdAt="2026-07-08T00:00:00Z"),
        _pod(id="podC", desiredStatus="EXITED", lastStartedAt="2026-07-06T00:00:00Z")])]),
    dict(name="several_ties_keep_runpods_order", call="resolve_pod", replies=[_ok([
        {"id": "podA", "name": OUR_NAME, "desiredStatus": "CREATED"},
        {"id": "podB", "name": OUR_NAME, "desiredStatus": "RUNNING"},
        {"id": "podC", "name": OUR_NAME, "desiredStatus": "CREATED"},
        {"id": "podD", "name": OUR_NAME, "desiredStatus": "RUNNING"}])]),
    # What is cached: the volume from networkVolumeId, else networkVolume.id.
    dict(name="volume_from_network_volume_object", call="resolve_pod", replies=[_ok([
        _pod(networkVolumeId=None, networkVolume={"id": "vol555", "name": "w"})])]),
    dict(name="volume_empty_id_falls_back", call="resolve_pod", replies=[_ok([
        _pod(networkVolumeId="", networkVolume={"id": "vol555"})])]),
    dict(name="volume_and_cost_absent_are_null", call="resolve_pod", replies=[_ok([
        {"id": "pod111", "name": OUR_NAME, "desiredStatus": "RUNNING"}])]),
    # A cached id: one GET while that pod lives; the other keys stay.
    dict(name="cached_live_pod_one_get", call="resolve_pod", state=CACHED,
         replies=[_ok(_pod(costPerHr=2.49))]),
    dict(name="cached_pod_renamed_is_still_ours", call="resolve_pod", state=CACHED,
         replies=[_ok(_pod(name="renamed-in-the-console"))]),
    dict(name="cached_stopped_pod_is_still_ours", call="resolve_pod", state=CACHED,
         replies=[_ok(_pod(desiredStatus="EXITED"))]),
    dict(name="cached_pod_without_name_or_volume", call="resolve_pod", state=CACHED,
         replies=[_ok({"id": "pod111", "desiredStatus": "RUNNING"})]),
    dict(name="cached_get_unreachable_keeps_the_hint", call="resolve_pod", state=CACHED,
         replies=[DOWN]),
    dict(name="cached_get_refused_keeps_the_hint", call="resolve_pod", state=CACHED,
         replies=[dict(status=401, body=_json({"error": "unauthorized"}))]),
    # A cached id RunPod no longer has: search by name, adopt or clear.
    dict(name="cached_gone_adopts_by_name", call="resolve_pod", state=CACHED,
         replies=[GONE, _ok([_pod(id="pod333", costPerHr=0.79, networkVolumeId="vol666")])]),
    dict(name="cached_gone_adopts_keeping_the_tunnel_keys", call="resolve_pod",
         state=_state(pod_id="pod111", created_at=1761640000.5, tunnel_pid=4242,
                      tunnel_only=False, tunnel_local_port="18001", last_startup_seconds=9.0,
                      note="kept"),
         replies=[GONE, _ok([_pod(id="pod333")])]),
    dict(name="no_cache_adopting_drops_creation_keys", call="resolve_pod",
         state=_state(tunnel_only=True, created_at=1761640000.5, tunnel_pid=None),
         replies=[_ok([_pod()])]),
    dict(name="cached_terminated_adopts_by_name", call="resolve_pod", state=CACHED,
         replies=[_ok(_pod(desiredStatus="TERMINATED")), _ok([_pod(id="pod333")])]),
    dict(name="cached_gone_nothing_by_name_clears", call="resolve_pod", state=CACHED,
         replies=[GONE, _ok([OTHER])]),
    dict(name="cached_terminated_nothing_by_name_clears", call="resolve_pod", state=CACHED,
         replies=[_ok(_pod(desiredStatus="TERMINATED")),
                  _ok([_pod(desiredStatus="TERMINATED")])]),
    dict(name="cached_gone_list_unreachable_keeps_the_hint", call="resolve_pod", state=CACHED,
         replies=[GONE, DOWN]),
    dict(name="cached_terminated_list_unreachable_keeps_the_hint", call="resolve_pod",
         state=CACHED, replies=[_ok(_pod(desiredStatus="TERMINATED")), DOWN]),
    dict(name="cached_gone_list_refused_keeps_the_hint", call="resolve_pod", state=CACHED,
         replies=[GONE, dict(status=403, body=_json({"error": "forbidden"}))]),
    dict(name="cached_gone_several_by_name", call="resolve_pod", state=CACHED,
         replies=[GONE, _ok([_pod(id="podB", desiredStatus="EXITED"), _pod(id="podC")])]),
    # base_url: the cached id with no request; otherwise discovery when allowed.
    dict(name="base_url_cached_tunnel_only", call="base_url", discover=True, state=CACHED),
    dict(name="base_url_cached_proxy", call="base_url", discover=True,
         state=_state(pod_id="pod111", tunnel_only=False)),
    dict(name="base_url_cached_predates_tunnel_mode_tunnel_on", call="base_url",
         discover=True, state=_state(pod_id="pod111"), config={"TUNNEL_LOCAL_PORT": "18001"}),
    dict(name="base_url_cached_predates_tunnel_mode_tunnel_off", call="base_url",
         discover=True, state=_state(pod_id="pod111"), config={"TUNNEL": "0"}),
    dict(name="base_url_cached_tunnel_false_word", call="base_url", discover=True,
         state=_state(pod_id="pod111"), config={"TUNNEL": " no "}),
    dict(name="base_url_cached_tunnel_only_not_a_bool", call="base_url", discover=True,
         state=_state(pod_id="pod111", tunnel_only=0), config={"VLLM_PORT": "8100"}),
    dict(name="base_url_no_cache_no_key", call="base_url", discover=True),
    dict(name="base_url_no_cache_no_discovery", call="base_url", discover=False,
         config={"RUNPOD_API_KEY": API_KEY}),
    dict(name="base_url_discovers_by_name", call="base_url", discover=True,
         config={"RUNPOD_API_KEY": API_KEY, "TUNNEL": "0"}, replies=[_ok([_pod()])]),
    dict(name="base_url_discovers_nothing", call="base_url", discover=True,
         config={"RUNPOD_API_KEY": API_KEY}, replies=[_ok([])]),
    dict(name="base_url_discovery_unreachable_is_none", call="base_url", discover=True,
         config={"RUNPOD_API_KEY": API_KEY}, replies=[DOWN]),
]

RESOLVE_RUNNER = r"""
import json, os, pathlib, shutil, sys
sys.path.insert(0, sys.argv[1])
import requests
from requests.adapters import BaseAdapter
from requests.structures import CaseInsensitiveDict
from rpctl import api, config, pod

# The in-repo fallback is a file from before `rp migrate` (viy does not read it).
config.LEGACY_STATE = pathlib.Path(os.environ["HOME"]) / "no-legacy-pod-state.json"
said = []
pod._say = lambda msg, color="": said.append(msg)

class Player(BaseAdapter):
    def __init__(self, replies, sent):
        super().__init__()
        self.replies, self.sent = replies, sent
    def send(self, request, stream=False, timeout=None, **kw):
        self.sent.append(f"{request.method} {request.url}")
        reply = self.replies.pop(0)
        r = requests.Response()
        r.status_code = reply.get("status", 200)
        r.headers = CaseInsensitiveDict(
            {"Content-Type": reply.get("content_type", "application/json")})
        r._content = reply.get("body", "").encode()
        r.url, r.request, r.reason, r.encoding = request.url, request, "synthetic", "utf-8"
        return r

cases = []
for case in json.loads(sys.argv[2]):
    shutil.rmtree(config.USER_STATE_DIR, ignore_errors=True)
    if case.get("state") is not None:
        config.USER_STATE_DIR.mkdir(parents=True)
        config.STATE_PATH.write_text(case["state"])
    sent, replies = [], list(case.get("replies", []))
    said.clear()
    class Client(api.RunPod):
        def __init__(self, key):
            super().__init__(key)
            self.s.mount("https://", Player(replies, sent))
    pod.RunPod = Client
    cfg = dict(config.DEFAULTS, POD_NAME="qwen-pod", **case.get("config", {}))
    try:
        if case["call"] == "resolve_pod":
            result = {"ok": pod.resolve_pod(Client(sys.argv[3]), cfg)}
        else:
            result = {"ok": pod.base_url(cfg, discover=case["discover"])}
    except api.RunPodError as e:
        result = {"error": str(e), "status_code": e.status_code, "transient": e.transient}
    if replies:
        raise SystemExit(f"{case['name']}: {len(replies)} replies unused")
    after = config.STATE_PATH.read_text() if config.STATE_PATH.exists() else None
    cases.append({**case, "sent": sent, "said": list(said), **result, "state_after": after,
                  "tunnel_only": pod.tunnel_only(cfg),
                  "transport_label": pod.transport_label(cfg)})
print(json.dumps(cases))
"""


def gen_pod_resolve(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([*rp_python(), "-I", "-c", RESOLVE_RUNNER, str(snap),
                              json.dumps(RESOLVE_CASES), API_KEY],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl pod.resolve_pod and pod.base_url: pod.json before (text or null), "
                 f"config keys over DEFAULTS with POD_NAME={OUR_NAME!r}, the HTTP answers "
                 "the call's requests get in order; out: the requests sent, what _say "
                 "printed, the pod or URL returned or RunPodError's text, status_code and "
                 "transient, pod.json after (text or null), and tunnel_only and "
                 "transport_label read after the call. Played through rpctl's api.RunPod "
                 "with a transport adapter, run by the installed rp's interpreter in a "
                 f"temporary HOME; the key is {API_KEY!r}",
        "cases": json.loads(out.stdout),
    }


# ------------------------------------------------------------------- pod/boot

# One case: config keys over rpctl's DEFAULTS, the endpoint key, the network
# volume id and the SSH public key, and what rpctl's boot.py builds from them:
# the vLLM argument list, the boot script and the create-pod body (or the
# exception building the body raised; `bad_key` is the key it is about, which
# Python's message does not name). With `host`, only the argument list,
# as the local backend asks for it. The scripts embed startup.py verbatim;
# to keep the file small, the one copy in each script is written as
# STARTUP_MARKER and the text itself is the case called "startup.py", and the
# body's dockerStartCmd, the script once more, as BOOT_SCRIPT_MARKER. The
# generator checks that both stand for exactly that text. Values are
# synthetic; the "live" case has the keys rpctl kb f019-f022 found in
# Gregory's config, with those public numbers.
STARTUP_MARKER = "<startup.py>"
BOOT_SCRIPT_MARKER = "<boot_script>"
BOOT_KEY = "sk-SYNTHETICENDPOINT"
BOOT_VOLUME = "vol222"
BOOT_PUBKEY = "ssh-ed25519 AAAAC3NzaSYNTHETIC test@viy-fixture"

BOOT_CASES = [
    dict(name="defaults"),
    dict(name="live_profile", cfg=dict(MAX_MODEL_LEN="262144", MAX_NUM_SEQS="4",
                                       GPU_MEMORY_UTILIZATION="0.94")),
    dict(name="fp8_kv", cfg=dict(KV_CACHE_DTYPE="fp8")),
    dict(name="mtp", cfg=dict(MTP_SPECULATIVE_TOKENS="3")),
    dict(name="tunnel_off", cfg=dict(TUNNEL="0")),
    dict(name="tunnel_no_padded", cfg=dict(TUNNEL=" no ")),
    dict(name="tunnel_yes", cfg=dict(TUNNEL="yes")),
    dict(name="prewarm_off", cfg=dict(WEIGHTS_PREWARM="0")),
    dict(name="caches_off_mm_warmup_on", cfg=dict(STARTUP_CACHE="0", MM_WARMUP="1",
                                                  MODEL_CACHE_MODE="online")),
    dict(name="everything_on", cfg=dict(
        KV_CACHE_DTYPE="fp8", MTP_SPECULATIVE_TOKENS="3", TRUST_REMOTE_CODE="1",
        SAFETENSORS_LOAD_STRATEGY="eager", HF_TOKEN="hf_synthetic",
        RUNPOD_WATCHDOG_API_KEY="rpa_watchdogsynthetic", GPU_COUNT="2",
        CUDA_VERSIONS=" 12.8, 12.9 ,,", GPU_TYPE_IDS=" NVIDIA A100 80GB PCIe , ,NVIDIA H100 NVL",
        DATA_CENTER_ID="US-KS-2", CLOUD_TYPE="COMMUNITY", GPU_TYPE_PRIORITY="availability")),
    dict(name="tools_and_search_off", cfg=dict(SEARCH_ENABLED="0", TOOLS_ENABLED="0")),
    dict(name="search_only", cfg=dict(TOOLS_ENABLED="0")),
    dict(name="tools_only", cfg=dict(SEARCH_ENABLED="0")),
    dict(name="no_tool_parser", cfg=dict(TOOL_CALL_PARSER="")),
    dict(name="enabled_is_only_1", cfg=dict(SEARCH_ENABLED="true", TOOLS_ENABLED="yes")),
    dict(name="prefix_caching_off", cfg=dict(PREFIX_CACHING="0")),
    dict(name="prefix_caching_empty", cfg=dict(PREFIX_CACHING="")),
    dict(name="optional_strings_empty", cfg=dict(
        SAFETENSORS_LOAD_STRATEGY="", TRUST_REMOTE_CODE="", MODEL_CACHE_MODE="",
        STARTUP_CACHE="", WEIGHTS_PREWARM="", MM_WARMUP="", CUDA_VERSIONS="")),
    dict(name="values_that_need_quoting", cfg=dict(
        MODEL_REPO="o/m it's \"x\" $HOME `id` \\ ;|&", SERVED_MODEL_NAME="",
        VLLM_PORT="80 00", MAX_IMAGES_PER_PROMPT="4,\"audio\":1",
        MAX_MODEL_LEN="65536\nexport X=1", GPU_MEMORY_UTILIZATION="0.9'",
        TOOL_CALL_PARSER="qwen3_xml --x", MODEL_CACHE_MODE="a b"),
        key="k'e\"y $x"),
    dict(name="safe_characters_unquoted", cfg=dict(
        MODEL_REPO="Org_1/Model-2.5+fp8@main%x=y:z,w", SERVED_MODEL_NAME="a.b-c_d")),
    dict(name="non_ascii_is_quoted", cfg=dict(SERVED_MODEL_NAME="модель", MODEL_REPO="o/é")),
    dict(name="no_public_key", pubkey=None),
    dict(name="empty_public_key", pubkey=""),
    dict(name="numbers_in_python_forms", cfg=dict(
        IDLE_TTL_MIN=" 30 ", MAX_POD_HOURS="1.5", GPU_COUNT=" 2 ", CONTAINER_DISK_GB="+80")),
    dict(name="idle_off", cfg=dict(IDLE_TTL_MIN="0", MAX_POD_HOURS="")),
    dict(name="idle_fractions", cfg=dict(IDLE_TTL_MIN="0.01", MAX_POD_HOURS="0.0001")),
    dict(name="idle_negative_zero", cfg=dict(IDLE_TTL_MIN="-0", MAX_POD_HOURS="-0.0001")),
    dict(name="idle_negative", cfg=dict(IDLE_TTL_MIN="-2.5")),
    dict(name="idle_exponent", cfg=dict(IDLE_TTL_MIN="1e1", MAX_POD_HOURS="1E30")),
    dict(name="gpu_count_not_a_number", cfg=dict(GPU_COUNT="two"), bad_key="GPU_COUNT"),
    dict(name="disk_not_whole", cfg=dict(CONTAINER_DISK_GB="60.5"), bad_key="CONTAINER_DISK_GB"),
    dict(name="idle_not_a_number", cfg=dict(IDLE_TTL_MIN="soon"), bad_key="IDLE_TTL_MIN"),
    dict(name="idle_blank", cfg=dict(IDLE_TTL_MIN="  "), bad_key="IDLE_TTL_MIN"),
    dict(name="idle_infinite", cfg=dict(IDLE_TTL_MIN="inf"), bad_key="IDLE_TTL_MIN"),
    dict(name="max_hours_nan", cfg=dict(MAX_POD_HOURS="nan"), bad_key="MAX_POD_HOURS"),
    dict(name="local_backend_host", cfg=dict(TUNNEL="0"), host="127.0.0.1"),
    dict(name="local_backend_host_tunnel_on", host="127.0.0.1"),
]

BOOT_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import boot, config
STARTUP, SCRIPT = sys.argv[3], sys.argv[4]
runtime = (boot.Path(boot.__file__).with_name("startup.py")).read_text()

def elide(script):
    assert script.count(runtime) == 1 and STARTUP not in script and SCRIPT not in script
    return script.replace(runtime, STARTUP)

cases = [{"name": "startup.py", "text": runtime}]
for case in json.loads(sys.argv[2]):
    cfg = dict(config.DEFAULTS, **case["cfg"])
    out = dict(case)
    if case["host"] is not None:
        out["vllm_args"] = boot.build_vllm_args(cfg, case["key"], host=case["host"])
        cases.append(out)
        continue
    out["vllm_args"] = boot.build_vllm_args(cfg, case["key"])
    script = boot.build_boot_script(cfg, case["key"])
    out["boot_script"] = elide(script)
    try:
        body = boot.build_pod_body(cfg, case["key"], case["volume_id"], case["pubkey"])
    except Exception as e:
        out["pod_body_error"] = f"{type(e).__name__}: {e}"
    else:
        assert body["dockerStartCmd"] == [script]
        body["dockerStartCmd"] = [SCRIPT]
        out["pod_body"] = body
    cases.append(out)
print(json.dumps(cases))
"""


def gen_pod_boot(snap):
    cases = [{"name": c["name"], "cfg": c.get("cfg", {}), "key": c.get("key", BOOT_KEY),
              "volume_id": BOOT_VOLUME, "pubkey": c.get("pubkey", BOOT_PUBKEY),
              "host": c.get("host"), **({"bad_key": c["bad_key"]} if "bad_key" in c else {})}
             for c in BOOT_CASES]
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([sys.executable, "-I", "-c", BOOT_RUNNER, str(snap),
                              json.dumps(cases), STARTUP_MARKER, BOOT_SCRIPT_MARKER],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl boot.build_vllm_args, build_boot_script and build_pod_body for "
                 "config keys over DEFAULTS (cfg), the endpoint key, the volume id and "
                 "the SSH public key: the argument list, the script and the body, or the "
                 "exception the body raised. In each script the embedded startup.py is "
                 f"written {STARTUP_MARKER!r} (its text is the case 'startup.py'), and the "
                 f"body's dockerStartCmd [script] as [{BOOT_SCRIPT_MARKER!r}]. Cases with a "
                 "host have the argument list only, as the local backend builds it",
        "cases": json.loads(out.stdout),
    }


# -------------------------------------------------------------------- pod/ssh

# One case: one of rpctl's ssh or tunnel functions (`call`, with `args`),
# pod.json, known_hosts and tunnel.log before (text, or none), config keys
# over DEFAULTS, the answers its commands get in order (`runs`: stdout,
# stderr, returncode, or `raise` "timeout" or "missing"), the HTTP answers
# its RunPod requests get in order, whether the tunnel's local end accepts
# (`listening`: one answer, or one per check), and `live`, whether pid 4242
# is a live ssh. `popen` is what the tunnel's ssh does: exit with `exits`
# (its stderr first), or keep running. Out: the commands run (argv, stdin,
# timeout), the requests sent, what rpctl printed with its colour, the
# result or the SystemExit, and the files after. The state directory is
# written <STATE>. Played through rpctl's own functions with subprocess,
# the clock, signals and the local port check replaced; keys, fingerprints
# and addresses are synthetic, the ports high and made up.
SSH_FP = "SHA256:Synthetic0000HostKeyFingerprintForTheTests0000"
SSH_FP_OTHER = "SHA256:Synthetic9999SomeoneElsesKeyFingerprint99999"
SSH_KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAASyntheticHostKeyForViyFixtures"
SSH_LOG_LINE = f"sshd host key: 256 {SSH_FP} root@f7fa (ED25519)"
SSH_POD = {"id": "pod111", "name": OUR_NAME, "desiredStatus": "RUNNING",
           "publicIp": "203.0.113.7", "portMappings": {"22": 40122}}
SSH_STATE = _state(pod_id="pod111", name=OUR_NAME, volume_id="vol222", tunnel_only=True)
SSH_CFG = {"NETWORK_VOLUME_ID": "vol222", "TUNNEL_LOCAL_PORT": "18001", "VLLM_PORT": "8000"}
KEYGEN_FOUND = dict(stdout=f"# Host rpctl-qwen-pod found: line 1 \n"
                           f"rpctl-qwen-pod ED25519 {SSH_FP}\n")
KEYGEN_NOT_FOUND = dict(returncode=1)
KEYSCAN = dict(stdout=f"# 203.0.113.7:40122 SSH-2.0-OpenSSH_8.9p1\n"
                      f"[203.0.113.7]:40122 {SSH_KEY}\n")
KEYGEN_LF = dict(stdout=f"256 {SSH_FP} x (ED25519)\n")
SSH_OK = dict()
KNOWN = f"rpctl-qwen-pod {SSH_KEY}\n"


def _logs(*lines):
    return dict(body="".join(f"data: {json.dumps({'line': l})}\n" for l in lines),
                content_type="text/event-stream")


SSH_CASES = [
    dict(name="alias_without_state", call="alias"),
    dict(name="alias_primary_volume", call="alias", state=SSH_STATE),
    dict(name="alias_backup_volume", call="alias", state=_state(volume_id="vol999")),
    dict(name="alias_no_primary_configured", call="alias", state=_state(volume_id="vol999"),
         config={"NETWORK_VOLUME_ID": ""}),
    dict(name="alias_empty_volume", call="alias", state=_state(volume_id="")),
    dict(name="ssh_opts_primary", call="ssh_opts", state=SSH_STATE),
    dict(name="ssh_opts_backup_volume", call="ssh_opts", state=_state(volume_id="vol999")),
    dict(name="tunnel_cmd_default_ports", call="tunnel_cmd", config={"TUNNEL_LOCAL_PORT": ""},
         args=["203.0.113.7", 40122]),
    dict(name="tunnel_cmd_own_ports", call="tunnel_cmd", config={"VLLM_PORT": "8100"},
         args=["203.0.113.7", 40122]),
    dict(name="endpoint_mapped", call="ssh_endpoint", args=[SSH_POD]),
    dict(name="endpoint_no_ip", call="ssh_endpoint", args=[{**SSH_POD, "publicIp": None}]),
    dict(name="endpoint_empty_ip", call="ssh_endpoint", args=[{**SSH_POD, "publicIp": ""}]),
    dict(name="endpoint_no_mappings", call="ssh_endpoint",
         args=[{**SSH_POD, "portMappings": None}]),
    dict(name="endpoint_other_ports_only", call="ssh_endpoint",
         args=[{**SSH_POD, "portMappings": {"8000": 40800}}]),
    dict(name="endpoint_port_zero", call="ssh_endpoint",
         args=[{**SSH_POD, "portMappings": {"22": 0}}]),
    dict(name="endpoint_port_as_text", call="ssh_endpoint",
         args=[{**SSH_POD, "portMappings": {"22": "40122"}}]),
    dict(name="endpoint_empty_pod", call="ssh_endpoint", args=[{}]),
    # The pinned key.
    dict(name="fingerprint_no_known_hosts", call="pinned_fingerprint", state=SSH_STATE),
    dict(name="fingerprint_pinned", call="pinned_fingerprint", state=SSH_STATE,
         known_hosts=KNOWN, runs=[KEYGEN_FOUND]),
    dict(name="fingerprint_not_pinned", call="pinned_fingerprint", state=SSH_STATE,
         known_hosts="rpctl-other " + SSH_KEY + "\n", runs=[KEYGEN_NOT_FOUND]),
    dict(name="forget_nothing_pinned", call="forget_host_key", state=SSH_STATE),
    dict(name="forget_pinned", call="forget_host_key", state=SSH_STATE, known_hosts=KNOWN,
         runs=[KEYGEN_FOUND, dict(stdout="# Host rpctl-qwen-pod found: line 1\n")]),
    dict(name="forget_backup_alias", call="forget_host_key", state=_state(volume_id="vol999"),
         known_hosts=KNOWN, runs=[KEYGEN_NOT_FOUND]),
    dict(name="scan_finds_the_key", call="scan_host_key", args=["203.0.113.7", 40122],
         runs=[KEYSCAN, KEYGEN_LF]),
    dict(name="scan_spaces_in_the_line", call="scan_host_key", args=["203.0.113.7", 40122],
         runs=[dict(stdout=f"  [203.0.113.7]:40122  {SSH_KEY}\t \n"), KEYGEN_LF]),
    dict(name="scan_times_out", call="scan_host_key", args=["203.0.113.7", 40122],
         runs=[dict(raise_="timeout")]),
    dict(name="scan_without_keyscan", call="scan_host_key", args=["203.0.113.7", 40122],
         runs=[dict(raise_="missing")]),
    dict(name="scan_no_ed25519_key", call="scan_host_key", args=["203.0.113.7", 40122],
         runs=[dict(stdout="# 203.0.113.7:40122 SSH-2.0-OpenSSH_8.9p1\n", returncode=1)]),
    dict(name="scan_keygen_prints_no_fingerprint", call="scan_host_key",
         args=["203.0.113.7", 40122], runs=[KEYSCAN, dict(stderr="(stdin) is not a key file.\n",
                                                          returncode=255)]),
    # First contact: pinned only when the boot log agrees.
    dict(name="first_contact_without_pod_id", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=_state(volume_id="vol222")),
    dict(name="first_contact_matching_key_is_pinned", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts="rpctl-other x\n",
         replies=[_logs("boot", SSH_LOG_LINE)], runs=[KEYSCAN, KEYGEN_LF]),
    dict(name="first_contact_backup_alias", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=_state(pod_id="pod111", volume_id="vol999"),
         replies=[_logs(SSH_LOG_LINE)], runs=[KEYSCAN, KEYGEN_LF]),
    dict(name="first_contact_mismatch_pins_nothing", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=SSH_STATE,
         replies=[_logs(SSH_LOG_LINE.replace(SSH_FP, SSH_FP_OTHER),
                        "sshd host key: 256 SHA256:Zz9 root@x (ED25519)")],
         runs=[KEYSCAN, KEYGEN_LF]),
    dict(name="first_contact_log_without_fingerprint", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=SSH_STATE,
         replies=[_logs("boot", f"sshd host key: 256 {SSH_FP} root@f7fa (RSA)")]),
    dict(name="first_contact_logs_unavailable", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=SSH_STATE, replies=[DOWN]),
    dict(name="first_contact_scan_finds_nothing", call="pin_checked_key",
         args=["203.0.113.7", 40122], state=SSH_STATE, replies=[_logs(SSH_LOG_LINE)],
         runs=[dict(raise_="timeout")]),
    # The preflight ssh.
    dict(name="preflight_pinned_and_in", call="preflight_ssh", args=["203.0.113.7", 40122],
         state=SSH_STATE, known_hosts=KNOWN, runs=[KEYGEN_FOUND, SSH_OK]),
    dict(name="preflight_host_key_changed", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts=KNOWN,
         runs=[KEYGEN_FOUND, dict(returncode=255, stderr=(
             "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n"
             "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n"
             "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n"
             "Host key verification failed.\n"))]),
    dict(name="preflight_host_key_failed_lowercase", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts=KNOWN,
         runs=[KEYGEN_FOUND, dict(returncode=255, stderr="No ED25519 HOST KEY is known\n")]),
    dict(name="preflight_permission_denied", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts=KNOWN,
         runs=[KEYGEN_FOUND, dict(returncode=255,
                                  stderr="root@203.0.113.7: Permission denied (publickey).\n")]),
    dict(name="preflight_refused_quietly", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts=KNOWN,
         runs=[KEYGEN_FOUND, dict(returncode=255, stderr="  \n")]),
    dict(name="preflight_first_contact_checked", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts="rpctl-other x\n",
         replies=[_logs(SSH_LOG_LINE)],
         runs=[KEYGEN_NOT_FOUND, KEYSCAN, KEYGEN_LF, SSH_OK, KEYGEN_FOUND]),
    dict(name="preflight_first_contact_unchecked", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, known_hosts="rpctl-other x\n",
         replies=[_logs("no fingerprint here")],
         runs=[KEYGEN_NOT_FOUND, SSH_OK, KEYGEN_FOUND]),
    dict(name="preflight_first_contact_nothing_pinned_after", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, replies=[_logs()], runs=[SSH_OK]),
    dict(name="preflight_first_contact_mismatch", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE,
         replies=[_logs(SSH_LOG_LINE.replace(SSH_FP, SSH_FP_OTHER))],
         runs=[KEYSCAN, KEYGEN_LF]),
    dict(name="preflight_first_contact_then_refused", call="preflight_ssh",
         args=["203.0.113.7", 40122], state=SSH_STATE, replies=[_logs(SSH_LOG_LINE)],
         runs=[KEYSCAN, KEYGEN_LF,
               dict(returncode=255, stderr="ssh: connect to host 203.0.113.7 port 40122: "
                                           "Connection refused\n")]),
    # tunnel.log's tail.
    dict(name="log_tail_no_file", call="tunnel_log_tail"),
    dict(name="log_tail_last_tunnel_only", call="tunnel_log_tail", tunnel_log=(
        "=== 2026-07-15 09:00:00 ssh tunnel to 203.0.113.7:40122 ===\nold one\n"
        "=== 2026-07-16 09:00:00 ssh tunnel to 203.0.113.7:40122 ===\n"
        "a\n\n  \nb\r\nc\rd\ne\nf\ng\n")),
    dict(name="log_tail_without_header", call="tunnel_log_tail",
         tunnel_log="first line dropped\nsecond\nthird"),
    dict(name="log_tail_header_only", call="tunnel_log_tail",
         tunnel_log="=== 2026-07-16 09:00:00 ssh tunnel to 203.0.113.7:40122 ==="),
    # `rp tunnel --status`.
    dict(name="status_down_not_pinned", call="tunnel_status", state=SSH_STATE,
         listening=False),
    dict(name="status_running_pinned", call="tunnel_status",
         state=_state(pod_id="pod111", volume_id="vol222", tunnel_pid=4242), live=True,
         known_hosts=KNOWN, runs=[KEYGEN_FOUND], listening=True),
    dict(name="status_someone_elses_listener", call="tunnel_status",
         state=_state(pod_id="pod111", tunnel_pid=99999999), listening=True),
    # Warnings before a chat.
    dict(name="warnings_proxy_pod", call="preflight",
         state=_state(pod_id="pod111", tunnel_only=False)),
    dict(name="warnings_tunnel_up", call="preflight",
         state=_state(pod_id="pod111", tunnel_only=True, tunnel_pid=4242), live=True,
         listening=False),
    dict(name="warnings_no_tunnel", call="preflight", state=SSH_STATE, listening=False),
    dict(name="warnings_untracked_listener", call="preflight", state=SSH_STATE,
         listening=True),
    dict(name="warnings_predates_tunnel_mode", call="preflight", state=_state(pod_id="pod111"),
         listening=False),
    dict(name="warnings_no_pod_tunnel_off", call="preflight", config={"TUNNEL": "0"}),
    # Starting and stopping the background tunnel.
    dict(name="start_already_running", call="start_tunnel", args=["203.0.113.7", 40122],
         state=_state(pod_id="pod111", tunnel_pid=4242), live=True),
    dict(name="start_port_taken", call="start_tunnel", args=["203.0.113.7", 40122],
         state=SSH_STATE, listening=True),
    dict(name="start_preflight_fails", call="start_tunnel", args=["203.0.113.7", 40122],
         state=SSH_STATE, known_hosts=KNOWN, listening=False,
         runs=[KEYGEN_FOUND, dict(returncode=255, stderr="Connection timed out during "
                                                         "banner exchange\n")]),
    dict(name="start_exits_at_once", call="start_tunnel", args=["203.0.113.7", 40122],
         state=SSH_STATE, known_hosts=KNOWN, listening=False, runs=[KEYGEN_FOUND, SSH_OK],
         popen=dict(exits=255, stderr="bind [127.0.0.1]:18001: Address already in use\n"
                                      "channel_setup_fwd_listener_tcpip: cannot listen to "
                                      "port: 18001\nCould not request local forwarding.\n")),
    dict(name="start_listens", call="start_tunnel", args=["203.0.113.7", 40122],
         state=SSH_STATE, known_hosts=KNOWN, listening=[False, False, True],
         runs=[KEYGEN_FOUND, SSH_OK], popen=dict()),
    dict(name="start_never_listens", call="start_tunnel", args=["203.0.113.7", 40122],
         state=SSH_STATE, known_hosts=KNOWN, listening=False,
         runs=[KEYGEN_FOUND, SSH_OK], popen=dict()),
    dict(name="stop_nothing_recorded", call="stop_tunnel", state=SSH_STATE),
    dict(name="stop_dead_pid", call="stop_tunnel",
         state=_state(pod_id="pod111", tunnel_pid=99999999, tunnel_local_port="18001")),
    dict(name="stop_junk_pid", call="stop_tunnel", state=_state(tunnel_pid="not a pid")),
    dict(name="stop_live", call="stop_tunnel",
         state=_state(pod_id="pod111", tunnel_pid=4242, tunnel_local_port="18001"), live=True),
    # The pod to tunnel to.
    dict(name="target_runpod_unreachable", call="resolve_ssh_target", replies=[DOWN]),
    dict(name="target_no_pod", call="resolve_ssh_target", replies=[_ok([])]),
    dict(name="target_no_mapping_yet", call="resolve_ssh_target", state=SSH_STATE,
         replies=[_ok({**SSH_POD, "portMappings": {}}), _ok({**SSH_POD, "portMappings": {}})]),
    dict(name="target_from_the_detail", call="resolve_ssh_target",
         replies=[_ok([{k: v for k, v in SSH_POD.items() if k != "portMappings"}]),
                  _ok(SSH_POD)]),
    dict(name="target_detail_gone_keeps_the_list_pod", call="resolve_ssh_target",
         replies=[_ok([SSH_POD]), GONE]),
    dict(name="target_several_pods", call="resolve_ssh_target",
         replies=[_ok([SSH_POD, {**SSH_POD, "id": "pod333", "desiredStatus": "EXITED"}]),
                  _ok(SSH_POD)]),
]

SSH_RUNNER = r"""
import json, os, pathlib, shutil, signal, subprocess, sys
sys.path.insert(0, sys.argv[1])
import requests
from requests.adapters import BaseAdapter
from requests.structures import CaseInsensitiveDict
from rpctl import api, config, pod

config.LEGACY_STATE = pathlib.Path(os.environ["HOME"]) / "no-legacy-pod-state.json"
STATE = str(config.USER_STATE_DIR)
COLORS = {pod.C_DIM: "dim", pod.C_OK: "ok", pod.C_WARN: "warn", pod.C_ERR: "error", "": "plain"}
said, ran, sent, signals = [], [], [], []
pod._say = lambda msg, color="": said.append([COLORS[color], msg])
pod.print = lambda *a, **kw: said.append(["plain", " ".join(map(str, a))]) if a else None
pod.time.sleep = lambda s: None

def scrub(value):
    if isinstance(value, str):
        return value.replace(STATE, "<STATE>")
    if isinstance(value, (list, tuple)):
        return [scrub(v) for v in value]
    if isinstance(value, dict):
        return {k: scrub(v) for k, v in value.items()}
    return value

class Player(BaseAdapter):
    def __init__(self, replies):
        super().__init__()
        self.replies = replies
    def send(self, request, stream=False, timeout=None, **kw):
        sent.append(f"{request.method} {request.url}")
        reply = self.replies.pop(0)
        r = requests.Response()
        r.status_code = reply.get("status", 200)
        r.headers = CaseInsensitiveDict(
            {"Content-Type": reply.get("content_type", "application/json")})
        r._content = reply.get("body", "").encode()
        r._content_consumed = True
        r.url, r.request, r.reason, r.encoding = request.url, request, "synthetic", "utf-8"
        return r

for case in json.loads(sys.argv[2]):
    shutil.rmtree(config.USER_STATE_DIR, ignore_errors=True)
    config.USER_STATE_DIR.mkdir(parents=True)
    for key, path in (("state", config.STATE_PATH), ("known_hosts", pod.KNOWN_HOSTS),
                      ("tunnel_log", pod.TUNNEL_LOG)):
        if case.get(key) is not None:
            path.write_text(case[key])
    said.clear(); ran.clear(); sent.clear(); signals.clear()
    runs = list(case.get("runs", []))
    replies = list(case.get("replies", []))
    listening = case.get("listening", False)
    answers = list(listening) if isinstance(listening, list) else None
    live = {4242} if case.get("live") else set()

    def run(argv, input=None, timeout=None, **kw):
        ran.append({"argv": scrub(list(argv)), "input": input, "timeout": timeout})
        reply = runs.pop(0)
        if reply.get("raise_") == "timeout":
            raise subprocess.TimeoutExpired(argv, timeout)
        if reply.get("raise_") == "missing":
            raise FileNotFoundError(2, "No such file or directory", argv[0])
        return subprocess.CompletedProcess(argv, reply.get("returncode", 0),
                                           reply.get("stdout", ""), reply.get("stderr", ""))
    pod.subprocess.run = run

    class Popen:
        pid = 4242
        def __init__(self, argv, stdout=None, stderr=None, start_new_session=False):
            ran.append({"argv": scrub(list(argv)), "input": None, "timeout": None,
                        "detached": start_new_session})
            self.returncode = case["popen"].get("exits")
            stderr.write(case["popen"].get("stderr", ""))
            if self.returncode is None:
                live.add(4242)
        def poll(self):
            return self.returncode
        def terminate(self):
            signals.append(["terminate", self.pid])
            live.discard(self.pid)
    pod.subprocess.Popen = Popen
    pod._tunnel_proc = None

    def tcp_open(host, port, timeout=5):
        if host != "127.0.0.1":
            raise SystemExit(f"{case['name']}: a probe of {host}")
        return answers.pop(0) if answers is not None else listening
    pod._tcp_open = tcp_open
    pod._pid_alive = lambda pid: pid in live
    pod._cmdline = lambda pid: [b"/usr/bin/ssh", b"-N"] if int(pid) in live else []
    def kill(pid, sig):
        signals.append(["kill", pid, int(sig)])
        live.discard(pid)
    pod.os.kill = kill

    class Client(api.RunPod):
        def __init__(self, key="rpa_SYNTHETICKEY0000"):
            super().__init__(key)
            self.s.mount("https://", Player(replies))
    pod.RunPod = Client
    cfg = dict(config.DEFAULTS, POD_NAME="qwen-pod", RUNPOD_API_KEY="rpa_SYNTHETICKEY0000",
               **{**json.loads(sys.argv[3]), **case.get("config", {})})
    args = case.get("args", [])
    call = case["call"]
    try:
        if call == "alias":
            result = pod.host_key_alias(cfg)
        elif call == "ssh_opts":
            result = pod.ssh_opts(cfg)
        elif call == "tunnel_cmd":
            result = pod._tunnel_cmd(cfg, *args)
        elif call == "ssh_endpoint":
            ip, port = pod.ssh_endpoint(*args)
            result = [ip, str(port)] if ip else None
        elif call == "pinned_fingerprint":
            result = pod.pinned_fingerprint(cfg)
        elif call == "forget_host_key":
            result = pod.forget_host_key(cfg)
        elif call == "scan_host_key":
            fp, key = pod.scan_host_key(*args)
            result = [fp, key] if fp else None
        elif call == "pin_checked_key":
            result = pod._pin_checked_key(cfg, *args)
        elif call == "preflight_ssh":
            result = pod._preflight_ssh(cfg, *args)
        elif call == "tunnel_log_tail":
            result = pod.tunnel_log_tail()
        elif call == "tunnel_status":
            result = pod.tunnel_status(cfg)
        elif call == "preflight":
            result = pod.preflight(cfg)
        elif call == "start_tunnel":
            result = pod.start_tunnel(cfg, *args, quiet=False)
        elif call == "stop_tunnel":
            result = pod.stop_tunnel(cfg, quiet=False)
        elif call == "resolve_ssh_target":
            _, pod_id, ip, port = pod._resolve_ssh_target(cfg)
            result = [pod_id, ip, port]
        out = {"ok": scrub(result)}
    except SystemExit as e:
        out = {"exit": e.code}
    for left, what in ((runs, "runs"), (replies, "replies"), (answers or [], "listening")):
        if left:
            raise SystemExit(f"{case['name']}: {len(left)} {what} unused")
    after = {key: path.read_text() if path.exists() else None
             for key, path in (("state", config.STATE_PATH), ("known_hosts", pod.KNOWN_HOSTS))}
    print(json.dumps({**case, "ran": list(ran), "sent": list(sent), "said": scrub(said),
                      "signals": list(signals), **out, **{k + "_after": v
                                                          for k, v in after.items()}}))
"""


def gen_pod_ssh(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([*rp_python(), "-I", "-c", SSH_RUNNER, str(snap),
                              json.dumps(SSH_CASES), json.dumps(SSH_CFG)],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl's ssh and tunnel functions in pod.py: per case the call and its args, "
                 "pod.json, known_hosts and tunnel.log before, config keys over DEFAULTS "
                 f"and {SSH_CFG} with POD_NAME={OUR_NAME!r}, the answers its commands "
                 "(`runs`) and RunPod requests (`replies`) get in order, whether the local "
                 "end accepts (`listening`), whether pid 4242 is a live ssh (`live`) and what "
                 "the tunnel's ssh does (`popen`); out: the commands run, the requests sent, "
                 "what rpctl printed with its colour, the signals sent, the result or the "
                 "SystemExit, and pod.json and known_hosts after. The state directory is "
                 "<STATE>. Run by the installed rp's interpreter in a temporary HOME",
        "cases": [json.loads(line) for line in out.stdout.splitlines()],
    }


# ------------------------------------------------------------ pod/bootreport

# One case: one of rpctl's boot-report or wait functions on synthetic log
# lines. `measure`: bootreport.measure(latest_boot(entries)) between two
# parse_ts times; `facts`: bootreport.facts; `render`: bootreport.render;
# `redact`: pod._redact_log with config keys over DEFAULTS; `progress`:
# pod._startup_progress, one fetch of the log per list in `fetches`, the
# summary, pod_ready and the StartupError after each; `watchdog`:
# pod._report_watchdog, what it said with its colour.
def _entries(*rows):
    return [dict(zip(("ts", "line", "source"), row)) if isinstance(row, tuple)
            else {"line": row} for row in rows]


BOOT_LOG = _entries(
    ("2026-06-25T23:25:00Z", "old boot", "system"),
    ("2026-06-25T23:35:49.5Z", "=== boot Thu ===", "container"),
    ("2026-06-25T23:35:50.7Z", "RPCTL_PHASE starting vLLM", "container"),
    ("2026-06-25T23:35:51Z", "RPCTL_PHASE weights prewarmed: 51.75 GiB in 80.0s", "container"),
    ("2026-06-25T23:36:10Z", "API non-default args: {...}", "container"),
    ("2026-06-25T23:36:40Z", "ENG world_size=1 rank=0", "container"),
    ("2026-06-25T23:36:47Z", "Starting to load model x", "container"),
    ("2026-06-25T23:38:19Z", "Loading weights took 90.87 seconds", "container"),
    ("2026-06-25T23:38:20Z", "Model loading took 51.1 GiB", "container"),
    ("2026-06-25T23:39:35Z", "torch.compile took 71.80 s in total", "container"),
    ("2026-06-25T23:39:43Z", "init engine (profile, ...) took 82.42 s", "container"),
    ("2026-06-25T23:40:02Z", "API server: HTTP server started", "container"),
)
CREATED, READY = "2026-06-25T23:23:03Z", "2026-06-25T23:40:05Z"
STAGE_ROWS = [["pull", "GPU allocation + image pull", 150.0],
              ["weights", "weights to GPU", 60.0]]
CANNOT = "RPCTL_PHASE WATCHDOG CANNOT STOP THIS POD: RunPod refused its key (HTTP 403)"
LONG_PHASE = "RPCTL_PHASE " + "restoring caches " * 15

BOOTREPORT_CASES = [
    {"name": "measure_rpctl_test", "call": "measure", "entries": BOOT_LOG,
     "created": CREATED, "ready": READY},
    {"name": "measure_no_boot_marker", "call": "measure", "entries": BOOT_LOG[2:],
     "created": CREATED, "ready": READY},
    {"name": "measure_marker_before_creation", "call": "measure", "entries": BOOT_LOG,
     "created": "2026-06-25T23:36:00Z", "ready": READY},
    {"name": "measure_two_boots", "call": "measure",
     "entries": BOOT_LOG + _entries(("2026-06-25T23:50:00Z", "=== boot Fri ===", "container"),
                                    ("2026-06-25T23:51:00.25Z", "RPCTL_PHASE starting vLLM",
                                     "container")),
     "created": CREATED, "ready": "2026-06-25T23:52:00Z"},
    {"name": "measure_untimed_and_odd_timestamps", "call": "measure",
     "entries": _entries("=== boot Sat ===",
                         ("yesterday", "RPCTL_PHASE starting vLLM", "container"),
                         ("2026-06-25T23:36:10.123456Z", "API non-default args", "container"),
                         ("2026-06-25T23:36:40.5", "ENG world_size=1", "container"),
                         ("", "Starting to load model", "container")),
     "created": CREATED, "ready": READY},
    {"name": "measure_last_detail_wins_and_gpu", "call": "measure",
     "entries": _entries(("2026-06-25T23:30:00Z", "Loading weights took 12.5 seconds", "container"),
                         ("2026-06-25T23:31:00Z", "Loading weights took 30.25 seconds", "container"),
                         ("2026-06-25T23:31:01Z", "RPCTL_PHASE gpu  NVIDIA A100-SXM4-80GB  ",
                          "container")),
     "created": CREATED, "ready": READY},
    {"name": "measure_ready_before_the_last_marker", "call": "measure", "entries": BOOT_LOG,
     "created": CREATED, "ready": "2026-06-25T23:39:00Z"},
    {"name": "measure_empty_log", "call": "measure", "entries": [],
     "created": CREATED, "ready": READY},

    {"name": "facts_rpctl_test", "call": "facts", "entries": _entries(
        "9a54fb40f516 Already exists", "0f06d63f7039 Pull complete",
        "0f06d63f7039 Retrying in 1 second", "Status: Downloaded newer image for x",
        "RPCTL_PHASE verified model cache; using offline metadata",
        "RPCTL_PHASE cache restore unavailable; compiling normally (FilterError)",
        "Compiling a graph for compile range (1, 2048) takes 52.85 s")},
    {"name": "facts_download_then_up_to_date", "call": "facts", "entries": _entries(
        "Status: Downloaded newer image for x", "Status: Image is up to date for x")},
    {"name": "facts_up_to_date_only", "call": "facts", "entries": _entries(
        "Status: Image is up to date for x")},
    {"name": "facts_layers_already_on_host", "call": "facts", "entries": _entries(
        "a Already exists", "b Already exists")},
    {"name": "facts_retries_only", "call": "facts", "entries": _entries(
        "a Retrying in 1 second", "a Retrying in 1 second")},
    {"name": "facts_caches_restored_and_hit", "call": "facts", "entries": _entries(
        "RPCTL_PHASE checking model cache with Hugging Face",
        "RPCTL_PHASE compilation caches restored (3 files)",
        "Directly load AOT compilation from x", "RPCTL_PHASE watchdog armed")},
    {"name": "facts_first_compilation", "call": "facts", "entries": _entries(
        "RPCTL_PHASE first compilation on this GPU", "RPCTL_PHASE watchdog off")},
    {"name": "facts_restore_failed_without_reason", "call": "facts", "entries": _entries(
        "RPCTL_PHASE cache restore unavailable")},
    {"name": "facts_watchdog_cannot_stop", "call": "facts", "entries": _entries(
        "RPCTL_PHASE watchdog armed", CANNOT + " " + "x" * 200)},
    {"name": "facts_watchdog_key_refused", "call": "facts", "entries": _entries(CANNOT)},
    {"name": "facts_error_line", "call": "facts", "entries": _entries(
        "RPCTL_ERROR   sshd setup or daemon failed (exit 23) " + "y" * 250)},
    {"name": "facts_nothing", "call": "facts", "entries": _entries("hello")},

    {"name": "render_over_budget", "call": "render", "stages": STAGE_ROWS, "details": {},
     "total": 210.0, "outcome": "ready"},
    {"name": "render_within_budget", "call": "render", "stages": STAGE_ROWS[1:],
     "details": {}, "total": 150.0, "outcome": "ready"},
    {"name": "render_exactly_the_budget", "call": "render", "stages": STAGE_ROWS[1:],
     "details": {}, "total": 180.0, "outcome": "ready"},
    {"name": "render_details", "call": "render",
     "stages": [["weights", "weights to GPU", 93.2], ["compile", "encoder profiling + compile", 75.0],
                ["endpoint", "SSH tunnel / local endpoint", 3.0]],
     "details": {"weights_read_s": 90.87, "prewarm_s": 80.0, "compile_s": 71.8},
     "total": 171.2, "outcome": "ready"},
    {"name": "render_read_without_prewarm", "call": "render",
     "stages": [["weights", "weights to GPU", 20.0]], "details": {"weights_read_s": 15.0},
     "total": 20.0, "outcome": "ready"},
    {"name": "render_failed", "call": "render", "stages": STAGE_ROWS, "details": {},
     "total": 400.0, "outcome": "failed"},
    {"name": "render_interrupted", "call": "render", "stages": STAGE_ROWS[1:], "details": {},
     "total": 61.0, "outcome": "interrupted"},
    {"name": "render_tie_names_the_first", "call": "render",
     "stages": [["imports", "vLLM CLI imports", 100.0], ["api", "API server warmup", 100.0],
                ["endpoint", "SSH tunnel / local endpoint", 0.4]],
     "details": {}, "total": 200.4, "outcome": "ready"},
    {"name": "render_halves", "call": "render",
     "stages": [["pull", "GPU allocation + image pull", 0.5],
                ["endpoint", "SSH tunnel / local endpoint", 2.5]],
     "details": {}, "total": 3.0, "outcome": "ready"},

    {"name": "redact_rpctl_test", "call": "redact",
     "line": "key=sk-secret123 token=hf_abc123 plain=opaque-secret",
     "config": {"RUNPOD_API_KEY": "opaque-secret"}},
    {"name": "redact_endpoint_key_in_arguments", "call": "redact",
     "line": "non-default args: {'api_key': ['sk-abcdefghijklmnop123456']}",
     "config": {"VLLM_API_KEY": "sk-abcdefghijklmnop123456"}},
    {"name": "redact_watchdog_key", "call": "redact",
     "line": "env RPCTL_WATCHDOG_KEY=rpa_SECRETVALUE1234567890",
     "config": {"RUNPOD_WATCHDOG_API_KEY": "rpa_SECRETVALUE1234567890"}},
    {"name": "redact_short_values_stay", "call": "redact",
     "line": "user=bob pass=hunter2 tok=1234567", "config": {"HF_TOKEN": "1234567"}},
    {"name": "redact_token_and_password_keys", "call": "redact",
     "line": "a=TOKENVALUE99 b=pa55word-long c=/workspace/models",
     "config": {"HF_TOKEN": "TOKENVALUE99", "PROXY_PASSWORD": "pa55word-long",
                "HF_HOME": "/workspace/models"}},
    {"name": "redact_key_shapes_need_a_word_boundary", "call": "redact",
     "line": "xsk-notakey (sk-yes) hf_ rpa_x-y_z mask-ok", "config": {}},

    {"name": "progress_model_ready", "call": "progress", "require_ssh": True,
     "fetches": [_entries("RPCTL_PHASE model ready; saving startup caches")]},
    {"name": "progress_a_later_boot_clears_ready", "call": "progress", "require_ssh": True,
     "fetches": [_entries("RPCTL_PHASE model ready; saving startup caches",
                          "=== boot Sat Jun 20 ===")]},
    {"name": "progress_ssh_error_ends_the_wait", "call": "progress", "require_ssh": True,
     "fetches": [_entries("RPCTL_PHASE starting sshd", "RPCTL_ERROR sshd setup failed",
                          "RPCTL_PHASE starting vLLM"), _entries("RPCTL_PHASE later")]},
    {"name": "progress_ssh_error_without_tunnel", "call": "progress", "require_ssh": False,
     "fetches": [_entries("RPCTL_PHASE starting sshd", "RPCTL_ERROR sshd setup failed")]},
    {"name": "progress_pull_then_retry", "call": "progress", "require_ssh": True,
     "fetches": [[{"line": "abc Downloading [=>   ]", "source": "system"}],
                 [{"line": "abc Retrying in 3 seconds", "source": "system"}],
                 [{"line": "abc Extracting", "source": "system"},
                  {"line": "abc Downloading", "source": "container"}]]},
    {"name": "progress_stages_of_vllm", "call": "progress", "require_ssh": True,
     "fetches": [[{"line": "Loading safetensors checkpoint shards", "source": "container"}],
                 [{"line": "Compiling a graph", "source": "container"},
                  {"line": "Compiling a graph", "source": "system"}],
                 [{"line": "Capturing CUDA graphs (mixed prefill-decode)"}],
                 [{"line": "Doing WarmUp for the sampler"}],
                 [{"line": "Application startup complete."}],
                 [{"line": "(APIServer) HTTP server started on 0.0.0.0:8000"}]]},
    {"name": "progress_long_phase_is_cut", "call": "progress", "require_ssh": True,
     "fetches": [_entries(LONG_PHASE)]},
    {"name": "progress_empty_log_keeps_the_summary", "call": "progress", "require_ssh": True,
     "fetches": [_entries("RPCTL_PHASE restoring caches"), []]},

    {"name": "watchdog_armed", "call": "watchdog", "state": "armed", "config": {}},
    {"name": "watchdog_armed_other_limits", "call": "watchdog", "state": "armed",
     "config": {"IDLE_TTL_MIN": "30", "MAX_POD_HOURS": "2.5"}},
    {"name": "watchdog_cannot_stop", "call": "watchdog",
     "state": "CANNOT STOP THIS POD: RunPod refused its key (HTTP 403)", "config": {}},
    {"name": "watchdog_off", "call": "watchdog", "state": "off", "config": {}},
    {"name": "watchdog_no_report", "call": "watchdog", "state": None, "config": {}},
    {"name": "watchdog_unknown_state", "call": "watchdog", "state": "dozing", "config": {}},
]

BOOTREPORT_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import bootreport, config, pod

COLORS = {pod.C_DIM: "dim", pod.C_OK: "ok", pod.C_WARN: "warn", pod.C_ERR: "error", "": "plain"}
said = []
pod._say = lambda msg, color="": said.append([COLORS[color], msg])
clock = [1000.0]
def monotonic():
    clock[0] += 100
    return clock[0]
pod.time.monotonic = monotonic

class Logs:
    def __init__(self, fetches):
        self.fetches = list(fetches)
    def pod_logs(self, pod_id, tail=100, budget_s=8):
        assert (pod_id, tail) == ("pod111", 30), (pod_id, tail)
        return self.fetches.pop(0)

for case in json.loads(sys.argv[2]):
    call, out = case["call"], {}
    if call == "measure":
        stages, details = bootreport.measure(
            bootreport.latest_boot(case["entries"]), bootreport.parse_ts(case["created"]),
            bootreport.parse_ts(case["ready"]))
        out = {"stages": [list(s) for s in stages], "details": details}
    elif call == "facts":
        out = {"facts": bootreport.facts(case["entries"])}
    elif call == "render":
        text, over = bootreport.render([tuple(s) for s in case["stages"]], case["details"],
                                       case["total"], outcome=case["outcome"])
        out = {"text": text, "over": over}
    elif call == "redact":
        out = {"redacted": pod._redact_log(case["line"], dict(config.DEFAULTS, **case["config"]))}
    elif call == "progress":
        api = Logs(case["fetches"])
        progress = pod._startup_progress(api, "pod111", require_ssh=case["require_ssh"],
                                         background=False)
        after = []
        for _ in case["fetches"]:
            try:
                after.append({"summary": progress(), "ready": progress.pod_ready()})
            except pod.StartupError as error:
                after.append({"error": str(error), "ready": progress.pod_ready()})
        out = {"after": after}
    elif call == "watchdog":
        said.clear()
        pod._report_watchdog(dict(config.DEFAULTS, **case["config"]), case["state"])
        out = {"said": list(said)}
    print(json.dumps({**case, **out}))
"""


def gen_pod_bootreport(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp, "XDG_STATE_HOME": tmp, "XDG_DATA_HOME": tmp}
        out = subprocess.run([sys.executable, "-I", "-c", BOOTREPORT_RUNNER, str(snap),
                              json.dumps(BOOTREPORT_CASES)],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl's boot report and wait summaries on synthetic platform-log lines, by "
                 "`call`: measure (bootreport.measure of latest_boot(entries) between "
                 "parse_ts(created) and parse_ts(ready): stages as [key, label, seconds] and "
                 "details), facts (bootreport.facts), render (bootreport.render with the "
                 "default budget: text and over), redact (pod._redact_log, config keys over "
                 "DEFAULTS), progress (pod._startup_progress for require_ssh, the log fetched "
                 "once per list in fetches: after each the summary or the StartupError, and "
                 "pod_ready), watchdog (pod._report_watchdog with config keys over DEFAULTS: "
                 "what it said with its colour)",
        "cases": [json.loads(line) for line in out.stdout.splitlines()],
    }


# ---------------------------------------------------------------- pod/failover

# One case: rpctl's `pod._create_pod` (the primary request, the retry
# without the exact CUDA version, and regions.failover with its questions
# and retries), `regions.retire_replaced`, `regions.expire`,
# `regions.cmd_backup`, `regions.rank` or `regions.is_capacity_error`,
# against a scripted RunPod (`world`) behind a transport adapter. The
# world: `free` regions take a pod; `later` maps a region to the refusals
# it gives before it takes one; `newer` regions take one only without
# allowedCudaVersions; `refuse` maps a region to a non-capacity refusal;
# `volumes`, `pods`, `stock` (dataCenters) and `cuda` ({region: [fit,
# old]}) are what RunPod lists; `*_error` make a call fail. `answers` are
# the user's, in order (null: no terminal); `interrupt` raises
# KeyboardInterrupt at the nth retry pause (`sleep`) or while deleting a
# volume (`delete`). Ids, keys and regions are synthetic.
FO_NOW = 1784194200.0          # 2026-07-16T09:30:00Z
FO_DAY = 86400
FULL = "create pod: There are no instances currently available"
A100 = "NVIDIA A100-SXM4-80GB"
PRIM = {"id": "prim", "name": "w", "size": 100, "dataCenterId": "EUR-IS-1"}
BK = {"id": "bk", "name": "w-backup", "size": 100, "dataCenterId": "US-MO-1"}
FO_CONFIG = {"POD_NAME": "qwen-pod", "NETWORK_VOLUME_NAME": "w", "NETWORK_VOLUME_ID": "prim",
             "FAILOVER_RETRY_MIN": "1", "CUDA_VERSIONS": "", "RUNPOD_API_KEY": API_KEY,
             "VLLM_API_KEY": "sk-SYNTHETICENDPOINT0000"}


def _dc(region, free, storage=True, listed=True):
    return {"id": region, "location": "synthetic", "listed": listed, "storageSupport": storage,
            "gpuAvailability": [{"gpuTypeId": A100, "stockStatus": "Low" if free else None,
                                 "available": free}]}


def _bk(last_used, **kw):
    return {"name": "w-backup", "region": "US-MO-1", "last_used": last_used, **kw}


TWO = [_dc("CA-MTL-3", True), _dc("EU-RO-1", True)]
FAILOVER_CASES = [
    # The primary request, and the retry without the exact CUDA version.
    dict(name="success_is_one_create_request", world=dict(free=["EUR-IS-1"])),
    dict(name="failover_off_exits_after_the_primary", config={"FAILOVER": "0"}, world={}),
    dict(name="failover_off_word", config={"FAILOVER": " no "}, world={}),
    dict(name="non_capacity_refusal_names_the_usual_causes",
         world=dict(refuse={"EUR-IS-1": "Insufficient balance"})),
    dict(name="cuda_success_path_asks_no_stock", config={"CUDA_VERSIONS": "13.0"},
         world=dict(free=["EUR-IS-1"])),
    dict(name="cuda_newer_drivers_only_retried_without_the_version",
         config={"CUDA_VERSIONS": "13.0"},
         world=dict(newer=["EUR-IS-1"], cuda={"EUR-IS-1": ["Low", None]})),
    dict(name="cuda_older_driver_free_keeps_the_filter", config={"CUDA_VERSIONS": "13.0"},
         world=dict(newer=["EUR-IS-1"], cuda={"EUR-IS-1": ["Low", "Low"]}, volumes=[PRIM]),
         answers=None),
    dict(name="cuda_floor_is_the_oldest_listed", config={"CUDA_VERSIONS": "12.9, 13.0,12.8",
                                                         "FAILOVER": "0"},
         world=dict(newer=["EUR-IS-1"], cuda={"EUR-IS-1": ["Low", None]})),
    dict(name="cuda_stock_unreadable_keeps_the_filter",
         config={"CUDA_VERSIONS": "13.0", "FAILOVER": "0"},
         world=dict(newer=["EUR-IS-1"], cuda_error=500)),
    dict(name="cuda_retry_refused_too", config={"CUDA_VERSIONS": "13.0", "FAILOVER": "0"},
         world=dict(cuda={"EUR-IS-1": ["High", None]})),
    dict(name="community_cloud_stock", config={"CUDA_VERSIONS": "13.0", "CLOUD_TYPE": "community",
                                               "FAILOVER": "0"},
         world=dict(newer=["EUR-IS-1"], cuda={"EUR-IS-1": ["Low", None]})),
    # The backup volume.
    dict(name="backup_asked_and_taken", world=dict(free=["US-MO-1"], volumes=[PRIM, BK]),
         answers=[True]),
    dict(name="no_to_the_backup_creates_nothing", world=dict(free=["US-MO-1"],
                                                             volumes=[PRIM, BK]),
         answers=[False]),
    dict(name="unrecorded_backup_starts_its_countdown", world=dict(free=["US-MO-1"],
                                                                   volumes=[PRIM, BK]),
         answers=[True]),
    dict(name="recorded_backup_is_touched", backups={"bk": _bk(FO_NOW - 5 * FO_DAY)},
         world=dict(free=["US-MO-1"], volumes=[PRIM, BK]), answers=[True]),
    dict(name="two_backups_the_last_used_wins",
         backups={"bk": _bk(FO_NOW - 9 * FO_DAY), "bk2": _bk(FO_NOW - FO_DAY, region="EU-RO-1")},
         world=dict(free=["EU-RO-1"], volumes=[PRIM, BK, dict(BK, id="bk2",
                                                               dataCenterId="EU-RO-1")]),
         answers=[True]),
    dict(name="unused_volume_is_no_backup",
         backups={"new9": _bk(FO_NOW - 60, region="CA-MTL-3", unused=True)},
         world=dict(volumes=[PRIM, dict(BK, id="new9", dataCenterId="CA-MTL-3")],
                    stock=[_dc("CA-MTL-3", False)]),
         answers=None),
    dict(name="backup_refuses_for_another_reason",
         world=dict(volumes=[PRIM, BK], refuse={"US-MO-1": "Insufficient balance"}),
         answers=[True]),
    # Moving the backup.
    dict(name="full_backup_then_the_switch_is_asked_and_done",
         world=dict(free=["CA-MTL-3"], volumes=[PRIM, BK],
                    stock=[_dc("US-MO-1", False), _dc("CA-MTL-3", True),
                           _dc("US-KS-2", True, storage=False), _dc("EU-SE-1", True, listed=False)]),
         answers=[True, True]),
    dict(name="no_to_the_switch_keeps_the_backup",
         world=dict(free=["CA-MTL-3"], volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)]),
         answers=[True, False]),
    dict(name="no_backup_yet_asks_to_make_one",
         world=dict(free=["CA-MTL-3"], volumes=[PRIM],
                    stock=[_dc("EUR-IS-1", True), _dc("CA-MTL-3", True)]),
         answers=[True]),
    dict(name="no_backup_yet_idle_days_in_the_question",
         config={"BACKUP_IDLE_DAYS": "7.6", "FAILOVER_RETRY_MIN": "0.5"},
         world=dict(free=["CA-MTL-3"], volumes=[PRIM], stock=[_dc("CA-MTL-3", True)]),
         answers=[True]),
    dict(name="refused_candidate_loses_its_volume_the_next_is_tried",
         world=dict(free=["EU-RO-1"], volumes=[PRIM, BK], stock=TWO), answers=[True, True]),
    dict(name="refusals_retried_until_one_takes_the_pod",
         world=dict(volumes=[PRIM, BK], later={"EU-RO-1": 3}, stock=TWO), answers=[True, True]),
    dict(name="primary_freeing_up_during_the_retries_wins",
         world=dict(volumes=[PRIM, BK], later={"EUR-IS-1": 2}, stock=[_dc("CA-MTL-3", True)]),
         answers=[True, True]),
    dict(name="old_backup_freeing_up_during_the_retries_wins",
         world=dict(volumes=[PRIM, BK], later={"US-MO-1": 2}, stock=[_dc("CA-MTL-3", True)]),
         answers=[True, True]),
    dict(name="retries_running_out_delete_the_new_volumes",
         world=dict(volumes=[PRIM, BK], stock=TWO), answers=[True, True]),
    dict(name="long_retries_say_how_long_is_left", config={"FAILOVER_RETRY_MIN": "2"},
         world=dict(volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)]), answers=[True, True]),
    dict(name="zero_retry_minutes_tries_each_region_once", config={"FAILOVER_RETRY_MIN": "0"},
         world=dict(volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)]), answers=[True, True]),
    dict(name="retry_minutes_not_a_number_is_zero", config={"FAILOVER_RETRY_MIN": "soon"},
         world=dict(volumes=[PRIM], stock=[_dc("CA-MTL-3", True)]), answers=[True]),
    dict(name="nowhere_free_keeps_the_backup_without_asking",
         world=dict(volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", False)]), answers=[True]),
    dict(name="nowhere_free_without_a_backup", world=dict(volumes=[PRIM], stock=[]),
         answers=None),
    dict(name="stock_unreadable", world=dict(volumes=[PRIM, BK], stock_error=500),
         answers=[True]),
    dict(name="stock_counts_hosts_at_or_above_the_cuda_floor",
         config={"CUDA_VERSIONS": "13.0"},
         world=dict(free=["CA-MTL-3"], volumes=[PRIM, BK],
                    stock=[_dc("US-MO-1", True), _dc("CA-MTL-3", False)],
                    cuda={"CA-MTL-3": ["Low", None]}),
         answers=[True, True]),
    dict(name="stock_history_breaks_ties", world=dict(
        free=["EU-RO-1"], volumes=[PRIM], stock=TWO), answers=[True],
         stock_rows=[{"at": "2026-07-07T00:00:00+00:00", "a100_free": {"EU-RO-1": True}},
                     {"at": "2026-05-19T00:00:00+00:00", "a100_free": {"CA-MTL-3": True}}]),
    dict(name="new_volume_creation_refused",
         world=dict(volumes=[PRIM, BK], stock=TWO, volume_create_error=402),
         answers=[True, True]),
    dict(name="unused_volume_delete_refused", world=dict(
        volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)], delete_error={"new1": 500}),
         answers=[True, True], config={"FAILOVER_RETRY_MIN": "0"}),
    dict(name="unused_volume_already_gone", world=dict(
        volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)], delete_error={"new1": 404}),
         answers=[True, True], config={"FAILOVER_RETRY_MIN": "0"}),
    dict(name="ctrl_c_during_the_retries_deletes_the_new_volumes",
         world=dict(volumes=[PRIM, BK], stock=TWO), answers=[True, True],
         interrupt={"sleep": 1}),
    dict(name="second_ctrl_c_during_cleanup_still_deletes_the_rest",
         world=dict(volumes=[PRIM, BK], stock=TWO), answers=[True, True],
         interrupt={"sleep": 2, "delete": ["new1"]}),
    dict(name="without_a_terminal_the_backup_is_tried_nothing_moved",
         world=dict(free=["CA-MTL-3"], volumes=[PRIM, BK], stock=[_dc("CA-MTL-3", True)]),
         answers=None),
    dict(name="without_a_terminal_no_backup_is_made",
         world=dict(free=["CA-MTL-3"], volumes=[PRIM], stock=[_dc("CA-MTL-3", True)]),
         answers=None),
    dict(name="fresh_primary_volume_stays_fresh_on_the_primary",
         world=dict(volumes=[PRIM, BK], later={"EUR-IS-1": 2}, stock=[_dc("CA-MTL-3", True)]),
         answers=[True, True], volume_is_new=True),
    dict(name="secrets_masked_in_the_refusal_log",
         world=dict(refuse={"EUR-IS-1": "echo sk-SYNTHETICENDPOINT0000 rpa_SYNTHETICKEY0000"})),
    # After readiness: the backup a new one replaced.
    dict(name="retire_nothing_pending", call="retire_replaced", volume="bk",
         backups={"bk": _bk(FO_NOW)}, world=dict(volumes=[PRIM, BK])),
    dict(name="retire_unknown_volume", call="retire_replaced", volume="prim",
         world=dict(volumes=[PRIM, BK])),
    dict(name="retire_deletes_the_old_backup", call="retire_replaced", volume="new1",
         backups={"bk": _bk(FO_NOW - FO_DAY),
                  "new1": _bk(FO_NOW, region="CA-MTL-3", replaces="bk")},
         world=dict(volumes=[PRIM, BK, dict(BK, id="new1", dataCenterId="CA-MTL-3")])),
    dict(name="retire_old_backup_already_gone", call="retire_replaced", volume="new1",
         backups={"bk": _bk(FO_NOW - FO_DAY),
                  "new1": _bk(FO_NOW, region="CA-MTL-3", replaces="bk")},
         world=dict(volumes=[PRIM])),
    dict(name="retire_never_deletes_a_volume_not_named_as_the_backup", call="retire_replaced",
         volume="new1", backups={"new1": _bk(FO_NOW, region="CA-MTL-3", replaces="prim")},
         world=dict(volumes=[PRIM])),
    dict(name="retire_check_fails", call="retire_replaced", volume="new1",
         backups={"new1": _bk(FO_NOW, region="CA-MTL-3", replaces="bk")},
         world=dict(volumes=[PRIM, BK], get_error={"bk": 503})),
    dict(name="retire_delete_fails", call="retire_replaced", volume="new1",
         backups={"new1": _bk(FO_NOW, region="CA-MTL-3", replaces="bk")},
         world=dict(volumes=[PRIM, BK], delete_error={"bk": 500})),
    dict(name="retire_delete_already_gone", call="retire_replaced", volume="new1",
         backups={"new1": _bk(FO_NOW, region="CA-MTL-3", replaces="bk")},
         world=dict(volumes=[PRIM, BK], delete_error={"bk": 404})),
    # rp reap's expiry of backups.
    dict(name="expire_not_due_costs_no_call", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - FO_DAY)}, world=dict(volumes=[BK])),
    dict(name="expire_idle_backup_is_deleted", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)}, world=dict(volumes=[PRIM, BK])),
    dict(name="expire_attached_backup_is_kept_its_clock_restarts", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)},
         world=dict(volumes=[BK], pods=[{"id": "p1", "networkVolumeId": "bk",
                                         "desiredStatus": "RUNNING"}])),
    dict(name="expire_attached_through_the_volume_object", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)},
         world=dict(volumes=[BK], pods=[{"id": "p1", "networkVolume": {"id": "bk"},
                                         "desiredStatus": "EXITED"}])),
    dict(name="expire_terminated_pod_attaches_nothing", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)},
         world=dict(volumes=[BK], pods=[{"id": "p1", "networkVolumeId": "bk",
                                         "desiredStatus": "TERMINATED"}])),
    dict(name="expire_never_deletes_a_volume_not_named_as_a_backup", call="expire",
         now=FO_NOW, backups={"prim": {"name": "w", "region": "EUR-IS-1",
                                       "last_used": FO_NOW - 99 * FO_DAY}},
         world=dict(volumes=[PRIM])),
    dict(name="expire_renamed_volume_is_forgotten", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY, name="old-backup")},
         world=dict(volumes=[BK])),
    dict(name="expire_zero_days_keeps_backups", call="expire", now=FO_NOW,
         config={"BACKUP_IDLE_DAYS": "0"}, backups={"bk": _bk(1000.0)},
         world=dict(volumes=[BK])),
    dict(name="expire_unused_within_the_grace_is_kept", call="expire", now=FO_NOW,
         backups={"new1": _bk(FO_NOW - 60, unused=True)},
         world=dict(volumes=[PRIM, dict(BK, id="new1")])),
    dict(name="expire_unused_after_the_grace_is_deleted", call="expire", now=FO_NOW,
         config={"BACKUP_IDLE_DAYS": "0"}, backups={"new1": _bk(FO_NOW - 3600, unused=True)},
         world=dict(volumes=[PRIM, dict(BK, id="new1")])),
    dict(name="expire_gone_volume_is_forgotten", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)}, world=dict(volumes=[])),
    dict(name="expire_check_fails_keeps_the_record", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)},
         world=dict(volumes=[BK], get_error={"bk": 500})),
    dict(name="expire_delete_fails", call="expire", now=FO_NOW,
         backups={"bk": _bk(FO_NOW - 31 * FO_DAY)},
         world=dict(volumes=[BK], delete_error={"bk": 500})),
    dict(name="expire_record_without_last_use", call="expire", now=FO_NOW,
         backups={"bk": {"name": "w-backup"}}, world=dict(volumes=[BK])),
    # rp backup.
    dict(name="backup_command_with_a_backup", call="backup", samples=1,
         backups={"bk": _bk(FO_NOW - 3 * FO_DAY)},
         world=dict(volumes=[PRIM, BK], stock=[_dc("EUR-IS-1", False), _dc("US-MO-1", True),
                                               _dc("CA-MTL-3", True), _dc("EU-RO-1", False)]),
         stock_rows=[{"at": "2026-07-15T00:00:00+00:00",
                      "a100_free": {"EUR-IS-1": True, "CA-MTL-3": False}},
                     {"at": "2026-07-14T00:00:00+00:00",
                      "a100_free": {"EUR-IS-1": True, "CA-MTL-3": True}}],
         failure_rows=[{"at": "2026-07-15T10:00:00+00:00", "region": "EUR-IS-1",
                        "capacity": True},
                       {"at": "2026-07-15T11:00:00+00:00", "region": "EUR-IS-1",
                        "capacity": True},
                       {"at": "2026-07-15T12:00:00+00:00", "region": "US-MO-1",
                        "capacity": True},
                       {"at": "2026-07-15T13:00:00+00:00", "region": "EUR-IS-1",
                        "capacity": False},
                       {"at": "2026-05-19T00:00:00+00:00", "region": "EU-RO-1",
                        "capacity": True}]),
    dict(name="backup_command_without_a_backup_failover_off", call="backup", samples=2,
         config={"FAILOVER": "0", "NETWORK_VOLUME_ID": ""},
         world=dict(volumes=[PRIM], stock=[_dc("EUR-IS-1", True), _dc("CA-MTL-3", True)]),
         failure_rows=[{"at": "2026-07-15T10:00:00+00:00", "region": "EUR-IS-1",
                        "capacity": False}]),
    dict(name="backup_command_never_used_backup_zero_days", call="backup", samples=0,
         config={"BACKUP_IDLE_DAYS": "0"}, backups={"bk": {"name": "w-backup"}},
         world=dict(volumes=[PRIM, BK], stock=[_dc("US-MO-1", False)])),
    dict(name="backup_command_stock_unreadable", call="backup", samples=1,
         world=dict(volumes=[PRIM], stock_error=500)),
    # Pure decisions.
    dict(name="rank_a100_now_beats_a_better_past", call="rank",
         samples=[{"A": True, "B": False}, {"A": True, "B": False}], exclude=[],
         past={"A": 0.1, "B": 1.0}),
    dict(name="rank_more_often_free_first_then_the_past", call="rank",
         samples=[{"A": True, "B": True, "C": True, "X": True},
                  {"A": False, "B": True, "C": True}], exclude=["X"],
         past={"C": 0.9, "B": 0.2}),
    dict(name="rank_ties_by_name", call="rank", samples=[{"Z": True, "M": True, "A": False}],
         exclude=[], past={}),
    dict(name="capacity_errors", call="capacity", texts=[
        FULL, "There are no longer any instances available with the requested specifications",
        "Not enough free GPUs", "not enough GPUs", "No GPU available", "no gpus are available",
        "Region OUT OF CAPACITY", "instance currently available? no",
        "Insufficient balance", "HTTP 401 unauthorized", "invalid gpuTypeIds",
        "no instance", "no longer instances", ""]),
]

FAILOVER_RUNNER = r"""
import contextlib, io, json, os, pathlib, re, shutil, sys, types
sys.path.insert(0, sys.argv[1])
import requests
from requests.adapters import BaseAdapter
from requests.structures import CaseInsensitiveDict
from rpctl import api, config, pod, regions

NOW, API_KEY, FULL = float(sys.argv[3]), sys.argv[4], sys.argv[5]
FO_CONFIG = json.loads(sys.argv[6])
HOME = os.environ["HOME"]
clock = [0.0]
# The wall clock moves with the monotonic one, as it does in rp up.
regions.time = types.SimpleNamespace(time=lambda: NOW + clock[0], monotonic=lambda: clock[0],
                                     sleep=lambda s: clock.__setitem__(0, clock[0] + s))
fake = regions.time
d = list(regions.failover.__defaults__)
d[-2], d[-1] = fake.sleep, fake.monotonic
regions.failover.__defaults__ = tuple(d)
d = list(regions.sample_stock.__defaults__)
d[-1] = fake.sleep
regions.sample_stock.__defaults__ = tuple(d)
TONE = {pod.C_DIM: "dim", pod.C_OK: "ok", pod.C_WARN: "warn", pod.C_ERR: "error", "": "plain"}
said, asked, sent, replies, presses = [], [], [], [], []
pod._say = lambda msg, color="": said.append([TONE[color], msg.replace(HOME, "$HOME")])
pod.notify = lambda *a, **k: None
real_ask = pod._ask
STOCK_ALIAS = re.compile(r'(s\d+_\d+)([fo]): gpuTypes\(input: \{id: "([^"]*)"\}\).*?'
                         r'dataCenterId: "([^"]*)"')


class World:
    def __init__(self, spec):
        self.spec = spec
        self.volumes = [dict(v) for v in spec.get("volumes", [])]
        self.later = dict(spec.get("later", {}))
        self.n = 0

    def answer(self, method, url, body):
        s = self.spec
        if url == api.GRAPHQL:
            query = body["query"]
            if "dataCenters" in query:
                if s.get("stock_error"):
                    return s["stock_error"], "upstream failed"
                return 200, {"data": {"dataCenters": s.get("stock", [])}}
            if s.get("cuda_error"):
                return s["cuda_error"], "upstream failed"
            data = {}
            for alias, kind, gpu, region in STOCK_ALIAS.findall(query):
                status = s.get("cuda", {}).get(region, [None, None])[kind == "o"]
                data[alias + kind] = [{"lowestPrice": {"stockStatus": status} if status else None}]
            return 200, {"data": data}
        path = url[len(api.BASE):]
        if path == "/pods" and method == "POST":
            region, vid = body["dataCenterIds"][0], body["networkVolumeId"]
            pod_ = {"id": f"pod-{region}", "costPerHr": 1.89, "networkVolumeId": vid}
            if region in s.get("refuse", {}):
                return 400, {"error": s["refuse"][region]}
            if region in s.get("newer", []):
                if "allowedCudaVersions" in body:
                    return 500, {"error": FULL}
                return 200, pod_
            if region in self.later:
                self.later[region] -= 1
                if self.later[region] < 0:
                    return 200, pod_
            if region not in s.get("free", []):
                return 500, {"error": FULL}
            return 200, pod_
        if path == "/pods":
            return 200, s.get("pods", [])
        if path == "/networkvolumes" and method == "GET":
            return 200, self.volumes
        if path == "/networkvolumes":
            if s.get("volume_create_error"):
                return s["volume_create_error"], {"error": "volume quota reached"}
            self.n += 1
            v = {"id": f"new{self.n}", "name": body["name"], "size": body["size"],
                 "dataCenterId": body["dataCenterId"]}
            self.volumes.append(v)
            return 200, v
        vid = path.rsplit("/", 1)[1]
        if method == "GET":
            if vid in s.get("get_error", {}):
                return s["get_error"][vid], {"error": "synthetic"}
            for v in self.volumes:
                if v["id"] == vid:
                    return 200, v
            return 404, {"error": "network volume not found"}
        if vid in s.get("delete_error", {}):
            return s["delete_error"][vid], {"error": "synthetic"}
        self.volumes = [v for v in self.volumes if v["id"] != vid]
        return 200, ""


class Player(BaseAdapter):
    def __init__(self, world, interrupt):
        super().__init__()
        self.world, self.interrupt = world, interrupt
    def send(self, request, stream=False, timeout=None, **kw):
        body = request.body
        if isinstance(body, bytes):
            body = body.decode()
        body = json.loads(body) if body else None
        shown = body
        if request.url == api.BASE + "/pods" and request.method == "POST":
            shown = {k: body[k] for k in ("name", "dataCenterIds", "networkVolumeId",
                                          "allowedCudaVersions") if k in body}
        sent.append({"method": request.method, "url": request.url, "body": shown})
        if request.method == "DELETE" and request.url.rsplit("/", 1)[1] in \
                self.interrupt.get("delete", []):
            replies.append({"hang": True})
            presses.append(len(sent))
            raise KeyboardInterrupt
        status, payload = self.world.answer(request.method, request.url, body)
        text = payload if isinstance(payload, str) else json.dumps(payload)
        replies.append({"status": status, "body": text})
        r = requests.Response()
        r.status_code = status
        r.headers = CaseInsensitiveDict({"Content-Type": "application/json"})
        r._content = text.encode()
        r.url, r.request, r.reason, r.encoding = request.url, request, "synthetic", "utf-8"
        return r


def rows(path):
    try:
        return [json.loads(l) for l in path.read_text().splitlines()]
    except OSError:
        return None


cases = []
for case in json.loads(sys.argv[2]):
    shutil.rmtree(config.USER_STATE_DIR, ignore_errors=True)
    config.USER_STATE_DIR.mkdir(parents=True)
    if "backups" in case:
        regions.BACKUPS.write_text(json.dumps(case["backups"]))
    for key, path in (("stock_rows", regions.STOCK), ("failure_rows", regions.FAILURES)):
        if key in case:
            path.write_text("".join(json.dumps(r) + "\n" for r in case[key]))
    for out in (said, asked, sent, replies, presses):
        out.clear()
    clock[0] = 0.0
    pod._answer_wait[0] = 0.0
    interrupt = case.get("interrupt", {})
    world = World(case.get("world", {}))
    class Client(api.RunPod):
        def __init__(self, key):
            super().__init__(key)
            self.s.mount("https://", Player(world, interrupt))
    regions.RunPod = Client
    cfg = {**config.DEFAULTS, **FO_CONFIG, **case.get("config", {})}
    keys = {k: v for k, v in cfg.items() if config.DEFAULTS.get(k) != v}
    answers = case.get("answers", [])
    if answers is None:
        pod._ask = real_ask
        sys.stdin = types.SimpleNamespace(isatty=lambda: False)
    else:
        def scripted(question, default, answers=list(answers)):
            asked.append([question, default])
            return answers.pop(0)
        pod._ask = scripted
    sleeps = [0]
    def capacity_sleep(seconds):
        sleeps[0] += 1
        if interrupt.get("sleep") == sleeps[0]:
            presses.append(len(sent))
            raise KeyboardInterrupt
        fake.sleep(seconds)
        pod._answer_wait[0] += seconds
    pod._capacity_sleep = capacity_sleep
    call = case.get("call", "create_pod")
    result = {}
    rp = Client(API_KEY)
    try:
        if call == "create_pod":
            got = pod._create_pod(rp, cfg, cfg["VLLM_API_KEY"], "ssh-ed25519 AAAAsynthetic",
                                  "prim", case.get("volume_is_new", False))
            result = {"ok": list(got)}
        elif call == "retire_replaced":
            result = {"ok": regions.retire_replaced(rp, cfg, case["volume"],
                                                    say=lambda m: pod._say(m, pod.C_DIM))}
        elif call == "expire":
            gone = regions.expire(rp, cfg, now=case["now"], say=lambda m: pod._say(m))
            result = {"ok": [v["id"] for v in gone]}
        elif call == "backup":
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                try:
                    regions.cmd_backup(cfg, types.SimpleNamespace(samples=case["samples"]))
                except api.RunPodError as e:
                    result = {"error": str(e)}
            result["stdout"] = out.getvalue().replace(HOME, "$HOME")
        elif call == "rank":
            result = {"ok": regions.rank(case["samples"], set(case["exclude"]), case["past"])}
        elif call == "capacity":
            result = {"ok": [regions.is_capacity_error(api.RunPodError(t)) for t in case["texts"]]}
    except SystemExit as e:
        result = {"exit": e.code}
    except KeyboardInterrupt:
        result = {"interrupted": True}
    sys.stdin = sys.__stdin__
    cases.append({**case, "cfg": keys, "sent": list(sent), "replies": list(replies),
                  "presses": list(presses), "asked": list(asked), "said": list(said), **result,
                  "region_after": cfg["DATA_CENTER_ID"], "answer_wait": pod._answer_wait[0],
                  "backups_after": json.loads(regions.BACKUPS.read_text())
                  if regions.BACKUPS.exists() else None,
                  "stock_after": rows(regions.STOCK), "failures_after": rows(regions.FAILURES)})
print(json.dumps(cases))
"""


def gen_pod_failover(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": tmp,
               "XDG_CONFIG_HOME": tmp + "/.config", "XDG_STATE_HOME": tmp + "/.local/state",
               "XDG_DATA_HOME": tmp + "/.local/share"}
        out = subprocess.run([*rp_python(), "-I", "-c", FAILOVER_RUNNER, str(snap),
                              json.dumps(FAILOVER_CASES), repr(FO_NOW), API_KEY, FULL, json.dumps(FO_CONFIG)],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
    return {
        "about": "rpctl's pod creation with region failover (pod._create_pod on volume 'prim' "
                 "with the user's answers, the default without a terminal), "
                 "regions.retire_replaced, regions.expire, regions.cmd_backup, regions.rank "
                 "and regions.is_capacity_error, config keys over DEFAULTS and FO_CONFIG, "
                 "against a scripted RunPod (`world`, see tools/fixtures.py) played through a "
                 "transport adapter at a fixed clock (FO_NOW). `sent` are the requests made "
                 "(a pod's body cut to name, dataCenterIds, networkVolumeId and "
                 "allowedCudaVersions), `replies` what each got (hang: a Ctrl-C arrived "
                 "while it was in flight), `presses` the request counts at each Ctrl-C, "
                 "`asked` the questions with their defaults, `said` the notices with their "
                 "colour ($HOME for the home directory), then the outcome and the state "
                 "files after",
        "cases": json.loads(out.stdout),
    }


# ------------------------------------------------------------- prompt/system

# One case: config on top of DEFAULTS (VLLM_API_KEY k, TRACE_ENABLED 0,
# CONSULT_ENABLED 0 unless the case says), files and symlinks under the case's
# root $T ("{GLOBAL}/" is the client's own config dir: config/rpctl here,
# config/viy when viy replays), the project marker, the working directory and
# a user system prompt (`user_system` in the file). The output is rp's `_effective_system()`, the
# workspace, the instruction files loaded and what `/instructions` prints,
# with $T and the date replaced. "$T" in config values is the case's root.
PROMPT_RUNNER = r"""
import contextlib, io, json, re, sys
sys.path.insert(0, sys.argv[1])
from rpctl import chat, config
case = json.loads(sys.argv[2])
cfg = dict(config.DEFAULTS, VLLM_API_KEY="k", TRACE_ENABLED="0", CONSULT_ENABLED="0")
cfg.update(case["config"])
c = chat.Chat(cfg, "http://127.0.0.1:1")
if case.get("system") is not None:
    c.system = case["system"]
loaded = c._instructions()
buf = io.StringIO()
with contextlib.redirect_stdout(buf):
    c._report_instructions()
print(json.dumps({
    "system": c._effective_system(), "today": c._today, "workspace": str(c._workspace()),
    "loaded": [[str(p), len(t), tr] for p, t, tr in loaded],
    "report": re.sub(r"\x1b\[[0-9;]*m", "", buf.getvalue()).splitlines()}))
"""

LONG = "Use tabs. " * 10            # 100 chars
PROMPT_CASES = [
    dict(name="defaults_tools_and_search_on"),
    dict(name="no_tools_search_on", config={"TOOLS_ENABLED": "0"}),
    dict(name="no_tools_no_search", config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0"}),
    dict(name="tools_on_search_off", config={"SEARCH_ENABLED": "0"}),
    dict(name="guide_off_tools_on", config={"SYSTEM_GUIDE": "0"}),
    dict(name="guide_off_nothing_on",
         config={"SYSTEM_GUIDE": "0", "TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0"}),
    dict(name="explore_off", config={"EXPLORE_ENABLED": "0"}),
    dict(name="explore_trust_off", config={"SYSTEM_GUIDE_TRUST_EXPLORE": "0"}),
    dict(name="consult_with_claude_installed",
         config={"CONSULT_ENABLED": "1", "CONSULT_CLAUDE_BIN": "$T/bin/claude"},
         files={"bin/claude": "#!/bin/sh\n"}, executable=["bin/claude"]),
    dict(name="consult_without_tools",
         config={"CONSULT_ENABLED": "1", "CONSULT_CLAUDE_BIN": "$T/bin/claude",
                 "TOOLS_ENABLED": "0"},
         files={"bin/claude": "#!/bin/sh\n"}, executable=["bin/claude"]),
    dict(name="user_system_prompt_first", system="Answer in French.",
         config={"TOOLS_ENABLED": "0"}),
    dict(name="empty_user_system_prompt", system="",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0", "SYSTEM_GUIDE": "0"}),
    dict(name="instruction_files_outermost_first", cwd="project/sub",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0"},
         files={"{GLOBAL}/AGENTS.md": "Global: be brief.\n",
                "AGENTS.md": "Root of the tree.\n",
                "project/AGENTS.md": "  Project rules.\n\n",
                "project/QWEN.md": "Qwen-specific.\n",
                "project/sub/AGENTS.md": "Below the project root: not read.\n"},
         symlinks={"project/CLAUDE.md": "AGENTS.md"}),
    dict(name="no_marker_reads_from_the_working_directory", cwd="project/sub",
         marker=False, config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0"},
         files={"project/AGENTS.md": "Parent of the cwd.\n",
                "project/sub/CLAUDE.md": "The cwd itself.\n"}),
    dict(name="tools_on_with_instruction_files",
         files={"project/AGENTS.md": "Run the tests with make test.\n"}),
    dict(name="truncated_innermost_and_the_rest_dropped",
         config={"TOOLS_ENABLED": "0", "INSTRUCTIONS_MAX_CHARS": "60"},
         files={"{GLOBAL}/AGENTS.md": "Global.\n", "project/AGENTS.md": LONG}),
    dict(name="budget_left_for_outer_files",
         config={"TOOLS_ENABLED": "0", "INSTRUCTIONS_MAX_CHARS": "50"},
         files={"{GLOBAL}/CLAUDE.md": "G" * 40, "project/AGENTS.md": "P" * 20}),
    dict(name="budget_counts_characters",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0", "INSTRUCTIONS_MAX_CHARS": "10"},
         files={"project/AGENTS.md": "é" * 13 + "\U0001F600"}),
    dict(name="budget_zero_and_bogus_values",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0", "INSTRUCTIONS_MAX_CHARS": "0"},
         files={"project/AGENTS.md": "Never sent.\n"}),
    dict(name="budget_bogus_means_default",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0", "INSTRUCTIONS_MAX_CHARS": "lots"},
         files={"project/AGENTS.md": "Sent.\n"}),
    dict(name="custom_names_and_empty_files",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0",
                 "INSTRUCTION_FILES": " NOTES.md, ,AGENTS.md,"},
         files={"project/NOTES.md": "Notes first.\n", "project/AGENTS.md": "  \n\n",
                "project/CLAUDE.md": "Not in the list.\n"}),
    dict(name="instructions_off",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0", "INSTRUCTIONS_ENABLED": "0"},
         files={"project/AGENTS.md": "Not sent.\n"}),
    dict(name="none_found",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0",
                 "INSTRUCTION_FILES": "RULES.md"}),
    dict(name="invalid_utf8_is_replaced",
         config={"TOOLS_ENABLED": "0", "SEARCH_ENABLED": "0"},
         raw_files={"project/AGENTS.md": "Caf\\xff ok\n"}),
    dict(name="workspace_dir_relative", config={"WORKSPACE_DIR": "ws"},
         files={"project/ws/AGENTS.md": "In the workspace.\n",
                "project/AGENTS.md": "Its parent, the project.\n"}),
    dict(name="workspace_dir_absolute", config={"WORKSPACE_DIR": " $T/elsewhere "},
         files={"elsewhere/AGENTS.md": "Elsewhere.\n"}),
    dict(name="workspace_dir_in_home", config={"WORKSPACE_DIR": "~/code"},
         files={"home/code/AGENTS.md": "Home code.\n"}),
]


def prompt_case(snap, case):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        at = lambda rel: tmp / rel.replace("{GLOBAL}", "config/rpctl")
        (tmp / "project").mkdir()
        if case.get("marker", True):
            (tmp / "project" / ".rpctl").mkdir()
        (tmp / case.get("cwd", "project")).mkdir(parents=True, exist_ok=True)
        for rel, text in case.get("files", {}).items():
            at(rel).parent.mkdir(parents=True, exist_ok=True)
            at(rel).write_bytes(text.encode())
        for rel, text in case.get("raw_files", {}).items():
            at(rel).parent.mkdir(parents=True, exist_ok=True)
            at(rel).write_bytes(text.encode().decode("unicode_escape").encode("latin-1"))
        for rel in case.get("executable", []):
            at(rel).chmod(0o755)
        for link, target in case.get("symlinks", {}).items():
            at(link).symlink_to(target)
        job = {"config": {k: v.replace("$T", str(tmp))
                          for k, v in case.get("config", {}).items()},
               "system": case.get("system")}
        out = subprocess.run([sys.executable, "-I", "-c", PROMPT_RUNNER, str(snap),
                              json.dumps(job)],
                             cwd=tmp / case.get("cwd", "project"), env=env, check=True,
                             capture_output=True, text=True)
        got = json.loads(out.stdout)
        for p, _, _ in got["loaded"]:
            if not pathlib.Path(p).is_relative_to(tmp):
                raise SystemExit(f"prompt/system {case['name']}: an instruction file "
                                 f"outside the case's tree was read: {p}")
        today = got.pop("today")

        def norm(v):
            if isinstance(v, str):
                return v.replace(str(tmp), "$T").replace(today, "{TODAY}")
            if isinstance(v, list):
                return [norm(x) for x in v]
            return v
        return {k: norm(v) for k, v in got.items()}


def gen_prompt_system(snap):
    cases = []
    for case in PROMPT_CASES:
        inputs = {
            "name": case["name"],
            "config": case.get("config", {}),
            "files": case.get("files", {}),
            "raw_files": case.get("raw_files", {}),
            "executable": case.get("executable", []),
            "symlinks": case.get("symlinks", {}),
            "marker": case.get("marker", True),
            "cwd": case.get("cwd", "project"),
            "user_system": case.get("system"),
        }
        cases.append({**inputs, **prompt_case(snap, case)})
    return {
        "about": "rpctl chat's system prompt (Chat._effective_system), its workspace, the "
                 "instruction files loaded ([path, chars, truncated]) and what "
                 "/instructions prints, for synthetic trees: $T is the case's root, HOME "
                 "$T/home, XDG_CONFIG_HOME $T/config, the project $T/project (with a "
                 ".rpctl/ marker unless marker is false); {GLOBAL} in a file name is the "
                 "client's config dir; DEFAULTS plus VLLM_API_KEY k, TRACE_ENABLED 0, "
                 "CONSULT_ENABLED 0, then the case's config; raw_files are byte strings "
                 "with \\xNN escapes; the date is {TODAY}",
        "cases": cases,
    }



# ------------------------------------------------------------ chat/context

# rpctl chat's context accounting: _estimate_tokens over message lists (with
# and without the tool schemas), _calibrate over sequences of real
# prompt_tokens (the chars-per-token after each), _prompt_budget, and
# _context_state with the system prompt. One python3 -I child for all cases,
# HOME and XDG_* in a fresh tree, no instruction files, no toolbox.
CONTEXT_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import chat, config
job = json.loads(sys.argv[2])

def make(over):
    cfg = dict(config.DEFAULTS, VLLM_API_KEY="k", TRACE_ENABLED="0", CONSULT_ENABLED="0",
               INSTRUCTIONS_ENABLED="0", TOOLBOX_ENABLED="0", TOOLS_ENABLED="0")
    cfg.update(over)
    return chat.Chat(cfg, "http://127.0.0.1:1")

out = {"estimates": [], "calibrations": [], "budgets": [], "states": []}
for case in job["estimates"]:
    c = make(case["config"])
    out["estimates"].append({
        "measure": list(c._measure(case["messages"])),
        "schema_chars": c._tool_schema_chars(),
        "with_schemas": c._estimate_tokens(case["messages"]),
        "without_schemas": c._estimate_tokens(case["messages"], schemas=False)})
for case in job["calibrations"]:
    c = make(case["config"])
    seen = []
    for step in case["steps"]:
        c._calibrate(step["messages"], step["prompt_tokens"])
        seen.append(c._chars_per_token)
    out["calibrations"].append({"chars_per_token": seen,
                                "estimate_after": c._estimate_tokens(case["steps"][0]["messages"])})
for case in job["budgets"]:
    c = make(case["config"])
    out["budgets"].append({"max_model_len": c.max_model_len, "budget": c._prompt_budget()})
for case in job["states"]:
    c = make(case["config"])
    c._today = "2026-07-03 (Friday)"
    c.messages = case["history"]
    tok, frac = c._context_state(case["extra"])
    out["states"].append({"tokens": tok, "fraction": frac,
                          "system": c._effective_system()})
print(json.dumps(out))
"""

_PROSE = ("The quick brown fox jumps over the lazy dog while the compiler "
          "checks every borrow twice. ")
CONTEXT_MESSAGES = {
    "empty": [],
    "plain": [{"role": "user", "content": "hello"},
              {"role": "assistant", "content": "Hi! How can I help?"}],
    "unicode": [{"role": "user", "content": "Привет, 世界! 🦀🦀 naïve café"}],
    "multipart_with_images": [{"role": "user", "content": [
        {"type": "text", "text": "what is in these?"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,BBBB"}},
        "not a dict", {"type": "text", "text": None}, {"type": "text"}]}],
    "none_content_reasoning_and_calls": [
        {"role": "user", "content": "fix it"},
        {"role": "assistant", "content": None, "reasoning": "plan: read then write",
         "tool_calls": [{"id": "c1", "type": "function",
                         "function": {"name": "read_file", "arguments": "{\"path\": \"a.py\"}"}},
                        {"id": "c2", "type": "function", "function": {}},
                        {"id": "c3", "type": "function",
                         "function": {"name": "write_file", "arguments": None}}]},
        {"role": "tool", "tool_call_id": "c1", "content": "print('hi')\n"},
        {"role": "assistant", "content": "Done.", "thought": True}],
    "long_prose": [{"role": "user", "content": _PROSE * 40},
                   {"role": "assistant", "content": _PROSE * 25}],
}
CONTEXT_CONFIGS = {
    "no_tools": {"SEARCH_ENABLED": "0"},
    "web_search_and_fetch": {"SEARCH_ENABLED": "1"},
    "web_search_only": {"SEARCH_ENABLED": "1", "FETCH_ENABLED": "0"},
}


def context_job():
    estimates = [{"name": f"{m}/{c}", "config": CONTEXT_CONFIGS[c],
                  "messages": CONTEXT_MESSAGES[m]}
                 for m in CONTEXT_MESSAGES for c in CONTEXT_CONFIGS]
    big = CONTEXT_MESSAGES["long_prose"]
    calibrations = [
        dict(name="converges_on_a_steady_ratio", config={"SEARCH_ENABLED": "0"},
             steps=[{"messages": big, "prompt_tokens": 1500}] * 4),
        dict(name="web_schemas_count", config={"SEARCH_ENABLED": "1"},
             steps=[{"messages": big, "prompt_tokens": 2100},
                    {"messages": big, "prompt_tokens": 1900}]),
        dict(name="skips_what_means_nothing", config={"SEARCH_ENABLED": "0"},
             steps=[{"messages": big, "prompt_tokens": None},
                    {"messages": big, "prompt_tokens": 0},
                    {"messages": big, "prompt_tokens": -5},
                    {"messages": big, "prompt_tokens": 150},
                    {"messages": CONTEXT_MESSAGES["plain"], "prompt_tokens": 900},
                    {"messages": big, "prompt_tokens": 100000},
                    {"messages": big, "prompt_tokens": 210},
                    {"messages": CONTEXT_MESSAGES["multipart_with_images"] + big,
                     "prompt_tokens": 3000},
                    {"messages": big, "prompt_tokens": 1200}]),
    ]
    budgets = [dict(name=n, config=c) for n, c in [
        ("defaults", {}),
        ("live_profile", {"MAX_MODEL_LEN": "262144", "MAX_TOKENS": "16384"}),
        ("reply_budget_eats_it", {"MAX_MODEL_LEN": "8192", "MAX_TOKENS": "8000"}),
        ("bogus_context_length", {"MAX_MODEL_LEN": "lots"}),
        ("spaces_around", {"MAX_MODEL_LEN": " 131072 ", "MAX_TOKENS": " 4096 "}),
    ]]
    states = [
        dict(name="fresh_chat_no_tools", config={"SEARCH_ENABLED": "0"}, history=[], extra=[]),
        dict(name="history_and_next_message", config={"SEARCH_ENABLED": "1"},
             history=CONTEXT_MESSAGES["none_content_reasoning_and_calls"],
             extra=[{"role": "user", "content": "and now?"}]),
        dict(name="guide_off_nothing_on",
             config={"SEARCH_ENABLED": "0", "SYSTEM_GUIDE": "0", "MAX_MODEL_LEN": "4096",
                     "MAX_TOKENS": "1024"},
             history=CONTEXT_MESSAGES["long_prose"], extra=[]),
    ]
    return {"estimates": estimates, "calibrations": calibrations, "budgets": budgets,
            "states": states}


def gen_chat_context(snap):
    job = context_job()
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        (tmp / "project" / ".rpctl").mkdir(parents=True)
        out = subprocess.run([sys.executable, "-I", "-c", CONTEXT_RUNNER, str(snap),
                              json.dumps(job)],
                             cwd=tmp / "project", env=env, check=True,
                             capture_output=True, text=True)
        got = json.loads(out.stdout)
        text = json.dumps(got).replace(str(tmp), "$T")
        got = json.loads(text)
    cases = []
    for kind in ("estimates", "calibrations", "budgets", "states"):
        for case, result in zip(job[kind], got[kind]):
            cases.append({"kind": kind, **case, **result})
    return {
        "about": "rpctl chat's context accounting: _measure, _tool_schema_chars and "
                 "_estimate_tokens over message lists (estimates), chars-per-token after "
                 "each _calibrate (calibrations, from 3.6), max_model_len and "
                 "_prompt_budget (budgets), _context_state with the system prompt "
                 "(states, the date fixed at 2026-07-03 (Friday), workspace $T/project); DEFAULTS plus "
                 "VLLM_API_KEY k, TRACE 0, CONSULT 0, INSTRUCTIONS 0, TOOLBOX 0, "
                 "TOOLS_ENABLED 0, then the case's config",
        "cases": cases,
    }


# ------------------------------------------------------------ chat/sessions

# rpctl's sessions.py driven by scripts of steps, each case in its own
# sessions directory. The clock reads 1761868800.25 plus the step's index, so
# every event's time is fixed; ids are random and come out as the name of the
# log that made them ("{a}"), the case's directory as $T. For each log: its
# bytes, its events, replay() and the sidecar; then the listing, searches and
# resolves the case asks for. Steps:
#   new      {log, project?, model?}      SessionLog.new
#   sync     {log, system?, messages, force?}  -> True/False
#   rename   {log, title}
#   auto     {log, title}                 set_auto_title -> True/False
#   fork     {log, as, system?, messages, title?}
#   close    {log, reason?}
#   open     {log, as}                    SessionLog.open -> "ok" / "busy"
#   raw      {log, text}                  appended to the log's file as is
#   redact   {log, literals}              log.redact = Redactor(literals)
#   files    {log}                        a file in the log's .files/ dir
#   unlink_sidecar {log}
#   trash    {log}                        -> "ok" / "busy"
SESSIONS_RUNNER = r"""
import json, os, pathlib, sys, time
sys.path.insert(0, sys.argv[1])
from rpctl import config, redact, sessions
job = json.loads(sys.argv[2])
base = pathlib.Path(sys.argv[3])
now = [0.0]
time.time = lambda: now[0]
out = []

def sub(v, t):
    if isinstance(v, str):
        return v.replace("$T", t)
    if isinstance(v, list):
        return [sub(x, t) for x in v]
    if isinstance(v, dict):
        return {k: sub(x, t) for k, x in v.items()}
    return v

def read(p):
    try:
        return p.read_text()
    except OSError:
        return None

for case in job:
    root = base / case["name"]
    (root / "sessions").mkdir(parents=True)
    config.SESSIONS_DIR = root / "sessions"
    t = str(root)
    logs, names, results = {}, {}, []
    for i, step in enumerate(case["steps"]):
        now[0] = 1761868800.25 + i
        s = sub(step, t)
        op = s["op"]
        log = logs.get(s.get("log"))
        r = None
        if op == "new":
            log = sessions.SessionLog.new(s.get("project"), s.get("model"))
            logs[s["log"]] = log
            names.setdefault(log.id, s["log"])
        elif op == "sync":
            r = log.sync(s.get("system"), s["messages"], force=s.get("force", False))
        elif op == "rename":
            log.rename(s["title"])
        elif op == "auto":
            r = log.set_auto_title(s["title"])
        elif op == "fork":
            twin = log.fork(s.get("system"), s["messages"], title=s.get("title"))
            logs[s["as"]] = twin
            names.setdefault(twin.id, s["as"])
        elif op == "close":
            log.close(s.get("reason", "exit"))
        elif op == "open":
            try:
                logs[s["as"]], _ = sessions.SessionLog.open(log.id)
                r = "ok"
            except sessions.SessionBusy:
                r = "busy"
        elif op == "raw":
            with open(log.path, "a") as f:
                f.write(s["text"])
        elif op == "redact":
            log.redact = redact.Redactor(s["literals"])
        elif op == "files":
            log.files_dir.mkdir()
            (log.files_dir / "1").write_text("before")
        elif op == "unlink_sidecar":
            log.path.with_suffix(".meta.json").unlink()
        elif op == "trash":
            try:
                sessions.trash(log.id)
                r = "ok"
            except sessions.SessionBusy:
                r = "busy"
        results.append(r)
    sdir = root / "sessions"
    files = {}
    for sid, name in names.items():
        path = sdir / f"{sid}.jsonl"
        raw = read(path)
        files[name] = {
            "raw": raw,
            "events": sessions.read_events(path) if raw is not None else None,
            "replay": sessions.replay(path) if raw is not None else None,
            "sidecar": json.loads(read(path.with_suffix(".meta.json")) or "null"),
            "files": sorted(p.name for p in sessions.files_dir(path).iterdir())
                     if sessions.files_dir(path).is_dir() else None,
        }
    trash = sdir / "trash"
    q = case.get("queries", {})
    got = {
        "results": results,
        "files": files,
        "trash": sorted(p.name for p in trash.iterdir()) if trash.is_dir() else None,
        "list": [sessions.list_sessions(sub(scope, t)) for scope in q.get("list", [])],
        "search": [sessions.search(query, sub(scope, t))
                   for query, scope in q.get("search", [])],
        "resolve": [sessions.resolve(
                        next((i for i, n in names.items() if ref == "@" + n), ref),
                        sub(scope, t))
                    for ref, scope in q.get("resolve", [])],
    }
    text = json.dumps(got, ensure_ascii=False).replace(t, "$T")
    for sid, name in names.items():
        text = text.replace(sid, "{" + name + "}")
    out.append(json.loads(text))
print(json.dumps(out, ensure_ascii=False))
"""


def _u(text):
    return {"role": "user", "content": text}


def _a(text, **kw):
    return {"role": "assistant", "content": text, **kw}


_SUMMARY_HEAD = "[Summary of the conversation so far. Continue from here.]"
_SUMMARY_NEXT = "\n\n---\n[Summary of the next part of the conversation]\n\n"
_SUMMARY_ACK = "Understood. I'll continue from the summary above."


def _summary(text):
    return [_u(_SUMMARY_HEAD + "\n\n" + text), _a(_SUMMARY_ACK)]


def _turn(n):
    return [_u(f"question {n}"), _a(f"answer {n}")]


def _compactions():
    """test_replay_keeps_what_compactions_summarised_away's histories."""
    msgs = _turn(1) + _turn(2) + _turn(3)
    out = [msgs]
    msgs = _summary("one and two") + _turn(3)
    out.append(msgs)
    msgs = msgs + _turn(4)
    out.append(msgs)
    msgs = msgs[:2] + [msgs[2], _a("[elided]")] + msgs[4:]
    out.append(msgs)
    head = dict(msgs[0], content=msgs[0]["content"] + _SUMMARY_NEXT + "three")
    msgs = [head, msgs[1]] + _turn(4)
    out.append(msgs)
    out.append(msgs + _turn(5))
    return out


_TOOL_TURN = [
    {"role": "user", "content": [
        {"type": "text", "text": "what is  here?\n"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}},
        {"type": "text", "text": "[rp chat: mounted /x]"}]},
    {"role": "assistant", "content": None, "thought": True, "tool_calls": [
        {"id": "c1", "type": "function", "function": {
            "name": "run_shell", "arguments": "{\"command\": \"ls\\npwd\"}"}}]},
    {"role": "tool", "tool_call_id": "c1", "content": "TOOL OUTPUT ünïcode"},
    {"role": "assistant", "content": "Two files. Привет 世界 🦀", "thought": True},
]

# A secret redact masks that the secret scanner leaves alone.
_TOKEN = "Authorization: Bearer fixture-bearer-0123456789abcdef"

SESSIONS_CASES = [
    dict(name="append_and_replay_roundtrip", steps=[
        dict(op="new", log="a", project="$T/proj", model="m"),
        dict(op="sync", log="a", system="be brief", messages=[_u("hello there")]),
        dict(op="sync", log="a", system="be brief",
             messages=[_u("hello there"), _a("hi")]),
        dict(op="close", log="a")],
        queries=dict(list=[None, "$T/proj"])),
    dict(name="nothing_written_until_first_message", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", system="sys", messages=[]),
        dict(op="close", log="a"),
        dict(op="auto", log="a", title="too early")],
        queries=dict(list=[None])),
    dict(name="rewrite_is_a_replace_and_search_finds_the_old", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("start"), _a("the zebra detail")]),
        dict(op="sync", log="a", messages=_summary("so far")),
        dict(op="close", log="a")],
        queries=dict(search=[["zebra", None], ["ZEBRA Detail", "$T/proj"],
                             ["zebra giraffe", None], ["", None], ["summary", None]])),
    dict(name="torn_last_line_is_trimmed_on_resume", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("a")]),
        dict(op="close", log="a"),
        dict(op="raw", log="a", text='{"t": "msg", "message": {"role": "assi'),
        dict(op="open", log="a", **{"as": "a2"}),
        dict(op="sync", log="a2", messages=[_u("a"), _a("b")]),
        dict(op="close", log="a2")]),
    dict(name="resume_with_nothing_new_writes_nothing", steps=[
        dict(op="new", log="a", project="$T/proj", model="m"),
        dict(op="sync", log="a", system="s", messages=_turn(1)),
        dict(op="close", log="a"),
        dict(op="open", log="a", **{"as": "a2"}),
        dict(op="sync", log="a2", system="s", messages=_turn(1)),
        dict(op="sync", log="a2", system=None, messages=_turn(1) + _turn(2)),
        dict(op="close", log="a2", reason="switch")]),
    dict(name="user_title_beats_automatic_title", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("x" * 100)]),
        dict(op="rename", log="a", title="My plan"),
        dict(op="raw", log="a", text='{"t": "title", "text": "auto later", "auto": true}\n'),
        dict(op="auto", log="a", title="model's"),
        dict(op="close", log="a")],
        queries=dict(list=[None])),
    dict(name="model_title_survives_resume", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("hello")]),
        dict(op="auto", log="a", title="Greeting the model"),
        dict(op="close", log="a"),
        dict(op="open", log="a", **{"as": "a2"}),
        dict(op="auto", log="a2", title="Another"),
        dict(op="close", log="a2")]),
    dict(name="second_writer_is_refused", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("a")]),
        dict(op="open", log="a", **{"as": "b"}),
        dict(op="trash", log="a"),
        dict(op="close", log="a"),
        dict(op="open", log="a", **{"as": "b"}),
        dict(op="close", log="b")]),
    dict(name="fork_copies_history_and_files", steps=[
        dict(op="new", log="a", project="$T/proj", model="m"),
        dict(op="sync", log="a", system="s", messages=_turn(1)),
        dict(op="files", log="a"),
        dict(op="fork", log="a", system="s", messages=_turn(1), **{"as": "b"}),
        dict(op="fork", log="a", system=None, messages=_turn(1), title="Plan B",
             **{"as": "c"}),
        dict(op="sync", log="b", system="s", messages=_turn(1) + _turn(2)),
        dict(op="close", log="a", reason="fork"),
        dict(op="close", log="b"),
        dict(op="close", log="c")],
        queries=dict(list=["$T/proj"])),
    dict(name="fork_of_a_model_titled_chat", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=_turn(1)),
        dict(op="auto", log="a", title="Model named"),
        dict(op="fork", log="a", messages=_turn(1), **{"as": "b"}),
        dict(op="auto", log="b", title="never"),
        dict(op="close", log="b"),
        dict(op="close", log="a")]),
    dict(name="fork_of_an_untitled_chat_titles_twice", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="fork", log="a", messages=_turn(1), **{"as": "b"}),
        dict(op="close", log="b")]),
    dict(name="listing_is_project_scoped_and_title_hits_rank_first", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("talk about apples")]),
        dict(op="close", log="a"),
        dict(op="new", log="b", project="$T/proj"),
        dict(op="sync", log="b", messages=[_u("unrelated"), _a("but apples appear here")]),
        dict(op="close", log="b"),
        dict(op="new", log="c", project="/elsewhere"),
        dict(op="sync", log="c", messages=[_u("apples elsewhere")]),
        dict(op="close", log="c"),
        dict(op="new", log="d"),
        dict(op="sync", log="d", messages=[_u("apples with no project")]),
        dict(op="close", log="d")],
        queries=dict(
            list=["$T/proj", None, "/elsewhere"],
            search=[["apples", "$T/proj"], ["apples", None], ["apples pears", None],
                    ["APPEAR apples", None], ["unrelated apples", "$T/proj"]],
            resolve=[["", "$T/proj"], ["@b", None], ["2", "$T/proj"], ["apples", "$T/proj"],
                     ["nothing", None]])),
    dict(name="scope_follows_the_project_tree", steps=[
        dict(op="new", log="self", project="$T/proj"),
        dict(op="sync", log="self", messages=[_u("self")]),
        dict(op="new", log="sub", project="$T/proj/sub"),
        dict(op="sync", log="sub", messages=[_u("sub")]),
        dict(op="new", log="parent", project="$T"),
        dict(op="sync", log="parent", messages=[_u("parent")]),
        dict(op="new", log="sib", project="$T/other"),
        dict(op="sync", log="sib", messages=[_u("sib")]),
        dict(op="new", log="prefix", project="$T/proj-two"),
        dict(op="sync", log="prefix", messages=[_u("prefix")]),
        dict(op="close", log="self"), dict(op="close", log="sub"),
        dict(op="close", log="parent"), dict(op="close", log="sib"),
        dict(op="close", log="prefix")],
        queries=dict(list=["$T/proj", "$T/proj/sub", "$T", "$T/proj/", "/"])),
    dict(name="stale_sidecar_is_rebuilt", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("first")]),
        dict(op="close", log="a"),
        dict(op="unlink_sidecar", log="a")],
        queries=dict(list=[None])),
    dict(name="trash_moves_the_log_and_its_files", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=[_u("a")]),
        dict(op="files", log="a"),
        dict(op="close", log="a"),
        dict(op="trash", log="a")],
        queries=dict(list=[None])),
    dict(name="redaction_masks_on_disk_only", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="redact", log="a", literals=["literal-secret-value"]),
        dict(op="sync", log="a", system=f"sys {_TOKEN}",
             messages=[_u(f"use {_TOKEN} and literal-secret-value")]),
        dict(op="sync", log="a", system=f"sys {_TOKEN}",
             messages=[_u(f"use {_TOKEN} and literal-secret-value"), _a("ok")]),
        dict(op="rename", log="a", title=f"about {_TOKEN}"),
        dict(op="fork", log="a", system=None, messages=[_u(f"again {_TOKEN}")],
             **{"as": "b"}),
        dict(op="close", log="a"),
        dict(op="close", log="b")]),
    dict(name="compactions_leave_earlier_turns", steps=[
        dict(op="new", log="a", project="$T/proj"),
        *[dict(op="sync", log="a", messages=m) for m in _compactions()],
        dict(op="close", log="a")]),
    dict(name="a_rewind_leaves_nothing_earlier", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=_turn(1) + _turn(2)),
        dict(op="sync", log="a", messages=_turn(1)),
        dict(op="sync", log="a", messages=_summary("s") + _turn(2)),
        dict(op="sync", log="a", messages=_summary("s")),
        dict(op="close", log="a")]),
    dict(name="corrupt_and_odd_lines_are_skipped", steps=[
        dict(op="new", log="a", project="$T/proj", model="m"),
        dict(op="sync", log="a", system="s", messages=[_u("a")]),
        dict(op="raw", log="a", text="not json\n"),
        dict(op="raw", log="a", text="[1, 2]\n"),
        dict(op="raw", log="a", text="\n"),
        dict(op="raw", log="a", text='{"t": "mystery", "ts": 5}\n'),
        dict(op="raw", log="a", text='{"t": "msg", "message": "not a dict"}\n'),
        dict(op="raw", log="a", text='{"t": "title", "text": ""}\n'),
        dict(op="raw", log="a", text='{"t": "system", "text": null, "ts": 0}\n'),
        dict(op="raw", log="a", text='{"t": "msg", "message": {"role": "user", '
                                     '"content": "raw ünï"}}\n'),
        dict(op="close", log="a")]),
    dict(name="titles_skip_notes_summaries_and_blanks", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", messages=_summary("old") + [_u("   \n\t "), _a("?")]
             + _TOOL_TURN),
        dict(op="close", log="a"),
        dict(op="new", log="b", project="$T/proj"),
        dict(op="sync", log="b", messages=[_u("🦀" * 30 + " Привет " * 10)]),
        dict(op="close", log="b"),
        dict(op="new", log="c", project="$T/proj"),
        dict(op="sync", log="c", messages=[_u("y" * 59 + "z")]),
        dict(op="close", log="c"),
        dict(op="new", log="d", project="$T/proj"),
        dict(op="sync", log="d", messages=[_a("only the model spoke")]),
        dict(op="close", log="d")],
        queries=dict(search=[["ls", None], ["пРИВЕТ", None], ["run_shell", None],
                             ["tool output", None]])),
    dict(name="system_prompt_changes_are_events", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", system=None, messages=[_u("a")]),
        dict(op="sync", log="a", system="first", messages=[_u("a")]),
        dict(op="sync", log="a", system="second", messages=[_u("a"), _a("b")]),
        dict(op="sync", log="a", system=None, messages=[_u("a"), _a("b")]),
        dict(op="close", log="a")],
        queries=dict(search=[["second", None]])),
    dict(name="rename_creates_the_file", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="rename", log="a", title="Design notes"),
        dict(op="close", log="a")],
        queries=dict(list=[None])),
    dict(name="forced_sync_of_nothing", steps=[
        dict(op="new", log="a", project="$T/proj"),
        dict(op="sync", log="a", system=None, messages=[], force=True),
        dict(op="close", log="a")]),
]


def gen_chat_sessions(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TZ": "UTC"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        (tmp / "cases").mkdir()
        out = subprocess.run([sys.executable, "-I", "-c", SESSIONS_RUNNER, str(snap),
                              json.dumps(SESSIONS_CASES), str(tmp / "cases")],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
        got = json.loads(out.stdout)
    return {
        "about": "rpctl's sessions.py run through scripts of steps (see tools/fixtures.py "
                 "chat/sessions): the clock at 1761868800.25 plus the step's index, ids "
                 "shown as the name of the log that made them, the case's directory as "
                 "$T. Per log its bytes, events, replay() and sidecar; then the listings, "
                 "searches and resolves the case asks for",
        "cases": [{**case, **result} for case, result in zip(SESSIONS_CASES, got)],
    }



# -------------------------------------------------------------- chat/modes

# rpctl's modes.py as data: KINDS, MODES (name, label, colour, blurb, the
# kinds each says yes to, in KINDS order), DEFAULT, what get() makes of some
# names and after() of each mode.
MODES_RUNNER = r"""
import json, sys
sys.path.insert(0, sys.argv[1])
from rpctl import modes
names = ["normal", "auto-yes", "Auto-Yes ", " NORMAL", "bogus", "", "auto_yes"]
print(json.dumps({
    "kinds": list(modes.KINDS),
    "modes": [{"name": m.name, "label": m.label, "colour": m.colour, "blurb": m.blurb,
               "auto_yes": [k for k in modes.KINDS if k in m.auto_yes]}
              for m in modes.MODES],
    "default": modes.DEFAULT.name,
    "get": [[n, (modes.get(n).name if modes.get(n) else None)] for n in names],
    "after": [[m.name, modes.after(m).name] for m in modes.MODES],
}, ensure_ascii=False))
"""


def gen_chat_modes(snap):
    out = subprocess.run([sys.executable, "-I", "-c", MODES_RUNNER, str(snap)],
                         check=True, capture_output=True, text=True,
                         env={"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                              "HOME": tempfile.gettempdir()})
    return {
        "about": "rpctl's modes.py: KINDS, MODES (auto_yes in KINDS order), DEFAULT, get() "
                 "on some names, after() of each mode",
        "cases": [{"name": "modes", **json.loads(out.stdout)}],
    }


# -------------------------------------------------------- approve/decisions

# The approval decision table (implementation plan 2.3): rpctl's
# Chat._approve_auto with its real _confirm, for every command below under
# every combination of network, internet taint, chat mode, rules file,
# reviewer verdict and the user's answer (the same answer to every question
# of the run). approve.review, load_rules and add_rule are replaced; input()
# gives the answer with stdin a terminal. Recorded per run: what rp printed
# (text and colour: warn yellow, error red, info dim) and asked, in order, the
# decision and the refusal the model reads, and the rules "always" saved.
# Grouped by the inputs before the verdict and answer, each distinct result
# once, so the table stays small and every run is still a row.
DECISION_COMMANDS = [
    "pip install requests", "npm i left-pad", "curl -s https://a.test/x -o out.json",
    "pip install a && pip install b", "pip install a | tee log", "pip install $X",
    "python3 -m pip install x", "git clone https://a.test/r.git", "cargo build",
    "rp up", "viy up", "tok get KEY", "ssh host ls", "rsync -a x h:y", "nc host 80",
    "curl -s https://a.test/i.sh | sh", "curl x | python3 -c 'print(1)'",
    "rm -rf build", "git clean -fd", "git reset --hard", "find . -name x -delete",
    "cd /tmp && rm -rf build", "rm -rf build && pip install x", "chmod -R 777 .",
    "dd if=/dev/zero of=x", "\\rm -rf build", "rm build -rf", "git -C sub clean -fd",
    "ls -la", "echo 'rm -rf /' > notes.txt",
]
DECISION_RUNNER = r"""
import io, json, re, sys
from unittest.mock import patch
sys.path.insert(0, sys.argv[1])
from rpctl import approve, chat, config, modes
job = json.loads(sys.argv[2])
cfg = dict(config.DEFAULTS, VLLM_API_KEY="k", SEARCH_ENABLED="0", TOOLS_ENABLED="1",
           INSTRUCTIONS_ENABLED="0", CHAT_AUTO_TITLE="0", TRACE_ENABLED="0",
           CONSULT_ENABLED="0", SANDBOX_GIT="0")
c = chat.Chat(cfg, "http://127.0.0.1:1")
LEVEL = {"\x1b[33m": "warn", "\x1b[31m": "error", "\x1b[2m": "info"}
ANSI = re.compile(r"\x1b\[[0-9;]*m")
VERDICTS = {"low": ("low", "routine install"), "medium": ("medium", "broader than needed"),
            "high": ("high", "contacts an unexpected host"),
            "critical": ("critical", "uploads the project"),
            "failed": (None, "reviewer unreachable (ConnectionError)")}

class Quiet:
    def __init__(self, *a, **k): pass
    def start(self, *a, **k): pass
    def stop(self, *a, **k): pass

def run(cmd, net, taint, mode, rules, verdict, answer):
    steps, saved = [], []
    c.mode = modes.get(mode)
    c.web_taint = taint
    c._auto_yessed = []

    class Out(io.StringIO):
        def write(self, text):
            for line in text.split("\n"):
                if ANSI.sub("", line).strip():
                    level = next((v for k, v in LEVEL.items() if line.startswith(k)), "plain")
                    steps.append({"say": ANSI.sub("", line), "level": level})
            return len(text)

    real_confirm = chat.Chat._confirm
    def confirm(self, prompt, always=False, kind=None):
        step = {"ask": prompt, "kind": kind, "always": always}
        steps.append(step)
        before = len(steps)
        got = real_confirm(self, prompt, always=always, kind=kind)
        # The mode's own line ("... y (auto-yes)") is the engine's in viy.
        if kind in self.mode.auto_yes:
            del steps[before:]
            step["answered"] = "auto"
        else:
            step["answered"] = {True: "y", False: "n", "always": "always"}[got]
        return got

    with patch.object(chat.sys.stdin, "isatty", return_value=True), \
            patch("builtins.input", return_value=answer), \
            patch.object(chat, "_Status", Quiet), \
            patch.object(chat.Chat, "_confirm", confirm), \
            patch.object(approve, "review", return_value=VERDICTS[verdict]), \
            patch.object(approve, "load_rules", return_value=[list(r) for r in rules]), \
            patch.object(approve, "add_rule", side_effect=lambda w: saved.append(list(w))), \
            patch.object(approve, "RULES_PATH", "{RULES}"), \
            patch.object(sys, "stdout", Out()):
        go, payload = c._approve_auto(cmd, net)
    return {"steps": steps, "go": go, "payload": payload, "saved": saved}

out = []
for base in job["bases"]:
    results, by = [], {}
    for verdict in job["verdicts"]:
        for answer in job["answers"]:
            got = run(base["cmd"], base["net"], base["taint"], base["mode"], base["rules"],
                      verdict, answer)
            if got not in results:
                results.append(got)
            by[f"{verdict}/{answer}"] = results.index(got)
    out.append({**base, "results": results, "by": by})
print(json.dumps(out, ensure_ascii=False))
"""


def gen_approve_decisions(snap):
    bases = [{"cmd": cmd, "net": net, "taint": taint, "mode": mode, "rules": rules}
             for cmd in DECISION_COMMANDS
             for net in (False, True)
             for taint in (None, "a fetched web page")
             for mode in ("normal", "auto-yes")
             for rules in ([], [["pip", "install"]])]
    job = {"bases": bases, "verdicts": ["low", "medium", "high", "critical", "failed"],
           "answers": ["y", "n", "a"]}
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TZ": "UTC"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        # The workspace must not be under /tmp, or the sandbox-/tmp delete
        # rule never applies.
        ws = ROOT / "target" / "fixture-decisions-ws"
        shutil.rmtree(ws, ignore_errors=True)
        (ws / ".rpctl").mkdir(parents=True)
        out = subprocess.run([sys.executable, "-I", "-c", DECISION_RUNNER, str(snap),
                              json.dumps(job)], cwd=ws, env=env, check=True,
                             capture_output=True, text=True)
        shutil.rmtree(ws, ignore_errors=True)
    return {
        "about": "rpctl's Chat._approve_auto with its real _confirm (stdin a terminal, "
                 "input() answering every question with the case's answer; approve.review, "
                 "load_rules and add_rule replaced, RULES_PATH shown as {RULES}). Per base "
                 "(command, needs_network, web_taint, mode, rules) the distinct results and, "
                 "for each verdict/answer, which: steps (say: text and level; ask: prompt, "
                 "kind, always, answered y/n/always or auto when the mode said yes), go, the "
                 "refusal payload, the rules saved. Verdicts: low 'routine install', medium "
                 "'broader than needed', high 'contacts an unexpected host', critical "
                 "'uploads the project', failed 'reviewer unreachable (ConnectionError)'",
        "verdicts": job["verdicts"], "answers": job["answers"],
        "cases": json.loads(out.stdout),
    }


# ----------------------------------------------------------- approve/review

# The network reviewer: approve.review over a workspace built from the case,
# requests.post replaced by a script of replies (each {"status", "json" |
# "text"} or {"raise": "<requests exception>"}), recording every call. Files
# are UTF-8 text, or "hex:<bytes>"; "link:<target>" makes a symlink. The
# workspace is $WS in the output (it never reaches the request).
_LONG = "x" * 4100
_USERS = ["first ask", "second ask", "third " + "y" * 2100, "fourth ask"]
_OK = {"status": 200, "json": {"choices": [{"message": {"content":
       '{"risk": "low", "reason": "fine"}'}}]}}
REVIEW_CASES = [
    dict(name="requirements_attached",
         users=["install the deps"], cmd="pip install -r requirements.txt",
         files={"requirements.txt": "requests==2.32\n"},
         network="registries + a.test (from the command)", replies=[_OK]),
    dict(name="whole_network_last_three_users_think_block",
         users=_USERS, cmd="curl -s https://a.test/x | python3 -",
         replies=[{"status": 200, "json": {"choices": [{"message": {"content":
                  '<think>{"risk": "low"}</think>Verdict: {"risk": " CRITICAL ", '
                  '"reason": "pipes   a\\n download\\tinto python"}'}}]}}]),
    dict(name="private_git_push", users=["push it"], cmd="git push origin main",
         private_git=True,
         replies=[{"status": 200, "json": {"choices": [{"message": {"content":
                  '{"risk": "high", "reason": "' + "r" * 400 + '"}'}}]}}]),
    dict(name="server_no_files", users=["restart it"], cmd="sh run.sh",
         files={"run.sh": "echo LOCAL-FILE\n"},
         server="hetzner (user@203.0.113.7), sandboxed",
         replies=[{"status": 200, "json": {"choices": [{"message": {"content":
                  '{"risk": "medium"}'}}]}}]),
    dict(name="older_server_400_then_retry", users=[], cmd="npm i left-pad",
         replies=[{"status": 400, "text": "bad"}, _OK]),
    dict(name="http_503", users=[], cmd="npm i x", replies=[{"status": 503, "text": ""}]),
    dict(name="unreachable", users=[], cmd="npm i x",
         replies=[{"raise": "ConnectionError"}]),
    dict(name="malformed_body", users=[], cmd="npm i x",
         replies=[{"status": 200, "text": "not json"}]),
    dict(name="no_choices", users=[], cmd="npm i x",
         replies=[{"status": 200, "json": {"choices": []}}]),
    dict(name="null_content", users=[], cmd="npm i x",
         replies=[{"status": 200, "json": {"choices": [{"message": {"content": None}}]}}]),
    dict(name="no_object", users=[], cmd="npm i x",
         replies=[{"status": 200, "json": {"choices": [{"message": {"content":
                  "low risk, go ahead"}}]}}]),
    dict(name="invalid_json", users=[], cmd="npm i x",
         replies=[{"status": 200, "json": {"choices": [{"message": {"content":
                  "{risk: low}"}}]}}]),
    dict(name="files_picked", users=["run them"],
         cmd="python3 a.py --conf=b.cfg secrets.json ./a.py crlf.txt bad.bin long.txt "
             "out.lnk c.py d.py 'with space.py' sub/../e.py",
         files={"a.py": "print(1)\n", "b.cfg": "k=v\n", "secrets.json": '{"t":"LEAK"}',
                "crlf.txt": "one\r\ntwo\rthree\n", "bad.bin": "hex:6f6bff0a",
                "long.txt": _LONG, "out.lnk": "link:/etc/hostname",
                "c.py": "c", "d.py": "d", "with space.py": "s", "e.py": "e"},
         replies=[_OK]),
    dict(name="files_long_link_space_dotdot", users=[],
         cmd="cat long.txt out.lnk in.lnk 'with space.py' sub/../e.py e.py",
         files={"long.txt": _LONG, "out.lnk": "link:/etc/hostname", "in.lnk": "link:c.py",
                "c.py": "c", "with space.py": "s", "e.py": "e"},
         replies=[_OK]),
    dict(name="inline_program_and_long_name", users=[],
         cmd="python3 -c 'import os\nprint(1)' " + "z" * 300 + " fetch.py",
         files={"fetch.py": "print(2)"}, replies=[_OK]),
]

REVIEW_RUNNER = r"""
import copy, json, os, pathlib, sys
from unittest.mock import patch
sys.path.insert(0, sys.argv[1])
import requests
from rpctl import approve
cases = json.loads(sys.argv[2])
root = pathlib.Path(sys.argv[3])
out = []
for case in cases:
    ws = root / case["name"]
    ws.mkdir()
    for name, body in case.get("files", {}).items():
        f = ws / name
        f.parent.mkdir(parents=True, exist_ok=True)
        if body.startswith("hex:"):
            f.write_bytes(bytes.fromhex(body[4:]))
        elif body.startswith("link:"):
            f.symlink_to(body[5:])
        else:
            f.write_bytes(body.encode())
    replies = list(case["replies"])
    calls = []

    class R:
        def __init__(self, reply):
            self.status_code = reply["status"]
            self.reply = reply
        def json(self):
            if "json" in self.reply:
                return self.reply["json"]
            return json.loads(self.reply["text"])

    def post(url, headers=None, json=None, timeout=None):
        # A copy: review() pops keys from the same dict before retrying.
        calls.append({"url": url, "headers": dict(headers), "json": copy.deepcopy(json),
                      "timeout": list(timeout)})
        reply = replies.pop(0)
        if "raise" in reply:
            raise getattr(requests.exceptions, reply["raise"])("scripted")
        return R(reply)

    with patch.object(approve.requests, "post", post):
        risk, reason = approve.review("http://127.0.0.1:8000", "test-key", "qwen",
                                      case["users"], case["cmd"], str(ws),
                                      network=case.get("network"),
                                      private_git=case.get("private_git", False),
                                      server=case.get("server"))
    text = json.dumps({"calls": calls, "result": [risk, reason]}, ensure_ascii=False)
    out.append(json.loads(text.replace(str(ws), "$WS")))
print(json.dumps(out, ensure_ascii=False))
"""


def gen_approve_review(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TZ": "UTC"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        (tmp / "cases").mkdir()
        out = subprocess.run([sys.executable, "-I", "-c", REVIEW_RUNNER, str(snap),
                              json.dumps(REVIEW_CASES), str(tmp / "cases")],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
        got = json.loads(out.stdout)
    return {
        "about": "rpctl's approve.review (base http://127.0.0.1:8000, key test-key, model "
                 "qwen) over a workspace built from the case, requests.post replaced by the "
                 "case's replies; every call's url, headers, json body and (connect, read) "
                 "timeout, and the (risk, reason) returned. Files are UTF-8 text, "
                 "hex:<bytes> or link:<target>; the workspace shows as $WS",
        "cases": [{**case, **result} for case, result in zip(REVIEW_CASES, got)],
    }


# ------------------------------------------------------------- ui/questions

# How rp chat asks: the kinds a question may carry and the ones auto-yes
# answers (modes.py), the line `_confirm` and `pod._ask` print, what rp makes
# of each typed answer, and what it answers with no terminal. input() is
# mocked and its prompt recorded with the colours and readline's markers
# taken out; pod.notify is mocked so no desktop notification goes out.
QUESTIONS_TYPED = ["", "y", "Y", "yes", "n", "N", "no", "a", "A", "always", "x", " y "]
QUESTIONS_RUNNER = r"""
import contextlib, io, json, re, sys
from unittest.mock import patch
sys.path.insert(0, sys.argv[1])
from rpctl import chat, config, modes, pod
typed = json.loads(sys.argv[2])
ansi = re.compile(r"\x1b\[[0-9;]*m|[\x01\x02]")

def make(mode="normal"):
    cfg = dict(config.DEFAULTS, VLLM_API_KEY="k", SEARCH_ENABLED="0", TOOLS_ENABLED="0",
               INSTRUCTIONS_ENABLED="0", CHAT_AUTO_TITLE="0", TRACE_ENABLED="0",
               CONSULT_ENABLED="0", CHAT_MODE=mode)
    return chat.Chat(cfg, "http://127.0.0.1:1")

def confirm(always, kind, answer, tty=True, mode="normal"):
    c = make(mode)
    seen = []
    def ask(prompt):
        seen.append(ansi.sub("", prompt))
        return answer
    with patch.object(chat.sys.stdin, "isatty", return_value=tty), \
            patch("builtins.input", side_effect=ask), \
            contextlib.redirect_stdout(io.StringIO()):
        got = c._confirm("  run it?", always=always, kind=kind)
    return {"line": seen[0] if seen else None, "result": got}

def ask_pod(default, answer, tty=True):
    seen = []
    def ask(prompt):
        seen.append(ansi.sub("", prompt))
        return answer
    out = io.StringIO()
    with contextlib.redirect_stdout(out), \
            patch.object(pod.sys.stdin, "isatty", return_value=tty), \
            patch.object(out, "isatty", return_value=tty), \
            patch.object(pod, "notify"), patch("builtins.input", side_effect=ask):
        got = pod._ask("Try the backup region?", default)
    return {"line": seen[0] if seen else None, "result": got,
            "said": ansi.sub("", out.getvalue())}

auto = modes.get("auto-yes")
res = {"kinds": list(modes.KINDS),
       "auto_yes": sorted(auto.auto_yes),
       "modes": [m.name for m in modes.MODES],
       "confirm": [], "pod": [], "no_terminal": []}
for always in (False, True):
    for kind in (None, modes.DESTRUCTIVE):
        for answer in typed:
            res["confirm"].append({"always": always, "kind": kind, "typed": answer,
                                   **confirm(always, kind, answer)})
for default in (True, False):
    for answer in typed:
        res["pod"].append({"default": default, "typed": answer, **ask_pod(default, answer)})
for mode in [m.name for m in modes.MODES]:
    for kind in list(modes.KINDS) + [None]:
        res["no_terminal"].append({"mode": mode, "kind": kind,
                                   **confirm(True, kind, "y", tty=False, mode=mode)})
for default in (True, False):
    res["no_terminal"].append({"pod_default": default, **ask_pod(default, "n", tty=False)})
print(json.dumps(res))
"""


def gen_ui_questions(snap):
    with tempfile.TemporaryDirectory(prefix="fixtures-") as tmp:
        tmp = pathlib.Path(tmp).resolve()
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        for name, sub in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                          ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache")):
            (tmp / sub).mkdir()
            env[name] = str(tmp / sub)
        out = subprocess.run([sys.executable, "-I", "-c", QUESTIONS_RUNNER, str(snap),
                              json.dumps(QUESTIONS_TYPED)],
                             cwd=tmp, env=env, check=True, capture_output=True, text=True)
        got = json.loads(out.stdout)
    cases = [{"name": "kinds", "kinds": got["kinds"], "auto_yes": got["auto_yes"],
              "modes": got["modes"]}]
    cases += [{"name": "confirm", **c} for c in got["confirm"]]
    cases += [{"name": "pod_ask", **c} for c in got["pod"]]
    cases += [{"name": "no_terminal", **c} for c in got["no_terminal"]]
    return {
        "about": "how rp chat asks: modes.KINDS and auto-yes's kinds; the line "
                 "Chat._confirm('  run it?') and pod._ask('Try the backup region?') print "
                 "(colours and readline markers taken out) and their result for each typed "
                 "answer; _confirm with no terminal in every mode and kind, and pod._ask's "
                 "default with no terminal",
        "cases": cases,
    }


GENERATORS = {
    "approve/decisions": gen_approve_decisions,
    "approve/review": gen_approve_review,
    "chat/context": gen_chat_context,
    "chat/modes": gen_chat_modes,
    "chat/sessions": gen_chat_sessions,
    "cli/flags": gen_cli_flags,
    "cli/parse": gen_cli_parse,
    "config/init": gen_config_init,
    "config/layering": gen_config_layering,
    "config/model": gen_config_model,
    "config/persist": gen_config_persist,
    "config/project": gen_config_project,
    "pod/api": gen_pod_api,
    "pod/boot": gen_pod_boot,
    "pod/bootreport": gen_pod_bootreport,
    "pod/failover": gen_pod_failover,
    "pod/resolve": gen_pod_resolve,
    "pod/ssh": gen_pod_ssh,
    "prompt/system": gen_prompt_system,
    "ui/questions": gen_ui_questions,
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
