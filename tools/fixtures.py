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


GENERATORS = {
    "config/init": gen_config_init,
    "config/layering": gen_config_layering,
    "config/model": gen_config_model,
    "config/persist": gen_config_persist,
    "config/project": gen_config_project,
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
