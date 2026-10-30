mod accounts;
mod chats;
mod fsutil;
mod sections;

use std::{env, error::Error, fs::File, path::PathBuf, process::ExitCode};

use accounts::Config;

pub type Res<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "\
Sync Claude desktop chats and sidebar sections across accounts.

  ccsync                   sync chats across added accounts
  ccsync <account>         also copy <account>'s sections to the signed-in account
  ccsync list              show accounts on this Mac
  ccsync add <email> [id]  add an account
  ccsync remove <account>  stop syncing an account

  -n, --dry-run            show what would change, change nothing

<account> is an email or any unique part of one. Section sync needs Claude quit.";

pub struct Paths {
    home: PathBuf,
    /// Set when pointed at a copy of the data, where the running app is irrelevant.
    sandboxed: bool,
}

impl Paths {
    fn from_env() -> Res<Self> {
        let sandbox = env::var_os("CCSYNC_HOME");
        let sandboxed = sandbox.is_some();
        let home = sandbox
            .or_else(|| env::var_os("HOME"))
            .ok_or("HOME isn't set")?;
        Ok(Self {
            home: home.into(),
            sandboxed,
        })
    }
    fn app(&self) -> PathBuf {
        self.home.join("Library/Application Support/Claude")
    }
    fn code_root(&self) -> PathBuf {
        self.app().join("claude-code-sessions")
    }
    fn cowork_root(&self) -> PathBuf {
        self.app().join("local-agent-mode-sessions")
    }
    fn data(&self) -> PathBuf {
        self.home.join(".ccsync")
    }
    fn backups(&self) -> PathBuf {
        self.data().join("backups")
    }
    fn brief(&self, path: &std::path::Path) -> String {
        let s = path.display().to_string();
        let s = s.replace(&format!("{}/", self.app().display()), "");
        s.replace(&self.home.display().to_string(), "~")
    }
}

/// Holds an exclusive lock until dropped, so two runs never overlap.
fn lock(paths: &Paths) -> Res<File> {
    std::fs::create_dir_all(paths.data())?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.data().join("lock"))?;
    file.lock()?;
    Ok(file)
}

fn sync(paths: &Paths, source: Option<&str>, dry: bool) -> Res {
    let _lock = if dry { None } else { Some(lock(paths)?) };
    let mut cfg = Config::load(paths)?;
    if cfg.accounts.len() < 2 {
        return Err("add at least two accounts first; see `ccsync list`".into());
    }
    // A typo should fail before anything is touched.
    let source = source.map(|q| cfg.resolve(q).cloned()).transpose()?;
    let chats_ok = chats::sync(paths, &mut cfg, dry)?;
    if let Some(source) = source {
        println!("{}", sections::sync(paths, &cfg, &source, dry)?);
    }
    if chats_ok {
        Ok(())
    } else {
        Err("some chat folders were skipped".into())
    }
}

fn run() -> Res {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let paths = Paths::from_env()?;
    match args.as_slice() {
        ["-h" | "--help" | "help"] => println!("{USAGE}"),
        ["-V" | "--version"] => println!("ccsync {}", env!("CARGO_PKG_VERSION")),
        ["list"] => accounts::list(&paths)?,
        ["add", email] => accounts::add(&paths, email, None)?,
        ["add", email, id] => accounts::add(&paths, email, Some(id))?,
        ["remove", account] => accounts::remove(&paths, account)?,
        _ => {
            let (flags, rest): (Vec<&str>, Vec<&str>) =
                args.iter().partition(|a| a.starts_with('-'));
            let dry = !flags.is_empty();
            if rest.len() > 1 || flags.iter().any(|f| !matches!(*f, "-n" | "--dry-run")) {
                return Err(USAGE.into());
            }
            sync(&paths, rest.first().copied(), dry)?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccsync: {e}");
            ExitCode::FAILURE
        }
    }
}
