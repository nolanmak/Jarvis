"""Exercise the actual guest validator at a deterministic metadata race point.

All mutations are inside this disposable guest. The workload loads the shipped
validator from an explicitly selected read-only input and changes a real file immediately after its first kernel fstat;
this avoids depending on scheduler timing. The parent supervisor subsequently
receives an unsafe hard-linked output and must refuse the whole export.
"""
import os
from pathlib import Path
import runpy
import time

validator = runpy.run_path('/inputs/validator.py')['export_files']
path = Path('/outputs/bad')
path.write_bytes(b'original')
identity = path.stat()
original_fstat = os.fstat
injected = False


def mutate_after_stat(fd):
    global injected
    info = original_fstat(fd)
    if not injected and (info.st_dev, info.st_ino) == (identity.st_dev, identity.st_ino):
        injected = True
        if RACE_MODE == 'hardlink':
            os.link(path, '/outputs/alias')
        elif RACE_MODE == 'restored_mtime':
            time.sleep(.01)
            path.write_bytes(b'tampered')
            os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns))
        elif RACE_MODE == 'replacement':
            path.rename('/outputs/old')
            path.write_bytes(b'tampered')
        else:
            raise AssertionError('unknown race fixture')
    return info


os.fstat = mutate_after_stat
try:
    try:
        validator(['bad'])
    except ValueError as error:
        assert str(error) == 'output_denied'
    else:
        raise AssertionError('export validator accepted a metadata race')
finally:
    os.fstat = original_fstat
assert injected
# Exercise the normal parent export path as well, with an unsafe file after
# the inner race refusal. No test hook or setting is added to production.
os.link(path, '/outputs/final-alias')
