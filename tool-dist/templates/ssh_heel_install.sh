# Installs the pinned `heel` fed on stdin, digest-addressed: the binary is
# staged under `mktemp`, verified by version and SHA-256, then renamed to
# `~/.local/bin/heel-<digest>` atomically, so a heel on PATH is never half
# written and never trusted by name.
#
# Rendered by `remote_heel` with `expected`: the pinned executable's
# SHA-256, lowercase hex.
set -eu

expected='{{expected}}'
bin="$HOME/.local/bin"
mkdir -p "$bin"

# The hex digest of `$1`, or nothing when no hasher is present.
digest() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -- "$1"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -- "$1"
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 -r -- "$1"
  else
    return 1
  fi | awk '{print $1}'
}

final="$bin/heel-$expected"
staged=$(mktemp "$bin/.heel.XXXXXX")
trap 'rm -f "$staged"' EXIT
cat > "$staged"
chmod 755 "$staged"
"$staged" --version >/dev/null
[ "$(digest "$staged")" = "$expected" ]
mv -f "$staged" "$final"
trap - EXIT
printf '%s\n' "$final"
