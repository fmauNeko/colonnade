# Colonnade

Colonnade is a database manager for many DBMS. It is cross-platform and written in Rust, with a native, GPU-accelerated interface built on [GPUI](https://gpui.rs) through [GPUI Kit](https://gpui-kit.com).

> Colonnade is at a very early stage: the application only opens its main window for now.

## Download

Every [GitHub release](https://github.com/fmauNeko/colonnade/releases) ships an archive per platform, each with a `.sha256` checksum:

| Platform                      | Archive                                                         |
| ----------------------------- | --------------------------------------------------------------- |
| macOS (Apple silicon / Intel) | `colonnade-vX.Y.Z-{aarch64,x86_64}-apple-darwin.zip`            |
| Linux (x86_64 / arm64)        | `colonnade-vX.Y.Z-{x86_64,aarch64}-unknown-linux-gnu.tar.gz`    |
| Windows (x64 / arm64)         | `colonnade-vX.Y.Z-{x86_64,aarch64}-pc-windows-msvc.zip`         |

The builds are not code-signed yet:

- **macOS** (11 or newer): unzip `Colonnade.app`, move it to `/Applications`, then clear the quarantine flag Gatekeeper sets on downloads: `xattr -dr com.apple.quarantine /Applications/Colonnade.app`.
- **Windows**: SmartScreen may warn on first launch; choose _More info_ then _Run anyway_.
- **Linux**: the archive contains the `colonnade` binary and a desktop entry (`ovh.dissidence.colonnade.desktop`). The binary needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13, Fedora 40 and later) and an X11 or Wayland desktop. It links `libxcb`, `libxkbcommon` and `libxkbcommon-x11`, and renders through Vulkan (EGL as a fallback): install your distribution's Vulkan driver, such as `mesa-vulkan-drivers`, if the window does not open.

## Development

[mise](https://mise.jdx.dev) manages the whole toolchain (Rust and the dev tools) from `mise.toml`, and its tasks are the project's commands:

```bash
mise install    # Rust + dev tools; also installs the commit-msg git hook
mise run run    # build and start Colonnade
mise tasks      # list every task
```

System requirements:

- **macOS**: the Xcode Command Line Tools (`xcode-select --install`); full Xcode is not needed.
- **Linux** (Debian/Ubuntu): GPUI's system libraries, installed with `mise run deps:linux`.
- **Windows**: the MSVC build tools and the Windows SDK.

| Task                  | Description                                                                     |
| --------------------- | ------------------------------------------------------------------------------- |
| `run`                 | Run Colonnade (debug build)                                                     |
| `build`               | Debug build                                                                     |
| `build:release`       | Release build                                                                   |
| `package`             | Package the release build into `dist/` (archive + SHA-256) for the host platform |
| `fmt` / `fmt:check`   | Format / check formatting                                                       |
| `clippy`              | Clippy with warnings as errors                                                  |
| `typos`               | Spell check                                                                     |
| `deny`                | Dependency audit: advisories, licenses, bans, sources (`deny.toml`)             |
| `lint:workflows`      | actionlint + zizmor on the GitHub Actions workflows                             |
| `lint:commits [range]` | Conventional Commits check of a git range (default `origin/develop..HEAD`)     |
| `lint`                | Every static check above except `lint:commits`                                  |
| `test` / `test:cov`   | Tests / tests with a coverage summary                                           |
| `ci`                  | `lint`, `test:cov` and `build`: what CI runs on develop                         |

### Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org): they decide the next version and feed the changelog. A commit-msg hook, installed by `mise install`, checks every message with [cocogitto](https://docs.cocogitto.io). `mise exec -- cog commit <type> "<description>" [scope]` builds a valid message for you, for example `mise exec -- cog commit feat "add connection dialog"`.

## Branching and releases

| Branch             | Purpose                                                        |
| ------------------ | -------------------------------------------------------------- |
| `main`             | Stable releases                                                |
| `develop`          | Integration branch, the bleeding edge; Renovate targets it     |
| `feature/*`, `fix/*` | Work in progress, merged into `develop` through pull requests |

1. Changes reach `develop` through pull requests. CI checks commit messages, lint, tests with coverage and the build, on Linux.
2. To release, merge `develop` into `main` with a pull request.
3. On `main`, [release-please](https://github.com/googleapis/release-please) opens or updates a release pull request that bumps the version in `Cargo.toml`/`Cargo.lock` and updates `CHANGELOG.md`.
4. Merging the release pull request tags `vX.Y.Z`, publishes the GitHub release, builds the archives for macOS, Linux and Windows (x86_64 and arm64) and attaches them, then merges `main` back into `develop`.

[Renovate](https://docs.renovatebot.com) keeps Cargo dependencies, the mise tools and the GitHub Actions up to date (`renovate.json`).

## License

[MIT](LICENSE)
