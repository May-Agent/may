set -eu
mkdir -p "$HOME/{{ dir }}"
curl --proto '=https' --tlsv1.2 -LsSf {{ installer }} | CARGO_HOME="$HOME/{{ dir }}" sh
printf '%s' {{ identity }} > "$HOME/{{ dir }}/installed"
