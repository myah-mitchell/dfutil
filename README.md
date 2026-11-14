# dfapp

dfapp installs command-line tools and deploys dotfiles from a content repo you keep in git. One `dfapp.toml` in that repo lists the tools, their version pins, and presets that group them; each host picks presets in its own `host.toml`. It runs as a full-screen terminal UI, or headless with `--json` output for ansible and scheduled runs.

## What it does

- Installs tools from GitHub release assets (with checksum verification when the release publishes one), crates.io through `cargo install`, npm, `uv tool install`, or a script, into versioned directories with symlinks into `~/.local/bin` (or `/usr/local/bin` for a system-wide install).
- Updates only the tools whose pinned version moved, or reinstalls all or some of them, and keeps the previous version for `dfapp rollback`.
- Pins a tool to a major, minor, or exact version, an exact tag, a tag prefix, or the latest release.
- Symlinks dotfile packages into `$HOME`. Files ending `.tmpl` are rendered with template variables and the active color palette first, and `dfapp capture` maps edits made to a rendered file back into its template.
- Layers secondary content repos, public or private, over the main one.
- Shows incoming and outgoing changes for every repo, syncs them while stashing and reapplying local edits, and blocks commits that would leak a secret variable's value.
- Generates shell init scripts for zsh, bash, and nushell.
- Runs `dfapp update` on a systemd user timer, or prints a cron line where there's no systemd user session.

Linux (x86_64 and aarch64, WSL2 included) is supported. Platform handling is kept in one module so macOS can be added later: the config already accepts `macos` platform keys, and the missing piece is a launchd agent for scheduled updates.

## Requirements

- A Rust toolchain to build. dfapp is developed and tested with Rust 1.99.
- `git` on the target host.
- `cargo`, `npm`, or `uv` only for tools installed that way. dfapp can install node and uv itself as ordinary tools.

## Build

```bash
cargo build --release
install -m 0755 target/release/dfapp ~/.local/bin/dfapp
```

## Quick start

The `example/` directory is a complete content repo. To try it in a scratch home without touching your own:

```bash
cp -r example /tmp/dfapp-demo-repo
git -C /tmp/dfapp-demo-repo init -q
git -C /tmp/dfapp-demo-repo add .
git -C /tmp/dfapp-demo-repo commit -qm init
export DFAPP_HOME=/tmp/dfapp-demo-home
dfapp init --repo /tmp/dfapp-demo-repo
dfapp var set git_name "Your Name"
dfapp var set git_email you@example.com
dfapp status
dfapp apply
```

`DFAPP_HOME` points every path dfapp uses (install prefix, state, config) at that directory. Leave it unset on a real host.

On a real host, point `init` at your content repo, either a checkout or a URL to clone:

```bash
dfapp init --repo git@github.com:<you>/<dotfiles>.git --preset base,claude
dfapp apply
```

Running `dfapp` with no arguments on a terminal opens the UI.

## Commands

| Command | Does |
| --- | --- |
| `init --repo <path-or-url>` | Writes `~/.config/dfapp/host.toml`, cloning the repo first when given a URL. On a terminal it asks for presets and for variables without a default |
| `status [--all]` | Lists tools with installed and available versions |
| `plan`, `apply` | Shows or makes the changes: installs, upgrades, removals, dotfile links. `--reinstall [names]` and `--only <names>` narrow it |
| `install`, `uninstall <tools>` | One-off changes on this host, saved to `host.toml` |
| `reinstall [tools]` | Reinstalls the named tools, or every selected tool |
| `rollback <tool>` | Switches back to the version before the last install. Updates then skip the version rolled back from until a newer one is out, or until you reinstall the tool |
| `deploy [--dry-run] [--force]` | Links and renders dotfiles |
| `capture [--dry-run]` | Copies edits made to rendered files back into their templates |
| `diff incoming`, `diff outgoing` | Shows upstream changes, or local changes not yet pushed |
| `sync` | Captures edits, pulls every repo with local changes stashed and reapplied, and redeploys |
| `update` | `sync`, then installs whatever changed. This is what the schedule runs |
| `tool add`, `preset create`, `preset select` | Edits `dfapp.toml` or `host.toml`, keeping comments |
| `palette list`, `palette set <name>` | Switches the color palette and re-renders templates |
| `var list`, `var set <name> <value>` | Template variables for this host |
| `hook install` | Installs the pre-commit hook that blocks secret values |
| `schedule enable [--frequency daily\|weekly]`, `disable`, `status` | Scheduled updates. `--interval` is an alias for `--frequency` |
| `config validate`, `config show` | Checks the configuration, or prints this host's merged choices |

`apply`, `reinstall`, and `uninstall` ask for confirmation on a terminal and refuse without `--yes` when there's none.

## Terminal UI

`dfapp` with no arguments opens a panel view. Four panels stack down the left side (Tools, Presets, Dotfiles, and Repos), and a detail pane beside them describes whatever is selected, with buttons for the actions that apply to it. In a terminal narrower than 90 columns, the detail pane moves under the panels. A job strip above the hint bar shows the running action or the last result, and actions run one at a time.

Everything works with the mouse as well as the keyboard: click a panel, a line, a mark box, a hint chip, or a button; click the palette in the top bar to switch it; scroll the wheel over a list or the detail pane. Press `?` for the full key list.

The Tools panel groups tools by status, by preset, or by `category` (see the [configuration reference](docs/config.md#tools)). Press `g`, or click the group switch, to change the grouping. Groups that need attention start open.

| Keys | Where | Does |
| --- | --- | --- |
| `1` to `4`, `Tab` | Anywhere | Focuses a panel |
| `j`, `k`, `G` | Lists | Moves down, up, to the bottom |
| `J`, `K`, `Ctrl+d`, `Ctrl+u` | Anywhere | Scrolls the detail pane a line or a page |
| `a`, `U` | Anywhere | Applies every planned change, syncs then applies |
| `L`, `?`, `q` | Anywhere | Opens the log, opens help, quits |
| `Space`, `m`, `/` | Tools | Marks the tool or opens a group, marks every tool shown, filters |
| `h`, `l`, `g` | Tools | Closes a group, opens it, changes the grouping |
| `u`, `r`, `x`, `b` | Tools | Installs or updates, reinstalls, uninstalls, rolls back |
| `n` | Tools | Adds a tool to `dfapp.toml` |
| `Space`, `n` | Presets | Turns a preset on or off, creates one from the marked tools |
| `d`, `c`, `p`, `e` | Dotfiles | Deploys, captures edits, switches the palette, edits a variable |
| `s`, `R` | Repos | Syncs, checks again |

`Esc` clears the filter, then the marks, then quits.

## Use from ansible

Every command takes `--json`. The output always has a `changed` key, and commands that install or sync also have `failed`. The exit code is non-zero on failure.

```yaml
- name: Apply dfapp
  ansible.builtin.command: dfapp --json --yes apply
  register: dfapp_apply
  changed_when: (dfapp_apply.stdout | from_json).changed
```

Progress lines go to stderr when `--json` is set, so stdout stays parseable.

## Configuration

[docs/config.md](docs/config.md) covers `dfapp.toml`, `host.toml`, tool definitions, pins, presets, packages, templates, palettes, secondary sources, and where dfapp keeps its state.

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests run offline: tool installs use `kind = "script"` tools and `file://` downloads, and git operations use local repos in temporary directories.
