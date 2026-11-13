# pom

> **Status:** Early MVP / experimental. Built as an AI-assisted project.

`pom` is a `pacman` and AUR helper written in Rust with a structured, inline terminal UI inspired by [`nh`](https://github.com/nix-community/nh) and [`nix-output-monitor`](https://github.com/maralorn/nix-output-monitor) (`nom`).

Instead of flooding your scrollback with raw `pacman` and `makepkg` logs, `pom` presents an `nh`-style pre-transaction diff summary, reviews `PKGBUILD` diffs via `$PAGER`, and renders build/install phases and a rolling 5-line log tail inside a `ratatui` `Viewport::Inline` progress panel at the bottom of your terminal.

---

## Features

- **Package Installation & Upgrades (`-S`, `-Syu`, `-Su`):**
  - Resolves official repository and AUR dependencies via `aur-depends` + `libalpm`.
  - Displays an `nh`-style grouped summary (**Official Repositories** vs **AUR**, tagging build-only dependencies with `[make]`, highlighting upgrades `old -> new`, new installs, downgrades, and conflicts).
  - Streams `pacman -S` / `pacman -U` and `makepkg` phases into a live `Viewport::Inline` progress bar (`downloading` → `verifying` → `extracting` → `building` → `packaging` → `installing` → `done`).
- **Removals (`-R`, `-Rns`, …):**
  - Previews the exact removal set unprivileged via `pacman --print` and displays an `nh`-style removal summary before streaming `pacman -R` progress into the inline bar.
- **Unified Search (`-Ss`):**
  - Queries local `libalpm` sync databases and the AUR RPC v5 API concurrently, showing AUR and official repository hits in bottom-up order (best/official matches closest to your prompt) with `[installed]` badges.
- **Pacman Passthrough:**
  - Any other operation (`-Q*`, `-U*`, `-D*`, `-F*`, `-T*`, `-Si`, `-Sc`, etc.) is forwarded directly to `pacman`, escalating with `sudo` or `doas` only when the operation mutates system state.
- **Security & Terminal Hygiene:**
  - Refuses to start as `root` (`makepkg`, `git`, and AUR RPC calls never run as root).
  - Parses `.SRCINFO` via the `srcinfo` crate rather than executing `PKGBUILD` files in Bash to discover metadata.
  - Tracks reviewed commits via `refs/pom/reviewed` and opens diffs in `$PAGER` (fallback `less -R`) before building (unless `--noconfirm` is passed).
  - Never uses the alternate screen buffer; respects `NO_COLOR` and non-TTY environments with plain line-by-line output.
  - Persists full `makepkg` logs to `~/.local/state/pom/logs/` on failure.

---

## Requirements

- **Arch Linux** (or derivative) with `pacman >= 7.1.0` (`libalpm >= 16.0.0`) and `/usr/include/alpm.h`
- **Rust toolchain** (2024 edition)
- Runtime tools: `pacman`, `makepkg`, `git`, `sudo` or `doas`, and `less` (or `$PAGER`)

---

## Building

```bash
git clone <repo-url>
cd pom
cargo build --release
```

The compiled binary will be at `./target/release/pom`.

---

## Usage

```bash
# Sync databases and upgrade official + AUR packages (default when run with no args)
pom -Syu

# Install official repo or AUR packages
pom -S gedit
pom -S paru-bin

# Search official repos and AUR (bottom-up order)
pom -Ss ripgrep

# Remove packages with summary + inline progress
pom -Rns gedit

# Passthrough to pacman
pom -Qi pacman
pom -Sc
```

---

## Development & Testing

```bash
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

---

## Current Limitations (MVP)

- No config file yet (behavior is controlled via CLI flags).
- No automatic `-git` / devel package version checking (`--devel`).
- No automatic PGP key importing or post-build `--removemake` cleanup.
- Cache directory (`~/.cache/pom/clone/`) is not yet managed by `pom -Scc` (which passes through to `pacman -Scc`).

---

## License

`GPL-3.0-or-later` (see [Cargo.toml](Cargo.toml)).
