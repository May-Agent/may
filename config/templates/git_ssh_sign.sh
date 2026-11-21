#!/bin/sh
# git's gpg.ssh.program in this sandbox. Signing hands the buffer git wants
# signed to the harness, which signs it on the host with the user's key and
# prints the signature; every other mode (verify, find-principals,
# check-novalidate) is ssh-keygen's own.
mode=
previous=
for argument in "$@"; do
    if [ "$previous" = -Y ]; then
        mode=$argument
    fi
    previous=$argument
    buffer=$argument
done
if [ "$mode" != sign ]; then
    exec ssh-keygen "$@"
fi
if ! {{ command }} < "$buffer" > "$buffer.sig"; then
    rm -f "$buffer.sig"
    exit 1
fi
