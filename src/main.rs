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
const MAGENTA: &str = "\x1b[35m";

// ── Package list I/O ────────────────────────────────────────────────

fn pkg_file_path() -> PathBuf {
    let exe = env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("."));
    // Walk up from target/debug or target/release to repo root
    let repo = dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists())
        .unwrap_or(Path::new("."));
    repo.join("packages.txt")
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
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
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
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
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
        .map(|s| s.as_str())
        .collect();
    if missing.is_empty() {
        println!(
            "{GREEN}✨ All {count} curated packages are already installed!{RESET}",
            count = pkgs.len()
        );
        return;
    }
    println!(
        "{BOLD}{MAGENTA}🚀 Installing {n} missing package(s){RESET}\n",
        n = missing.len()
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

fn cmd_snap(pkg_path: &Path) {
    let system = system_manual_packages();
    let curated = load_packages(pkg_path);
    let uncurated: Vec<&String> = system.difference(&curated).collect();

    if uncurated.is_empty() {
        println!("{GREEN}✨ All manual system packages are already curated!{RESET}");
        return;
    }

    println!(
        "{BOLD}{CYAN}📸 Snapshot — {n} uncurated manual packages{RESET}\n",
        n = uncurated.len()
    );
    println!(
        "{DIM}For each package, type {RESET}{BOLD}y{RESET}{DIM} to add, \
         {RESET}{BOLD}n{RESET}{DIM} to skip, \
         {RESET}{BOLD}q{RESET}{DIM} to quit:{RESET}\n"
    );

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut to_add = Vec::new();

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

    if to_add.is_empty() {
        println!("\n{DIM}No packages added.{RESET}");
        return;
    }
    cmd_add(pkg_path, &to_add);
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
    {GREEN}snap{RESET}             Interactively pick from system packages\n\
\n\
{BOLD}OPTIONS:{RESET}\n    \
    {YELLOW}--dry-run{RESET}        Show what would happen (install only)\n    \
    {YELLOW}--help, -h{RESET}       Show this help\n",
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
        let dir = std::env::temp_dir().join("apt-sync-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test-packages.txt");
        let mut pkgs = BTreeSet::new();
        pkgs.insert("curl".to_string());
        pkgs.insert("git".to_string());
        pkgs.insert("zsh".to_string());
        save_packages(&path, &pkgs).unwrap();
        let loaded = load_packages(&path);
        assert_eq!(pkgs, loaded);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn diff_logic() {
        let mut curated = BTreeSet::new();
        curated.insert("git".into());
        curated.insert("curl".into());
        curated.insert("zsh".into());
        let mut system = BTreeSet::new();
        system.insert("git".into());
        system.insert("vim".into());
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
        let dir = std::env::temp_dir().join("apt-sync-test-header");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("header-test.txt");
        let pkgs = BTreeSet::from(["git".to_string()]);
        save_packages(&path, &pkgs).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("# apt-sync curated packages\n"));
        assert!(raw.contains("# one package per line"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn add_remove_roundtrip() {
        let dir = std::env::temp_dir().join("apt-sync-test-addrem");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("addrem-test.txt");
        save_packages(&path, &BTreeSet::new()).unwrap();

        cmd_add(&path, &["curl".into(), "git".into(), "zsh".into()]);
        let pkgs = load_packages(&path);
        assert_eq!(pkgs.len(), 3);

        cmd_remove(&path, &["git".into()]);
        let pkgs = load_packages(&path);
        assert_eq!(pkgs.len(), 2);
        assert!(!pkgs.contains("git"));

        let _ = fs::remove_file(&path);
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
}
