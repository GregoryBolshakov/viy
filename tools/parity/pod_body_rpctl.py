"""The create-pod body `rp up` would send, written to a file and never sent:
rpctl's config as `rp up` loads it in the working directory, its endpoint
key as configured (none is made), the SSH key rp finds, the region and
volume given, as `pod._create_pod`'s `attempt` builds it. For comparing
with viy's (crates/viy-pod/examples/pod_body.rs, kb f116); the file holds
the endpoint key, so it is written 0600.

    python3 tools/parity/pod_body_rpctl.py RPCTL_TREE OUT REGION VOLUME_ID
"""
import json
import os
import sys


def main(argv):
    tree, out, region, volume = argv
    sys.path.insert(0, tree)
    from rpctl import boot, config, pod
    cfg = config.load()
    key = cfg.get("VLLM_API_KEY") or ""
    if not key:
        sys.exit("VLLM_API_KEY is not set in rpctl's config")
    body = boot.build_pod_body(dict(cfg, DATA_CENTER_ID=region), key, volume,
                               pod.find_ssh_pubkey())
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(body, f)


if __name__ == "__main__":
    main(sys.argv[1:])
