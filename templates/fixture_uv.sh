#!/bin/sh
PATH=/usr/bin:/bin
case "$1" in
--version) printf '%s\n' 'uv {{ version }}'; exit 0;;
python)
    for request; do :; done
    dir="$UV_PYTHON_INSTALL_DIR/$request"
    mkdir -p "$dir/bin" "$UV_PYTHON_INSTALL_DIR/.temp"
    cat > "$dir/bin/python3.13" <<'INTERPRETER'
{{ interpreter|trim }}
INTERPRETER
    chmod 755 "$dir/bin/python3.13"
    ln -s python3.13 "$dir/bin/python3"
    ln -s "$dir" "$UV_PYTHON_INSTALL_DIR/cpython-3.13-minor"
    : > "$UV_PYTHON_INSTALL_DIR/.lock"
    exit 0;;
esac
printf '%s\n' 'uv {{ version }}'
