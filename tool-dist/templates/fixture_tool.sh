#!/bin/sh
{% if !aliases.is_empty() %}case "${0##*/}" in {{ command }}) ;; *) PATH=/usr/bin:/bin exec "${0##*/}" "$@";; esac
case "$1" in --list) printf '%s\n' {{ aliases.join(" ") }}; exit 0;; esac
{% endif %}printf '%s\n' '{{ command }} {{ version }}'
