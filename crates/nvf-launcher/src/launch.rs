//! Starting the game: which program, with which arguments. The launcher passes no `--data`:
//! the game reads its folder from `[Game] Path` of the home's INI like a start without the
//! launcher, so a player's run through the launcher is a player's run in every way.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fnv_install::GameInstall;

#[cfg(windows)]
pub const GAME_EXE: &str = "nvforever.exe";
#[cfg(not(windows))]
pub const GAME_EXE: &str = "nvforever";

/// How the game is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// The main menu (the same as starting the program with no arguments).
    MainMenu,
    /// The newest save, or the one given.
    Load(PathBuf),
    /// Straight into the opening at Doc Mitchell's.
    NewGame,
    /// The free-camera explorer.
    Explorer,
}

impl Start {
    pub fn args(&self) -> Vec<String> {
        match self {
            Start::MainMenu => vec!["--play".into(), "--menu".into()],
            Start::Load(p) => vec!["--play".into(), "--load".into(), p.display().to_string()],
            Start::NewGame => vec!["--play".into(), "--new-game".into()],
            Start::Explorer => vec!["--explore".into()],
        }
    }
}

/// The game program: `[Launcher] Exe` of the INI when it is a file, else the one beside the
/// launcher (a release archive has both), else a build of this checkout (developers).
pub fn find_game(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    let here = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let beside = here.join(GAME_EXE);
    if beside.is_file() {
        return Some(beside);
    }
    // target/release/nvforever-launcher.exe beside engine/target/release/nvforever.exe.
    let root = here.ancestors().find(|a| a.join("engine").join("Cargo.toml").is_file())?;
    ["release", "release-quick"].iter().map(|p| root.join("engine").join("target").join(p).join(GAME_EXE)).find(|p| p.is_file())
}

/// The game the launcher started, and the error it gave on its way out (#2938): the game
/// says why it cannot start (no game folder, files missing) only on its stderr, which a
/// player starting it from the launcher never sees.
pub struct Game {
    child: Child,
    tail: Arc<Mutex<ErrorTail>>,
    reader: Option<JoinHandle<()>>,
}

impl Game {
    /// `None` while the game runs; once it has closed, `Ok` for a clean close, else what to
    /// tell the player.
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        let status = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(s)) => s,
            Err(e) => return Some(Err(format!("The game could not be watched: {e}"))),
        };
        // The last lines may still be on their way through the pipe: a short wait for the
        // reader, never a block (a process the game started could keep the pipe open).
        if let Some(r) = self.reader.take() {
            let until = Instant::now() + Duration::from_millis(500);
            while !r.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let error = self.tail.lock().ok().and_then(|t| t.text());
        Some(exit_note(status.success(), status.code(), error.as_deref()))
    }
}

/// Start `exe` with `start`'s arguments, in its own folder (it finds `schemas/` beside itself).
/// Its stderr goes through the launcher's own (a terminal still shows every line) and its
/// last error is kept for [`Game::poll`].
pub fn spawn(exe: &Path, start: &Start) -> std::io::Result<Game> {
    let mut c = Command::new(exe);
    c.args(start.args());
    if let Some(dir) = exe.parent() {
        c.current_dir(dir);
    }
    watch(c)
}

fn watch(mut c: Command) -> std::io::Result<Game> {
    let mut child = c.stderr(Stdio::piped()).spawn()?;
    let tail = Arc::new(Mutex::new(ErrorTail::default()));
    let reader = child.stderr.take().map(|err| {
        let tail = Arc::clone(&tail);
        std::thread::spawn(move || {
            let mut err = BufReader::new(err);
            let mut buf = Vec::new();
            while err.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\r', '\n']);
                eprintln!("{line}");
                if let Ok(mut t) = tail.lock() {
                    t.push(line);
                }
                buf.clear();
            }
        })
    });
    Ok(Game { child, tail, reader })
}

/// The game's last `error: ` line and the lines that carry it on (its setup errors span
/// several), up to the next log line.
#[derive(Debug, Default)]
pub struct ErrorTail {
    lines: Vec<String>,
    closed: bool,
}

/// How many lines of one error are kept.
const ERROR_LINES: usize = 12;

impl ErrorTail {
    pub fn push(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("error: ") {
            self.lines = vec![rest.trim().to_string()];
            self.closed = false;
        } else if !self.lines.is_empty() && !self.closed {
            let t = line.trim();
            if is_log_line(line) || self.lines.len() >= ERROR_LINES {
                self.closed = true;
            } else if !t.is_empty() {
                self.lines.push(t.to_string());
            }
        }
    }

    pub fn text(&self) -> Option<String> {
        (!self.lines.is_empty()).then(|| self.lines.join(" "))
    }
}

