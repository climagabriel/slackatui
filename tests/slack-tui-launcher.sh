#!/usr/bin/env bash
# maturity: low; issues-found: 0
set -euo pipefail

plugin_root=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
launcher=$plugin_root/bin/slack-tui
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
FAKE_APPLICATION
chmod +x "$fake_application"

cat > "$fake_tools/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf '[%s]\n' "$@" >> "$SLACK_TUI_CARGO_ARGUMENT_LOG"
command=$1
target_dir=
quiet=0
while (($#)); do
    case "$1" in
        --target-dir)
            shift
            target_dir=$1
            ;;
        --quiet)
            quiet=1
            ;;
    esac
    shift
done
[[ -n "$target_dir" ]]
if ((!quiet)); then
    echo "fake cargo: verbose $command output" >&2
fi
mkdir --parents "$target_dir/release"
if [[ "$command" == clean ]]; then
    rm --force "$target_dir/release/slack-tui"
else
    cp "$SLACK_TUI_FAKE_APPLICATION" "$target_dir/release/slack-tui"
fi
FAKE_CARGO
chmod +x "$fake_tools/cargo"

export SLACK_TUI_CARGO_ARGUMENT_LOG=$cargo_argument_log
export SLACK_TUI_APPLICATION_ARGUMENT_LOG=$application_argument_log
export SLACK_TUI_FAKE_APPLICATION=$fake_application
test_path=$fake_tools:$PATH
home=$test_root/home
stdout=$test_root/stdout
stderr=$test_root/stderr

# An ordinary first run builds quietly and passes every application argument
# unchanged, including an argument containing whitespace.
HOME=$home PATH=$test_path "$launcher" \
    --channel '#zero-member-test' 'two words' > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
[[ ! -s "$stderr" ]]
grep --fixed-strings --quiet '[--quiet]' "$cargo_argument_log"
printf '%s\n' '[--channel]' '[#zero-member-test]' '[two words]' \
    > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# A current cached binary is silent and does not call cargo.
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$home PATH=$test_path "$launcher" --list > "$stdout" 2> "$stderr"
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
    HOME=$home PATH=$test_path "$launcher" "$verbose_flag" --list \
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
    HOME=$home PATH=$test_path "$launcher" "$build_flag" --list \
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
HOME=$home PATH=$test_path "$launcher" -- --build -v --list \
    > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
[[ ! -s "$stderr" ]]
[[ ! -s "$cargo_argument_log" ]]
printf '%s\n' '[--build]' '[-v]' '[--list]' \
    > "$test_root/expected-application-arguments"
cmp "$test_root/expected-application-arguments" "$application_argument_log"

# The legacy environment rebuild stays quiet.
: > "$cargo_argument_log"
: > "$application_argument_log"
HOME=$home PATH=$test_path SLACK_TUI_REBUILD=1 "$launcher" --list \
    > "$stdout" 2> "$stderr"
[[ ! -s "$stdout" ]]
[[ ! -s "$stderr" ]]
grep --fixed-strings --quiet '[--quiet]' "$cargo_argument_log"

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

echo 'slack-tui launcher tests: passed'
