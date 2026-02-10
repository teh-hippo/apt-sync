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

    for path in &gz_paths {
        if let Ok(output) = Command::new("zcat").arg(path).output() {
            buf.push_str(&String::from_utf8_lossy(&output.stdout));
        }
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
            "{DIM}Would run: sudo apt-get install -y {}{RESET}",
            missing.join(" ")
        );
        return;
    }
    let status = Command::new("sudo")
        .args(["apt-get", "install", "-y"])
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

fn cmd_why(names: &[String]) {
    let log = read_history_logs();
    let entries = parse_history(&log);

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
    let rest_no_flags: Vec<String> = rest.iter().filter(|a| !a.starts_with('-')).cloned().collect();

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
            cmd_why(&rest_no_flags);
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
}
