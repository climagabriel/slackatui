#!/usr/bin/env bash
# maturity: low; issues-found: 0
set -euo pipefail
unset XDG_CACHE_HOME SLACK_TUI_REBUILD SLACK_TUI_FAKE_BUILD_FAILURE

repo_root=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
launcher=$repo_root/bin/slack-tui
test_root=$(mktemp --directory /tmp/slack-tui-launcher.XXXXXX)
trap 'rm --recursive --force -- "$test_root"' EXIT

fake_tools=$test_root/fake-tools
fake_application=$test_root/fake-application
cargo_argument_log=$test_root/cargo-arguments
application_argument_log=$test_root/application-arguments
mkdir --parents "$fake_tools"

cat > "$fake_application" <<'FAKE_APPLICATION'
#!/usr/bin/env bash
set -euo pipefail
printf '[%s]\n' "$@" > "$SLACK_TUI_APPLICATION_ARGUMENT_LOG"
printf '%s\n' "${RUST_BACKTRACE:-unset}" > "$SLACK_TUI_BACKTRACE_LOG"
FAKE_APPLICATION
chmod +x "$fake_application"

cat > "$fake_tools/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf '[%s]\n' "$@" >> "$SLACK_TUI_CARGO_ARGUMENT_LOG"
command=$1
target_dir=
quiet=0
build_directory=debug
prune_all=1
while (($#)); do
    case "$1" in
        --target-dir)
            shift
            target_dir=$1
            ;;
        --quiet)
            quiet=1
            ;;
        --release)
            prune_all=0
            build_directory=release
            ;;
        --profile)
            prune_all=0
            shift
            [[ "$1" == dev ]]
            ;;
    esac
    shift
done
[[ -n "$target_dir" ]]
if [[ "$command" == clean && "$prune_all" == 1 ]]; then
    rm --recursive --force -- "$target_dir"
    exit 0
fi
if ((!quiet)); then
    echo "fake cargo: verbose $command output" >&2
fi
if [[ "$command" == build && "${SLACK_TUI_FAKE_BUILD_FAILURE:-0}" == 1 ]]; then
    echo 'fake cargo: build failed' >&2
    exit 42
fi
mkdir --parents "$target_dir/$build_directory"
if [[ "$command" == clean ]]; then
    rm --force "$target_dir/$build_directory/slack-tui"
else
    cp "$SLACK_TUI_FAKE_APPLICATION" "$target_dir/$build_directory/slack-tui"
fi
if [[ "$build_directory" == debug ]]; then
    [[ "$CARGO_PROFILE_DEV_DEBUG" == 2 ]]
    [[ "$CARGO_PROFILE_DEV_STRIP" == none ]]
    [[ "$CARGO_PROFILE_DEV_OPT_LEVEL" == 0 ]]
    [[ "$CARGO_PROFILE_DEV_PANIC" == unwind ]]
fi
FAKE_CARGO
chmod +x "$fake_tools/cargo"

export SLACK_TUI_CARGO_ARGUMENT_LOG=$cargo_argument_log
export SLACK_TUI_APPLICATION_ARGUMENT_LOG=$application_argument_log
export SLACK_TUI_FAKE_APPLICATION=$fake_application
export SLACK_TUI_BACKTRACE_LOG=$test_root/backtrace
test_path=$fake_tools:$PATH
test_home=$test_root/home
stdout=$test_root/stdout
stderr=$test_root/stderr

# An ordinary first run announces the build and shows Cargo progress and passes every application argument
# unchanged, including an argument containing whitespace.
HOME=$test_home PATH=$test_path "$launcher" \
    --channel '#zero-member-test' 'two words' > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
grep --fixed-strings --quiet 'slack-tui: building (' "$stderr"
grep --fixed-strings --quiet 'fake cargo: verbose build output' "$stderr"
if grep --fixed-strings --quiet '[--quiet]' "$cargo_argument_log"; then
    echo 'ordinary build unexpectedly made cargo quiet' >&2
    exit 1
