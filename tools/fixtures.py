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


GENERATORS = {
    "cli/flags": gen_cli_flags,
    "cli/parse": gen_cli_parse,
    "config/init": gen_config_init,
    "config/layering": gen_config_layering,
    "config/model": gen_config_model,
    "config/persist": gen_config_persist,
    "config/project": gen_config_project,
    "pod/api": gen_pod_api,
    "pod/boot": gen_pod_boot,
    "pod/resolve": gen_pod_resolve,
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
