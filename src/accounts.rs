//! Which accounts ccsync keeps matched, stored in `~/.ccsync/accounts.json`.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::{Value, json};

use crate::{
    Paths, Res,
    chats::{Kind, classify, is_uuid, uuid_dirs},
    fsutil::write_atomic,
};

#[derive(Clone, Debug)]
pub struct Account {
    pub email: String,
    pub id: String,
}

#[derive(Default, Debug)]
pub struct Config {
    pub accounts: Vec<Account>,
    /// Accounts covered by a backup of the chat folders.
    pub backed_up: Vec<String>,
    /// The account whose sections were last chosen, and who still hasn't received them.
    pub sections_from: Option<String>,
    pub sections_pending: Vec<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

impl Config {
    fn path(paths: &Paths) -> PathBuf {
        paths.data().join("accounts.json")
    }

    pub fn load(paths: &Paths) -> Res<Self> {
        let path = Self::path(paths);
        let raw = match fs::read(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            other => other?,
        };
        let v: Value =
            serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut accounts = Vec::new();
        for a in v["accounts"].as_array().into_iter().flatten() {
            let (Some(email), Some(id)) = (
                a["email"].as_str(),
                a["id"].as_str().filter(|id| is_uuid(id)),
            ) else {
                return Err(format!("{}: bad account entry {a}", path.display()).into());
            };
            accounts.push(Account {
                email: email.to_owned(),
                id: id.to_owned(),
            });
        }
        Ok(Self {
            accounts,
            backed_up: strings(&v["backedUp"]),
            sections_from: v["sectionsFrom"].as_str().map(str::to_owned),
            sections_pending: strings(&v["sectionsPending"]),
        })
    }

    pub fn save(&self, paths: &Paths) -> Res {
        let accounts: Vec<Value> = self
            .accounts
            .iter()
            .map(|a| json!({"email": a.email, "id": a.id}))
            .collect();
        let body = json!({
            "accounts": accounts,
            "backedUp": self.backed_up,
            "sectionsFrom": self.sections_from,
            "sectionsPending": self.sections_pending,
        });
        write_atomic(
            &Self::path(paths),
            serde_json::to_string_pretty(&body)?.as_bytes(),
        )
    }

    /// Finds an account by exact email, or by any part of an email that fits only one.
    pub fn resolve(&self, query: &str) -> Res<&Account> {
        let query = query.trim().to_lowercase();
        if let Some(exact) = self.accounts.iter().find(|a| a.email == query) {
            return Ok(exact);
        }
        let hits: Vec<_> = self
            .accounts
            .iter()
            .filter(|a| a.email.contains(&query))
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("no added account matches {query:?}; see `ccsync list`").into()),
            many => {
                let emails: Vec<_> = many.iter().map(|a| a.email.as_str()).collect();
                Err(format!(
                    "{query:?} fits more than one account: {}",
                    emails.join(", ")
                )
                .into())
            }
        }
    }

    pub fn email_of<'a>(&'a self, id: &'a str) -> &'a str {
        self.accounts
            .iter()
            .find(|a| a.id == id)
            .map_or(id, |a| &a.email)
    }
}

/// Every account with a folder on this Mac.
fn on_this_mac(paths: &Paths) -> Res<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for root in [paths.code_root(), paths.cowork_root()] {
        ids.extend(uuid_dirs(&root)?);
    }
    Ok(ids)
}

fn distinct_chats(paths: &Paths, id: &str) -> Res<usize> {
    let mut chats = HashSet::new();
    let dir = paths.code_root().join(id);
    for org in uuid_dirs(&dir)? {
        for entry in fs::read_dir(dir.join(org))? {
            let name = entry?.file_name();
            if let Some(Kind::Chat(chat)) = name.to_str().map(|n| classify(n, false, false)) {
                chats.insert(chat.to_owned());
            }
        }
    }
    Ok(chats.len())
}

fn read_identity(path: &Path) -> Option<(String, String, SystemTime)> {
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let v: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let account = &v["oauthAccount"];
    let id = account["accountUuid"].as_str().filter(|id| is_uuid(id))?;
    let email = account["emailAddress"].as_str()?.trim().to_lowercase();
    (!email.is_empty()).then(|| (id.to_owned(), email, mtime))
}