fi
printf '%s\n' '[--channel]' '[#zero-member-test]' '[two words]' \
    > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"
[[ $(head -1 "$stderr") == 'slack-tui: building (first run)...' ]]
printf 'outdated\n' > "$test_home/.cache/slack-tui/source.sha256"
: > "$cargo_argument_log"
HOME=$test_home PATH=$test_path "$launcher" --list > "$stdout" 2> "$stderr"
[[ $(head -1 "$stderr") == 'slack-tui: building (source changed)...' ]]
grep --fixed-strings --quiet 'fake cargo: verbose build output' "$stderr"

# A current cached binary is silent and does not call cargo.
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$test_home PATH=$test_path "$launcher" --list > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
[[ ! -s "$stderr" ]]
[[ ! -s "$cargo_argument_log" ]]
printf '%s\n' '[--list]' > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# --verbose and -v narrate the cached decision without forcing a build and are
# exact synonyms from the application's point of view.
for verbose_flag in --verbose -v; do
    case "$verbose_flag" in
        --verbose) verbose_name=verbose ;;
        -v) verbose_name=v ;;
    esac
    : > "$cargo_argument_log"
    : > "$application_argument_log"
    HOME=$test_home PATH=$test_path "$launcher" "$verbose_flag" --list \
        > "$test_root/$verbose_name.stdout" \
        2> "$test_root/$verbose_name.stderr"
    [[ ! -s "$cargo_argument_log" ]]
    grep --fixed-strings --quiet 'slack-tui: cached binary is current' \
        "$test_root/$verbose_name.stderr"
    cmp "$test_root/expected-application-arguments" "$application_argument_log"
done
cmp "$test_root/verbose.stdout" "$test_root/v.stdout"
cmp "$test_root/verbose.stderr" "$test_root/v.stderr"

# --build and --rebuild force the same rebuild. Both enable launcher and cargo
# output without a separate verbosity flag.
for build_flag in --build --rebuild; do
    : > "$cargo_argument_log"
    : > "$application_argument_log"
    HOME=$test_home PATH=$test_path "$launcher" "$build_flag" --list \
        > "$test_root/${build_flag#--}.stdout" \
        2> "$test_root/${build_flag#--}.stderr"
    grep --fixed-strings --quiet '[clean]' "$cargo_argument_log"
    grep --fixed-strings --quiet '[build]' "$cargo_argument_log"
    if grep --fixed-strings --quiet '[--quiet]' "$cargo_argument_log"; then
        echo "$build_flag unexpectedly made cargo quiet" >&2
        exit 1
    fi
    grep --fixed-strings --quiet \
        'slack-tui: building (forced by --build/--rebuild)...' \
        "$test_root/${build_flag#--}.stderr"
    grep --fixed-strings --quiet 'fake cargo: verbose clean output' \
        "$test_root/${build_flag#--}.stderr"
    grep --fixed-strings --quiet 'fake cargo: verbose build output' \
        "$test_root/${build_flag#--}.stderr"
    cmp "$test_root/expected-application-arguments" "$application_argument_log"
done
cmp "$test_root/build.stdout" "$test_root/rebuild.stdout"
cmp "$test_root/build.stderr" "$test_root/rebuild.stderr"

# The delimiter is consumed and disables launcher parsing for later flags.
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$test_home PATH=$test_path "$launcher" -- --build -v --list \
    > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
[[ ! -s "$stderr" ]]
[[ ! -s "$cargo_argument_log" ]]
printf '%s\n' '[--build]' '[-v]' '[--list]' \
    > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# The environment rebuild also announces progress.
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$test_home PATH=$test_path SLACK_TUI_REBUILD=1 "$launcher" --list \
    > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
