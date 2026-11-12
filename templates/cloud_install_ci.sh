set -eu
bin="$HOME/{{ dir }}/bin"
mkdir -p "$bin"
tar -xzf - -C "$bin" may
chmod 755 "$bin/may"
printf '%s' {{ identity }} > "$HOME/{{ dir }}/installed"