/// Maps account IDs to emails, from files that record both. The app keeps no direct list.
fn known_emails(paths: &Paths) -> Res<BTreeMap<String, String>> {
    let mut files = vec![paths.home.join(".claude.json")];
    let root = paths.cowork_root();
    for account in uuid_dirs(&root)? {
        for org in uuid_dirs(&root.join(&account))? {
            let Ok(chats) = fs::read_dir(root.join(&account).join(org)) else {
                continue;
            };
            files.extend(
                chats
                    .flatten()
                    .map(|chat| chat.path().join(".claude/.claude.json")),
            );
        }
    }
    let mut newest: BTreeMap<String, (String, SystemTime)> = BTreeMap::new();
    for (id, email, mtime) in files.iter().filter_map(|f| read_identity(f)) {
        if newest.get(&id).is_none_or(|(_, seen)| mtime > *seen) {
            newest.insert(id, (email, mtime));
        }
    }
    Ok(newest
        .into_iter()
        .map(|(id, (email, _))| (id, email))
        .collect())
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn list(paths: &Paths) -> Res {
    let cfg = Config::load(paths)?;
    let known = known_emails(paths)?;
    let mut ids = on_this_mac(paths)?;
    ids.extend(cfg.accounts.iter().map(|a| a.id.clone()));
    let mut rows = Vec::new();
    for id in &ids {
        let added = cfg.accounts.iter().find(|a| &a.id == id);
        let email = added
            .map(|a| a.email.as_str())
            .or(known.get(id).map(String::as_str))
            .unwrap_or("(unknown email)");
        let state = if added.is_some() {
            "added"
        } else {
            "not added"
        };
        rows.push((email, short(id), distinct_chats(paths, id)?, state));
    }
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    for (email, id, chats, state) in rows {
        println!("{email:width$}  {id}  {chats:>5} chats  {state}");
    }
    Ok(())
}

pub fn add(paths: &Paths, email: &str, id: Option<&str>) -> Res {
    let email = email.trim().to_lowercase();
    if !email.contains('@') {
        return Err(format!("{email:?} doesn't look like an email").into());
    }
    let ids = on_this_mac(paths)?;
    let id = match id {
        Some(prefix) => {
            let hits: Vec<_> = ids.iter().filter(|id| id.starts_with(prefix)).collect();
            match hits.as_slice() {
                [one] => (*one).clone(),
                [] => {
                    return Err(format!(
                        "no account on this Mac starts with {prefix:?}; see `ccsync list`"
                    )
                    .into());
                }
                _ => return Err(format!("{prefix:?} fits more than one account").into()),
            }
        }
        None => known_emails(paths)?
            .into_iter()
            .find(|(id, known)| *known == email && ids.contains(id))
            .map(|(id, _)| id)
            .ok_or_else(|| {
                format!("can't tell which account is {email}; pass its id from `ccsync list`")
            })?,
    };
    let mut cfg = Config::load(paths)?;
    if let Some(taken) = cfg.accounts.iter().find(|a| a.email == email || a.id == id) {
        return Err(format!("already added: {} ({})", taken.email, short(&taken.id)).into());
    }
    println!("added {email} ({})", short(&id));
    cfg.accounts.push(Account { email, id });
    cfg.save(paths)
}

pub fn remove(paths: &Paths, query: &str) -> Res {
    let mut cfg = Config::load(paths)?;
    let account = cfg.resolve(query)?.clone();
    cfg.accounts.retain(|a| a.id != account.id);
    cfg.backed_up.retain(|id| *id != account.id);
    cfg.sections_pending.retain(|id| *id != account.id);
    if cfg.sections_from.as_deref() == Some(&account.id) {
        cfg.sections_from = None;
        cfg.sections_pending.clear();
    }
    cfg.save(paths)?;
    println!("removed {}; chats already copied stay put", account.email);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chats::tests::{A, B, ORG, config, sandbox};

    #[test]
    fn resolves_by_any_unique_part() {
        let cfg = config();
        assert_eq!(cfg.resolve("a@x.co").unwrap().id, A);
        assert_eq!(cfg.resolve("B@X").unwrap().id, B);
        assert!(cfg.resolve("x.co").is_err());
        assert!(cfg.resolve("nobody").is_err());
    }

    #[test]
    fn add_list_remove_round_trip() {
        let paths = sandbox("accounts");
        fs::create_dir_all(paths.code_root().join(A).join(ORG)).unwrap();
        fs::create_dir_all(paths.code_root().join(B).join(ORG)).unwrap();

        assert!(add(&paths, "a@x.co", None).is_err());
        add(&paths, "A@x.co", Some("aaaa")).unwrap();
        add(&paths, "b@x.co", Some(B)).unwrap();
        assert!(add(&paths, "a@x.co", Some("bbbb")).is_err());
        list(&paths).unwrap();

        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.accounts[0].email, "a@x.co");
        assert_eq!(cfg.accounts[1].id, B);

        remove(&paths, "a@x").unwrap();
        assert_eq!(Config::load(&paths).unwrap().accounts.len(), 1);
    }

    #[test]
    fn add_finds_a_known_email() {
        let paths = sandbox("identity");
        fs::create_dir_all(paths.code_root().join(A).join(ORG)).unwrap();
        let identity = json!({"oauthAccount": {"accountUuid": A, "emailAddress": "A@x.co"}});
        fs::write(paths.home.join(".claude.json"), identity.to_string()).unwrap();

        add(&paths, "a@x.co", None).unwrap();
        assert_eq!(Config::load(&paths).unwrap().accounts[0].id, A);
    }
}
