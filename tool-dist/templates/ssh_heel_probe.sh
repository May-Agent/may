# Finds a `heel` whose digest is the distribution's pinned one, so a heel
# already on the server is trusted only when it is the pinned binary.
#
# Rendered by `remote_heel` with `expected`: the pinned executable's
# SHA-256, lowercase hex.

expected='{{expected}}'

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

# `$1` is a usable pinned heel: executable, reports a version, and its
# digest is the pinned one.
verify() {
  [ -x "$1" ] || return 1
  "$1" --version >/dev/null 2>&1 || return 1
  [ "$(digest "$1")" = "$expected" ] || return 1
}

# The digest-addressed install this script's counterpart writes, first.
for heel in "$HOME/.local/bin/heel-$expected" "$(command -v heel 2>/dev/null)" "$HOME/.local/bin/heel"; do
  if [ -n "$heel" ] && verify "$heel"; then
    printf '%s\n' "$heel"
    exit 0
  fi
done
exit 0
