# 📦 apt-sync

Curated APT package manager — sync the packages you *actually* care about across machines.

**Zero crate dependencies.** Pure Rust + `std`. Tiny binary.

## Quick Start

```bash
# Build
cargo build --release

# Snapshot your current system — interactively pick packages
./target/release/apt-sync snap

# Or add packages directly
apt-sync add zsh git curl jq podman

# See what's installed vs missing
apt-sync status

# Install missing packages on a new machine
apt-sync install          # for real
apt-sync install --dry-run  # just show me
```

## Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `status` | `s` | Show installed/missing curated packages |
| `list` | `ls` | List all curated packages |
| `add <pkg...>` | `a` | Add package(s) to curated list |
| `remove <pkg...>` | `rm` | Remove package(s) from curated list |
| `install` | `i` | Install missing curated packages |
| `diff` | `d` | Compare system packages vs curated list |
| `snap` | — | Interactively pick from system packages |

## How It Works

- **`packages.txt`** — a simple text file listing packages you care about (one per line)
- Commit it to git → sync across machines
- `apt-sync install` installs anything missing
- `apt-sync diff` shows what's on your system but not curated (libs, defaults, etc.)

## New Machine Setup

```bash
git clone <this-repo>
cd apt-sync
cargo build --release
./target/release/apt-sync install
```

## Options

- `--dry-run` — show what `install` would do without doing it
- `--help` / `-h` — show help
