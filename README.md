# claude-desktop-sync

`ccsync` keeps Claude desktop chats and sidebar sections matched across accounts on a Mac. It only runs when you run it. A slim Rust rework of [ccdejavu](https://github.com/wjames111/ccdejavu), which has a background watcher this doesn't.

```sh
cargo install --path .
```

## Use

```sh
ccsync list                  # accounts on this Mac
ccsync add you@work.com      # or: ccsync add you@work.com 3f1a9c2e
ccsync remove you@work.com

ccsync                       # sync chats
ccsync you@work.com          # sync chats, then copy that account's sections to the signed-in account
ccsync -n work               # dry run; any unique part of an email works
```

Quit Claude first; syncing refuses to run beside it. Sync before you switch accounts, because Claude loads the new account's copies as soon as you switch and saves them back over anything synced after that.

```text
quit Claude → ccsync → reopen Claude → switch accounts
```

## Chats

Missing and older chats are copied to every org folder of every added account; the newest copy wins. A delete carries over, moving the chat to `~/.ccsync/trash/`. Only `local_<id>.json`, Cowork `local_<id>/` folders and `deleted_<id>` markers are touched.

Copies that open different transcripts (`cliSessionId`) are skipped and reported, because the newest one may be a stale copy Claude has just saved. Delete the copies of the wrong transcript and sync again.

An account's first sync backs up both chat folders to `~/.ccsync/backups/`. If any chat file looks unfamiliar, that folder is skipped rather than guessed at. Files are cloned on APFS, so backups are nearly free.

## Sections

`ccsync <account>` copies that account's sections to whichever account Claude is signed in to. You only name the source. Same-named sections keep the signed-in account's section ID, missing ones are created, and extra ones are replaced. Chats the account doesn't have are left out.

Reopen Claude after running it, and the signed-in account uploads the result. The storage it changes is backed up first.

Claude stores each account's sections on its server, and only uploads for the account it starts signed in to. To update another account, switch to it in Claude and repeat.

This relies on undocumented Claude internals, and an app update could break it. To restore, quit Claude and copy the files from `~/.ccsync/backups/sections-*` back.

## Develop

```sh
cargo test
cargo clippy --all-targets
```

`CCSYNC_HOME=/path/to/copy` points everything at a copy of the data.

## License

MIT. Derived from ccdejavu by Will James; see [LICENSE](LICENSE).
