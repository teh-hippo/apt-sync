use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

// ── Colors ──────────────────────────────────────────────────────────
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

// ── Package list I/O ────────────────────────────────────────────────

const PKG_FILENAME: &str = "packages.txt";

fn pkg_file_path() -> PathBuf {
    // 1. Explicit override via env var
    if let Ok(path) = env::var("APT_SYNC_FILE") {
        return PathBuf::from(path);
    }

    // 2. XDG config dir (~/.config/apt-sync/packages.txt)
    let config_dir = env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            let home = env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        },
        PathBuf::from,
    )
    .join("apt-sync");
    let xdg_path = config_dir.join(PKG_FILENAME);
    if xdg_path.exists() {
        return xdg_path;
    }

    // 3. Dev mode: walk up from binary to find Cargo.toml
    let exe = env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    if let Some(repo) = dir.ancestors().find(|p| p.join("Cargo.toml").exists()) {
        return repo.join(PKG_FILENAME);
    }

    // 4. Default to XDG path (will be created on first write)
    let _ = fs::create_dir_all(&config_dir);
    xdg_path
}

fn load_packages(path: &Path) -> BTreeSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    parse_packages(&contents)
}

fn parse_packages(contents: &str) -> BTreeSet<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn save_packages(path: &Path, pkgs: &BTreeSet<String>) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(f, "# apt-sync curated packages")?;
    writeln!(f, "# one package per line, comments start with #")?;
    for p in pkgs {
        writeln!(f, "{p}")?;
    }
    Ok(())
}

// ── System queries ──────────────────────────────────────────────────

fn system_manual_packages() -> BTreeSet<String> {
    let output = Command::new("apt-mark")
        .arg("showmanual")
        .output()
        .expect("failed to run apt-mark — is apt installed?");
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn installed_set(pkgs: &BTreeSet<String>) -> BTreeSet<String> {
    if pkgs.is_empty() {
        return BTreeSet::new();
    }
    let output = Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\t${Status}\n"])
        .args(pkgs)
        .stderr(std::process::Stdio::null())
        .output()
        .expect("failed to run dpkg-query — is dpkg installed?");
    parse_installed(&String::from_utf8_lossy(&output.stdout))
}

fn parse_installed(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let (pkg, status) = line.split_once('\t')?;
            status
                .contains("install ok installed")
                .then(|| pkg.to_string())
        })
        .collect()
}

// ── Apt history ─────────────────────────────────────────────────────

struct HistoryEntry {
    date: String,
    commandline: String,
    requested_by: Option<String>,
    installed: Vec<String>,
}

fn read_history_logs() -> String {
    let mut buf = String::new();

    // Read rotated .gz logs (oldest first → newest last)
    let mut gz_paths: Vec<PathBuf> = fs::read_dir("/var/log/apt")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "gz")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("history"))
        })
        .collect();
    gz_paths.sort();
    gz_paths.reverse(); // highest number = oldest, read oldest first

    if !gz_paths.is_empty()
        && let Ok(output) = Command::new("zcat").args(&gz_paths).output()
    {
        buf.push_str(&String::from_utf8_lossy(&output.stdout));
    }

    // Current log last (most recent)
    if let Ok(current) = fs::read_to_string("/var/log/apt/history.log") {
        buf.push_str(&current);
    }

    buf
}

fn parse_history(log: &str) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut date = String::new();
    let mut cmdline = String::new();
    let mut requested = None;
    let mut installed = Vec::new();

    for line in log.lines() {
        if let Some(d) = line.strip_prefix("Start-Date: ") {
            date = d.trim().to_string();
            cmdline.clear();
            requested = None;
            installed.clear();
        } else if let Some(c) = line.strip_prefix("Commandline: ") {
            cmdline = c.trim().to_string();
        } else if let Some(r) = line.strip_prefix("Requested-By: ") {
            requested = Some(r.trim().to_string());
        } else if let Some(pkgs) = line.strip_prefix("Install: ") {
            installed = parse_history_packages(pkgs);
        } else if line.starts_with("End-Date: ") && !installed.is_empty() {
            entries.push(HistoryEntry {
                date: date.clone(),
                commandline: cmdline.clone(),
                requested_by: requested.clone(),
                installed: installed.clone(),
            });
        }
    }

    entries
}

fn parse_history_packages(pkgs_line: &str) -> Vec<String> {
    // Entries are separated by "), " — commas inside parens are part of the entry
    pkgs_line
        .split("), ")
        .filter_map(|entry| {
            let name = entry.split(':').next()?;
            if name.is_empty() {
                return None;
            }
            if entry.contains("automatic") {
                return None;
            }
            Some(name.to_string())
        })
        .collect()
}

fn find_install_history<'a>(entries: &'a [HistoryEntry], pkg: &str) -> Vec<&'a HistoryEntry> {
    entries
        .iter()
        .filter(|e| e.installed.iter().any(|p| p == pkg))
        .collect()
}

