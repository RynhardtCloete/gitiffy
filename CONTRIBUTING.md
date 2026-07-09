# Contributing to gittify

Thanks for your interest in contributing! Bug reports, feature ideas, and pull
requests are all welcome. Please read the licensing terms below before opening
a pull request, since this project is not under a standard open source license.

## Licensing of contributions

gittify is source-available under the
[PolyForm Noncommercial License 1.0.0](./LICENSE.md). The project may also be
offered under separate commercial terms by the copyright holder in the future.

By submitting a contribution (code, documentation, or any other material) to
this repository, you agree that:

1. You are the original author of the contribution, or you otherwise have the
   right to submit it under these terms.
2. Your contribution is licensed to the project under the PolyForm
   Noncommercial License 1.0.0.
3. You additionally grant Rynhardt Cloete a perpetual, worldwide,
   non-exclusive, royalty-free, irrevocable license to use, reproduce, modify,
   distribute, sublicense, and relicense your contribution, including under
   commercial terms.

Point 3 is what allows the project to offer a commercial edition one day
without tracking down every past contributor. If you are not comfortable with
these terms, please don't submit code; issues and bug reports are still very
welcome.

## Reporting issues

- Search existing issues first.
- Include your OS, how you launched the app (`gittify-egui`, `gittify-bin`),
  and steps to reproduce. For rendering or graph bugs, a screenshot and the
  repository shape that triggers it (branch/merge structure) help a lot.

## Development setup

You need the Rust toolchain pinned by `rust-toolchain.toml` (rustup picks it up
automatically) and a system `git` on your PATH, since the write path shells out
to it.

```
cargo test --workspace                                     # unit + integration tests
cargo clippy --workspace --all-targets                     # lints
cargo build --manifest-path crates/gg-ui-egui/Cargo.toml   # egui backend (excluded from workspace)
cargo run -p gittify-egui                                  # run the desktop app
cargo run -p gittify-bin -- /path/to/repo                  # CLI graph renderer
```

The `gg-ui-egui` and `gg-ui-gpui` crates are deliberately excluded from the
default workspace build; CI compiles them on their own lanes. See the
[README](./README.md#workspace-layout) for what each crate does.

## Design invariants

Pull requests that break these will be asked to restructure:

- **The UI toolkit and git backend are both swappable.** `gg-graph` and
  `gg-app` must never name a GPUI, egui, or `gix` type. Rendering goes through
  the `GraphCanvas` abstraction in `gg-ui-traits`.
- **`gix` never leaks past `gg-git-read`**, and `std::process` never leaks
  past `gg-git-write`. All git access composes through the `GitEngine` facade
  in `gg-git`.
- **Virtualization is mandatory.** The graph layout engine only computes rows
  scrolled into view. Don't introduce whole-history passes on the UI path.

## Pull requests

- Keep PRs focused; one change per PR.
- Run `cargo test --workspace` and `cargo clippy --workspace --all-targets`
  before pushing. CI also runs `cargo-deny`, so new dependencies must have
  compatible licenses and no advisories.
- Add tests for behavior changes, especially in `gg-graph` and the diff/git
  crates, which are pure and easy to test.
- Write commit messages in the imperative mood ("Add stash pop confirmation",
  not "Added...").
