#!/usr/bin/env bash
#
# Launcher for coon's three frontends.
#
#   ./run.sh                          TUI, demo data
#   ./run.sh /path/to/project         TUI, clangd on that project
#   ./run.sh --web /path/to/project   web client on http://localhost:8080
#   ./run.sh --desktop                Tauri window, demo data
#
# Options:
#   -w, --web            serve the browser client instead of the TUI
#   -d, --desktop        run the Tauri desktop shell
#   -p, --port PORT      port for --web (default 8080)
#   -r, --release        build with optimisations
#   -h, --help           this message
#
# Anything after `--` is passed through to the binary, so the layout flags
# still work:  ./run.sh -- --node-width 24

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

FRONTEND=tui
PORT=8080
PROFILE=()
PROJECT=""
PASSTHROUGH=()

usage() { sed -n '2,19p' "$0" | sed 's/^#\s\?//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        -w|--web)     FRONTEND=web; shift ;;
        -d|--desktop) FRONTEND=desktop; shift ;;
        -p|--port)    PORT="${2:?--port needs a value}"; shift 2 ;;
        -r|--release) PROFILE=(--release); shift ;;
        -h|--help)    usage; exit 0 ;;
        --)           shift; PASSTHROUGH=("$@"); break ;;
        -*)           echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *)
            [[ -n "$PROJECT" ]] && { echo "only one project path, got '$PROJECT' and '$1'" >&2; exit 2; }
            PROJECT="$1"; shift ;;
    esac
done

if [[ -n "$PROJECT" ]]; then
    [[ -d "$PROJECT" ]] || { echo "no such directory: $PROJECT" >&2; exit 1; }
    PROJECT="$(cd "$PROJECT" && pwd)"

    command -v clangd >/dev/null \
        || echo "warning: clangd is not in PATH — LSP mode needs it" >&2
    [[ -f "$PROJECT/compile_commands.json" ]] \
        || echo "warning: no compile_commands.json in $PROJECT — falling back to a directory walk" >&2
else
    echo "no project path given — running with demo data" >&2
fi

ARGS=()
[[ -n "$PROJECT" ]] && ARGS+=("$PROJECT")

case "$FRONTEND" in
    tui)
        exec cargo run "${PROFILE[@]}" -p coon -- "${ARGS[@]}" "${PASSTHROUGH[@]+"${PASSTHROUGH[@]}"}"
        ;;
    web)
        echo "serving on http://localhost:$PORT" >&2
        exec cargo run "${PROFILE[@]}" -p coon -- "${ARGS[@]}" --web "$PORT" "${PASSTHROUGH[@]+"${PASSTHROUGH[@]}"}"
        ;;
    desktop)
        # `desktop` is outside the workspace default members on purpose: it
        # pulls the whole Tauri tree, so it only builds when asked for.
        exec cargo run "${PROFILE[@]}" -p desktop -- "${ARGS[@]}" "${PASSTHROUGH[@]+"${PASSTHROUGH[@]}"}"
        ;;
esac
