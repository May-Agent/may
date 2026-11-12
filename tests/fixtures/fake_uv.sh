#!/bin/sh
# A fake `uv` for the python shim tests: `venv ... <directory>` builds an
# environment whose interpreters print `session`; anything else prints the
# `VIRTUAL_ENV` it runs with and its arguments. `venv` is slow enough for
# racing commands to overlap.
if [ "$1" = venv ]; then
    for directory; do :; done
    sleep 0.2
    mkdir -p "$directory/bin"
    for python in python python3; do
        printf '#!/bin/sh\necho session "$@"\n' > "$directory/bin/$python"
        chmod 755 "$directory/bin/$python"
    done
    echo created >> "$HOME/venv-runs"
    exit 0
fi
echo "uv venv=${VIRTUAL_ENV:-unset} $*"
