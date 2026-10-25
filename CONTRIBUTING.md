# Contributing

Thank you for looking at dvb. Bug reports and ideas are welcome as
[issues](https://github.com/broken-branch/dvb/issues).

## Pull requests

The maintainers read pull requests from outside contributors and, if they want the
change, re-make it in a separate development repository; outside pull requests
are not merged as they are. Workflows on an outside pull request run only after a
maintainer approves them.

Commit with your GitHub noreply address as author and committer
(`login@users.noreply.github.com` or `id+login@users.noreply.github.com`): CI's
identity check refuses any other address.

Report security problems privately, as [SECURITY.md](SECURITY.md) describes.

## Build and check

Read the [testing guide](docs/testing.md) for prerequisites. `scripts/check.sh`
requires Node 26, the stable Rust toolchain (rustup's `stable`, as
[rust-toolchain.toml](rust-toolchain.toml) selects), cargo-nextest 0.9.148 and
cargo-deny 0.20.2:

```sh
cargo install --locked cargo-nextest --version 0.9.148
cargo install --locked cargo-deny --version 0.20.2
```

Set `CARGO_TARGET_DIR` to a private, writable build directory, then run the same
gates as CI:

```sh
scripts/check.sh            # everything
scripts/check.sh rust       # or one part: rust, ui or contracts
```

Start with the [code maps](docs/map/README.md) to find the code for a change, and
follow the conventions in [AGENTS.md](AGENTS.md).
