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

Needs cargo ([rustup.rs](https://rustup.rs)), a C compiler for the bundled SQLite, and `flock`. The launcher rebuilds when the source checksum changes and builds into the cache, never beside the source.

An archive is required. The root is `--root`, else `$SLACKDUMPS`, else `/srv/slackdumps`. Under it, one directory per conversation set, each holding what `slackdump archive` writes:

```
<root>/
  full/<channel>_<yyyymmdd>/slackdump.sqlite    # channels
  dms/<name>_<yyyymmdd>/slackdump.sqlite        # direct messages
  threads/my-threads_<yyyymm>/slackdump.sqlite  # the owner's threads
```

Sign-in is optional and Linux-only: the client imports the session of a Slack desktop app already signed in on the same machine, from `~/snap/slack/current/.config/Slack` or `~/.config/Slack` (`SLACK_APP_DIR` overrides). `SLACK_TOKEN` (an `xoxc-` token) and `SLACK_COOKIE` (the `d` cookie, `xoxd-`) bypass the import. Credentials go only to slack.com hosts.

## Develop

```sh
cargo test --locked
tests/slack-tui-launcher.sh
bin/slack-tui-capture --binary bin/slack-tui --keys "g j Enter sleep:5"   # text, ANSI, HTML and PNG of the screen; needs tmux and Chrome
```

See the [development instructions](.claude/agents/slack-tui-developer.md) for the crate layout, the auth model, the Web API writes the tool makes, and the live-write test rules.

## Provenance

slack-tui was imported from a private repository with its commit history on 2026-09-15. This repository is a GitHub fork of [MasonLiebe/slackatui](https://github.com/MasonLiebe/slackatui); the import merge replaced the fork's contents with slack-tui, and the previous contents remain in the earlier commits.
