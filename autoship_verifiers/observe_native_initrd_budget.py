import importlib.util
import inspect
import json
import os
import stat
from pathlib import Path

root = Path.cwd()
source = root / 'scripts' / 'codex-build-vm.py'
spec = importlib.util.spec_from_file_location('autoship_native_vm_runtime', source)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
encode = module.initrd
parameters = list(inspect.signature(encode).parameters.values())
optional = [p for p in parameters[1:] if p.default is not inspect.Parameter.empty]
if len(optional) < 2:
    print(json.dumps(['legacy']))
else:
    uncompressed = 128 * 1024 ** 2
    compressed = 129 * 1024 ** 2
    def entry(data):
        return [('payload', stat.S_IFREG | 0o600, data, 0, 0)]
    def outcome(call):
        try:
            call()
        except Exception:
            return 'refused'
        return 'accepted'
    large_archive = outcome(lambda: encode(entry(b'x' * (uncompressed + 1)), uncompressed, None))
    incompressible = os.urandom(compressed + 1)
    large_output = outcome(lambda: encode(entry(incompressible), None, compressed))
    small_archive = outcome(lambda: encode(entry(b'bounded'), uncompressed, compressed))
    print(json.dumps(['bounded', large_archive, large_output, 'encoded' if small_archive == 'accepted' else small_archive]))
