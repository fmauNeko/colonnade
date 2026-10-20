# Colonnade

Colonnade is a database manager for many DBMS. It is cross-platform and written in Rust, with a native, GPU-accelerated interface built on [GPUI](https://gpui.rs) through [GPUI Kit](https://gpui-kit.com).

## Features

- **Connections sidebar**: saved connections in nested folders, ordered freely with drag and drop (or Alt+Up / Alt+Down). Create, edit, duplicate and delete connections and folders from forms and context menus; paste a connection URL to fill the form; test a connection before saving it.
- **Connection colors**: each connection gets a color generated from its driver, host and port (or SQLite file path). Pick any color or a preset, or reset to the generated one.
- **Databases**: PostgreSQL, MySQL, MariaDB and SQLite. Expanding a connection lazily loads its databases, schemas, tables and views.
- **Table browsing**: a virtualized grid, loaded 200 rows at a time while scrolling, with server-side sort on header click and refresh.
- **Grouped tabs**: table and query tabs from many connections at once, kept together under a header strip in each connection's color.
- **Query tabs**: an SQL editor highlighted for the connection's dialect. Run the selection or the whole text (`secondary-enter`), stop a running query (`secondary-.`), and get a result grid (up to 10,000 rows) or an affected-row count per statement.
- **Zed-like themes**: built-in Default Light and Default Dark, plus Kit and Zed theme files (see [Themes](#themes)).
- **Window per connection**: an optional mode where each connection gets its own window, its title bar, status bar, sidebar edge and active tab tinted Peacock-style from the connection color. Switching modes keeps every open tab.

`secondary` is Cmd on macOS and Ctrl elsewhere.

Not available yet: editing table data, filters, export, SQL autocomplete, SSH tunnels, SQL Server, and restoring tabs after a restart.

### Notes

- MySQL 8 accounts using `caching_sha2_password` need TLS (on by default); RSA key exchange is disabled. Accounts with `mysql_native_password` work without TLS.
- SQLite files are opened as they are: Colonnade never creates a missing file and never changes its journal mode.
- Closing a tab, a tab group or a window, deleting a connection, or quitting through Colonnade's own Quit command (menu or `secondary-q`) asks before discarding typed SQL or a running query. A quit started outside the app (Dock or taskbar Quit, logout, shutdown, `kill`) can't be intercepted.

## Configuration

Colonnade stores its configuration as human-readable JSON in one directory, picked in this order:

1. `COLONNADE_CONFIG_DIR`, when set and non-empty.
2. `$XDG_CONFIG_HOME/colonnade`, when `XDG_CONFIG_HOME` is set to an absolute path (on every platform).
3. `~/.config/colonnade` on macOS and Linux, `%APPDATA%\Colonnade` on Windows.

| Path               | Content                                              |
| ------------------ | ---------------------------------------------------- |
| `settings.json`    | Theme mode and choices, window mode                  |
| `connections.json` | Connections, folders, their order and expansion      |
| `themes/`          | Your theme files                                     |

Passwords never go to these files: each connection keeps its password in the OS keychain, or asks for it each time (kept in memory until you disconnect). Writes are atomic. A file that can't be read is moved aside as `<name>.corrupt-<timestamp>` and reported. A file written by a newer Colonnade version is used read-only and never overwritten.

## Themes

Drop Kit theme files or Zed theme-family files into `themes/`: they appear without a restart and style both the UI and SQL highlighting. Invalid files are reported, not fatal. Open the theme selector with `secondary-k secondary-t`: it previews themes live, Esc reverts and Enter saves. The theme mode follows the system or forces light or dark, with separate light and dark choices. Settings open with `secondary-,`.

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
| `db:up` / `db:down`   | Start / stop and wipe the PostgreSQL, MySQL and MariaDB test servers (Docker)    |
| `test:db`             | Driver tests against the `db:up` servers (Docker required); fails if none ran   |
| `ci`                  | `lint`, `test:cov` and `build`; CI also runs `test:db` in its `db-tests` job     |

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
