# viy

viy runs an abliterated Qwen3.8-27B on a rented RunPod GPU with vLLM and
gives you a chat in the terminal to work with it from any project folder.

It is a Rust rewrite of rp, a Python tool I use for the same job. rp is not
public. viy keeps its commands and its behaviour and adds a few things of its
own.

The name is Viy, from Gogol's 1835 story. Viy is the iron-faced chief of the
gnomes. He can do nothing until his eyelids are lifted, and then he sees what
is hidden. `viy up` lifts them.

## Status

Work in progress. What works today:

- Pod commands: `viy up`, `status`, `stop`, `down`, `logs`, `tunnel`, `boots`,
  `backup`, `volume`, and the idle reap that ends billing when you forget a pod
- The chat UI, with saved chats: `viy chat`, `resume`, `fork`, `archive`,
  `unarchive`, `delete`, `exec`
- A chat opens without a pod. You can read, search and resume old chats
  offline, and start the pod from inside the chat with `/up`

The model cannot run commands or edit files from the chat yet. The approval
rules and file guards for that are written, the tool loop that uses them is
next.

## Build

The Rust version is pinned in `rust-toolchain.toml`, rustup picks it up.

```sh
cargo install --path crates/viy --locked
viy init      # marks the current folder as a project
viy up        # creates or resumes the pod, waits for vLLM
viy chat
```

You need a RunPod API key in viy's config, and `ssh` for the tunnel. A pod is
billed by the hour, `viy stop` or `viy down` ends it.

## Tests

```sh
cargo test --workspace
```

Some tests compare viy with rp, call by call. They need rp's source next to
this repository and run only with the `oracle` cargo feature, so they are off
by default.

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
