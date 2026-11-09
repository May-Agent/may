"""Takes every jobserver token its MAKEFLAGS fifo hands out, reports how many
it holds to the file named by argv[1], and keeps them until it is killed."""
import os
import sys
import time

fifo = os.environ["MAKEFLAGS"].split("--jobserver-auth=fifo:")[1]
fd = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
held = 0
quiet_since = time.monotonic()
while time.monotonic() - quiet_since < 1.0:
    try:
        if os.read(fd, 1):
            held += 1
            quiet_since = time.monotonic()
    except BlockingIOError:
        time.sleep(0.01)
with open(sys.argv[1] + ".tmp", "w") as out:
    out.write(str(held))
os.rename(sys.argv[1] + ".tmp", sys.argv[1])
time.sleep(600)
