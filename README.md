# slack-tui

A Slack client for the terminal, in Rust on [ratatui](https://ratatui.rs). It reads the SQLite archives that [slackdump](https://github.com/rusq/slackdump) writes and, signed in, uses the Slack Web API for live reads and actions: threads, unreads, search, posting, reactions, uploads, canvases.

## Run

```sh
git clone https://github.com/climagabriel/slackatui
cd slackatui
bin/slack-tui --root /path/to/slackdumps   # builds into ~/.cache/slack-tui on first run, then launches
bin/slack-tui --help                        # launcher flags, then the application's own flags
bin/slack-tui --rebuild                     # rebuild unconditionally
```

Needs cargo ([rustup.rs](https://rustup.rs)) and a C compiler for the bundled SQLite to build, and `flock`. The launcher rebuilds when the source checksum changes and builds into the cache, never beside the source; a current cached binary is run directly, so a headless caller (a service) needs no toolchain.

An archive is required. The root is `--root`, else `$SLACKDUMPS`, else `/srv/slackdumps`. Under it, one directory per conversation set, each holding what `slackdump archive` writes:

```
<root>/
  full/<channel>_<yyyymmdd>/slackdump.sqlite    # channels
  dms/<name>_<yyyymmdd>/slackdump.sqlite        # direct messages
  threads/my-threads_<yyyymm>/slackdump.sqlite  # the owner's threads
```

Sign-in is optional and Linux-only: the client imports the session of a Slack desktop app already signed in on the same machine, from `~/snap/slack/current/.config/Slack` or `~/.config/Slack` (`SLACK_APP_DIR` overrides). `SLACK_TOKEN` (an `xoxc-` token) and `SLACK_COOKIE` (the `d` cookie, `xoxd-`) bypass the import. The imported pair is cached owner-only in `~/.config/slack-tui/auth.json` and reused by later runs, including headless `--call`. Credentials go only to slack.com hosts.

## Develop

```sh
cargo test --locked
tests/slack-tui-launcher.sh
bin/slack-tui-capture --binary bin/slack-tui --keys "g j Enter sleep:5"   # text, ANSI, HTML and PNG of the screen; needs tmux and Chrome
```

See the [development instructions](.claude/agents/slack-tui-developer.md) for the crate layout, the auth model, the Web API writes the tool makes, and the live-write test rules.

## Canvases without the interface

A canvas can be read and written without opening the interface, so a script
can list its sections and replace one. The write flag depends on the token: an
app token with `canvases:write` writes through `canvases.edit` with
`--canvas-edit`, and the imported desktop session writes through the internal
endpoints in the next section.

```sh
bin/slack-tui --dump-canvas F0123ABCD                       # the whole canvas as Markdown
bin/slack-tui --canvas-sections F0123ABCD                   # one JSON line per section: id, editable, markdown
printf '%s\n' '- a new bullet' \
  | bin/slack-tui --canvas-edit F0123ABCD --op insert-at-end --markdown-file -
bin/slack-tui --canvas-edit F0123ABCD --op replace --section temp:C:ABC --markdown-file new.md
```

`F0123ABCD` is a canvas file id; `temp:C:ABC` is a section id from
`--canvas-sections`.

`--op` is one of `replace`, `insert-after`, `insert-before`, `insert-at-start`,
`insert-at-end`, `delete`. Every operation but `insert-at-start` and
`insert-at-end` needs `--section SECTION_ID`; every one but `delete` needs
`--markdown-file PATH` (or `-` for standard input). The write ends by reloading
the canvas and printing the affected section. Slack offers no atomic revision
check, so a `replace` or `delete` can overwrite an edit that landed since the
last read; reload with `--canvas-sections` immediately before the write.

Writing through `canvases.edit` needs a token Slack accepts for it. The
imported desktop session is an `xoxc-` token, and Slack answers
`not_allowed_token_type` for `canvases.edit` on it, whether the token is sent
as a bearer or in the form body. Reads work on the desktop session; writes
need an app token with the `canvases:write` scope (`xoxb-` or `xoxp-`) passed
through `SLACK_TOKEN`. The interface's `:w` canvas save is subject to the same
limit.

## The internal canvas endpoints

These write a canvas on the imported desktop session, with no app token. The
desktop client does not call `canvases.edit`; it POSTs a Protocol Buffers
(protobuf) change set to `<workspace>/canvas/-/edit-document`, with the `xoxc-`
token in the form body. Three flags use that protocol:

```sh
bin/slack-tui --canvas-internal-dump F0123ABCD                        # one JSON line per section
printf '%s\n' '- a new bullet' \
  | bin/slack-tui --canvas-internal-append F0123ABCD --markdown-file -
bin/slack-tui --canvas-internal-set F0123ABCD --section temp:C:ABC --markdown-file new.md
```

`--canvas-internal-dump` calls `files.info` for the canvas's `quip_thread_id`,
then fetches `canvas/-/load-data/editor/1` (the path the desktop client
sends), which returns the document as protobuf: every section with its id,
key, kind, edit token and text. The section ids are the ones
`--canvas-sections` prints.

`--canvas-internal-set` replaces one section's text and leaves its key and
kind as they were. `--canvas-internal-append` adds a section at the end; the
server assigns its kind and gives it a key that sorts after the last existing
section's. The text is markdown, nested two levels deep in protobuf field 12
of a one-section change set. Both were exercised against a live canvas.

`data_binary` is base64 of the change set, percent-encoded once by the form
encoder; pre-quoting it double-encodes it and Slack answers `400
invalid_arguments`. The browser's change sets also carry a rich-text state
(protobuf field 38), which plain-text appends and replacements do not need.

## Provenance

slack-tui was imported from a private repository with its commit history on 2026-09-15. This repository is a GitHub fork of [MasonLiebe/slackatui](https://github.com/MasonLiebe/slackatui); the import merge replaced the fork's contents with slack-tui, and the previous contents remain in the earlier commits.
