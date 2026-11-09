#!/bin/sh
# Installs viy from crates.io:
#
#   curl -fsSL https://raw.githubusercontent.com/GregoryBolshakov/viy/main/install.sh | sh
#
# It installs Rust with rustup when cargo is missing, builds viy with
# `cargo install viy --locked` and tells you what else viy needs.
# VIY_VERSION=0.2.0 installs that version instead of the newest.
set -eu

MIN_RUST=1.97.1

say() { printf 'viy-install: %s\n' "$1"; }
die() { say "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# 0 when version $1 is at least $2 (both x.y.z).
version_ge() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -n1)" = "$2" ]
}

main() {
    user_path=$PATH
    [ "$(uname -s)" = Linux ] || die "viy runs on Linux only (its sandbox is bubblewrap)."
    have cc || die "a C compiler is needed to build viy. Install gcc or clang, then run this again."

    if ! have cargo && [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
    fi
    if ! have cargo; then
        have curl || die "curl is needed to install Rust."
        say "cargo not found, installing Rust with rustup"
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs |
            sh -s -- -y --profile minimal --no-modify-path
        PATH="$HOME/.cargo/bin:$PATH"
    fi

    rust=$(rustc --version | cut -d' ' -f2)
    if ! version_ge "$rust" "$MIN_RUST"; then
        if have rustup; then
            say "Rust $rust is older than $MIN_RUST, updating the stable toolchain"
            rustup toolchain install stable --profile minimal
            rustup default stable
        else
            die "viy needs Rust $MIN_RUST or newer, this cargo has $rust. Install Rust with rustup (https://rustup.rs) and run this again."
        fi
    fi

    if [ -n "${VIY_VERSION:-}" ]; then
        cargo install viy --locked --version "$VIY_VERSION"
    else
        cargo install viy --locked
    fi

    missing=""
    have ssh || missing="$missing ssh (the tunnel to the pod)"
    have bwrap || missing="$missing, bubblewrap (the sandbox for the model's commands)"
    if [ -n "$missing" ]; then
        say "viy also needs:${missing#,}"
    fi

    case ":$user_path:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) say "add $HOME/.cargo/bin to your PATH" ;;
    esac
    say "done. Put your RunPod API key in ~/.config/viy/config as RUNPOD_API_KEY=..., then run viy in a project folder."
}

main "$@"
