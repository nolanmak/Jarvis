"""Test-only observer for real matcher waits, kills, and reap completion.

The packaged bridge and matcher run unchanged. No deadlines, clocks, process
results, or JSON-RPC replies are replaced by this observer.
"""
import json
import runpy
import subprocess
import sys

AUDIT_PREFIX = 'GREP_TIMEOUT_AUDIT='
matchers = []
Popen = subprocess.Popen


class ObservedProcess(Popen):
    def __init__(self, args, *positional, **keywords):
        self.observation = None
        super().__init__(args, *positional, **keywords)
        if list(args[1:4]) == ['-I', '-S', '-c']:
            self.observation = {'timed_waits': [], 'expired': False,
                                'killed': False, 'reaped': False, 'returncode': None}
            matchers.append(self.observation)

    def wait(self, timeout=None):
        if self.observation is not None and timeout is not None:
            self.observation['timed_waits'].append(timeout)
        try:
            status = super().wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            if self.observation is not None:
                self.observation['expired'] = True
            raise
        if self.observation is not None:
            self.observation['reaped'] = True
            self.observation['returncode'] = status
        return status

    def kill(self):
        result = super().kill()
        if self.observation is not None:
            self.observation['killed'] = True
        return result


subprocess.Popen = ObservedProcess
sys.argv = sys.argv[1:]
# Packaged MCP argv includes -I; the observer's interpreter already uses it.
if sys.argv[:1] == ['-I']:
    sys.argv = sys.argv[1:]
try:
    runpy.run_path(sys.argv[0], run_name='__main__')
finally:
    print(AUDIT_PREFIX + json.dumps(matchers), file=sys.stderr, flush=True)