/// A line of the game's log rather than the rest of an error: `setup: …`, `warning: …` (a
/// short lowercase name before `: `), or the engine's own records (a time stamp or colour
/// code first).
fn is_log_line(line: &str) -> bool {
    if line.starts_with(|c: char| c == '\x1b' || c.is_ascii_digit()) {
        return true;
    }
    line.split_once(": ").is_some_and(|(name, _)| {
        !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || " _-".contains(c))
    })
}

/// What the launcher says once the game has closed: nothing after a clean close, else the
/// game's own error when it gave one, else its exit code.
pub fn exit_note(success: bool, code: Option<i32>, error: Option<&str>) -> Result<(), String> {
    match (success, error, code) {
        (true, ..) => Ok(()),
        (false, Some(e), _) => Err(format!("The game closed with an error: {e}")),
        (false, None, Some(c)) => Err(format!("The game closed with an error (exit code {c}).")),
        (false, None, None) => Err("The game closed unexpectedly.".to_string()),
    }
}

/// Steam's and GOG's usual places for the game [our choice: their default libraries, as the
/// game's first start on Windows opens its folder dialog at], to fill the Game folder box
/// when none is set. `home` is the user's home folder, for Linux's (Steam, its older
/// `~/.steam` link and the Flatpak).
pub fn usual_game_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    const GAME: &str = "Fallout New Vegas";
    if cfg!(windows) {
        return vec![PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common").join(GAME), PathBuf::from(r"C:\GOG Games").join(GAME)];
    }
    let Some(h) = home else { return Vec::new() };
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
        .iter()
        .map(|s| h.join(s).join("steamapps").join("common").join(GAME))
        .collect()
}

/// The game program a typed path names (`[Launcher] Exe`, typed where there is no file
/// picker): `None` for an empty box (found by itself again), the program inside a folder
/// that holds it, a leading `~/` read as `home`. An error says what is wrong.
pub fn program_at(typed: &str, home: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let t = typed.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let mut p = match (t.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(t),
    };
    if p.is_dir() {
        p = p.join(GAME_EXE);
    }
    if p.is_file() {
        Ok(Some(p))
    } else {
        Err(format!("There is no {} at {}.", GAME_EXE, p.display()))
    }
}

/// The first of `dirs` that holds the game.
pub fn first_game_dir(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().find(|d| GameInstall::open(d).is_ok()).cloned()
}

/// Open one of the launcher's own folders with the system's file browser.
pub fn open_folder(dir: &Path) {
    open(&dir.display().to_string());
}

/// Whether `url` may be handed to the system: an `https://` web page and nothing else. URLs
/// come from the network (`news.json`, GitHub), and the system opener runs whatever it is
/// given: a local program, a `\\host\share` path, a `file:` or other protocol handler.
/// Only URL characters are let through (RFC 3986's, no spaces, controls, quotes, backslashes
/// or commas, which explorer reads as its own switches); a page whose address needs others
/// is refused rather than guessed at.
pub fn is_web_url(url: &str) -> bool {
    let u = url.trim();
    let url_char = |c: char| c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+;=%".contains(c);
    u.len() > 8 && u.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) && u.chars().all(url_char)
}

/// Open a web page in the browser; anything but an `https://` URL is refused.
pub fn open_url(url: &str) {
    if is_web_url(url) {
        open(url.trim());
    } else {
        eprintln!("launcher: not opening {url:?}: only https:// pages are opened");
    }
}

