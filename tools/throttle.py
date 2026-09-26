#!/usr/bin/env python3
"""Copy stdin to stdout at no more than RATE bytes per second (e.g. 12M)."""
import sys
import time

rate = sys.argv[1] if len(sys.argv) > 1 else "12M"
limit = int(rate[:-1]) * {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}[rate[-1]] if rate[-1].isalpha() else int(rate)
chunk = 1 << 20
start = time.monotonic()
sent = 0
src, dst = sys.stdin.buffer, sys.stdout.buffer
while True:
    data = src.read(chunk)
    if not data:
        break
    dst.write(data)
    sent += len(data)
    ahead = sent / limit - (time.monotonic() - start)
    if ahead > 0:
        time.sleep(ahead)
dst.flush()
