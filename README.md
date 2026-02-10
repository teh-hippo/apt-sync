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
| `why <pkg...>` | `w` | Show install history for package(s) |

## How It Works

- **`packages.txt`** — a simple text file listing packages you care about (one per line)
- Commit it to git → sync across machines
- `apt-sync install` installs anything missing
- `apt-sync diff` shows what's on your system but not curated (libs, defaults, etc.)

## New Machine Setup

### With mise (recommended)

Requires a `GITHUB_TOKEN` with `repo` scope (private repo):

```bash
export GITHUB_TOKEN=ghp_...
mise use -g github:teh-hippo/apt-sync
apt-sync install
```

### From source

```bash
git clone https://github.com/teh-hippo/apt-sync.git
cd apt-sync
cargo build --release
./target/release/apt-sync install
```

## Releasing

Releases are automated via GitHub Actions. To cut a new release:

```bash
# Bump version in Cargo.toml, then:
git add Cargo.toml Cargo.lock
git commit -m "release: vX.Y.Z"
git tag vX.Y.Z
git push origin master --tags
```

The workflow builds an x86_64 Linux binary, creates a GitHub release with
the tarball and SHA256 checksum, and generates release notes automatically.

## Options

- `--dry-run` — show what `install` would do without doing it
- `--help` / `-h` — show help

## License

[MIT](LICENSE)