fn open(target: &str) {
    #[cfg(windows)]
    let r = Command::new("explorer").arg(target).spawn();
    #[cfg(target_os = "macos")]
    let r = Command::new("open").arg(target).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let r = Command::new("xdg-open").arg(target).spawn();
    if let Err(e) = r {
        eprintln!("launcher: could not open {target}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        assert_eq!(Start::MainMenu.args(), ["--play", "--menu"]);
        assert_eq!(Start::NewGame.args(), ["--play", "--new-game"]);
        assert_eq!(Start::Explorer.args(), ["--explore"]);
        let a = Start::Load(PathBuf::from("s").join("q.nvfsave")).args();
        assert_eq!(&a[..2], ["--play", "--load"]);
        assert!(a[2].ends_with("q.nvfsave"));
    }

    #[test]
    fn the_games_setup_error_is_kept_whole() {
        let mut t = ErrorTail::default();
        for l in [
            "warning: language file skipped: x",
            "setup: something",
            "error: The Fallout: New Vegas folder set in /h/NVForever.ini",
            "(/games/fnv) cannot be used:",
            "",
            "Data has no FalloutNV.esm",
            "pass --data <Fallout New Vegas folder>, or set Path= under [Game] in /h/NVForever.ini",
            "setup: a log line after it",
            "more text",
        ] {
            t.push(l);
        }
        assert_eq!(
            t.text().as_deref(),
            Some("The Fallout: New Vegas folder set in /h/NVForever.ini (/games/fnv) cannot be used: Data has no FalloutNV.esm pass --data <Fallout New Vegas folder>, or set Path= under [Game] in /h/NVForever.ini")
        );
        // A later error replaces it; engine records end it.
        t.push("error: no Fallout: New Vegas folder is set: pass --data");
        t.push("2026-11-17T10:00:00Z  WARN bevy: x");
        t.push("tail");
        assert_eq!(t.text().as_deref(), Some("no Fallout: New Vegas folder is set: pass --data"));
        let mut none = ErrorTail::default();
        none.push("setup: fine");
        assert_eq!(none.text(), None);
    }

    /// A program that says an error on stderr and stops with an error: its words reach the
    /// launcher's message, as the game's setup error does.
    #[test]
    fn a_program_that_fails_says_why() {
        let shell = |script: &str| {
            let mut c = if cfg!(windows) { Command::new("cmd") } else { Command::new("sh") };
            c.args(if cfg!(windows) { ["/C", script] } else { ["-c", script] });
            c
        };
        let wait = |mut g: Game| loop {
            if let Some(r) = g.poll() {
                return r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let script = if cfg!(windows) { "echo error: no Fallout: New Vegas folder is set 1>&2 & exit /b 2" } else { "echo 'error: no Fallout: New Vegas folder is set' >&2; exit 2" };
        assert_eq!(wait(watch(shell(script)).unwrap()), Err("The game closed with an error: no Fallout: New Vegas folder is set".into()));
        assert_eq!(wait(watch(shell("exit 0")).unwrap()), Ok(()));
        let silent = if cfg!(windows) { "exit /b 5" } else { "exit 5" };
        assert_eq!(wait(watch(shell(silent)).unwrap()), Err("The game closed with an error (exit code 5).".into()));
    }

    #[test]
    fn a_typed_program_is_checked() {
        let home = std::env::temp_dir().join(format!("nvf-launcher-program-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join("NVForever").join("engine").join("target").join("release");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(program_at("  ", Some(&home)), Ok(None));
        assert!(program_at(&dir.display().to_string(), Some(&home)).unwrap_err().contains(GAME_EXE));
        let exe = dir.join(GAME_EXE);
        std::fs::write(&exe, b"x").unwrap();
        assert_eq!(program_at(&exe.display().to_string(), Some(&home)), Ok(Some(exe.clone())));
        assert_eq!(program_at(&format!(" {} ", dir.display()), Some(&home)), Ok(Some(exe.clone())), "the folder that holds it");
        let tilde = format!("~/NVForever/engine/target/release/{GAME_EXE}");
        assert_eq!(program_at(&tilde, Some(&home)), Ok(Some(home.join(format!("NVForever/engine/target/release/{GAME_EXE}")))));
        assert!(program_at(&tilde, None).is_err(), "no home: ~ is not guessed");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn exit_notes() {
        assert_eq!(exit_note(true, Some(0), Some("x")), Ok(()));
        assert_eq!(exit_note(false, Some(1), Some("no folder")), Err("The game closed with an error: no folder".into()));
        assert_eq!(exit_note(false, Some(3), None), Err("The game closed with an error (exit code 3).".into()));
        assert_eq!(exit_note(false, None, None), Err("The game closed unexpectedly.".into()));
    }

    #[test]
    fn usual_game_dirs_are_found_when_the_game_is_there() {
        let home = std::env::temp_dir().join(format!("nvf-launcher-usual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = usual_game_dirs(Some(&home));
        assert!(!dirs.is_empty());
        if cfg!(windows) {
            // Fixed drive paths there: only their shape is checked.
            assert!(dirs.iter().all(|d| d.ends_with("Fallout New Vegas")));
            return;
        }
        assert!(dirs.iter().all(|d| d.starts_with(&home) && d.ends_with("steamapps/common/Fallout New Vegas")));
        assert_eq!(first_game_dir(&dirs), None);
        let game = &dirs[1];
        std::fs::create_dir_all(game.join("Data")).unwrap();
        assert_eq!(first_game_dir(&dirs), None, "a Data folder without the master is not the game");
        std::fs::write(game.join("Data").join("FalloutNV.esm"), b"x").unwrap();
        assert_eq!(first_game_dir(&dirs).as_ref(), Some(game));
        assert!(usual_game_dirs(None).is_empty());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn only_https_pages_are_opened() {
        assert!(is_web_url("https://nv-forever.com/news/070"));
        assert!(is_web_url("HTTPS://github.com/LootJunkieAzza/NVForever/releases/tag/v0.7.0"));
        for bad in [
            "",
            "https://",
            "http://nv-forever.com",
            r"C:\Windows\System32\calc.exe",
            r"\\host\share\x.exe",
            "file:///C:/x.exe",
            "ms-settings:",
            "javascript:alert(1)",
            "https://x.com\" a.exe",
            "https://x.com/a b",
            "https://x.com/a\tb",
            "https://x.com/a,/select,C:\\x.exe",
            "https://x.com/\\x",
            "https://x.com/é",
            "aéééé…",
            "httpsé://x.com",
        ] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }
}