fn format_pkg_list(pkgs: &[&str]) -> String {
    const MAX: usize = 10;
    if pkgs.len() <= MAX {
        pkgs.join(", ")
    } else {
        let mut s = pkgs[..MAX].join(", ");
        s.push_str(&format!(" + {} more", pkgs.len() - MAX));
        s
    }
}

fn siblings<'a>(entry: &'a HistoryEntry, name: &str) -> Vec<&'a str> {
    entry
        .installed
        .iter()
        .map(String::as_str)
        .filter(|p| *p != name)
        .collect()
}

fn same_day_neighbors<'a>(
    entries: &'a [HistoryEntry],
    entry: &HistoryEntry,
    name: &str,
    sibling_set: &BTreeSet<&str>,
) -> Vec<&'a str> {
    let day = entry.date.split_whitespace().next().unwrap_or("");
    entries
        .iter()
        .filter(|e| {
            e.date.split_whitespace().next().unwrap_or("") == day
                && !(e.date == entry.date && e.commandline == entry.commandline)
        })
        .flat_map(|e| e.installed.iter().map(String::as_str))
        .filter(|p| *p != name && !sibling_set.contains(p))
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect()
}

// ── Shell history and journal context ───────────────────────────────

struct ShellHistoryEntry {
    timestamp: i64,
    command: String,
}

fn read_journal_pwd(apt_date: &str, commandline: &str) -> Option<String> {
    // Normalize "2026-02-10  21:50:50" to "2026-02-10 21:50:50" (single space)
    let normalized = apt_date.split_whitespace().collect::<Vec<_>>().join(" ");

    // Query journal with ±60s window around the apt command
    let output = Command::new("journalctl")
        .args([
            "_COMM=sudo",
            "--no-pager",
            &format!("--since={normalized} -5seconds"),
            &format!("--until={normalized} +60seconds"),
        ])
        .output()
        .ok()?;

    let journal = String::from_utf8_lossy(&output.stdout);
    parse_journal_pwd(&journal, commandline)
}

fn parse_journal_pwd(journal_output: &str, commandline: &str) -> Option<String> {
    // Extract key package names from commandline to match against COMMAND field
    let pkg_names: Vec<&str> = commandline
        .split_whitespace()
        .skip_while(|w| w.starts_with('-') || *w == "apt-get" || *w == "apt" || *w == "install")
        .filter(|w| !w.starts_with('-'))
        .collect();

    if pkg_names.is_empty() {
        return None;
    }

    let home = env::var("HOME").ok()?;

    for line in journal_output.lines() {
        // Look for PWD= and COMMAND= anywhere in the line
        if !line.contains("PWD=") || !line.contains("COMMAND=") {
            continue;
        }

        let mut pwd = None;
        let mut command = None;

        // Extract PWD value
        if let Some(pwd_start) = line.find("PWD=") {
            let after_pwd = &line[pwd_start + 4..];
            // PWD value ends at the next semicolon or space-semicolon
            let pwd_end = after_pwd
                .find(" ;")
                .or_else(|| after_pwd.find('\n'))
                .unwrap_or(after_pwd.len());
            pwd = Some(&after_pwd[..pwd_end]);
        }

        // Extract COMMAND value
        if let Some(cmd_start) = line.find("COMMAND=") {
            let after_cmd = &line[cmd_start + 8..];
            // COMMAND value goes to end of line or next separator
            let cmd_end = after_cmd.find('\n').unwrap_or(after_cmd.len());
            command = Some(&after_cmd[..cmd_end]);
        }

        // Check if this line matches our apt command
        if let (Some(p), Some(c)) = (pwd, command) {
            // Match if command contains apt and any of our package names
            if c.contains("apt") && pkg_names.iter().any(|pkg| c.contains(pkg)) {
                // Replace $HOME with ~ for display
                let display_path = if let Some(rel) = p.strip_prefix(&home) {
                    if rel.is_empty() {
                        "~".to_string()
                    } else {
                        format!("~{rel}")
                    }
                } else {
                    p.to_string()
                };
                return Some(display_path);
            }
        }
    }

    None
}

fn read_shell_history() -> Vec<ShellHistoryEntry> {
    // Detect history file
    let history_path = env::var("HISTFILE")
        .ok()
        .or_else(|| {
            env::var("HOME").ok().and_then(|home| {
                let zsh_hist = PathBuf::from(&home).join(".zsh_history");
                let bash_hist = PathBuf::from(&home).join(".bash_history");
                if zsh_hist.exists() {
                    Some(zsh_hist.to_string_lossy().to_string())
                } else if bash_hist.exists() {
                    Some(bash_hist.to_string_lossy().to_string())
                } else {
                    None
                }
            })
        });

    let Some(path) = history_path else {
        return Vec::new();
    };

    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };

    parse_shell_history(&contents)
}

fn parse_shell_history(contents: &str) -> Vec<ShellHistoryEntry> {
    let mut entries = Vec::new();

    for line in contents.lines() {
        // Zsh format: ": epoch:0;command"
        if let Some(rest) = line.strip_prefix(": ")
            && let Some((epoch_part, cmd)) = rest.split_once(';')
            && let Some(epoch_str) = epoch_part.split(':').next()
            && let Ok(timestamp) = epoch_str.parse::<i64>()
        {
            entries.push(ShellHistoryEntry {
                timestamp,
                command: cmd.to_string(),
            });
        }
        // Note: bash history without timestamps is not supported
        // (would need to track #epoch lines, but this system uses zsh)
    }

    entries
}

