## Project Overview

pom - a Rust-based `pacman` and AUR helper with a structured, inline terminal user interface inspired by `nh` and `nix-output-monitor` (`nom`). The tool resolves official repository and AUR dependencies, manages unprivileged `makepkg` builds alongside privileged `pacman` transactions, and presents build phases, dependency graphs, and package diffs in a live inline viewport rather than dumping raw command output.

## Tech Stack & Core Dependencies

- **Language:** Rust (2024 edition)
- **Async Runtime:** `tokio` (process spawning, async I/O, concurrent network and git tasks)
- **CLI Parsing:** `clap` (derive API, with passthrough support for native `pacman` flags)
- **Package Management & Parsing:**
  - `alpm` / `alpm-utils` / `pacmanconf` (bindings to `libalpm` for local/sync DB queries and version comparisons)
  - `aur-depends` (AUR + official repository dependency resolution and split-package handling)
  - `srcinfo` (parsing `.SRCINFO` metadata; never parse raw Bash `PKGBUILD` files directly)
  - `raur` + `reqwest` (querying AUR RPC v5 API; note `https://aur.archlinux.org/rpc` without trailing slash)
- **UI & Terminal Rendering:**
  - `ratatui` + `crossterm` (using `Viewport::Inline` to preserve terminal scrollback)
  - `owo-colors` (ANSI styling outside the TUI loop)
- **Error Handling:** `thiserror` for library/domain modules, `anyhow` or `miette` for CLI top-level reporting.

## Architecture & Module Boundaries

```text
src/
├── main.rs          # Entrypoint, root guard, CLI dispatch, signal/panic raw-mode cleanup
├── cli.rs           # Pacman-compatible flag classifier (-S, -Syu, -Ss, -R, passthrough)
├── alpm_ctx.rs      # PackageDatabase trait, AlpmCtx (libalpm wrapper), MockDatabase
├── aur/
│   ├── mod.rs       # AUR module exports
│   ├── rpc.rs       # Async AUR RPC v5 client & search wrapper (raur)
│   ├── git.rs       # Async git clone/fetch in ~/.cache/pom/clone/, .SRCINFO & refs/pom/reviewed diffs
│   └── resolver.rs  # Adapter wrapping aur-depends into owned TransactionPlan
├── builder/
│   ├── mod.rs       # Transaction & removal pipeline orchestrator
│   ├── makepkg.rs   # Unprivileged makepkg runner (LC_ALL=C), phase parser, log persistence
│   └── pacman.rs    # Privileged sudo/doas pacman invocations, streaming parser & TTY handoff
└── ui/
    ├── mod.rs       # UI module exports
    ├── app.rs       # Async UI state machine, UiEvent channel, SuspendGuard, plain-mode fallback
    ├── viewport.rs  # Ratatui Viewport::Inline renderer (nom-style tree, spinners & bottom progress bar)
    └── summary.rs   # nh-style pre-transaction/removal diff tables & -Ss search formatter
```

## Critical Invariants & Security Rules

1. **Privilege Separation:**
- **NEVER** execute `makepkg`, `git`, or AUR network calls as `root`. Refuse to run or drop privileges if invoked under `sudo`.
- Only escalate privileges (`sudo` or `doas`) when invoking `pacman` for database syncs or installing built `.pkg.tar.zst` archives.

1. **TTY Ownership & TUI Suspension:**
- The inline TUI (`ratatui`) **must** suspend rendering, leave raw mode, and flush the viewport before handing control to any interactive subprocess (`sudo` password prompts, `pacman` confirmation prompts, or `$EDITOR` during `PKGBUILD` inspection).
- Resume the inline viewport only after the interactive subprocess exits.

1. **No Raw `PKGBUILD` Evaluation:**
- Never source or execute `PKGBUILD` files in Bash to discover metadata. Always parse `.SRCINFO` via the `srcinfo` crate or query the AUR RPC API.

1. **Mandatory `PKGBUILD` Review:**
- Always present diffs for newly cloned or updated AUR repositories before executing `makepkg`, unless explicitly bypassed via a CLI flag (e.g., `--noconfirm`).

## UI & UX Conventions (`nh` / `nom` Style)

- **Inline Viewport Only:** Do not use the alternate screen buffer (`EnterAlternateScreen`). All progress trees, spinners, and log tails must render inline so final summaries remain in the user's scrollback history.
- **`makepkg` Phase Parsing:** Intercept `stdout` and `stderr` from `makepkg --noconfirm` (after pre-installing dependencies) and parse `==>` / `->` status markers to transition package node states:
`Pending` -> `Cloning` -> `Verifying` -> `Extracting` -> `Building` -> `Packaging` -> `Installing` -> `Done` / `Failed`.
- **Log Suppression & Failure Dumps:** Keep compiler output capped to a small rolling ring buffer (e.g., last 5 lines per active build node) in the live UI. If a build fails, dump the full captured log buffer and persist the log file path clearly in the error summary.
- **Pre-Transaction Summary (`nh`-style):** Display a grouped diff table before confirmation:
  - Separate **Official Repositories** from **AUR** targets (and **Removals** for `-R`).
  - Highlight upgrades (`old_ver -> new_ver`), new installs, downgrades, and removals/conflicts with distinct semantic colors.
  - Respect `NO_COLOR` and non-TTY (`!stdin.is_terminal()`) environments by falling back to plain line-by-line output.
- **Streaming `pacman` Progress:** When `sudo -v` is validated and no interactive conflict prompts are needed, stream non-interactive `pacman -S` / `-U` / `-R` (`LC_ALL=C --noprogressbar --noconfirm`) output into the `Viewport::Inline` bottom progress bar; fall back to TTY handoff whenever interactive prompts are required.

## Build, Test & Verification Commands

Note: Compiling against `libalpm` requires an Arch Linux environment (or an Arch container) with `pacman` headers installed.

- **Check compilation:** `cargo check --all-targets`
- **Run unit & integration tests:** `cargo test`
- **Linting (strict):** `cargo clippy --all-targets -- -D warnings`
- **Formatting check:** `cargo fmt -- --check`

## Testing Guidelines for Agents

- **Decouple from Host `libalpm`:** Abstract `alpm` queries behind `PackageDatabase` (`src/alpm_ctx.rs`) so search merging, repo-upgrade detection, and summaries can be unit-tested with `MockDatabase` on non-Arch hosts or CI runners. For `aur-depends` resolver tests, use a `TempDir`-backed `TestAlpmDb` + `MockRaur`.
- **Snapshot Test UI & Parsers:**
- Write unit tests for the `makepkg` output parser against real `makepkg` log fixtures (including localized output edge cases — always spawn `makepkg` with `LC_ALL=C`).
- Use `ratatui::backend::TestBackend` to test the `nom`-style tree layout and `nh`-style diff tables without requiring an active TTY.
