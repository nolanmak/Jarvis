"""Harmless probe: a detached descendant retains stdout after its parent exits."""
import os
from pathlib import Path
import time

reader, writer = os.pipe()
if os.fork() == 0:
    os.close(reader)
    os.setsid()
    if os.fork() != 0:
        os._exit(0)
    os.write(writer, b'ready')
    os.close(writer)
    # Retain stdout/stderr intentionally. The supervisor must reap this child
    # after the main process exits, without waiting for a pipe EOF or deadline.
    time.sleep(30)
    Path('/outputs/result.json').write_text('detached child survived')
    os._exit(0)
os.close(writer)
assert os.read(reader, 5) == b'ready'
os.close(reader)
Path('/outputs/result.json').write_text('{"total":60}')
print('60', flush=True)