grep --fixed-strings --quiet 'slack-tui: building (' "$stderr"
grep --fixed-strings --quiet 'fake cargo: verbose build output' "$stderr"
if grep --fixed-strings --quiet '[--quiet]' "$cargo_argument_log"; then
    echo 'ordinary build unexpectedly made cargo quiet' >&2
    exit 1
fi

# A failing build keeps the cached binary/stamp and never launches it.
cp "$test_home/.cache/slack-tui/source.sha256" "$test_root/stamp-before-failure"
cp "$test_home/.cache/slack-tui/bin/slack-tui" "$test_root/binary-before-failure"
: > "$application_argument_log"
failure_status=0
HOME=$test_home PATH=$test_path SLACK_TUI_REBUILD=1 SLACK_TUI_FAKE_BUILD_FAILURE=1 \
    "$launcher" --list > "$stdout" 2> "$stderr" || failure_status=$?
[[ "$failure_status" == 42 ]]
[[ ! -s "$application_argument_log" ]]
grep --fixed-strings --quiet 'fake cargo: build failed' "$stderr"
cmp "$test_root/stamp-before-failure" "$test_home/.cache/slack-tui/source.sha256"
cmp "$test_root/binary-before-failure" "$test_home/.cache/slack-tui/bin/slack-tui"

# Help reads current application help from the source without cargo, a cached
# binary, or cache mutation.
help_home=$test_root/help-home
rm --force "$cargo_argument_log" "$application_argument_log"
HOME=$help_home PATH=$test_path "$launcher" --help > "$stdout" 2> "$stderr"
[[ ! -s "$stderr" ]]
[[ ! -e "$cargo_argument_log" ]]
[[ ! -e "$application_argument_log" ]]
[[ ! -e "$help_home/.cache/slack-tui" ]]
grep --fixed-strings --quiet -- '--build, --rebuild' "$stdout"
grep --fixed-strings --quiet -- '--verbose, -v' "$stdout"
grep --fixed-strings --quiet 'do not force a rebuild' "$stdout"
grep --fixed-strings --quiet -- '--root DIR' "$stdout"

# Debug forces a dev build and full backtrace, with a separate binary/stamp.
cp "$test_home/.cache/slack-tui/source.sha256" "$test_root/release-stamp"
: > "$cargo_argument_log"
HOME=$test_home PATH=$test_path RUST_BACKTRACE=0 "$launcher" --build-debug --list \
    > "$stdout" 2> "$stderr"
grep --fixed-strings --quiet '[--profile]' "$cargo_argument_log"
grep --fixed-strings --quiet '[dev]' "$cargo_argument_log"
grep --fixed-strings --quiet '[clean]' "$cargo_argument_log"
grep --fixed-strings --quiet '[build]' "$cargo_argument_log"
if grep --fixed-strings --quiet '[--release]' "$cargo_argument_log"; then
    echo 'debug build unexpectedly touched release' >&2
    exit 1
fi
[[ $(< "$SLACK_TUI_BACKTRACE_LOG") == full ]]
[[ -x "$test_home/.cache/slack-tui/bin/slack-tui-debug" ]]
[[ -s "$test_home/.cache/slack-tui/source-debug.sha256" ]]
cmp "$test_root/release-stamp" "$test_home/.cache/slack-tui/source.sha256"
printf '%s\n' '[--list]' > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# Plain launch still uses the current release without rebuilding or enabling backtraces.
: > "$cargo_argument_log"
HOME=$test_home PATH=$test_path RUST_BACKTRACE=0 "$launcher" --list > "$stdout" 2> "$stderr"
[[ ! -s "$cargo_argument_log" ]]
[[ $(< "$SLACK_TUI_BACKTRACE_LOG") == 0 ]]

# A repeated debug request rebuilds; debug takes precedence over release flags in either position.
: > "$cargo_argument_log"
HOME=$test_home PATH=$test_path "$launcher" --build --build-debug --rebuild --list \
    > "$stdout" 2> "$stderr"
