//! Evidence for resolving Claude Desktop transcript ID changes.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::chats::is_uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    uuid: Option<String>,
    parent_uuid: Option<String>,
    logical_parent_uuid: Option<String>,
    session_id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
}

fn project_dirs(home: &Path) -> Option<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(home.join(".claude/projects")).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Some(dirs)
}

fn transcript_path(dirs: &[PathBuf], id: &str) -> Option<PathBuf> {
    let name = format!("{id}.jsonl");
    let mut found = None;
    for path in dirs.iter().map(|dir| dir.join(&name)) {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && found.is_none() => found = Some(path),
            Ok(_) => return None,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    found
}

type Parents = HashMap<String, Option<String>>;

fn read_head(path: &Path, id: &str, parents: &mut Parents) -> Option<String> {
    let mut head = None;
    for line in BufReader::new(File::open(path).ok()?).lines() {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line).ok()?;
        if record.is_sidechain {
            continue;
        }
        let Some(uuid) = record.uuid else { continue };
        let parent = if record.subtype.as_deref() == Some("compact_boundary") {
            record.logical_parent_uuid
        } else {
            record.parent_uuid
        };
        // A copied message can have a different parent after compaction. Don't infer ancestry
        // through that edge when the files disagree.
        parents
            .entry(uuid.clone())
            .and_modify(|known| {
                if *known != parent {
                    *known = None;
                }
            })
            .or_insert(parent);
        if matches!(record.kind.as_str(), "user" | "assistant") {
            head = (record.session_id.as_deref() == Some(id)).then_some(uuid);
        }
    }
    head
}

/// Only choose a strict descendant of every other transcript's last main-thread message.
/// Also require the saved chat tips so an incomplete local transcript cannot hide later work.
pub fn continuation<'a>(
    home: &Path,
    ids: impl Iterator<Item = &'a str>,
    saved_tips: &[String],
) -> Option<&'a str> {
    let dirs = project_dirs(home)?;
    let mut parents = Parents::new();
    let mut heads = Vec::new();
    for id in ids {
        if !is_uuid(id) {
            return None;
        }
        let path = transcript_path(&dirs, id)?;
        heads.push((id, read_head(&path, id, &mut parents)?));
    }
    for (id, head) in &heads {
        let mut ancestors = HashSet::new();
        let mut current = head.as_str();
        while let Some(parent) = parents.get(current) {
            if !ancestors.insert(current) {
                return None;
            }
            match parent.as_deref() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        if heads
            .iter()
            .all(|(other, tip)| other == id || (tip != head && ancestors.contains(tip.as_str())))
            && saved_tips
                .iter()
                .all(|tip| ancestors.contains(tip.as_str()))
        {
            return Some(id);
        }
    }
    None
}
