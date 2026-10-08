"""Actual character-device refusal and unprivileged device creation probe."""
import errno
import os
import runpy
import stat

assert os.geteuid() != 0
assert stat.S_ISCHR(os.stat('/dev/null').st_mode)
validator = runpy.run_path('/inputs/validator.py')['export_files']
original_open = os.open


def device_directory(name, *args, **kwargs):
    return original_open('/dev' if name == '/outputs' else name, *args, **kwargs)


# Exercise the shipped validator against an existing kernel character device,
# without granting mknod or changing the production supervisor's configuration.
os.open = device_directory
try:
    try:
        validator(['null'])
    except ValueError as error:
        assert str(error) == 'output_denied'
    else:
        raise AssertionError('export validator accepted a character device')
finally:
    os.open = original_open
try:
    os.mknod('/outputs/bad', stat.S_IFCHR | 0o600, os.makedev(1, 3))
except OSError as error:
    assert error.errno in (errno.EPERM, errno.EACCES)
else:
    raise AssertionError('unprivileged workload acquired device-creation authority')
# The normal parent export also refuses the requested, uncreatable device.
