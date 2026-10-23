# Contributing

Thanks for helping with berth. It's young, so issues that describe what you
tried to do are as welcome as pull requests.

## Setting up

You need Linux with cgroup v2 (Ubuntu 24.04 is the reference), Rust 1.88 or
newer, and `musl-tools`.

```sh
make                      # static release build
sudo make install         # berth, berthd, the systemd unit, the berth group
sudo usermod -aG berth "$USER"
sudo systemctl start berthd
sudo berth pull ubuntu:24.04
```

## Before you open a pull request

```sh
make lint    # cargo fmt --check, clippy with warnings as errors
make test    # unit and CLI tests
make e2e     # end-to-end, against your running berthd
```

CI runs all three on Ubuntu 24.04. A change to how berths are built, run or
isolated needs an end-to-end test in `tests/e2e.rs` that shows it from inside a
berth. A change to parsing or the wire needs a unit test beside the code.

## Conventions

- **Commits and PR titles** follow
  [Conventional Commits](https://www.conventionalcommits.org):
  `type(scope): summary`, lower-case and imperative, with no trailing period.
  The types are `feat`, `fix`, `refactor`, `test`, `docs`, `ci`, `build`,
  `chore`, `style` and `perf`. The scope is optional. When the summary can't
  say why, the body does.
- **Code** is formatted by `rustfmt` (see `rustfmt.toml`). Modules start with
  a `//!` line saying what they're for. Comments say why, not what.
- **Unsafe code** stays in small functions with a comment on what makes it
  sound. Most of it belongs in `src/sys`.
- **Design changes** (isolation, the API, on-disk layout) update
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) in the same pull request.

## Security

Please don't open public issues for ways out of a berth. See
[SECURITY.md](SECURITY.md).
