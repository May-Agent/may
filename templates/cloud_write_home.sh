set -eu
umask 077
mkdir -p "$HOME/.may"
cat > "$HOME/.may/{{ file }}.partial"
mv -f "$HOME/.may/{{ file }}.partial" "$HOME/.may/{{ file }}"