fn apt_date_to_epoch(apt_date: &str) -> Option<i64> {
    // Normalize "2026-02-10  21:50:50" to "2026-02-10 21:50:50"
    let normalized = apt_date.split_whitespace().collect::<Vec<_>>().join(" ");

    let output = Command::new("date")
        .args(["-d", &normalized, "+%s"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<i64>().ok()
}

fn is_interesting_command(cmd: &str) -> bool {
    let first_word = cmd.split_whitespace().next().unwrap_or("");
    !matches!(
        first_word,
        "ls" | "clear" | "exit" | "pwd" | "echo" | "cat" | "true" | "history" | ""
    )
}

fn find_nearby_commands(
    history: &[ShellHistoryEntry],
    target_epoch: i64,
    window_secs: i64,
    show_all: bool,
) -> Vec<String> {
    let mut nearby: Vec<(&ShellHistoryEntry, i64)> = history
        .iter()
        .filter_map(|entry| {
            let delta = (entry.timestamp - target_epoch).abs();
            if delta <= window_secs {
                Some((entry, delta))
            } else {
                None
            }
        })
        .collect();

    // Sort by proximity to target time
    nearby.sort_by_key(|(_, delta)| *delta);

    let mut commands = Vec::new();
    for (entry, _) in nearby {
        // Skip apt install commands
        if entry.command.contains("apt-get install") || entry.command.contains("apt install") {
            continue;
        }

        // Skip trivial commands unless show_all
        if !show_all && !is_interesting_command(&entry.command) {
            continue;
        }

        commands.push(entry.command.clone());

        // Cap at 5 commands
        if commands.len() >= 5 {
            break;
        }
    }

    commands
}

// ── Commands ────────────────────────────────────────────────────────

fn cmd_status(pkg_path: &Path) {
    let pkgs = load_packages(pkg_path);
    if pkgs.is_empty() {
        println!("{YELLOW}📭 No curated packages yet. Use `apt-sync add <pkg>` to get started!{RESET}");
        return;
    }
    println!(
        "{BOLD}{CYAN}📦 apt-sync status{RESET}  {DIM}({} curated){RESET}\n",
        pkgs.len()
    );
    let installed = installed_set(&pkgs);
    let mut n_installed = 0u32;
    let mut n_missing = 0u32;
    for p in &pkgs {
        if installed.contains(p) {
            println!("  {GREEN}✔ {p}{RESET}");
            n_installed += 1;
        } else {
            println!("  {RED}✘ {p}{RESET}  {DIM}(not installed){RESET}");
            n_missing += 1;
        }
    }
    println!();
    println!("  {GREEN}{n_installed} installed{RESET}  {RED}{n_missing} missing{RESET}");
    if n_missing > 0 {
        println!("  {DIM}Run `apt-sync install` to install missing packages{RESET}");
    }
}

fn cmd_list(pkg_path: &Path) {
    let pkgs = load_packages(pkg_path);
    if pkgs.is_empty() {
        println!("{YELLOW}📭 No curated packages yet.{RESET}");
        return;
    }
    for p in &pkgs {
        println!("{p}");
    }
}

fn cmd_add(pkg_path: &Path, names: &[String]) {
    let mut pkgs = load_packages(pkg_path);
    let mut added = Vec::new();
    let mut already = Vec::new();
    for name in names {
        if pkgs.insert(name.clone()) {
            added.push(name.as_str());
        } else {
            already.push(name.as_str());
        }
    }
    save_packages(pkg_path, &pkgs).expect("failed to write packages.txt");
    for a in &added {
        println!("  {GREEN}＋ {a}{RESET}");
    }
    for a in &already {
        println!("  {DIM}  {a} (already listed){RESET}");
    }
    if !added.is_empty() {
        println!(
            "\n{CYAN}📝 Added {} package(s) to packages.txt{RESET}",
            added.len()
        );
    }
}

fn cmd_remove(pkg_path: &Path, names: &[String]) {
    let mut pkgs = load_packages(pkg_path);
    let mut removed = Vec::new();
    let mut not_found = Vec::new();
    for name in names {
        if pkgs.remove(name) {
            removed.push(name.as_str());
        } else {
            not_found.push(name.as_str());
        }
    }
    save_packages(pkg_path, &pkgs).expect("failed to write packages.txt");
    for r in &removed {
        println!("  {RED}－ {r}{RESET}");
    }
    for n in &not_found {
        println!("  {DIM}  {n} (not in list){RESET}");
    }
    if !removed.is_empty() {
        println!(
            "\n{CYAN}📝 Removed {} package(s) from packages.txt{RESET}",
            removed.len()
        );
    }
}

fn cmd_install(pkg_path: &Path, dry_run: bool) {
    let pkgs = load_packages(pkg_path);
    if pkgs.is_empty() {
        println!("{YELLOW}📭 No curated packages to install.{RESET}");
        return;
    }
    let installed = installed_set(&pkgs);
    let missing: Vec<&str> = pkgs
        .iter()
        .filter(|p| !installed.contains(*p))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        println!(
            "{GREEN}✨ All {} curated packages are already installed!{RESET}",
            pkgs.len()
        );
        return;
    }
    println!(
        "{BOLD}{CYAN}🚀 Installing {} missing package(s){RESET}\n",
        missing.len()
    );
    for m in &missing {
        println!("  {CYAN}• {m}{RESET}");
    }
    println!();
    if dry_run {
        println!("{YELLOW}🏜️  Dry run — nothing was installed{RESET}");
        println!(
            "{DIM}Would run: apt-get install -y {}{RESET}",
            missing.join(" ")
        );
        return;
    }
    let status = Command::new("apt-get")
        .args(["install", "-y"])
        .args(&missing)
        .status()
        .expect("failed to run apt-get");
    if status.success() {
        println!("\n{GREEN}✨ Done! All packages installed.{RESET}");
    } else {
        println!("\n{RED}💥 apt-get exited with errors{RESET}");
    }
}

fn cmd_diff(pkg_path: &Path) {
    let curated = load_packages(pkg_path);
    let system = system_manual_packages();
    let on_system_only: Vec<&String> = system.difference(&curated).collect();
    let in_list_only: Vec<&String> = curated.difference(&system).collect();

    if on_system_only.is_empty() && in_list_only.is_empty() {
        println!("{GREEN}✨ System and curated list are in perfect sync!{RESET}");
        return;
    }
    if !on_system_only.is_empty() {
        println!(
            "{BOLD}{YELLOW}🔍 On system but not curated{RESET} {DIM}({} packages){RESET}\n",
            on_system_only.len()
        );
        for p in &on_system_only {
            println!("  {YELLOW}? {p}{RESET}");
        }
        println!();
    }
    if !in_list_only.is_empty() {
        println!(
            "{BOLD}{RED}📋 Curated but not on system{RESET} {DIM}({} packages){RESET}\n",
            in_list_only.len()
        );
        for p in &in_list_only {
            println!("  {RED}✘ {p}{RESET}");
        }
        println!();
    }
    println!(
        "{DIM}Use `apt-sync add <pkg>` to curate, `apt-sync install` to install missing{RESET}"
    );
}

#[allow(clippy::significant_drop_tightening)]
fn cmd_snap(pkg_path: &Path) {
    let system = system_manual_packages();
    let curated = load_packages(pkg_path);
    let uncurated: Vec<&String> = system.difference(&curated).collect();

    if uncurated.is_empty() {
        println!("{GREEN}✨ All manual system packages are already curated!{RESET}");
        return;
    }

    println!(
        "{BOLD}{CYAN}📸 Snapshot — {} uncurated manual packages{RESET}\n",
        uncurated.len()
    );
    println!(
        "{DIM}For each package, type {RESET}{BOLD}y{RESET}{DIM} to add, \
         {RESET}{BOLD}n{RESET}{DIM} to skip, \
         {RESET}{BOLD}q{RESET}{DIM} to quit:{RESET}\n"
    );

    let stdin = io::stdin();
    let mut to_add = Vec::new();

    {
        let mut reader = stdin.lock();
        for pkg in &uncurated {
            print!("  {CYAN}{pkg}{RESET}  [y/n/q] ");
            io::stdout().flush().unwrap();
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                break;
            }
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => to_add.push((*pkg).clone()),
                "q" | "quit" => break,
                _ => {}
            }
        }
    }

    if to_add.is_empty() {
        println!("\n{DIM}No packages added.{RESET}");
        return;
    }
    cmd_add(pkg_path, &to_add);
}

