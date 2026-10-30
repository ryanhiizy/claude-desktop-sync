use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::Res;

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

pub fn temp_sibling(path: &Path) -> PathBuf {
    let id = TEMP_ID.fetch_add(1, Relaxed);
    path.with_file_name(format!(".ccsync-{}-{id}.tmp", process::id()))
}

pub fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Runs `f` over `items` on every core, stopping at the first error.
pub fn par<T: Sync>(items: &[T], f: impl Fn(&T) -> Res + Sync) -> Res {
    let next = AtomicUsize::new(0);
    let failure = Mutex::new(None);
    let cores = thread::available_parallelism().map_or(1, |n| n.get());
    thread::scope(|s| {
        for _ in 0..cores.min(items.len()) {
            s.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, Relaxed)) {
                    if let Err(e) = f(item) {
                        failure.lock().unwrap().get_or_insert(e);
                        next.store(items.len(), Relaxed);
                    }
                }
            });
        }
    });
    failure.into_inner().unwrap().map_or(Ok(()), Err)
}

/// Replaces `dst` through a rename so the app never reads half a chat. `dst` takes `src`'s mtime,
/// which stops a file bouncing between accounts. A write the app made since planning wins.
pub fn copy_chat_file(src: &Path, dst: &Path) -> Res {
    let meta = fs::metadata(src)?;
    let mtime = meta.modified()?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(dst);
    let result = (|| -> Res {
        fs::copy(src, &tmp)?;
        let mode = meta.permissions().mode();
        if mode & 0o200 == 0 {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode | 0o200))?;
        }
        let file = fs::File::options().write(true).open(&tmp)?;
        file.set_modified(mtime)?;
        file.sync_all()?;
        if mode & 0o200 == 0 {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        }
        match fs::symlink_metadata(dst).and_then(|m| m.modified()) {
            Ok(current) if current > mtime => Ok(()),
            _ => Ok(fs::rename(&tmp, dst)?),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Copies a tree for backup, cloning on APFS. Files the running app removes mid-walk are skipped.
pub fn copy_tree(src: &Path, dst: &Path) -> Res {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        fs::create_dir_all(dst.join(&rel))?;
        let entries = match fs::read_dir(src.join(&rel)) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            other => other?,
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            let child = rel.join(entry.file_name());
            if kind.is_dir() {
                pending.push(child);
            } else if kind.is_symlink() {
                symlink(fs::read_link(entry.path())?, dst.join(&child))?;
            } else if kind.is_file() {
                files.push(child);
            }
        }
    }
    par(&files, |rel| match fs::copy(src.join(rel), dst.join(rel)) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    })
}

/// Durably replaces `path` with `body`.
pub fn write_atomic(path: &Path, body: &[u8]) -> Res {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = prepare_atomic(path, body)?;
    let renamed = fs::rename(&tmp, path);
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(renamed?)
}

/// Writes `body` beside `path`, ready to be renamed over it.
pub fn prepare_atomic(path: &Path, body: &[u8]) -> Res<PathBuf> {
    let tmp = temp_sibling(path);
    let written = fs::write(&tmp, body).and_then(|()| fs::File::open(&tmp)?.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(tmp)
}
