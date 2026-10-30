#!/bin/sh
# Decides which CI jobs a change needs. Writes "code=true|false" and "packaging=true|false" to $GITHUB_OUTPUT
# (stdout when it is not set).
#   code       anything except documentation and repository chrome: rustfmt, Clippy, the tests, cargo deny
#   packaging  the files the AppImage, tarball and Flatpak are built from, or how they are built
# Inputs come from the environment, never from the command line (the workflow passes event data through env:):
#   EVENT_NAME  github.event_name; only a pull_request is diffed, every other event (push to main, manual or scheduled run) runs everything
#   BASE_SHA    the commit the pull request is based on (github.event.pull_request.base.sha)
# Run it in a checkout that has the history of both commits. If the base is not there, everything runs.
set -eu

out=${GITHUB_OUTPUT:-/dev/stdout}
code=false
packaging=false

# Not code: the paths the old trigger filter ignored (*.md covers THIRD_PARTY.md and the pull request template).
is_doc() {
    case $1 in
        *.md | docs/* | LICENSE | .github/ISSUE_TEMPLATE/* | .gitignore) return 0 ;;
    esac
    return 1
}

# Packaging inputs: the build scripts and images, the workflows, the manifests, the shell build and QML, the data files
# and the translations (they are compiled into every package). The Rust and C++ sources are covered by `check`.
is_packaging() {
    case $1 in
        build/* | .github/* | Cargo.toml | Cargo.lock | */Cargo.toml | */Cargo.lock | src/shell/CMakeLists.txt | src/shell/qml/* | data/* | po/*) return 0 ;;
    esac
    return 1
}

if [ "${EVENT_NAME:-}" != pull_request ]; then
    code=true
    packaging=true
elif [ -z "${BASE_SHA:-}" ] || ! git cat-file -e "${BASE_SHA}^{commit}" 2>/dev/null; then
    echo "changes.sh: base commit '${BASE_SHA:-}' not available, running everything" >&2
    code=true
    packaging=true
else
    files=$(git -c core.quotePath=false diff --name-only "$BASE_SHA" HEAD)
    echo "Changed files:"
    printf '%s\n' "$files" | sed 's/^/  /'
    # The loop reads a here-document, not a pipe, so the variables it sets are kept.
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        is_doc "$f" && continue
        code=true
        if is_packaging "$f"; then packaging=true; fi
    done <<EOF
$files
EOF
fi

echo "code=$code" >>"$out"
echo "packaging=$packaging" >>"$out"
echo "code=$code packaging=$packaging"