fn cmd_why(names: &[String], window_mins: u32, show_all: bool) {
    let log = read_history_logs();
    let entries = parse_history(&log);
    let shell_history = read_shell_history();
    let window_secs = i64::from(window_mins) * 60;

    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let hits = find_install_history(&entries, name);
        if hits.is_empty() {
            println!("{DIM}{name}: no install history found{RESET}");
            continue;
        }
        println!("{BOLD}{CYAN}{name}{RESET}");
        for entry in &hits {
            let date = entry.date.split_whitespace().next().unwrap_or(&entry.date);
            println!("  {GREEN}📅 {date}{RESET}  {DIM}{}{RESET}", entry.commandline);
            if let Some(ref user) = entry.requested_by {
                println!("     {DIM}by {user}{RESET}");
            }

            // Working directory from journal
            if let Some(pwd) = read_journal_pwd(&entry.date, &entry.commandline) {
                println!("     {DIM}in: {pwd}{RESET}");
            }

            let sibs = siblings(entry, name);
            if !sibs.is_empty() {
                println!("     {DIM}with: {}{RESET}", format_pkg_list(&sibs));
            }
            let sibling_set: BTreeSet<&str> = sibs.iter().copied().collect();
            let neighbors = same_day_neighbors(&entries, entry, name, &sibling_set);
            if !neighbors.is_empty() {
                println!(
                    "     {DIM}also that day: {}{RESET}",
                    format_pkg_list(&neighbors)
                );
            }

            // Shell history context
            if let Some(epoch) = apt_date_to_epoch(&entry.date) {
                let nearby = find_nearby_commands(&shell_history, epoch, window_secs, show_all);
                if !nearby.is_empty() {
                    println!("     {DIM}around then:{RESET}");
                    for cmd in &nearby {
                        println!("       {DIM}{cmd}{RESET}");
                    }
                }
            }
        }
    }
}

