//! Keeps chat files matched across every org folder of every added account.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::SystemTime,
};

use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};

use crate::{
    Paths, Res,
    accounts::Config,
    fsutil::{copy_chat_file, copy_tree, par, stamp},
};

/// Paths in a Cowork chat folder that belong to the signed-in account, not the chat.
const NOT_CHAT_DATA: [&str; 5] = [
    ".claude/.claude.json",
    ".claude/policy-limits.json",
    ".claude/cache",
    ".claude/backups",
    "uploads-tmp",
];

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Real (non-symlink) UUID-named folders in `dir`. A missing `dir` has none.
pub fn uuid_dirs(dir: &Path) -> Res<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str().filter(|n| is_uuid(n))
        {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

#[derive(PartialEq, Debug)]
pub enum Kind<'a> {
    Chat(&'a str),
    ChatDir(&'a str),
    Marker(&'a str),
    Other,
}

/// Anything not recognized here is never read, written or removed.
pub fn classify(name: &str, is_dir: bool, chat_dirs: bool) -> Kind<'_> {
    let id = |s: Option<&'_ str>| s.filter(|s| is_uuid(s)).is_some();
    let local = name.strip_prefix("local_");
    if is_dir {
        return if chat_dirs && id(local) {
            Kind::ChatDir(local.unwrap())
        } else {
            Kind::Other
        };
    }
    let chat = local.and_then(|s| s.strip_suffix(".json"));
    let marker = name.strip_prefix("deleted_");
    match () {
        () if id(chat) => Kind::Chat(chat.unwrap()),
        () if id(marker) => Kind::Marker(marker.unwrap()),
        () => Kind::Other,
    }
}

#[derive(Debug)]
pub enum Action {
    Copy { src: PathBuf, dst: PathBuf },
    Mkdir(PathBuf),
    Trash { src: PathBuf, dst: PathBuf },
}

impl Action {
    fn describe(&self, paths: &Paths) -> String {
        match self {
            Self::Copy { src, dst } => format!("copy {} -> {}", paths.brief(src), paths.brief(dst)),
            Self::Mkdir(dst) => format!("mkdir {}", paths.brief(dst)),
            Self::Trash { src, dst } => {
                format!("trash {} -> {}", paths.brief(src), paths.brief(dst))
            }
        }
    }
}

type Stamped = (PathBuf, SystemTime);
/// Chat id -> member index -> what that member has.
type Have<V> = HashMap<String, HashMap<usize, V>>;

#[derive(Default)]
struct Snapshot {
    chats: Have<Stamped>,
    dirs: Have<PathBuf>,
    markers: Have<PathBuf>,
    /// Chat ids with an unexpected entry somewhere: a write through one could go astray.
    blocked: HashSet<String>,
}

fn put<V>(map: &mut Have<V>, key: &str, member: usize, value: V) {
    map.entry(key.to_owned()).or_default().insert(member, value);
}

fn scan(members: &[PathBuf], chat_dirs: bool) -> Res<Snapshot> {
    let mut s = Snapshot::default();
    for (member, dir) in members.iter().enumerate() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            match classify(name, kind.is_dir(), chat_dirs) {
                Kind::Chat(id) if !kind.is_file() => {
                    return Err(
                        format!("{}: not a regular file ({id})", entry.path().display()).into(),
                    );
                }
                Kind::Chat(id) => match entry.metadata() {
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    meta => put(&mut s.chats, id, member, (entry.path(), meta?.modified()?)),
                },
                Kind::ChatDir(id) => put(&mut s.dirs, id, member, entry.path()),
                Kind::Marker(id) if kind.is_file() => put(&mut s.markers, id, member, entry.path()),
                Kind::Marker(id) => {
                    s.blocked.insert(id.to_owned());
                }
                Kind::Other => match classify(name, !kind.is_dir(), chat_dirs) {
                    Kind::Chat(id) | Kind::ChatDir(id) | Kind::Marker(id) => {
                        s.blocked.insert(id.to_owned());
                    }
                    Kind::Other => {}
                },
            }
        }
    }
    Ok(s)
}