grep --fixed-strings --quiet '[clean]' "$cargo_argument_log"
grep --fixed-strings --quiet '[dev]' "$cargo_argument_log"
if grep --fixed-strings --quiet '[--release]' "$cargo_argument_log"; then
    echo 'combined build flags unexpectedly selected release' >&2
    exit 1
fi

# Help remains side-effect-free even when combined with debug; -- passes the flag through.
: > "$cargo_argument_log"
HOME=$help_home PATH=$test_path "$launcher" --build-debug --help > "$stdout" 2> "$stderr"
[[ ! -s "$cargo_argument_log" ]]
[[ ! -e "$help_home/.cache/slack-tui" ]]
grep --fixed-strings --quiet -- '--build-debug' "$stdout"
HOME=$test_home PATH=$test_path "$launcher" -- --build-debug > "$stdout" 2> "$stderr"
[[ ! -s "$cargo_argument_log" ]]
printf '%s\n' '[--build-debug]' > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# Prune keeps runnable binaries, stamps and Slack data; never launches or builds.
mkdir --parents "$test_home/.cache/slack-tui/live/profiles"
printf 'keep\n' > "$test_home/.cache/slack-tui/live/profiles/test.json"
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$test_home PATH=$test_path "$launcher" --build-cache-prune > "$stdout" 2> "$stderr"
[[ ! -e "$test_home/.cache/slack-tui/target" ]]
[[ -x "$test_home/.cache/slack-tui/bin/slack-tui" ]]
[[ -x "$test_home/.cache/slack-tui/bin/slack-tui-debug" ]]
[[ -s "$test_home/.cache/slack-tui/source-debug.sha256" ]]
cmp "$test_root/release-stamp" "$test_home/.cache/slack-tui/source.sha256"
[[ $(< "$test_home/.cache/slack-tui/live/profiles/test.json") == keep ]]
[[ ! -s "$application_argument_log" ]]
grep --fixed-strings --quiet '[clean]' "$cargo_argument_log"
: > "$cargo_argument_log"
HOME=$test_home PATH=$test_path "$launcher" --build-cache-prune > "$stdout" 2> "$stderr"
[[ ! -s "$cargo_argument_log" ]]
HOME=$test_home PATH=$test_path "$launcher" --list > "$stdout" 2> "$stderr"
[[ ! -s "$cargo_argument_log" ]]
for conflict in --build --build-debug --list; do
    if HOME=$test_home PATH=$test_path "$launcher" --build-cache-prune "$conflict" > "$stdout" 2> "$stderr"; then
        echo 'conflicting prune arguments accepted' >&2; exit 1
    fi
done
ln --symbolic "$test_home/.cache/slack-tui/live" "$test_home/.cache/slack-tui/target"
if HOME=$test_home PATH=$test_path "$launcher" --build-cache-prune > "$stdout" 2> "$stderr"; then
    echo 'symlink target accepted' >&2; exit 1
fi
[[ -s "$test_home/.cache/slack-tui/live/profiles/test.json" ]]
exec {held_lock}>"$test_home/.cache/slack-tui/build.lock"
flock --exclusive "$held_lock"
lock_status=0
HOME=$test_home PATH=$test_path timeout 1 "$launcher" --build-cache-prune > "$stdout" 2> "$stderr" || lock_status=$?
[[ "$lock_status" == 124 ]]
[[ ! -s "$stdout" ]]
grep --fixed-strings --quiet 'waiting for another build or cache operation' "$stderr"
flock --unlock "$held_lock"
exec {held_lock}>&-
HOME=$test_home PATH=$test_path "$launcher" --build-cache-prune --help > "$stdout" 2> "$stderr"
[[ -L "$test_home/.cache/slack-tui/target" ]]
HOME=$test_home PATH=$test_path "$launcher" -- --build-cache-prune > "$stdout" 2> "$stderr"
grep --fixed-strings --quiet '[--build-cache-prune]' "$application_argument_log"

echo 'slack-tui launcher tests: passed'