// ── Help ────────────────────────────────────────────────────────────

fn print_help() {
    println!(
        "\n\
{BOLD}{CYAN}📦 apt-sync{RESET} — curated APT package manager\n\
\n\
{BOLD}USAGE:{RESET}\n    \
    apt-sync <command> [options]\n\
\n\
{BOLD}COMMANDS:{RESET}\n    \
    {GREEN}status{RESET}  {DIM}(s){RESET}     Show installed/missing curated packages\n    \
    {GREEN}list{RESET}    {DIM}(ls){RESET}    List all curated packages\n    \
    {GREEN}add{RESET}     {DIM}(a){RESET}     Add package(s) to curated list\n    \
    {GREEN}remove{RESET}  {DIM}(rm){RESET}    Remove package(s) from curated list\n    \
    {GREEN}install{RESET} {DIM}(i){RESET}     Install missing curated packages\n    \
    {GREEN}diff{RESET}    {DIM}(d){RESET}     Compare system packages vs curated list\n    \
    {GREEN}snap{RESET}             Interactively pick from system packages\n    \
    {GREEN}why{RESET}     {DIM}(w){RESET}     Show install history for package(s)\n\
\n\
{BOLD}OPTIONS:{RESET}\n    \
    {YELLOW}--dry-run{RESET}        Show what would happen (install only)\n    \
    {YELLOW}--window=N{RESET}       Minutes before/after install to search history (why only, default: 5)\n    \
    {YELLOW}--all{RESET}            Show all commands in history window (why only, default: interesting only)\n    \
    {YELLOW}--help, -h{RESET}       Show this help\n\
\n\
{BOLD}CONFIG:{RESET}\n    \
    Packages file: {DIM}$APT_SYNC_FILE{RESET} or {DIM}~/.config/apt-sync/packages.txt{RESET}\n",
    );
}

