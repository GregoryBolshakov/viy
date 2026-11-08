# viy

A Rust harness for abliterated models, treats them as hostile, but keeps them
useful.

The GitHub page tells the lore behind the name much better:
<https://github.com/GregoryBolshakov/viy>

## Install

```sh
cargo install viy --locked
viy init      # marks the current folder as a project
```

It needs Rust 1.97.1 or newer. You also need a RunPod API key in viy's
config, and `ssh` for the tunnel.

To build from a clone instead, run `cargo install --path crates/viy --locked`.
rustup picks up the Rust version pinned in `rust-toolchain.toml`.

## Usage

Run `viy` as you run any other agentic terminal. The interface should feel
familiar. Everything happens inside it, with commands such as `/resume`,
`/compact`, `/rewind` and many others. `/help` lists them all.

The core difference from other CLIs is `/up` and `/down`. The GPU is yours,
you rent it, so you start it and end it. `/up` creates or resumes the pod and
waits for the model, `/down` terminates it. The weights stay on the volume,
and a pod is billed by the hour until you end it.

viy also works without a running pod. You can't send new prompts then, but
you can still go through your chats and read and search your old messages.

## Why

Abliterated models have their refusals removed, and they can do real harm.
Many people think they should not be run at all. But it is still a tool, and
people will find ways to run these models. Nobody can control that. What we
can control is how predictable the tool is, and how well it holds up against
side effects nobody asked for and against people who steer the model from
outside, with prompt injection, poisoned packages or links built to leak your
data, for example.

Holding the model itself back matters just as much. Each new generation of
abliterated models is more capable than the last, and soon one may be able to
escape a sandbox on its own and do damage nobody can predict. viy is built
with that in mind.

## License

viy is source-available under the viy License, see `LICENSE`. In short:

- You can use viy for free, change it and share your changes, also in your
  paid work as a developer.
- You cannot sell it, host it as a service, bundle it into a paid product,
  take money from providers for its defaults, or use its code to build a
  competing tool.
- A copy with more than 50 users must keep the viy name and look.

For anything else, such as a commercial license, write to me. One file
derived from Ratatui keeps its MIT license, see `THIRD_PARTY_NOTICES`.

## Contributing

Issues and pull requests are welcome. By sending a contribution you agree to
section 6 of the license: I may use it in viy under any terms, and everyone
else gets it under the viy License.