#[derive(Default)]
struct ChatFields {
    session_id: Option<String>,
    transcript: Option<String>,
    title: Option<String>,
}

struct ChatVisitor;

impl<'de> Visitor<'de> for ChatVisitor {
    type Value = ChatFields;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a chat object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut found = ChatFields::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "sessionId" => found.session_id = map.next_value()?,
                "cliSessionId" => found.transcript = map.next_value()?,
                "title" => found.title = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(found)
    }
}

/// Stops an app update that changes the format from spreading: every chat must parse and name
/// itself. Skips values without allocating them, since these files total a gigabyte.
fn check_chat(path: &Path, id: &str) -> Res<ChatFields> {
    let raw = fs::read(path)?;
    let mut de = serde_json::Deserializer::from_slice(&raw);
    let found = de
        .deserialize_map(ChatVisitor)
        .and_then(|found| de.end().map(|()| found));
    let found = found.map_err(|e| format!("{}: not a chat file: {e}", path.display()))?;
    let session_id = found.session_id.as_deref();
    if session_id.and_then(|s| s.strip_prefix("local_")) != Some(id) {
        return Err(format!("{}: unexpected sessionId {session_id:?}", path.display()).into());
    }
    Ok(found)
}

/// Copies of a chat that open different transcripts, by chat id then transcript.
type Conflicts<'a> = BTreeMap<&'a str, (Option<String>, BTreeMap<String, Vec<&'a Path>>)>;

/// Modified time can't choose between these: Claude stamps a stale copy as new when it saves one,
/// so the newest copy may be the wrong one.
fn find_conflicts<'a>(checked: Vec<(&'a str, &'a Path, ChatFields)>) -> Conflicts<'a> {
    let mut conflicts = Conflicts::new();
    for (id, path, fields) in checked {
        let (title, copies) = conflicts.entry(id).or_default();
        if fields.title.is_some() {
            *title = fields.title;
        }
        if let Some(transcript) = fields.transcript {
            copies.entry(transcript).or_default().push(path);
        }
    }
    conflicts.retain(|_, (_, copies)| copies.len() > 1);
    conflicts
}

fn report_conflict(
    id: &str,
    title: Option<&str>,
    copies: &BTreeMap<String, Vec<&Path>>,
    paths: &Paths,
) {
    let title = title.unwrap_or("untitled");
    println!("skipped \"{title}\" (local_{id}): its copies open different transcripts");
    for (transcript, files) in copies {
        println!("  {transcript}");
        for file in files {
            println!("    {}", paths.brief(file));
        }
    }
    println!("  delete the copies of the wrong transcript, then sync again");
}

fn newest_wins(
    have: &HashMap<usize, Stamped>,
    count: usize,
    dst: impl Fn(usize) -> PathBuf,
) -> Vec<Action> {
    let Some((src, best)) = (0..count).filter_map(|m| have.get(&m)).fold(
        None,
        |best: Option<&Stamped>, f| match best {
            Some(b) if f.1 <= b.1 => Some(b),
            _ => Some(f),
        },
    ) else {
        return Vec::new();
    };
    (0..count)
        .filter(|m| have.get(m).is_none_or(|f| f.1 < *best))
        .map(|m| Action::Copy {
            src: src.clone(),
            dst: dst(m),
        })
        .collect()
}

/// What one member's chat folder holds, relative to its root.
#[derive(Default)]
struct DirTree {
    dirs: Have<()>,
    files: Have<Stamped>,
}

/// Returns false if the folder holds anything but plain files and folders.
fn walk(root: &Path, rel: &str, member: usize, tree: &mut DirTree) -> Res<bool> {
    put(&mut tree.dirs, rel, member, ());
    let entries = match fs::read_dir(root.join(rel)) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        other => other?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(false);
        };
        let child = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        if NOT_CHAT_DATA.contains(&child.as_str()) || (name.ends_with(".tmp") && is_temp(name)) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !walk(root, &child, member, tree)? {
                return Ok(false);
            }
        } else if kind.is_file() {
            match entry.metadata() {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                meta => put(
                    &mut tree.files,
                    &child,
                    member,
                    (entry.path(), meta?.modified()?),
                ),
            }
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_temp(name: &str) -> bool {
    name.starts_with(".ccsync-") || name.starts_with(".ccdejavu-")
}

/// Merges one Cowork chat folder file by file. Only a delete marker ever removes anything, and a
/// folder holding something unexpected is left alone rather than half-synced.
fn plan_chat_dir(
    members: &[PathBuf],
    id: &str,
    have: &HashMap<usize, PathBuf>,
) -> Res<Vec<Action>> {
    let mut tree = DirTree::default();
    for (&member, root) in have {
        if !walk(root, "", member, &mut tree)? {
            return Ok(Vec::new());
        }
    }
    if tree.files.keys().any(|rel| tree.dirs.contains_key(rel)) {
        return Ok(Vec::new());
    }
    let at = |m: usize, rel: &str| members[m].join(format!("local_{id}")).join(rel);
    let mut out = Vec::new();
    for (rel, present) in &tree.dirs {
        out.extend(
            (0..members.len())
                .filter(|m| !present.contains_key(m))
                .map(|m| Action::Mkdir(at(m, rel))),
        );
    }
    for (rel, copies) in &tree.files {
        out.extend(newest_wins(copies, members.len(), |m| at(m, rel)));
    }
    Ok(out)
}

fn plan(members: &[PathBuf], root: &Path, trash: &Path, s: &Snapshot) -> Res<Vec<Action>> {
    let trash_path = |src: &Path| -> Res<PathBuf> {
        let rel = src.strip_prefix(root)?;
        Ok(trash.join(root.file_name().unwrap_or_default()).join(rel))
    };
    let mut actions = Vec::new();
    for (id, have) in &s.markers {
        if s.blocked.contains(id) {
            continue;
        }
        let Some(marker) = (0..members.len()).find_map(|m| have.get(&m)) else {
            continue;
        };
        for (m, dir) in members.iter().enumerate() {
            if !have.contains_key(&m) {
                actions.push(Action::Copy {
                    src: marker.clone(),
                    dst: dir.join(format!("deleted_{id}")),
                });
            }
            let chat = s
                .chats
                .get(id)
                .and_then(|c| c.get(&m))
                .map(|(path, _)| path);
            let chat_dir = s.dirs.get(id).and_then(|d| d.get(&m));
            for src in chat.into_iter().chain(chat_dir) {
                actions.push(Action::Trash {
                    src: src.clone(),
                    dst: trash_path(src)?,
                });
            }
        }
    }
    let live = |id: &String| !s.blocked.contains(id) && !s.markers.contains_key(id);
    for (id, have) in s.chats.iter().filter(|(id, _)| live(id)) {
        actions.extend(newest_wins(have, members.len(), |m| {
            members[m].join(format!("local_{id}.json"))
        }));
    }
    for (id, have) in s.dirs.iter().filter(|(id, _)| live(id)) {
        actions.extend(plan_chat_dir(members, id, have)?);
    }
    Ok(actions)
}

/// Returns how many actions completed, and the first failure.
fn apply(actions: &[Action]) -> (usize, Res) {
    let done = AtomicUsize::new(0);
    let (mkdirs, rest): (Vec<_>, Vec<_>) =
        actions.iter().partition(|a| matches!(a, Action::Mkdir(_)));
    let run = |action: &&Action| -> Res {
        match action {
            Action::Mkdir(dst) => fs::create_dir_all(dst)?,
            Action::Copy { src, dst } => copy_chat_file(src, dst)?,
            Action::Trash { src, dst } => {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                if fs::symlink_metadata(dst).is_ok() {
                    return Err(format!("{} already exists", dst.display()).into());
                }
                fs::rename(src, dst)?;
            }
        }
        done.fetch_add(1, Relaxed);
        Ok(())
    };
    let result = mkdirs
        .iter()
        .try_for_each(&run)
        .and_then(|()| par(&rest, run));
    (done.into_inner(), result)
}

fn sync_root(
    members: &[PathBuf],
    root: &Path,
    trash: &Path,
    chat_dirs: bool,
    paths: &Paths,
    dry: bool,
) -> Res<(usize, usize)> {
    let mut s = scan(members, chat_dirs)?;
    let chats: Vec<(&String, &PathBuf)> = s
        .chats
        .iter()
        .flat_map(|(id, have)| have.values().map(move |(path, _)| (id, path)))
        .collect();
    let checked = Mutex::new(Vec::with_capacity(chats.len()));
    par(&chats, |(id, path)| {
        let fields = check_chat(path, id)?;
        checked
            .lock()
            .unwrap()
            .push((id.as_str(), path.as_path(), fields));
        Ok(())
    })?;
    let mut conflicts = find_conflicts(checked.into_inner().unwrap());
    conflicts.retain(|id, _| !s.markers.contains_key(*id));
    let skipped = conflicts.len();
    for (id, (title, copies)) in &conflicts {
        report_conflict(id, title.as_deref(), copies, paths);
    }
    let conflicted: Vec<String> = conflicts.keys().map(|id| (*id).to_owned()).collect();
    s.blocked.extend(conflicted);
    let actions = plan(members, root, trash, &s)?;
    if dry {
        let mut lines: Vec<_> = actions.iter().map(|a| a.describe(paths)).collect();
        lines.sort_unstable();
        lines.iter().for_each(|line| println!("{line}"));
        return Ok((actions.len(), skipped));
    }
    let (done, result) = apply(&actions);
    result.map_err(|e| format!("stopped after {done} of {} changes: {e}", actions.len()))?;
    Ok((done, skipped))
}

fn member_folders(root: &Path, cfg: &Config) -> Res<Vec<PathBuf>> {
    let mut members = Vec::new();
    for account in &cfg.accounts {
        let dir = root.join(&account.id);
        members.extend(uuid_dirs(&dir)?.into_iter().map(|org| dir.join(org)));
    }
    members.sort_unstable();
    Ok(members)
}

/// Syncs both chat roots, backing them up first whenever an account hasn't been covered yet.
/// Returns false if a root was skipped; the other still syncs.
pub fn sync(paths: &Paths, cfg: &mut Config, dry: bool) -> Res<bool> {
    let roots = [
        ("code", paths.code_root(), false),
        ("cowork", paths.cowork_root(), true),
    ];
    let stamp = stamp();
    let uncovered = cfg.accounts.iter().any(|a| !cfg.backed_up.contains(&a.id));
    if uncovered {
        let dest = paths.backups().join(stamp.to_string());
        if dry {
            println!("would back up chat folders to {}", paths.brief(&dest));
        } else {
            let partial = dest.with_extension("partial");
            for (_, root, _) in &roots {
                if root.exists() {
                    copy_tree(root, &partial.join(root.file_name().unwrap_or_default()))
                        .map_err(|e| format!("backup failed, nothing synced: {e}"))?;
                }
            }
            fs::rename(partial, &dest)?;
            cfg.backed_up = cfg.accounts.iter().map(|a| a.id.clone()).collect();
            cfg.save(paths)?;
            println!("backed up chat folders to {}", paths.brief(&dest));
        }
    }
    let trash = paths.data().join("trash").join(stamp.to_string());
    let mut ok = true;
    for (name, root, chat_dirs) in &roots {
        let members = member_folders(root, cfg)?;
        if members.len() < 2 {
            continue;
        }
        match sync_root(&members, root, &trash, *chat_dirs, paths, dry) {
            Ok((n, skipped)) => {
                ok &= skipped == 0;
                let verb = if dry { "planned" } else { "made" };
                println!(
                    "{name}: {n} changes {verb} across {} folders",
                    members.len()
                );
            }
            Err(e) => {
                ok = false;
                println!(
                    "{name}: skipped: {}",
                    paths.brief(Path::new(&e.to_string()))
                );
            }
        }
    }
    Ok(ok)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::accounts::Account;
    use std::time::Duration;

    pub const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    pub const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    pub const ORG: &str = "cccccccc-0000-4000-8000-000000000003";
    const CHAT: &str = "dddddddd-0000-4000-8000-000000000004";

    pub fn sandbox(name: &str) -> Paths {
        let home = std::env::temp_dir().join(format!("ccsync-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        Paths {
            home,
            sandboxed: true,
        }
    }

    pub fn config() -> Config {
        let account = |email: &str, id: &str| Account {
            email: email.into(),
            id: id.into(),
        };
        Config {
            accounts: vec![account("a@x.co", A), account("b@x.co", B)],
            backed_up: vec![A.into(), B.into()],
        }
    }

    fn folder(paths: &Paths, account: &str) -> PathBuf {
        let dir = paths.code_root().join(account).join(ORG);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chat(dir: &Path, id: &str, body: &str, age: u64) {
        let path = dir.join(format!("local_{id}.json"));
        fs::write(
            &path,
            format!(r#"{{"sessionId":"local_{id}","body":"{body}"}}"#),
        )
        .unwrap();
        let time = SystemTime::now() - Duration::from_secs(age);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn classifies_only_known_names() {
        assert_eq!(
            classify(&format!("local_{CHAT}.json"), false, false),
            Kind::Chat(CHAT)
        );
        assert_eq!(
            classify(&format!("local_{CHAT}"), true, true),
            Kind::ChatDir(CHAT)
        );
        assert_eq!(classify(&format!("local_{CHAT}"), true, false), Kind::Other);
        assert_eq!(
            classify(&format!("deleted_{CHAT}"), false, false),
            Kind::Marker(CHAT)
        );
        assert_eq!(classify("local_nope.json", false, false), Kind::Other);
    }

    #[test]
    fn newest_copy_reaches_every_account() {
        let paths = sandbox("newest");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat(&a, CHAT, "old", 100);
        chat(&b, CHAT, "new", 10);
        let other = "eeeeeeee-0000-4000-8000-000000000005";
        chat(&a, other, "only-a", 50);
        fs::write(a.join("unrelated.txt"), "keep").unwrap();

        assert!(sync(&paths, &mut config(), false).unwrap());

        let body = fs::read_to_string(a.join(format!("local_{CHAT}.json"))).unwrap();
        assert!(body.contains("new"));
        assert!(b.join(format!("local_{other}.json")).exists());
        assert!(!b.join("unrelated.txt").exists());
        let before = fs::metadata(a.join(format!("local_{CHAT}.json")))
            .unwrap()
            .modified()
            .unwrap();
        assert!(sync(&paths, &mut config(), false).unwrap());
        assert_eq!(
            before,
            fs::metadata(b.join(format!("local_{CHAT}.json")))
                .unwrap()
                .modified()
                .unwrap()
        );
    }

    fn chat_on(dir: &Path, id: &str, transcript: Option<&str>, age: u64) {
        let path = dir.join(format!("local_{id}.json"));
        let transcript = transcript.map_or(String::new(), |t| format!(r#","cliSessionId":"{t}""#));
        fs::write(
            &path,
            format!(r#"{{"sessionId":"local_{id}","title":"Work"{transcript}}}"#),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(age))
            .unwrap();
    }

    fn transcript_of(dir: &Path, id: &str) -> serde_json::Value {
        let raw = fs::read(dir.join(format!("local_{id}.json"))).unwrap();
        serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["cliSessionId"].clone()
    }

    #[test]
    fn copies_opening_different_transcripts_are_left_alone() {
        let paths = sandbox("conflict");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat_on(&a, CHAT, Some("right"), 100);
        chat_on(&b, CHAT, Some("stale"), 10);
        let other = "eeeeeeee-0000-4000-8000-000000000005";
        chat(&a, other, "only-a", 50);

        assert!(!sync(&paths, &mut config(), false).unwrap());

        assert_eq!(transcript_of(&a, CHAT), "right");
        assert_eq!(transcript_of(&b, CHAT), "stale");
        assert!(b.join(format!("local_{other}.json")).exists());
    }

    #[test]
    fn deleting_the_wrong_copy_resolves_a_conflict() {
        let paths = sandbox("resolve");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat_on(&a, CHAT, Some("right"), 100);
        chat_on(&b, CHAT, Some("stale"), 10);
        assert!(!sync(&paths, &mut config(), false).unwrap());

        fs::remove_file(b.join(format!("local_{CHAT}.json"))).unwrap();

        assert!(sync(&paths, &mut config(), false).unwrap());
        assert_eq!(transcript_of(&b, CHAT), "right");
    }

    #[test]
    fn a_copy_without_a_transcript_is_no_conflict() {
        let paths = sandbox("no-transcript");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat_on(&a, CHAT, None, 100);
        chat_on(&b, CHAT, Some("started"), 10);

        assert!(sync(&paths, &mut config(), false).unwrap());
        assert_eq!(transcript_of(&a, CHAT), "started");
    }

    #[test]
    fn a_delete_still_wins_over_a_conflict() {
        let paths = sandbox("conflict-delete");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat_on(&a, CHAT, Some("one"), 100);
        chat_on(&b, CHAT, Some("two"), 10);
        fs::write(a.join(format!("deleted_{CHAT}")), "").unwrap();

        assert!(sync(&paths, &mut config(), false).unwrap());
        assert!(!a.join(format!("local_{CHAT}.json")).exists());
        assert!(!b.join(format!("local_{CHAT}.json")).exists());
    }

    #[test]
    fn delete_marker_moves_chats_to_trash() {
        let paths = sandbox("delete");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat(&b, CHAT, "doomed", 10);
        fs::write(a.join(format!("deleted_{CHAT}")), "").unwrap();

        assert!(sync(&paths, &mut config(), false).unwrap());

        assert!(!b.join(format!("local_{CHAT}.json")).exists());
        assert!(b.join(format!("deleted_{CHAT}")).exists());
        let trashed = fs::read_dir(paths.data().join("trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            trashed
                .join("claude-code-sessions")
                .join(B)
                .join(ORG)
                .join(format!("local_{CHAT}.json"))
                .exists()
        );
    }

    #[test]
    fn unfamiliar_chat_stops_the_root() {
        let paths = sandbox("bad");
        let (a, b) = (folder(&paths, A), folder(&paths, B));
        chat(&a, CHAT, "fine", 10);
        let other = "eeeeeeee-0000-4000-8000-000000000005";
        fs::write(
            b.join(format!("local_{other}.json")),
            r#"{"sessionId":"local_wrong"}"#,
        )
        .unwrap();

        assert!(!sync(&paths, &mut config(), false).unwrap());
        assert!(!b.join(format!("local_{CHAT}.json")).exists());
    }

    #[test]
    fn dry_run_and_cowork_folders() {
        let paths = sandbox("cowork");
        let root = paths.cowork_root();
        let a = root.join(A).join(ORG);
        let b = root.join(B).join(ORG);
        fs::create_dir_all(a.join(format!("local_{CHAT}/.claude"))).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join(format!("local_{CHAT}/note.md")), "hi").unwrap();
        fs::write(
            a.join(format!("local_{CHAT}/.claude/.claude.json")),
            "account",
        )
        .unwrap();

        assert!(sync(&paths, &mut config(), true).unwrap());
        assert!(!b.join(format!("local_{CHAT}")).exists());

        assert!(sync(&paths, &mut config(), false).unwrap());
        assert_eq!(
            fs::read_to_string(b.join(format!("local_{CHAT}/note.md"))).unwrap(),
            "hi"
        );
        assert!(
            !b.join(format!("local_{CHAT}/.claude/.claude.json"))
                .exists()
        );
    }

    #[test]
    fn first_sync_of_an_account_backs_up() {
        let paths = sandbox("backup");
        let a = folder(&paths, A);
        folder(&paths, B);
        chat(&a, CHAT, "x", 10);
        let mut cfg = config();
        cfg.backed_up.clear();

        assert!(sync(&paths, &mut cfg, false).unwrap());

        assert_eq!(cfg.backed_up.len(), 2);
        let backup = fs::read_dir(paths.backups())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            backup
                .join("claude-code-sessions")
                .join(A)
                .join(ORG)
                .join(format!("local_{CHAT}.json"))
                .exists()
        );
    }
}