// ── Main ────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    let pkg_path = pkg_file_path();
    let cmd = args[0].as_str();
    let rest = &args[1..];
    let dry_run = rest.iter().any(|a| a == "--dry-run");

    // Parse --window=N for why command
    let window_mins = rest
        .iter()
        .find_map(|a| a.strip_prefix("--window=")?.parse().ok())
        .unwrap_or(5);
    let show_all = rest.iter().any(|a| a == "--all");

    let rest_no_flags: Vec<String> = rest
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect();

    match cmd {
        "status" | "s" => cmd_status(&pkg_path),
        "list" | "ls" => cmd_list(&pkg_path),
        "add" | "a" => {
            if rest_no_flags.is_empty() {
                eprintln!("{RED}Usage: apt-sync add <pkg...>{RESET}");
                return ExitCode::FAILURE;
            }
            cmd_add(&pkg_path, &rest_no_flags);
        }
        "remove" | "rm" => {
            if rest_no_flags.is_empty() {
                eprintln!("{RED}Usage: apt-sync remove <pkg...>{RESET}");
                return ExitCode::FAILURE;
            }
            cmd_remove(&pkg_path, &rest_no_flags);
        }
        "install" | "i" => cmd_install(&pkg_path, dry_run),
        "diff" | "d" => cmd_diff(&pkg_path),
        "snap" => cmd_snap(&pkg_path),
        "why" | "w" => {
            if rest_no_flags.is_empty() {
                eprintln!("{RED}Usage: apt-sync why <pkg...>{RESET}");
                return ExitCode::FAILURE;
            }
            cmd_why(&rest_no_flags, window_mins, show_all);
        }
        _ => {
            eprintln!("{RED}Unknown command: {cmd}{RESET}");
            print_help();
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("apt-sync-test-{name}"));
            let _ = fs::create_dir_all(path.parent().unwrap());
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    impl std::ops::Deref for TempFile {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    fn entry(timestamp: i64, command: &str) -> ShellHistoryEntry {
        ShellHistoryEntry {
            timestamp,
            command: command.to_string(),
        }
    }

    #[test]
    fn parse_empty() {
        assert!(parse_packages("").is_empty());
    }

    #[test]
    fn parse_with_comments_and_blanks() {
        let input = "# comment\n\nzsh\ngit\n# another\ncurl\n";
        let pkgs = parse_packages(input);
        assert_eq!(pkgs.len(), 3);
        assert!(pkgs.contains("zsh"));
        assert!(pkgs.contains("git"));
        assert!(pkgs.contains("curl"));
    }

    #[test]
    fn parse_trims_whitespace() {
        let input = "  zsh  \n  git  \n";
        let pkgs = parse_packages(input);
        assert!(pkgs.contains("zsh"));
        assert!(pkgs.contains("git"));
    }

    #[test]
    fn parse_deduplicates() {
        let input = "zsh\nzsh\ngit\n";
        let pkgs = parse_packages(input);
        assert_eq!(pkgs.len(), 2);
    }

    #[test]
    fn roundtrip_save_load() {
        let tmp = TempFile::new("roundtrip.txt");
        let mut pkgs = BTreeSet::new();
        pkgs.insert("curl".to_string());
        pkgs.insert("git".to_string());
        pkgs.insert("zsh".to_string());
        save_packages(&tmp, &pkgs).unwrap();
        let loaded = load_packages(&tmp);
        assert_eq!(pkgs, loaded);
    }

    #[test]
    fn diff_logic() {
        let curated = BTreeSet::from(["git".into(), "curl".into(), "zsh".into()]);
        let system = BTreeSet::from(["git".into(), "vim".into()]);
        let on_system_only: Vec<&String> = system.difference(&curated).collect();
        let in_list_only: Vec<&String> = curated.difference(&system).collect();
        assert_eq!(on_system_only, vec![&"vim".to_string()]);
        assert!(in_list_only.contains(&&"curl".to_string()));
        assert!(in_list_only.contains(&&"zsh".to_string()));
    }

    #[test]
    fn load_nonexistent_file() {
        let path = Path::new("/tmp/apt-sync-nonexistent-test.txt");
        assert!(load_packages(path).is_empty());
    }

    #[test]
    fn parse_only_comments() {
        let input = "# just a comment\n# another comment\n";
        assert!(parse_packages(input).is_empty());
    }

    #[test]
    fn save_preserves_header() {
        let tmp = TempFile::new("header.txt");
        let pkgs = BTreeSet::from(["git".to_string()]);
        save_packages(&tmp, &pkgs).unwrap();
        let raw = fs::read_to_string(&*tmp).unwrap();
        assert!(raw.starts_with("# apt-sync curated packages\n"));
        assert!(raw.contains("# one package per line"));
    }

    #[test]
    fn add_remove_roundtrip() {
        let tmp = TempFile::new("addrem.txt");
        save_packages(&tmp, &BTreeSet::new()).unwrap();

        cmd_add(&tmp, &["curl".into(), "git".into(), "zsh".into()]);
        let pkgs = load_packages(&tmp);
        assert_eq!(pkgs.len(), 3);

        cmd_remove(&tmp, &["git".into()]);
        let pkgs = load_packages(&tmp);
        assert_eq!(pkgs.len(), 2);
        assert!(!pkgs.contains("git"));
    }

    #[test]
    fn parse_installed_output() {
        let output = "curl\tinstall ok installed\n\
                      git\tdeinstall ok config-files\n\
                      zsh\tinstall ok installed\n";
        let set = parse_installed(output);
        assert_eq!(set.len(), 2);
        assert!(set.contains("curl"));
        assert!(set.contains("zsh"));
        assert!(!set.contains("git"));
    }

    #[test]
    fn parse_installed_empty() {
        assert!(parse_installed("").is_empty());
    }

    #[test]
    fn parse_installed_malformed() {
        let output = "no-tab-here\n\tleading-tab\n";
        assert!(parse_installed(output).is_empty());
    }

    #[test]
    fn add_duplicate_is_idempotent() {
        let tmp = TempFile::new("dup.txt");
        save_packages(&tmp, &BTreeSet::new()).unwrap();

        cmd_add(&tmp, &["git".into(), "git".into(), "curl".into()]);
        let pkgs = load_packages(&tmp);
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs.contains("git"));
        assert!(pkgs.contains("curl"));

        // Adding again doesn't duplicate
        cmd_add(&tmp, &["git".into()]);
        let pkgs = load_packages(&tmp);
        assert_eq!(pkgs.len(), 2);
    }

    #[test]
    fn parse_history_entry() {
        let log = "\
Start-Date: 2026-02-10  12:11:38
Commandline: apt-get install -y build-essential
Requested-By: user (1000)
Install: build-essential:amd64 (12.12ubuntu1), gcc:amd64 (4:15.2.0-4ubuntu1, automatic)
End-Date: 2026-02-10  12:12:00
";
        let entries = parse_history(log);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-02-10  12:11:38");
        assert_eq!(entries[0].commandline, "apt-get install -y build-essential");
        assert_eq!(entries[0].requested_by.as_deref(), Some("user (1000)"));
        // Only non-automatic packages
        assert_eq!(entries[0].installed, vec!["build-essential"]);
    }

    #[test]
    fn parse_history_skips_upgrades() {
        let log = "\
Start-Date: 2026-02-06  08:54:10
Commandline: apt full-upgrade --autoremove --purge
Requested-By: user (1000)
Upgrade: python3.13:amd64 (3.13.7-1ubuntu0.2, 3.13.7-1ubuntu0.3)
End-Date: 2026-02-06  08:55:14
";
        let entries = parse_history(log);
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_history_multiple_entries() {
        let log = "\
Start-Date: 2025-07-17  11:55:46
Commandline: apt-get install --assume-yes apt-transport-https ca-certificates
Requested-By: user (1000)
Install: apt-transport-https:amd64 (3.0.0), ca-certificates:amd64 (1.0, automatic)
End-Date: 2025-07-17  11:56:00

Start-Date: 2026-01-15  14:26:08
Commandline: apt -y install apt-transport-https
Install: apt-transport-https:amd64 (3.1.6ubuntu2)
End-Date: 2026-01-15  14:26:10
";
        let entries = parse_history(log);
        assert_eq!(entries.len(), 2);
        let hits = find_install_history(&entries, "apt-transport-https");
        assert_eq!(hits.len(), 2);
        // Second entry has no requested_by
        assert!(hits[1].requested_by.is_none());
    }

    #[test]
    fn parse_history_packages_filters_automatic() {
        let line = "build-essential:amd64 (12.12), gcc:amd64 (15.2, automatic), make:amd64 (4.4, automatic)";
        let pkgs = parse_history_packages(line);
        assert_eq!(pkgs, vec!["build-essential"]);
    }

    #[test]
    fn find_history_no_match() {
        let entries = parse_history("");
        assert!(find_install_history(&entries, "nonexistent").is_empty());
    }

    #[test]
    fn why_shows_siblings() {
        let log = "\
Start-Date: 2025-08-10  10:00:00
Commandline: apt-get install uidmap aardvark-dns
Requested-By: user (1000)
Install: uidmap:amd64 (1.0), aardvark-dns:amd64 (1.0)
End-Date: 2025-08-10  10:01:00
";
        let entries = parse_history(log);
        let hits = find_install_history(&entries, "uidmap");
        assert_eq!(hits.len(), 1);
        let sibs = siblings(hits[0], "uidmap");
        assert_eq!(sibs, vec!["aardvark-dns"]);
    }

    #[test]
    fn why_shows_same_day_context() {
        let log = "\
Start-Date: 2025-08-10  10:00:00
Commandline: apt-get install uidmap aardvark-dns
Install: uidmap:amd64 (1.0), aardvark-dns:amd64 (1.0)
End-Date: 2025-08-10  10:01:00

Start-Date: 2025-08-10  14:00:00
Commandline: apt-get install podman slirp4netns
Install: podman:amd64 (1.0), slirp4netns:amd64 (1.0)
End-Date: 2025-08-10  14:01:00
";
        let entries = parse_history(log);
        let hits = find_install_history(&entries, "uidmap");
        let sibs = siblings(hits[0], "uidmap");
        let sibling_set: BTreeSet<&str> = sibs.iter().copied().collect();
        let neighbors = same_day_neighbors(&entries, hits[0], "uidmap", &sibling_set);
        assert_eq!(neighbors, vec!["podman", "slirp4netns"]);
    }

    #[test]
    fn why_same_day_excludes_siblings() {
        let log = "\
Start-Date: 2025-08-10  10:00:00
Commandline: apt-get install uidmap aardvark-dns
Install: uidmap:amd64 (1.0), aardvark-dns:amd64 (1.0)
End-Date: 2025-08-10  10:01:00

Start-Date: 2025-08-10  14:00:00
Commandline: apt-get install aardvark-dns podman
Install: aardvark-dns:amd64 (1.0), podman:amd64 (1.0)
End-Date: 2025-08-10  14:01:00
";
        let entries = parse_history(log);
        let hits = find_install_history(&entries, "uidmap");
        let sibs = siblings(hits[0], "uidmap");
        assert!(sibs.contains(&"aardvark-dns"));
        let sibling_set: BTreeSet<&str> = sibs.iter().copied().collect();
        let neighbors = same_day_neighbors(&entries, hits[0], "uidmap", &sibling_set);
        // aardvark-dns is already a sibling, should not appear in same-day
        assert!(!neighbors.contains(&"aardvark-dns"));
        assert!(neighbors.contains(&"podman"));
    }

    #[test]
    fn why_no_context_when_solo() {
        let log = "\
Start-Date: 2025-08-10  10:00:00
Commandline: apt-get install uidmap
Install: uidmap:amd64 (1.0)
End-Date: 2025-08-10  10:01:00
";
        let entries = parse_history(log);
        let hits = find_install_history(&entries, "uidmap");
        let sibs = siblings(hits[0], "uidmap");
        assert!(sibs.is_empty());
        let sibling_set: BTreeSet<&str> = sibs.iter().copied().collect();
        let neighbors = same_day_neighbors(&entries, hits[0], "uidmap", &sibling_set);
        assert!(neighbors.is_empty());
    }

    #[test]
    fn why_format_pkg_list_truncation() {
        let short: Vec<&str> = vec!["a", "b", "c"];
        assert_eq!(format_pkg_list(&short), "a, b, c");

        let exact: Vec<&str> = (0..10).map(|i| ["a","b","c","d","e","f","g","h","i","j"][i]).collect();
        assert_eq!(format_pkg_list(&exact), "a, b, c, d, e, f, g, h, i, j");

        let long: Vec<&str> = vec!["a","b","c","d","e","f","g","h","i","j","k","l","m"];
        let result = format_pkg_list(&long);
        assert!(result.ends_with("+ 3 more"));
        assert!(result.starts_with("a, b, c"));
    }

    #[test]
    fn parse_journal_pwd_extracts_path() {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/testuser".to_string());
        let journal = format!(
            "Feb 10 21:50:50 host sudo[12345]: PWD={home}/dotfiles ; USER=root ; COMMAND=/usr/bin/apt-get install -y uidmap\n\
             Feb 10 21:50:51 host systemd[1]: Starting apt-daily.service"
        );
        let result = parse_journal_pwd(&journal, "apt-get install -y uidmap");
        assert_eq!(result, Some("~/dotfiles".to_string()));
    }

    #[test]
    fn parse_journal_pwd_replaces_home() {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/testuser".to_string());
        let journal = format!(
            "Feb 10 21:50:50 host sudo[12345]: PWD={home}/projects/foo ; USER=root ; COMMAND=/usr/bin/apt install bar"
        );
        let result = parse_journal_pwd(&journal, "apt install bar");
        assert_eq!(result, Some("~/projects/foo".to_string()));
    }

    #[test]
    fn parse_journal_pwd_no_match() {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/testuser".to_string());
        let journal = format!(
            "Feb 10 21:50:50 host sudo[12345]: PWD={home}/dotfiles ; USER=root ; COMMAND=/usr/bin/apt-get install -y somepackage\n\
             Feb 10 21:50:51 host systemd[1]: Starting apt-daily.service"
        );
        let result = parse_journal_pwd(&journal, "apt-get install -y differentpackage");
        assert_eq!(result, None);
    }

    #[test]
    fn find_nearby_commands_window() {
        let history = vec![
            entry(1000, "git status"),
            entry(1100, "cd ~/project"),
            entry(1500, "make build"),
            entry(1800, "vim README.md"),
        ];
        let nearby = find_nearby_commands(&history, 1200, 300, false);
        // Within ±300s of 1200: 1000 (200s away), 1100 (100s away), 1500 (300s away)
        // 1800 is 600s away, excluded
        assert_eq!(nearby.len(), 3);
        assert!(nearby.contains(&"cd ~/project".to_string()));
        assert!(nearby.contains(&"git status".to_string()));
        assert!(nearby.contains(&"make build".to_string()));
        assert!(!nearby.contains(&"vim README.md".to_string()));
    }

    #[test]
    fn find_nearby_commands_excludes_apt() {
        let history = vec![
            entry(1000, "git status"),
            entry(1050, "apt-get install foo"),
            entry(1100, "apt install bar"),
        ];
        let nearby = find_nearby_commands(&history, 1050, 300, false);
        assert_eq!(nearby.len(), 1);
        assert_eq!(nearby[0], "git status");
    }

    #[test]
    fn find_nearby_commands_caps_at_5() {
        let mut history = Vec::new();
        for i in 0..10 {
            history.push(entry(1000 + i * 10, &format!("command{i}")));
        }
        let nearby = find_nearby_commands(&history, 1050, 300, false);
        assert_eq!(nearby.len(), 5);
    }

    #[test]
    fn find_nearby_commands_filters_trivial() {
        let history = vec![
            entry(1000, "ls -la"),
            entry(1010, "clear"),
            entry(1020, "git status"),
            entry(1030, "pwd"),
            entry(1040, "cargo build"),
        ];
        let nearby = find_nearby_commands(&history, 1020, 300, false);
        // Only git status and cargo build should be included
        assert_eq!(nearby.len(), 2);
        assert!(nearby.contains(&"git status".to_string()));
        assert!(nearby.contains(&"cargo build".to_string()));
        assert!(!nearby.contains(&"ls -la".to_string()));
        assert!(!nearby.contains(&"clear".to_string()));
        assert!(!nearby.contains(&"pwd".to_string()));
    }

    #[test]
    fn find_nearby_commands_show_all() {
        let history = vec![
            entry(1000, "ls -la"),
            entry(1010, "clear"),
            entry(1020, "git status"),
        ];
        let nearby = find_nearby_commands(&history, 1010, 300, true);
        // With show_all=true, all commands should be included
        assert_eq!(nearby.len(), 3);
        assert!(nearby.contains(&"ls -la".to_string()));
        assert!(nearby.contains(&"clear".to_string()));
        assert!(nearby.contains(&"git status".to_string()));
    }

    #[test]
    fn parse_zsh_history_entries() {
        let contents = "\
: 1723305600:0;git status
: 1723305610:0;cd ~/project
: 1723305620:0;cargo build
not a valid line
: invalid:0;skipped
";
        let entries = parse_shell_history(contents);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].timestamp, 1723305600);
        assert_eq!(entries[0].command, "git status");
        assert_eq!(entries[1].timestamp, 1723305610);
        assert_eq!(entries[1].command, "cd ~/project");
        assert_eq!(entries[2].timestamp, 1723305620);
        assert_eq!(entries[2].command, "cargo build");
    }
}
