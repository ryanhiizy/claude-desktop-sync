//! Copies an account's sidebar sections to the signed-in account.
//!
//! Claude keeps every account's sections in one local store, keyed by `account/org`, and mirrors
//! them in its preferences. Each account's definitions also live on Claude's server, and on an
//! account switch the server copy wins, so only the signed-in account can be changed. A pending
//! marker naming its exact `account/org` makes the next launch upload the local copy.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use rusty_leveldb::{DB, Options, WriteBatch};
use serde_json::{Map, Value, json};

use crate::{
    Paths, Res,
    accounts::{Account, Config},
    fsutil::{copy_tree, prepare_atomic, stamp, temp_sibling},
};

const KEY_PREFIX: &[u8] = b"_https://claude.ai\x00\x01";
const PENDING: &str = "ccd-sync-pending:ccd/dframe-store";
const STORE: &str = "dframe-store";
const MIRRORS: [&str; 2] = ["dframe-group-scopes", "dframe-code-sections"];
const LOCAL_CHAT: &str = "code:local_";

fn decode(raw: &[u8]) -> Res<String> {
    match raw.split_first() {
        Some((1, latin1)) => Ok(latin1.iter().map(|&b| b as char).collect()),
        Some((0, utf16)) if utf16.len() % 2 == 0 => {
            let units = utf16
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c));
            Ok(char::decode_utf16(units).collect::<Result<_, _>>()?)
        }
        _ => Err("unknown Chromium value encoding".into()),
    }
}

fn encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + s.len() * 2);
    out.push(0);
    out.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

fn storage_key(key: &str) -> Vec<u8> {
    [KEY_PREFIX, key.as_bytes()].concat()
}

fn read(db: &mut DB, key: &str) -> Res<Option<String>> {
    db.get(&storage_key(key))
        .map(|raw| decode(&raw))
        .transpose()
}

fn read_json(db: &mut DB, key: &str) -> Res<Value> {
    let raw = read(db, key)?.ok_or_else(|| format!("Claude's storage has no {key}"))?;
    Ok(serde_json::from_str(&raw)?)
}

fn object_mut<'a>(v: &'a mut Value, what: &str) -> Res<&'a mut Map<String, Value>> {
    v.as_object_mut()
        .ok_or_else(|| format!("unrecognized {what}").into())
}

