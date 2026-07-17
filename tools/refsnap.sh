#!/bin/sh
# Extract one rpctl commit, read-only, into target/rpctl-ref/<short>/.
#
#   tools/refsnap.sh            the commit named in tests/rpctl-ref.txt
#   tools/refsnap.sh <commit>   another one (to compare before re-pinning)
#
# git archive reads rpctl's repository without writing to it (a worktree would
# add metadata there). Result and run folders are left out: some of them hold
# replays of Gregory's chats (CLAUDE.md, "Gregory's chats are off limits").
set -eu
cd "$(git rev-parse --show-toplevel)"
REF=../Qwen3.8-27B-abliterated-setup
commit=${1:-$(sed -n 's/^commit: *//p' tests/rpctl-ref.txt)}
full=$(git -C "$REF" rev-parse --verify "$commit^{commit}")
short=$(git -C "$REF" rev-parse --short "$full")
dest=target/rpctl-ref/$short
if [ -d "$dest" ]; then echo "$dest"; exit 0; fi
rm -rf "$dest.tmp"
mkdir -p "$dest.tmp"
git -C "$REF" archive --format=tar "$full" -- . \
    ':(exclude)bench/results' ':(exclude)bench/workflows/runs' \
    ':(exclude)netaccess/evals/results' ':(exclude)transcripts' \
    | tar -x -C "$dest.tmp"
chmod -R a-w "$dest.tmp"
chmod u+w "$dest.tmp"
mv "$dest.tmp" "$dest"
chmod u-w "$dest"
echo "$dest"
