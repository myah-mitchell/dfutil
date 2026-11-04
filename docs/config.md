# Configuration reference

This page describes the files dfapp reads: `dfapp.toml` in a content repo, palette files, dotfile packages, and the machine-local `host.toml`. It's for whoever maintains a content repo. The [`example/`](../example) directory is a working repo that uses most of what's described here.

## Contents

- [Files and precedence](#files-and-precedence)
- [host.toml](#hosttoml)
- [settings](#settings)
- [tools](#tools)
- [Version pins](#version-pins)
- [presets](#presets)
- [packages](#packages)
- [Templates and variables](#templates-and-variables)
- [Palettes](#palettes)
- [Secondary sources](#secondary-sources)
- [Sync and scheduled updates](#sync-and-scheduled-updates)
- [Where dfapp keeps things](#where-dfapp-keeps-things)

## Files and precedence

A content repo is a git repo with a `dfapp.toml` at its root, a `palettes/` directory, and one directory per dotfile package. Each host has a `host.toml`, by default at `~/.config/dfapp/host.toml`, that points at the repo and records this host's choices.

Where both set something, the host wins: `host.toml`'s `palette`, `scope`, `build`, and `vars` override the repo's `[settings]` and `[vars]` defaults. A [secondary source](#secondary-sources) layers its own `dfapp.toml` over the main one.

Config load fails on any unknown key, unknown reference (a preset naming a tool that doesn't exist, say), bad pin, or tool with no asset for the current platform. Each error names the file and key. Run `dfapp config validate` after editing.

## host.toml

`dfapp init` writes this file with mode `0600`, since it can hold secret variable values. `dfapp install`, `uninstall`, `preset select`, `palette set`, and `var set` edit it in place and keep its comments.

```toml
repo = "~/dotfiles"
presets = ["base", "claude"]
tools = ["jq"]              # added on top of the presets
exclude_tools = ["zoxide"]  # removed from what the presets select
packages = []
exclude_packages = []
scope = "user"              # or "system"
build = "prebuilt"          # or "source"
palette = "latte"

[vars]
git_email = "you@example.com"

[sources.work]
url = "git@git.example.com:<you>/<work-dotfiles>.git"
```

| Key | Meaning |
| --- | --- |
| `repo` | Path to the main content repo checkout. `~` is expanded |
| `presets` | Presets selected on this host |
| `tools`, `exclude_tools` | Tools added to or removed from the preset selection |
| `packages`, `exclude_packages` | The same for dotfile packages |
| `scope` | `user` installs under `~/.local`; `system` installs under `/usr/local` and needs write access there |
| `build` | `source` builds tools that have a `crate` with `cargo install`; `prebuilt` downloads release assets |
| `palette` | Active palette name |
| `vars` | Template variable values for this host |
| `sources` | Secondary sources for this host only, in the same form as the repo's `[sources]` |

## settings

```toml
[settings]
palette = "mocha"
presets = ["base"]  # what `dfapp init` selects when given none
shells = ["zsh"]    # shells to generate init scripts for
pull = "rebase"     # or "ff-only"
cache_ttl = 3600    # seconds a version lookup stays cached
scope = "user"
build = "prebuilt"
```

Shell names are `zsh`, `bash`, and `nushell`. Presets can add more shells.

## tools

Each `[tools.<name>]` table defines one tool. Names use letters, digits, `.`, `_`, and `-`.

```toml
[tools.ripgrep]
description = "Fast recursive grep"
repo = "BurntSushi/ripgrep"
crate = "ripgrep"
pin = "15"
bins = ["rg"]
assets.linux-x86_64 = "ripgrep-*-x86_64-unknown-linux-musl.tar.gz"
assets.linux-aarch64 = "ripgrep-*-aarch64-unknown-linux-gnu.tar.gz"
```

| Key | Meaning |
| --- | --- |
| `kind` | `github`, `cargo`, `npm`, `uv`, or `script`. Defaults to `github` when `repo` is set |
| `repo` | GitHub `owner/name`, for release downloads and version lookups |
| `crate` | crates.io crate. With `build = "source"`, the tool is built with `cargo install --locked` |
| `package` | npm or PyPI package name, when it differs from the tool name |
| `pin` | See [Version pins](#version-pins) |
| `assets.<key>` | Release asset glob for a platform. Keys are `<os>-<arch>` (`linux-x86_64`, `linux-aarch64`), then `<os>` (`linux`), then `any`, tried in that order |
| `bins` | Binaries to link into the bin directory: `name` finds a file of that name in the release; `name=path/in/archive` names it exactly. Defaults to one binary named after the tool |
| `links` | Other files to link, as `"<path under the prefix>" = "<path in the release>"`. A tool with `links` and no `bins` links no binary |
| `init.<shell>` | A command whose output is the tool's shell init script, such as `"starship init zsh"` |
| `requires` | Tools that must be installed first. An npm tool requires `node` |
| `post_install` | A command run with `sh -c` after every install |
| `when` | Restricts the tool to a platform: `os`, `arch`, and `wsl = true` or `false` |
| `install`, `version`, `uninstall` | Commands for `kind = "script"` |

### Release downloads

dfapp picks the asset matching the platform's glob, downloads it, and checks it against a published checksum when the release has one: either a `<asset>.sha256` sidecar or a list such as `checksums.txt` or `SHA256SUMS`. A mismatch fails the install. Archives (`.tar.gz`, `.tgz`, `.tar.xz`, `.txz`, `.tar`, `.zip`, and single-file `.gz`) are unpacked; anything else is installed as a single file.

GitHub limits unauthenticated API calls to 60 an hour. dfapp uses `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`, in that order. The token is only sent to `api.github.com`, so release assets in private repos can't be downloaded.

### Script tools

A script tool runs `install` with `sh -c` from the repo root, then runs `version` and takes the first version-looking string from its output. The commands get these environment variables, and so does any tool's `post_install` command:

| Variable | Value |
| --- | --- |
| `DFAPP_TOOL` | The tool name |
| `DFAPP_VERSION` | The resolved version, when there is one |
| `DFAPP_PREFIX`, `DFAPP_BIN_DIR` | The install prefix and its bin directory |
| `DFAPP_TOOL_DIR` | A directory the script may install into |
| `DFAPP_REPO` | The main content repo |
| `DFAPP_SCOPE`, `DFAPP_OS`, `DFAPP_ARCH`, `DFAPP_WSL` | The scope and platform; `DFAPP_WSL` is `1` under WSL |
| `HOME` | The home dfapp deploys into, which differs from your own only under `DFAPP_HOME` |
| `PATH` | Your `PATH` with the bin directory first |

A tool's `init` commands and a script tool's `uninstall` command run with the same `HOME` and `PATH`.

Only `bins` entries in `name=path` form are linked for a script tool, with the path relative to `DFAPP_TOOL_DIR`. A script that puts its own launcher on `PATH`, like Claude Code's installer, needs no `bins`.

### Files dfapp won't overwrite

A link is only created or replaced when the path is empty or already a link into dfapp's tools directory. A real file or someone else's link in the way fails that tool's install, so a system binary is never replaced.

## Version pins

| Pin | Installs |
| --- | --- |
| empty, `latest`, `any`, or `*` | The newest stable release |
| `15` | The newest `15.x.y` |
| `15.1` | The newest `15.1.y` |
| `15.1.0` or `v15.1.0` | That version |
| `tag:nightly`, or a tag like `fork-v0.46.2` | That exact tag |
| `fork-ea-v0.46.7-*` | The newest release whose tag starts with the prefix, prereleases included |

`latest` and version pins skip prereleases; only a tag prefix pin picks them up. `dfapp apply` upgrades a tool when its pin resolves to a different version than the installed one, and reinstalls it when its definition changed (a new asset glob, say) or the build mode switched.

## presets

```toml
[presets.claude]
description = "Claude Code and its status line"
extends = ["base"]
tools = ["claude", "ccstatusline"]
packages = ["claude"]
shells = []
build = "source"  # optional: a preset can ask for source builds
```

A host's selection is its presets, expanded through `extends`, plus `host.toml`'s `tools` and `packages`, minus its excludes, plus anything a selected tool `requires`. `dfapp status` shows why each tool is selected.

## packages

A package is a directory in the repo whose contents mirror `$HOME`. `zsh/.zshrc` deploys to `~/.zshrc`, and `zellij/.config/zellij/config.kdl` to `~/.config/zellij/config.kdl`. Declare each one:

```toml
[packages.zellij]
description = "Zellij config and layouts"
when = { wsl = false }
```

Every file is symlinked, one file at a time. A `.linkignore` in the package lists globs to skip, one per line, with `#` comments. If a real file is already at a target path, it's moved to `<target>.bak.<timestamp>` first. Links dfapp made earlier whose source file is gone are removed on the next deploy.

## Templates and variables

A file whose name ends `.tmpl` is rendered and the `.tmpl` suffix dropped: `git/.gitconfig.tmpl` deploys to `~/.gitconfig`. The rendered copy lives under `~/.local/share/dfapp/rendered/`, and the link in `$HOME` points there.

```text
[user]
	email = {{ git_email }}
```

`{{ name }}` inserts a value; `{{{{` writes a literal `{{`. An unknown name fails the deploy with the file and line. These names are available:

| Name | Value |
| --- | --- |
| `home`, `user`, `hostname` | This host |
| `os`, `arch`, `wsl` | The platform; `wsl` is `true` or `false` |
| `prefix`, `bin_dir` | Where tools are installed |
| `repo` | The main content repo |
| `palette_name`, `palette.<key>` | The active palette and its values |
| anything in `[vars]` | Declared variables |

Declare variables in the repo, with or without defaults; hosts set values with `dfapp var set`:

```toml
[vars]
editor = "nvim"
git_email = { description = "Email for git commits" }
api_token = { description = "Token for the API", secret = true }
```

A variable can't reuse a built-in name.

### Editing rendered files

Editors write through the symlink into the rendered copy. `dfapp deploy` won't overwrite a rendered file that was edited this way; `dfapp capture` copies the edit back into the template instead. Unchanged lines keep their template text, and in changed lines any text equal to a variable's value is turned back into `{{ name }}`. Capture prints the lines where that happened, so a coincidental match can be checked. It refuses when the template or variables changed since the last render, since the edit can't be mapped safely then. `dfapp sync` and `dfapp update` capture first.

### Secret variables

A `secret = true` variable's value is never printed. `dfapp hook install` adds a pre-commit hook to every content repo that blocks a commit whose staged changes contain a secret value of four or more characters.

## Palettes

Each file in `palettes/` is one palette, named after the file. Values may be nested; `[theme] bat = "x"` is `palette.theme.bat`.

```toml
# palettes/mocha.toml
base = "#1e1e2e"
text = "#cdd6f4"
accent = "#cba6f7"
```

`dfapp palette set latte` switches the host and re-renders every template.

## Secondary sources

A source is another content repo layered over the main one, declared in the repo's `dfapp.toml` or a host's `host.toml`:

```toml
[sources.work]
url = "git@git.example.com:<you>/<work-dotfiles>.git"
branch = "main"
path = "~/work-dotfiles"  # optional; defaults to ~/.local/share/dfapp/sources/work
```

dfapp clones missing sources on `sync` and `update`, using your own git credentials, so a private repo works over SSH or a credential helper. A source's `dfapp.toml` may add tools, presets, packages, variables, and palettes. A tool or preset with the same name replaces the main repo's definition whole, and `dfapp config validate` notes it. A package directory present in both repos merges file by file, with the source winning.

## Sync and scheduled updates

`dfapp sync` captures template edits, then for each repo stashes local changes (untracked files included), pulls with `--rebase` (or `--ff-only` under `pull = "ff-only"`), and reapplies the stash. If the pull or the reapply conflicts, the repo is put back exactly as it was and the run reports it, so a scheduled run never leaves conflict markers in live dotfiles.

`dfapp schedule enable` installs a systemd user timer that runs `dfapp update --yes` daily (or weekly). Without a systemd user session it prints an equivalent crontab line to add by hand.

## Where dfapp keeps things

| Path | Holds |
| --- | --- |
| `~/.config/dfapp/host.toml` | This host's choices |
| `~/.local/share/dfapp/tools/<tool>/<version>/` | Installed tools, current and previous version |
| `~/.local/bin/` | Links to tool binaries |
| `~/.local/share/dfapp/rendered/` | Rendered templates |
| `~/.local/share/dfapp/init/` | Shell init scripts; source `init.zsh`, `init.bash`, or `init.nu` |
| `~/.local/share/dfapp/sources/` | Secondary source checkouts |
| `~/.local/state/dfapp/` | Install and deploy records, and the run lock |
| `~/.cache/dfapp/` | Cached version lookups and downloads |

With `scope = "system"`, tools go under `/usr/local/lib/dfapp/tools`, links under `/usr/local/bin`, and the install record in `/var/lib/dfapp/tools.json`. Dotfiles, init scripts, and the rest stay per user. The XDG base directory variables move the per-user paths as usual.
