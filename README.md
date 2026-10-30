# claude-sync

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
ccsync you@work.com          # sync chats, then copy that account's sections to every added account
ccsync -n work               # dry run; any unique part of an email works
```

Restart Claude afterwards so it shows the changes.

## Chats

Missing and older chats are copied to every org folder of every added account; the newest copy wins. A delete carries over, moving the chat to `~/.ccsync/trash/`. Only `local_<id>.json`, Cowork `local_<id>/` folders and `deleted_<id>` markers are touched.

An account's first sync backs up both chat folders to `~/.ccsync/backups/`. If any chat file looks unfamiliar, that folder is skipped rather than guessed at. Files are cloned on APFS, so backups are nearly free.

## Sections

The account you name wins: same-named sections keep each account's section ID, missing ones are created, and extra ones are replaced. Chats an account doesn't have are left out. Section sync needs Claude fully quit, and backs up the storage it changes first.

Claude stores each account's section definitions on its server, and only uploads for the account signed in when the app starts. Switching accounts replaces the local copy with that account's server copy. So one command can't finish every account at once:

1. Quit Claude and run `ccsync you@work.com`. Every account is written locally, and the signed-in one uploads when you reopen Claude.
2. For each other account the first time you use it: switch to it, quit Claude, run `ccsync`. Pending accounts are remembered, and each is finished once, so later edits there survive.

This relies on undocumented Claude internals, and an app update could break it. To restore, quit Claude and copy the files from `~/.ccsync/backups/sections-*` back.

## Develop

```sh
cargo test
cargo clippy --all-targets
```

`CCSYNC_HOME=/path/to/copy` points everything at a copy of the data.

## License

MIT. Derived from ccdejavu by Will James; see [LICENSE](LICENSE).
