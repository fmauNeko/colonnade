# AGENTS

## Project Snapshot

| Aspect    | Details                                                                       |
| --------- | ----------------------------------------------------------------------------- |
| Type      | Single Rust crate (`colonnade`): library (`src/lib.rs`) + thin binary (`src/main.rs`), a cross-platform desktop application |
| Purpose   | Database manager: PostgreSQL, MySQL/MariaDB, SQLite (`sqlx` on a dedicated Tokio runtime) |
| UI        | [GPUI Kit](https://gpui-kit.com) (`gpui-kit` crate): GPUI + styled components |
| Stack     | Rust 2024 edition, toolchain and dev tools pinned in `mise.toml`              |
| Testing   | `cargo test` with `#[gpui_kit::test]` headless window tests; `#[ignore]` driver tests via `test:db` |
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
mise run db:up            # start the PostgreSQL/MySQL/MariaDB test servers (Docker)
mise run test:db          # driver tests against them (db:down stops and wipes them)
mise run ci               # everything the develop CI runs
mise run build:release && mise run package   # dist/ archive for the host platform
mise run deps:linux       # Debian/Ubuntu system libraries (Linux only)
```

Always go through `mise run` / `mise exec --` so the pinned toolchain is used: mise sets `RUSTUP_TOOLCHAIN`.

## Layout

| Path                         | Content                                                                      |
| ---------------------------- | ---------------------------------------------------------------------------- |
| `src/main.rs` | Thin entry point: calls `colonnade::app::run` |
| `src/lib.rs` | Crate root: every module is `pub mod` |
| `src/app.rs` | Bootstrap: init order, actions, key bindings, menus, guarded quit (`request_quit`) |
| `src/assets.rs` | Asset sources: Colonnade icons layered over the GPUI Kit defaults |
| `src/catalog/` | Connection catalog: `model` (connections, folders, tree), `ops` (tree operations), `store` (persistence), `url` (URL import) |
| `src/color.rs` | Generated connection colors, contrast and Peacock derivation |
| `src/db/` | Database core: `model`, `sql` (statement builder), `backend` (traits), `factory` (driver selection), `runtime` (Tokio bridge), `postgres`, `mysql` (MySQL/MariaDB), `sqlite`, `fake` (test-only backend) |
| `src/dialogs/` | Dialogs: `connection`, `folder`, `password`, `confirm`, `color` |
| `src/grid.rs` | Shared result grid: Kit table columns, cell display, row delegate |
| `src/keybindings.rs` | `bind`: key binding registration that resolves the `secondary-` alias for the host OS |
| `src/paths.rs` | Config dir resolution (Zed-style, XDG-aware) and config file paths |
| `src/persist.rs` | Atomic JSON persistence, corrupt-file backup, newer-version detection |
| `src/prompts.rs` | Prompt hooks for passwords, confirmations and notices (scripted in tests) |
| `src/query_tab/` | Query tab: `state` (SQL, run state, results), `view` (dialect editor, run, stop) |
| `src/secrets.rs` | Secret store and OS keychain integration (`Secret` with redacted `Debug`) |
| `src/sessions.rs` | Connection sessions: connect, passwords, schema cache, disconnect |
| `src/settings.rs` | Settings model and store (`settings.json`) |
| `src/settings_ui.rs` | Settings dialog and theme selector entry |
| `src/sidebar/` | Connections sidebar: `node` (tree nodes), `dnd` (drag and drop), `view` (tree, context menus, keyboard) |
| `src/sql_dialects/` | Dialect SQL highlighting languages (`postgres.scm`, `mysql.scm`, `sqlite.scm`) |
| `src/table_tab/` | Table tab: `state` (paging, server-side sort), `view` (toolbar, error panel, grid) |
| `src/themes/` | Themes: `catalog` (available themes, selection), `selector`, `watch` (live reload), `zed` (Zed converter), `fixtures/` (invented test themes) |
| `src/tint.rs` | Peacock-style chrome tint for connection windows |
| `src/windows.rs` | Window registry, window modes, guarded window lifecycle |
| `src/workspace/` | Workspace: `tabs` (tab store, close guards), `view_factory` (`TabViewFactory` registry), `tab_strip` (grouped tabs), `status_bar`, `view` (window view) |
| `script/test-db.sh` | Runs the `#[ignore]` driver tests per filter, fails if a filter ran none |
| `compose.yaml` | PostgreSQL 17, MySQL 8.4, MariaDB 11.4 test servers for `db:up` |
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
- Bind keys through `crate::keybindings::bind`, never `KeyBinding::new` with `secondary-` (it panics in GPUI >= 0.3.10; only `KeyBinding::load` resolves the alias). `window.press` in tests resolves `secondary-` already.
- `gpui-kit` pins an exact `gpui-pre` snapshot of Zed's GPUI, so any update can change GPUI's API. Renovate never automerges it.
- Docs: <https://gpui-kit.com> and <https://docs.rs/gpui-kit>.

### Testing

- Tests sit next to the code (`#[cfg(test)] mod tests`) and use Given/When/Then comments.
- UI behavior is tested in headless windows with `#[gpui_kit::test]` and `gpui_kit::test::TestWindowExt` (`find`, `click`, `press`, `input`, ...). Elements a test looks up need an `.id(...)` and `.test_support()`; `.test_support()` is inert outside tests.
- Assert behavior (visibility, state, callbacks), never UI copy.
- UI tests use the fake backend (`db::fake`) on the inline runtime: never a real Tokio runtime, `notify` watcher, OS keychain, native file picker or network.
- Never mutate the environment (`std::env::set_var` is unsafe in Rust 2024): environment-dependent code takes an injected lookup.
- Dialogs and confirmations go through `prompts` hooks, which tests replace with scripted answers.
- PostgreSQL/MySQL/MariaDB driver tests are `#[ignore]` and live under `db::postgres::` and `db::mysql::`; `mise run test:db` runs both filters (CI `db-tests` job). SQLite driver tests use temp files and run in the normal suite.
- `secrets::tests::keychain_round_trip` hits the real OS keychain: `#[ignore]`, run by hand, never in CI.

### Architecture

- App-level models (catalog, sessions, settings, tab store, theme catalog) are GPUI globals/entities shared by all windows; each window only holds views over them, so switching window modes never loses a tab, its text or a running query.
- Tab views come from the `TabViewFactory` registry (`workspace::view_factory`), keyed by tab kind and built lazily per window.
- Closing tabs, groups, windows or quitting runs the close guards in `workspace::tabs`, which confirm through `prompts` before discarding typed SQL or a running query.
- User themes come from `themes::catalog` with its own watcher (`themes::watch`); never call Kit's `ThemeRegistry::watch_dir` on the themes dir, it misreads Zed files.
- Kit's `Theme` is one global: never mutate it per window. Per-window color (Peacock tint) is drawn by Colonnade's chrome (`tint`).
- Only config files live in the config dir (`paths`). Future data, state or cache files must honor `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` the same way (absolute, non-empty values only).

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
      ├→ db-tests (test:db, PostgreSQL/MySQL/MariaDB service containers)
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
