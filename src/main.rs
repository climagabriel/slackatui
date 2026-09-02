//! slack-tui: read the local slackdump archives in the terminal.

mod app;
mod archive;
mod render;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::{App, Focus};
use crate::archive::Corpus;
use crate::render::Tz;

const HELP: &str = "\
slack-tui - read the local slackdump archives in the terminal

usage: slack-tui [--root DIR] [--channel NAME] [--local]
       slack-tui --list [--root DIR]
       slack-tui --dump NAME [--limit N] [--width W] [--root DIR] [--local]
       slack-tui --help

Opens every slackdump.sqlite under <root>/full/ and <root>/dms/ read-only
(a resume writing the same database at the same time is fine) and shows
the conversations on the left, the messages of the selected one on the
right. Enter on a message opens its thread; Esc goes back.

flags
  --root DIR      archive root (default $SLACKDUMPS, then /srv/slackdumps)
  --channel NAME  open this conversation at once (#team-alpha, @someone, or the id)
  --local         show times in local time instead of UTC
  --list          print the conversations and exit, no terminal UI
  --dump NAME     print the newest messages of one conversation as text and exit
  --limit N       with --dump: how many top-level messages (default 100)
  --width W       with --dump: wrap width (default 100)
  --help          this text

environment
  SLACKDUMPS           archive root when --root is not given
  SLACK_SELF_USER_ID   your own user id, to name direct messages by the
                       other party (derived from the DM archive otherwise)

keys (also ? inside)
  j/k move, Ctrl-d/Ctrl-u half page, g/G oldest/newest, h/l or Tab panes,
  Enter thread, Esc back, / filter or search, d go to date, v raw JSON,
  o show a hit or a thread root in the channel, r reload, s sort, q quit

exit codes
  0  ok        1  no archive, or the conversation was not found
  2  bad usage

examples
  slack-tui
  slack-tui --channel '#team-alpha'
  slack-tui --dump alerts-alpha --limit 20
  SLACKDUMPS=/data/slackdumps slack-tui --list
";

struct Opts {
    root: PathBuf,
    channel: Option<String>,
    tz: Tz,
    list: bool,
    dump: Option<String>,
    limit: usize,
    width: usize,
}

fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts {
        root: std::env::var_os("SLACKDUMPS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/srv/slackdumps")),
        channel: None,
        tz: Tz::Utc,
        list: false,
        dump: None,
        limit: 100,
        width: 100,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--root" => opts.root = PathBuf::from(value("--root")?),
            "--channel" => opts.channel = Some(value("--channel")?),
            "--local" => opts.tz = Tz::Local,
            "--list" => opts.list = true,
            "--dump" => opts.dump = Some(value("--dump")?),
            "--limit" => {
                opts.limit = value("--limit")?
                    .parse()
                    .map_err(|_| "--limit wants a number".to_string())?
            }
            "--width" => {
                opts.width = value("--width")?
                    .parse()
                    .map_err(|_| "--width wants a number".to_string())?
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(opts)
}

fn main() {
    std::process::exit(run());
}

/// Print, and treat a closed pipe (`| head`) as a normal end.
fn emit(text: &str) -> i32 {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|_| out.flush()) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            1
        }
    }
}

fn run() -> i32 {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("slack-tui: {e}\n\n{HELP}");
            return 2;
        }
    };
    let corpus = match Corpus::open(&opts.root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            return 1;
        }
    };
    let mut app = App::new(corpus, opts.tz);
    if opts.list {
        let mut text = String::new();
        for &i in &app.filtered {
            let c = app.conv(i);
            let a = &app.corpus.archives[c.archive];
            text.push_str(&format!(
                "{:<48} {:>8}  {} → {}  {}\n",
                c.name,
                c.msgs,
                app.tz.fmt(c.first_id / 1_000_000, "%Y-%m-%d"),
                app.tz.fmt(c.last_id / 1_000_000, "%Y-%m-%d"),
                a.rel
            ));
        }
        return emit(&text);
    }
    if let Some(name) = opts.dump.as_deref().or(opts.channel.as_deref()) {
        let Some(idx) = app.corpus.find_conv(name) else {
            eprintln!("slack-tui: no conversation named '{name}'; --list shows them");
            return 1;
        };
        if !app.open_conv(idx) {
            eprintln!("slack-tui: {}", app.status);
            return 1;
        }
        if opts.dump.is_some() {
            if let Some(o) = app.open.as_mut() {
                let keep = opts.limit.min(o.list.len());
                let drop = o.list.len() - keep;
                o.list.msgs.drain(0..drop);
                o.list.cursor = keep.saturating_sub(1);
                o.list.top_note = None;
                o.list.bottom_note = None;
                o.list.mark_dirty();
            }
            let text = ui::dump(&mut app, opts.width) + "\n";
            return emit(&text);
        }
    }
    match tui(&mut app) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            1
        }
    }
}

fn tui(app: &mut App) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    if app.open.is_none() {
        app.focus = Focus::Convs;
    }
    let result = loop {
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, app)) {
            break Err(e);
        }
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => app.on_key(k),
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        if app.quit {
            break Ok(());
        }
    };
    ratatui::restore();
    result
}
