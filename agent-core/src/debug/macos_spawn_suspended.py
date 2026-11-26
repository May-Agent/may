import ctypes
import os
import shutil
import sys

prog_arg = sys.argv[1]
resolved = prog_arg if "/" in prog_arg else (shutil.which(prog_arg) or prog_arg)
prog = os.fsencode(resolved)
raw_args = [os.fsencode(a) for a in sys.argv[1:]]
raw_env = [os.fsencode(f"{k}={v}") for k, v in os.environ.items()]

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
attr = ctypes.c_void_p()
if libc.posix_spawnattr_init(ctypes.byref(attr)) != 0:
    sys.exit(126)
# POSIX_SPAWN_SETEXEC (0x0040) | POSIX_SPAWN_START_SUSPENDED (0x0080)
if libc.posix_spawnattr_setflags(ctypes.byref(attr), ctypes.c_short(0x0040 | 0x0080)) != 0:
    sys.exit(126)

argv_t = ctypes.c_char_p * (len(raw_args) + 1)
envp_t = ctypes.c_char_p * (len(raw_env) + 1)
argv = argv_t(*raw_args, None)
envp = envp_t(*raw_env, None)
rc = libc.posix_spawn(None, prog, None, ctypes.byref(attr), argv, envp)
sys.stderr.write(f"posix_spawn({resolved}) failed: {os.strerror(rc)}\n")
sys.exit(127 if rc == 2 else 126)
