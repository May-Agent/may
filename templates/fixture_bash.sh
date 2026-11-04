#!/bin/sh
case "$1" in --version) printf '%s\n' 'GNU bash, version {{ version }}(1)-release (fixture)'; exit 0;; esac
exec /bin/bash "$@"