fn ids_by_name(scope: &Value) -> Res<HashMap<String, String>> {
    let mut out = HashMap::new();
    let mut seen = HashSet::new();
    let groups = scope["groups"].as_array().ok_or("section list missing")?;
    for group in groups {
        let (Some(id), Some(name)) = (group["id"].as_str(), group["name"].as_str()) else {
            return Err("section without an id or name".into());
        };
        if id.is_empty()
            || name.is_empty()
            || !seen.insert(id)
            || out.insert(name.to_owned(), id.to_owned()).is_some()
        {
            return Err(format!("empty or duplicate section {name:?}").into());
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct Remapped {
    groups: Value,
    layout: Value,
    removed: Vec<String>,
}

/// Rewrites the source's sections for one destination, source winning. A section with the same
/// name keeps the destination's id; new ones bring the source's. Chats the destination doesn't
/// have (`keep` says no) are left out, since cloud chats and routines belong to their account.
fn remap(
    source: &Value,
    target: &Value,
    layout: &Value,
    keep: &dyn Fn(&str) -> bool,
) -> Res<Remapped> {
    let from = ids_by_name(source)?;
    let to = ids_by_name(target)?;
    let mapping: HashMap<&str, &str> = from
        .iter()
        .map(|(name, id)| (id.as_str(), to.get(name).unwrap_or(id).as_str()))
        .collect();
    if mapping.values().collect::<HashSet<_>>().len() != mapping.len() {
        return Err("sections from the source collide in the destination".into());
    }
    let map_id = |id: Option<&str>| -> Res<Value> {
        let id = id.ok_or("section reference isn't a string")?;
        Ok(Value::from(*mapping.get(id).ok_or_else(|| {
            format!("reference to unknown section {id}")
        })?))
    };
    let keep_members = |members: &mut Value| {
        if let Some(list) = members.as_array_mut() {
            list.retain(|m| m.as_str().is_some_and(keep));
        }
    };

    let mut groups = source.clone();
    for group in groups["groups"]
        .as_array_mut()
        .ok_or("section list missing")?
    {
        group["id"] = map_id(group["id"].as_str())?;
    }
    if let Some(assignments) = groups.get_mut("assignments") {
        let assignments = object_mut(assignments, "section assignments")?;
        assignments.retain(|chat, _| keep(chat));
        for section in assignments.values_mut() {
            *section = map_id(section.as_str())?;
        }
    }
    if let Some(order) = groups.get_mut("order") {
        let order = object_mut(order, "section order")?;
        let mut mapped = Map::new();
        for (section, members) in order.iter_mut() {
            keep_members(members);
            let id = map_id(Some(section))?;
            mapped.insert(id.as_str().unwrap_or_default().to_owned(), members.take());
        }
        *order = mapped;
    }
    if let Some(ordered) = groups
        .get_mut("orderedGroups")
        .and_then(Value::as_array_mut)
    {
        for id in ordered {
            *id = map_id(id.as_str())?;
        }
    }
    if let Some(after) = groups.get("pinnedAfter").and_then(Value::as_str)
        && mapping.contains_key(after)
    {
        groups["pinnedAfter"] = map_id(Some(after))?;
    }

    let mut layout = layout.clone();
    for section in layout["sections"]
        .as_array_mut()
        .ok_or("section layout missing")?
    {
        if section["kind"] == "manual" {
            section["id"] = map_id(section["id"].as_str())?;
            if let Some(members) = section.get_mut("members") {
                keep_members(members);
            }
        }
    }
    let mut removed: Vec<String> = to
        .into_keys()
        .filter(|name| !from.contains_key(name))
        .collect();
    removed.sort_unstable();
    Ok(Remapped {
        groups,
        layout,
        removed,
    })
}

fn leveldb_dir(paths: &Paths) -> PathBuf {
    paths.app().join("Local Storage/leveldb")
}

/// A dry run reads a throwaway copy, so it works beside a running Claude and changes nothing.
struct Storage {
    db: DB,
    copy: Option<PathBuf>,
}

impl Storage {
    fn open(paths: &Paths, dry: bool) -> Res<Self> {
        let mut dir = leveldb_dir(paths);
        let mut copy = None;
        if dry {
            let temp = temp_sibling(&std::env::temp_dir().join("x"));
            copy_tree(&dir, &temp)?;
            dir.clone_from(&temp);
            copy = Some(temp);
        }
        let opts = Options {
            create_if_missing: false,
            ..Options::default()
        };
        let db = DB::open(&dir, opts).map_err(|e| format!("can't open Claude's storage: {e}"))?;
        Ok(Self { db, copy })
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Some(copy) = &self.copy {
            let _ = fs::remove_dir_all(copy);
        }
    }
}

/// The scope with the most chats, for an account that has several orgs.
fn source_scope(groups: &Map<String, Value>, source: &str) -> Option<String> {
    let prefix = format!("{source}/");
    let count = |v: &Value| v["assignments"].as_object().map_or(0, Map::len);
    let mut scopes: Vec<_> = groups
        .iter()
        .filter(|(scope, _)| scope.starts_with(&prefix))
        .collect();
    scopes.sort_by_key(|(scope, _)| scope.as_str());
    scopes
        .iter()
        .rev()
        .max_by_key(|(_, v)| count(v))
        .map(|(scope, _)| (*scope).clone())
}

pub fn sync(paths: &Paths, cfg: &Config, source: &Account, dry: bool) -> Res<String> {
    let mut storage = Storage::open(paths, dry)?;
    let db = &mut storage.db;
    let owner = read(db, "ccd-sync-owner")?
        .filter(|o| !o.is_empty())
        .ok_or("no signed-in account found")?;
    if owner == source.id {
        return Ok(format!(
            "{} is signed in, so there's nothing to copy; switch to the account to update",
            source.email
        ));
    }
    let org = read(db, "ccd-sync-org")?
        .filter(|o| !o.is_empty() && o != "mixed")
        .ok_or("signed-in org isn't settled")?;
    let dest = format!("{owner}/{org}");
    if db.get(&storage_key(PENDING)).is_some() {
        return Err(
            "Claude has sidebar edits waiting to upload; open it once to let them finish".into(),
        );
    }

    let mut store = read_json(db, STORE)?;
    if store["version"] != 1 {
        return Err("unsupported sidebar store version".into());
    }
    let groups = object_mut(&mut store["state"]["customGroupsByScope"], "sidebar store")?;
    let src = source_scope(groups, &source.id)
        .ok_or_else(|| format!("no saved sections for {}; sign in to it once", source.email))?;
    let source_groups = groups[&src].clone();
    if source_groups["groups"].as_array().is_none_or(Vec::is_empty) {
        return Err(format!("{} has no sections to copy", source.email).into());
    }
    let target_groups = groups.get(&dest).cloned().unwrap_or(json!({"groups": []}));
    let layouts = object_mut(&mut store["state"]["codeSidebarByScope"], "sidebar store")?;
    let source_layout = layouts
        .get(&src)
        .ok_or_else(|| format!("no section layout for {}", source.email))?;

    let dir = paths.code_root().join(&owner).join(&org);
    let keep = |member: &str| {
        member.strip_prefix(LOCAL_CHAT).is_some_and(|id| {
            let name = format!("local_{id}.json");
            !name.contains('/') && (dry || dir.join(&name).exists())
        })
    };
    let new = remap(&source_groups, &target_groups, source_layout, &keep)?;
    let target = cfg.email_of(&owner);
    if target_groups == new.groups && layouts.get(&dest) == Some(&new.layout) {
        return Ok(format!("{target} already has {}'s sections", source.email));
    }
    let chats = new.groups["assignments"].as_object().map_or(0, Map::len);
    let sections = new.groups["groups"].as_array().map_or(0, Vec::len);
    let replaced = match new.removed.as_slice() {
        [] => String::new(),
        names => format!(", replacing {}", names.join(", ")),
    };
    let summary = format!(
        "{} -> {target}: {sections} sections, {chats} chats{replaced}",
        source.email
    );
    if dry {
        return Ok(format!("would copy sections {summary}"));
    }
    layouts.insert(dest.clone(), new.layout.clone());
    object_mut(&mut store["state"]["customGroupsByScope"], "sidebar store")?
        .insert(dest.clone(), new.groups.clone());

    let config_path = paths.app().join("claude_desktop_config.json");
    let raw_config = fs::read(&config_path)?;
    let mut config: Value = serde_json::from_slice(&raw_config)?;
    let prefs = &mut config["preferences"]["epitaxyPrefs"];
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let mut updates: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, value) in MIRRORS.iter().zip([&new.groups, &new.layout]) {
        let wire = format!("LSS-persisted.{name}");
        let mut envelope = read_json(db, &wire)?;
        for mirror in [&mut prefs[*name], &mut envelope["value"]] {
            object_mut(mirror, "section mirror")?.insert(dest.clone(), value.clone());
        }
        envelope["timestamp"] = now.into();
        updates.push((wire, encode(&envelope.to_string())));
    }
    updates.push((STORE.to_owned(), encode(&store.to_string())));
    updates.push((PENDING.to_owned(), encode(&dest)));

    let backup = paths.backups().join(format!("sections-{}", stamp()));
    copy_tree(&leveldb_dir(paths), &backup.join("leveldb"))?;
    fs::write(backup.join("claude_desktop_config.json"), &raw_config)?;

    let originals: Vec<_> = updates
        .iter()
        .map(|(key, _)| (key.clone(), db.get(&storage_key(key))))
        .collect();
    let new_config = prepare_atomic(
        &config_path,
        serde_json::to_string_pretty(&config)?.as_bytes(),
    )?;
    let mut batch = WriteBatch::default();
    for (key, value) in &updates {
        batch.put(&storage_key(key), value);
    }
    db.write(batch, true)?;
    if let Err(e) = fs::rename(&new_config, &config_path) {
        let _ = fs::remove_file(&new_config);
        let mut undo = WriteBatch::default();
        for (key, old) in &originals {
            match old {
                Some(old) => undo.put(&storage_key(key), old),
                None => undo.delete(&storage_key(key)),
            }
        }
        let rolled_back = db.write(undo, true);
        return Err(format!(
            "preferences write failed: {e}; storage rollback: {rolled_back:?}; backup: {}",
            paths.brief(&backup)
        )
        .into());
    }
    db.close()?;
    Ok(format!(
        "copied sections {summary}\nreopen Claude to upload them; backup: {}",
        paths.brief(&backup)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chats::tests::{A, B, ORG, config, sandbox};

    const CHAT: &str = "dddddddd-0000-4000-8000-000000000004";
    const GONE: &str = "eeeeeeee-0000-4000-8000-000000000005";

    fn source() -> (Value, Value) {
        let chat = format!("code:local_{CHAT}");
        let gone = format!("code:local_{GONE}");
        let groups = json!({
            "groups": [{"id": "s-bang", "name": "!"}, {"id": "s-me", "name": "me"}],
            "assignments": {chat.clone(): "s-bang", gone.clone(): "s-me", "code:cloud_x": "s-me"},
            "order": {"s-bang": [chat.clone()], "s-me": [gone, "code:cloud_x"]},
        });
        let layout = json!({"sections": [
            {"id": "pinned", "kind": "pinned"},
            {"id": "s-bang", "kind": "manual", "name": "!", "members": [chat]},
            {"id": "s-me", "kind": "manual", "name": "me"},
        ], "prefsSeeded": true});
        (groups, layout)
    }

    fn write_fixture(paths: &Paths, pairs: &[(&str, &str)]) {
        fs::create_dir_all(leveldb_dir(paths)).unwrap();
        let mut db = DB::open(leveldb_dir(paths), Options::default()).unwrap();
        for (key, value) in pairs {
            db.put(&storage_key(key), &encode(value)).unwrap();
        }
        db.flush().unwrap();
        db.close().unwrap();
    }

    fn only_chat(id: &str) -> bool {
        id == format!("code:local_{CHAT}")
    }

    #[test]
    fn utf16_and_latin1_round_trip() {
        assert_eq!(decode(&encode("héllo ✓ 😀")).unwrap(), "héllo ✓ 😀");
        assert_eq!(decode(&[1, b'h', 0xe9]).unwrap(), "hé");
        assert!(decode(&[2]).is_err());
        assert!(decode(&[0, 1]).is_err());
    }

    #[test]
    fn source_wins_and_keeps_matching_ids() {
        let (groups, layout) = source();
        let target =
            json!({"groups": [{"id": "t-bang", "name": "!"}, {"id": "t-old", "name": "old"}]});
        let out = remap(&groups, &target, &layout, &only_chat).unwrap();

        let chat = format!("code:local_{CHAT}");
        assert_eq!(
            out.groups["groups"],
            json!([{"id": "t-bang", "name": "!"}, {"id": "s-me", "name": "me"}])
        );
        assert_eq!(out.groups["assignments"], json!({chat.clone(): "t-bang"}));
        assert_eq!(out.groups["order"], json!({"t-bang": [chat], "s-me": []}));
        assert_eq!(out.layout["sections"][1]["id"], "t-bang");
        assert_eq!(out.layout["sections"][2]["id"], "s-me");
        assert_eq!(out.layout["sections"][0]["id"], "pinned");
        assert_eq!(out.removed, ["old"]);
    }

    #[test]
    fn refuses_unknown_references_and_duplicates() {
        let (mut groups, layout) = source();
        groups["assignments"][format!("code:local_{CHAT}")] = json!("nope");
        assert!(remap(&groups, &json!({"groups": []}), &layout, &only_chat).is_err());
        let dup = json!({"groups": [{"id": "a", "name": "x"}, {"id": "b", "name": "x"}]});
        assert!(ids_by_name(&dup).is_err());
    }

    /// Builds Claude's data for A (source, 1 org) and B (signed in), both holding `CHAT`.
    fn fixture(name: &str, owner: &str) -> Paths {
        let paths = sandbox(name);
        let (groups, layout) = source();
        for account in [A, B] {
            let dir = paths.code_root().join(account).join(ORG);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("local_{CHAT}.json")), "{}").unwrap();
        }
        let a = format!("{A}/{ORG}");
        let b = format!("{B}/{ORG}");
        let target =
            json!({"groups": [{"id": "t-bang", "name": "!"}], "assignments": {}, "order": {}});
        let b_layout = json!({"sections": []});
        let scopes = json!({a.clone(): groups, b.clone(): target});
        let layouts = json!({a: layout, b: b_layout});
        let store = json!({"version": 1, "state": {"customGroupsByScope": scopes, "codeSidebarByScope": layouts}});
        let envelope = |v: &Value| json!({"value": v, "timestamp": 1}).to_string();
        write_fixture(
            &paths,
            &[
                ("ccd-sync-owner", owner),
                ("ccd-sync-org", ORG),
                (STORE, &store.to_string()),
                ("LSS-persisted.dframe-group-scopes", &envelope(&scopes)),
                ("LSS-persisted.dframe-code-sections", &envelope(&layouts)),
            ],
        );
        let prefs = json!({"keep": true, "preferences": {"epitaxyPrefs": {"dframe-group-scopes": scopes, "dframe-code-sections": layouts}}});
        fs::write(
            paths.app().join("claude_desktop_config.json"),
            prefs.to_string(),
        )
        .unwrap();
        paths
    }

    fn account(cfg: &Config, id: &str) -> Account {
        cfg.accounts.iter().find(|a| a.id == id).unwrap().clone()
    }

    fn reopen(paths: &Paths) -> DB {
        DB::open(
            leveldb_dir(paths),
            Options {
                create_if_missing: false,
                ..Options::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn signed_in_account_is_marked_for_upload() {
        let paths = fixture("marked", B);
        let cfg = config();

        let out = sync(&paths, &cfg, &account(&cfg, A), false).unwrap();

        assert!(out.contains("reopen Claude"), "{out}");
        let mut db = reopen(&paths);
        let scope = format!("{B}/{ORG}");
        assert_eq!(read(&mut db, PENDING).unwrap().unwrap(), scope);
        let store = read_json(&mut db, STORE).unwrap();
        let groups = &store["state"]["customGroupsByScope"][&scope];
        assert_eq!(groups["groups"][0]["id"], "t-bang");
        assert_eq!(groups["assignments"].as_object().unwrap().len(), 1);
        let mirror = read_json(&mut db, "LSS-persisted.dframe-group-scopes").unwrap();
        assert_eq!(&mirror["value"][&scope], groups);
        let config: Value = serde_json::from_slice(
            &fs::read(paths.app().join("claude_desktop_config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            &config["preferences"]["epitaxyPrefs"]["dframe-group-scopes"][&scope],
            groups
        );
        assert_eq!(config["keep"], true);
        let backups: Vec<_> = fs::read_dir(paths.backups()).unwrap().collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn naming_the_signed_in_account_changes_nothing() {
        let paths = fixture("source-signed-in", A);
        let cfg = config();
        let before = fs::read(paths.app().join("claude_desktop_config.json")).unwrap();

        let out = sync(&paths, &cfg, &account(&cfg, A), false).unwrap();

        assert!(out.contains("nothing to copy"), "{out}");
        assert_eq!(
            fs::read(paths.app().join("claude_desktop_config.json")).unwrap(),
            before
        );
        assert!(reopen(&paths).get(&storage_key(PENDING)).is_none());
    }

    #[test]
    fn other_accounts_are_left_alone() {
        let paths = fixture("others", B);
        let cfg = config();

        sync(&paths, &cfg, &account(&cfg, A), false).unwrap();

        let store = read_json(&mut reopen(&paths), STORE).unwrap();
        let (groups, _) = source();
        assert_eq!(
            store["state"]["customGroupsByScope"][format!("{A}/{ORG}")],
            groups
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let paths = fixture("dry", B);
        let cfg = config();
        let before = fs::read(paths.app().join("claude_desktop_config.json")).unwrap();

        let out = sync(&paths, &cfg, &account(&cfg, A), true).unwrap();

        assert!(out.contains("would copy"), "{out}");
        assert_eq!(
            fs::read(paths.app().join("claude_desktop_config.json")).unwrap(),
            before
        );
        assert!(reopen(&paths).get(&storage_key(PENDING)).is_none());
    }

    #[test]
    fn refuses_to_overwrite_unsent_edits() {
        let paths = fixture("unsent", B);
        {
            let mut db = reopen(&paths);
            db.put(&storage_key(PENDING), &encode("x")).unwrap();
            db.flush().unwrap();
            db.close().unwrap();
        }
        let cfg = config();
        assert!(sync(&paths, &cfg, &account(&cfg, A), false).is_err());
    }
}
