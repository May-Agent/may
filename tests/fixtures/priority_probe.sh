# Kernel-scheduling probe for Linux, run as `sh -c`: prints
# `parentNice parentIO childNice childIO`, the I/O priority in the kernel's
# IOPRIO_PRIO_VALUE encoding. The parent values are this shell's own, the
# child values are read by processes it spawns, so a run shows the ambient
# priority and whether a descendant inherits it.
#
# Only the shell and system tools run here: the Linux sandbox grants execute
# to system directories alone, so a compiled probe could not run inside it.
set -eu

# Field 19 of /proc/<pid>/stat is the nice value. The command name in field 2
# may contain spaces, so fields are counted after its closing parenthesis.
stat_nice() {
    fields=${1##*) }
    # shellcheck disable=SC2086 # splitting the fields is the point
    set -- $fields
    printf '%s\n' "${17}"
}

# ionice prints `<class>: prio <level>`, or the bare class for idle, which has
# no levels; IOPRIO_PRIO_VALUE is `(class << 13) | level`.
io_value() {
    case $1 in
        "none: prio "*) class=0 ;;
        "realtime: prio "*) class=1 ;;
        "best-effort: prio "*) class=2 ;;
        idle)
            printf '%s\n' $((3 << 13))
            return
            ;;
        *)
            printf 'priority_probe: unexpected ionice output: %s\n' "$1" >&2
            exit 1
            ;;
    esac
    printf '%s\n' $(((class << 13) | ${1##* }))
}

# `read` is a builtin, so /proc/self is this shell; `ionice -p $$` asks about
# this shell too. `cat` and a bare `ionice` each report themselves: children.
read -r parent_stat </proc/self/stat
parent_nice=$(stat_nice "$parent_stat")
parent_io=$(io_value "$(ionice -p $$)")
child_nice=$(stat_nice "$(cat /proc/self/stat)")
child_io=$(io_value "$(ionice)")
printf '%s %s %s %s\n' "$parent_nice" "$parent_io" "$child_nice" "$child_io"
