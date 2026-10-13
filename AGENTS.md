# AGENTS

## Project Snapshot

| Aspect    | Details                                                                       |
| --------- | ----------------------------------------------------------------------------- |
| Type      | Single Rust crate (`colonnade`), a cross-platform desktop application         |
| Purpose   | Database manager for many DBMS                                                |
| UI        | [GPUI Kit](https://gpui-kit.com) (`gpui-kit` crate): GPUI + styled components |
| Stack     | Rust 2024 edition, toolchain and dev tools pinned in `mise.toml`              |
| Testing   | `cargo test` with `#[gpui_kit::test]` headless window tests                   |
| Linting   | rustfmt, Clippy (pedantic, `-D warnings`), typos, cargo-deny, actionlint, zizmor |
| Commits   | Conventional Commits, enforced by a cocogitto commit-msg hook                 |
| CI/CD     | GitHub Actions; release-please on `main`; Renovate on `develop`               |

## Quick Reference

```bash
mise install              # toolchain + dev tools (+ commit-msg hook)
mise run run              # start the app (debug)
mise run lint             # fmt:check, clippy, typos, lint:workflows, deny
mise run test             # tests
mise run test:cov         # tests with coverage summary
mise run ci               # everything the develop CI runs
mise run build:release && mise run package   # dist/ archive for the host platform
mise run deps:linux       # Debian/Ubuntu system libraries (Linux only)
```

Always go through `mise run` / `mise exec --` so the pinned toolchain is used: mise sets `RUSTUP_TOOLCHAIN`.

## Layout

| Path                         | Content                                                                      |
| ---------------------------- | ---------------------------------------------------------------------------- |
| `src/main.rs`                | Entry point: creates the GPUI application with the GPUI Kit assets           |
| `src/app.rs`                 | App setup (actions, key bindings, menus), main window, views and their tests |
| `script/package.sh`          | Release archive packaging for macOS, Linux and Windows                       |
| `packaging/`                 | macOS `Info.plist` template, Linux desktop entry                             |
| `.github/workflows/`         | `develop.yaml` (CI), `release-please.yaml` (release), `build.yaml` (archives) |
| `mise.toml`                  | Tool versions, tasks and the postinstall hook                                |
| `deny.toml`, `cog.toml`      | cargo-deny policy, cocogitto commit-msg hook                                 |

## Universal Conventions

### Code Style

- `cargo fmt` formatted and Clippy-clean with the lints in `Cargo.toml` (pedantic, `unwrap_used`, `expect_used`, `unsafe_code = "forbid"`).
- No `unwrap()`/`expect()` outside tests (`clippy.toml` allows them in tests). Propagate errors with `?`, or report them where they can be handled.
- Lint suppressions are a last resort: use `#[expect(lint, reason = "...")]` on the smallest item, never `#[allow]` without a reason, never crate-wide.
- Match exhaustively on enums you own; no `_ =>` arm.

### GPUI Kit

- Import GPUI through the Kit: `use gpui_kit::prelude::*;` plus explicit `gpui_kit::{...}` items. Styled components live in `gpui_kit::component`; theme colors come from `gpui_kit::component::ActiveTheme` (`cx.theme()`).
- Call `gpui_kit::init(cx)` before using components, and open windows with `gpui_kit::open_window`, which mounts the Kit `Root`.
- Define actions with `gpui_kit::actions!`.
- `gpui-kit` pins an exact `gpui-pre` snapshot of Zed's GPUI, so any update can change GPUI's API. Renovate never automerges it.
- Docs: <https://gpui-kit.com> and <https://docs.rs/gpui-kit>.

### Testing

- Tests sit next to the code (`#[cfg(test)] mod tests`) and use Given/When/Then comments.
- UI behavior is tested in headless windows with `#[gpui_kit::test]` and `gpui_kit::test::TestWindowExt` (`find`, `click`, `press`, `input`, ...). Elements a test looks up need an `.id(...)` and `.test_support()`; `.test_support()` is inert outside tests.
- Assert behavior (visibility, state, callbacks), never UI copy.

### Dependencies

- Prefer existing crates over new ones; every new dependency must pass `mise run deny` (licenses allowed in `deny.toml`, no unknown registries or git sources).
- New tools go in `mise.toml` `[tools]` with an exact version, so Renovate can update them.

## Branching & PR Workflow

| Branch      | Purpose             | Merges To            |
| ----------- | ------------------- | -------------------- |
| `main`      | Stable releases     | -                    |
| `develop`   | Integration branch  | `main` (via release) |
| `feature/*` | New features        | `develop`            |
| `fix/*`     | Bug fixes           | `develop`            |

### PR Requirements

1. Branch from `develop`.
2. Conventional commit messages (`feat:`, `fix:`, `docs:`, `ci(github):`, ...): they drive the version bump and the changelog.
3. `mise run ci` passes.
4. Update this file and `README.md` when behavior, tasks or structure change.

## CI/CD Pipeline

### On Push to `develop` / PR to non-main

File: `.github/workflows/develop.yaml`, Linux only (`ubuntu-24.04`):

```
commits (PRs only)
lint ─┬→ unit-tests (test:cov)
      └→ build
```

### On Push to `main` (Release)

Files: `.github/workflows/release-please.yaml`, `.github/workflows/build.yaml`

- release-please (`release-please-config.json`, `.release-please-manifest.json`) maintains the release PR: version in `Cargo.toml`/`Cargo.lock` and `CHANGELOG.md`. Tags are `vX.Y.Z`; before 1.0, breaking changes bump the minor version.
- When the release PR is merged: `build.yaml` builds and packages the six targets (macOS, Linux and Windows on x86_64 and arm64) and attaches the archives and checksums to the release; `backmerge` merges `main` into `develop`.
- `build.yaml` can also be dispatched manually: without a tag, it only uploads the archives as workflow artifacts.

## Definition of Done

- [ ] `mise run ci` passes
- [ ] New behavior is covered by a test that fails without the change
- [ ] UI changes checked by running the app (`mise run run`)
- [ ] No secrets committed
- [ ] `README.md` / `AGENTS.md` updated if behavior, tasks or structure changed
- [ ] Conventional commit messages
