#!/usr/bin/env python3
"""Explicit paid native probes, one at a time, with retained isolated evidence.

Build the test binary through cargo-managed.sh first. This launcher never builds,
changes the production daemon, or publishes to GitHub. The adoption probe uses
real Git/native models but simulated GitHub; the issue probe simulates only its
remote-main identity lookup. An explicit private VM policy supplies declared
provider credentials; no required-workflow host transport fallback is allowed.
"""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
import re
import uuid
import stat
import sys
from pathlib import Path
import subprocess
import tempfile
import time


_spec=importlib.util.spec_from_file_location('probe_routing',Path(__file__).with_name('native-probe-routing.py'))
routing=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(routing)


TESTS = {
    "issue": "self_improve::native_acceptance::live_native_disposable_scope_build_verify_review_and_seed",
    "adoption": "self_improve::adoption_acceptance::live_native_source_less_adoption_repairs_reviews_and_merges",
    "adjudicator": "self_improve::native_acceptance::live_native_final_adjudicator_resolves_synthetic_contradiction",
    "writer": "self_improve::native_acceptance::live_native_writer_inventory_replay_1538",
    "file_edit": "self_improve::native_route_acceptance::live_native_guarded_file_edit",
    "claude_file_edit": "self_improve::native_route_acceptance::live_native_guarded_claude_file_edit",
    "routed_transport": "self_improve::native_route_acceptance::live_native_operator_routed_transport",
}


def reachable_build_memory(probe, declared_build_mb):
    # Native writer replay and contradiction probes invoke the supplied-evidence
    # adjudicator, which clears tools before fresh_review_scope. Their Python
    # fixture observation runs in the controller's bounded command scope.
    # Routed transport uses the production configured constructor with no Bash.
    # Issue/adoption exercise builder and repair paths with guarded VM builds.
    roles={'issue':'builder','adoption':'builder','writer':'reviewer','adjudicator':'reviewer','routed_transport':'reviewer','file_edit':'file_only','claude_file_edit':'file_only'}
    if set(roles)!=set(TESTS) or probe not in roles:
        raise ValueError('probe capabilities must be explicitly classified')
    if roles[probe] in ('reviewer','file_only'):
        return 0
    if type(declared_build_mb) is not int or not 512<=declared_build_mb<=4096:
        raise ValueError('invalid declared build guest memory')
    return declared_build_mb


def resource_budget(native_mb, build_mb, available, controller_mb=256, defer_run_admission=False):
    if (type(native_mb) is not int or not 512<=native_mb<=4096
            or type(build_mb) is not int or (build_mb!=0 and not 512<=build_mb<=4096)):
        raise ValueError('invalid declared guest memory')
    if type(controller_mb) is not int or not 256<=controller_mb<=2048:
        raise ValueError('controller allowance must be 256 through 2048 MiB')
    if type(defer_run_admission) is not bool:
        raise ValueError('invalid deferred admission selector')
    if type(available) is not int or available<0:
        raise ValueError('invalid host available memory')
    run=(native_mb+build_mb)*1024**2+1024**3
    aggregate=run+2*1024**3
    # Aggregate MemoryMax is a ceiling, not an additional RAM reservation.
    # This conservative estimate admits starting the test controller only.
    # RunScope still checks real controller/sibling/ancestor/host usage before
    # each process dispatch. The allowance is not a controller RSS guarantee.
    needed=run+(controller_mb+256)*1024**2
    startup_floor=(controller_mb+256)*1024**2 if defer_run_admission else needed
    if aggregate>10*1024**3 or available<startup_floor:
        raise ValueError('insufficient host headroom for composed isolated probe; wait for capacity')
    return {'run_memory_bytes':run,'aggregate_ceiling_bytes':aggregate,
            'controller_allowance_bytes':controller_mb*1024**2,
            'host_margin_bytes':256*1024**2,'preflight_required_bytes':needed,
            'observed_available_bytes':available,'final_admission':'RunScope',
            'deferred_run_admission':defer_run_admission,'startup_floor_bytes':startup_floor,
            'run_admitted_by_preflight':False}


# Host build lease: the same flock pair managed builds (disk-build.lock) and
# deployment builds (disk-update.lock) hold. A VM run waits in line on it so
# no build can start beside the VM, instead of racing a MemAvailable reading.
LEASE_LOCKS=('disk-update.lock','disk-build.lock')


class LeaseUnavailable(RuntimeError):
    """The host build lease stayed busy for the whole bounded wait."""


class LeaseInvalid(LeaseUnavailable):
    """A lock file failed validation; never retried."""


def host_state_dir(environ):
    """channel-core state_dir::resolve, from the wrapper's own environment."""
    state_home=environ.get('XDG_STATE_HOME','')
    if state_home and os.path.isabs(state_home):
        return Path(state_home)/'augmentagent'
    home=environ.get('HOME','')
    if not home:
        raise LeaseInvalid('no state directory for the host build lease (HOME unset)')
    return Path(home)/'.local/state/augmentagent'


def holder_pid(path, locks_text=None):
    """First FLOCK WRITE holder of path in /proc/locks (major:minor hex, inode decimal)."""
    try:
        info=os.stat(path,follow_symlinks=False)
        if locks_text is None:
            locks_text=Path('/proc/locks').read_text()
    except OSError:
        return None
    device=f'{os.major(info.st_dev):02x}:{os.minor(info.st_dev):02x}:{info.st_ino}'
    for line in locks_text.splitlines():
        fields=line.split()
        if len(fields)<6 or '->' in line or fields[1]!='FLOCK' or fields[3]!='WRITE':
            continue
        if fields[5]==device and fields[4].isdigit():
            return int(fields[4])
    return None


def describe_holder(path):
    pid=holder_pid(path)
    if pid is None:
        return 'unknown holder'
    try:
        comm=Path(f'/proc/{pid}/comm').read_text().strip()
    except OSError:
        comm='(exited)'
    return f'pid {pid} {comm}'


class HostBuildLease:
    def __init__(self, fds, waited_seconds):
        self.fds=fds
        self.waited_seconds=waited_seconds
        self.locks=list(LEASE_LOCKS)

    def release(self):
        while self.fds:
            os.close(self.fds.pop())


def _try_lock(state, name):
    """Open/validate like Rust Lease::try_kind_at; return fd, or None when busy."""
    fd=os.open(state/name,os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC,0o600)
    try:
        info=os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink!=1 or info.st_uid!=os.geteuid():
            raise LeaseInvalid(f'{name} must be an owned regular single-link file')
        try:
            fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            return None
        # Migrate historical umask-derived modes through the held inode only.
        os.fchmod(fd,0o600)
        return fd
    except BaseException:
        try:
            os.close(fd)
        except OSError:
            pass
        raise


def acquire_host_build_lease(state, wait_seconds, status, poll=0.1, progress_every=60,
                             clock=time.monotonic, sleep=time.sleep):
    """Wait in line for both host locks; each attempt is all-or-nothing.

    Never sits on disk-update.lock while waiting for disk-build.lock, so a
    waiting VM run does not block deploys. Validation errors are not retried.
    """
    state=Path(state)
    os.makedirs(state,mode=0o700,exist_ok=True)
    started=clock();announced=None;next_progress=started+progress_every
    while True:
        held=[];busy=None
        try:
            for name in LEASE_LOCKS:
                fd=_try_lock(state,name)
                if fd is None:
                    busy=name;break
                held.append(fd)
        except OSError as error:
            for fd in held:os.close(fd)
            raise LeaseInvalid(f'host build lease unavailable: {error.strerror or error}') from None
        except BaseException:
            for fd in held:os.close(fd)
            raise
        now=clock()
        if busy is None:
            waited=now-started if announced else 0.0
            if announced:
                status(f'host build lease acquired after {waited:.1f}s')
            return HostBuildLease(held,waited)
        for fd in held:os.close(fd)
        waited=now-started
        if waited>=wait_seconds:
            raise LeaseUnavailable(f'host build lease still busy: {busy} held by {describe_holder(state/busy)} '
                                   f'after {waited:.1f}s (limit {wait_seconds:g}s); managed builds did not drain, retry later')
        if announced is None:
            announced=busy
            status(f'waiting in line for host build lease ({busy} held by {describe_holder(state/busy)}); up to {wait_seconds:g}s')
        elif now>=next_progress:
            next_progress=now+progress_every
            status(f'still waiting for host build lease: {waited:.0f}s of {wait_seconds:g}s ({busy})')
        sleep(poll)


def admit_host(state, native_mb, build_mb, controller_mb, defer, wait_seconds, settle_seconds,
               read_available, status, sleep=time.sleep, clock=time.monotonic, settle_poll=2):
    """Lease first, then memory: a reading taken while builds can still start is meaningless."""
    # Validate static inputs before queueing so a bad declaration never holds the lease.
    resource_budget(native_mb,build_mb,2**62,controller_mb,defer)
    lease=acquire_host_build_lease(state,wait_seconds,status)
    try:
        started=clock()
        while True:
            available=read_available()
            try:
                budget=resource_budget(native_mb,build_mb,available,controller_mb,defer)
            except ValueError:
                waited=clock()-started
                if waited>=settle_seconds:
                    required=resource_budget(native_mb,build_mb,2**62,controller_mb,defer)['startup_floor_bytes']
                    raise ValueError(f'insufficient host headroom for composed isolated probe with the host build lease held: '
                                     f'required {required} bytes, last reading {available} bytes after {waited:.1f}s settle') from None
                sleep(settle_poll)
                continue
            budget['settle_seconds']=round(clock()-started,3)
            return lease,budget
    except BaseException:
        lease.release()
        raise


def binary_digest(binary):
    digest=hashlib.sha256()
    with binary.open('rb') as artifact:
        for chunk in iter(lambda:artifact.read(1024*1024),b''):
            digest.update(chunk)
    return digest.hexdigest()


def verify_test_inventory(binary, expected):
    # libtest listing executes no test body. An unknown filter otherwise exits
    # successfully after running zero tests, which cannot be acceptance evidence.
    result=subprocess.run([str(binary),'--list','--ignored','--exact',expected],
                          stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=15)
    if result.returncode or len(result.stdout)>1024*1024 or len(result.stderr)>1024*1024:
        raise ValueError('selected binary test inventory unavailable')
    try:
        lines=result.stdout.decode('utf-8').splitlines()
    except UnicodeDecodeError:
        raise ValueError('selected binary test inventory is not valid UTF-8') from None
    tests=[line for line in lines if line.endswith(': test')]
    if tests!=[expected+': test']:
        raise ValueError('selected binary lacks the exact ignored acceptance test; rebuild through cargo-managed.sh')
    return hashlib.sha256(result.stdout).hexdigest()


COMPLETION_WRAPPER = """
import json, os, subprocess, sys
os.umask(0o077)
path, nonce = sys.argv[1:3]
result = subprocess.run(sys.argv[3:])
with open(path, 'x') as stream:
    json.dump({'nonce': nonce, 'exit_code': result.returncode}, stream)
    stream.flush()
    os.fsync(stream.fileno())
raise SystemExit(result.returncode if result.returncode >= 0 else 1)
"""

def completion_status(service_status, receipt, nonce, transcript, test):
    # systemd considers an explicitly stopped service successful. Only a
    # wrapper that observed the test process finish can attest completion.
    if service_status != 0 or receipt != {'nonce': nonce, 'exit_code': 0}:
        return service_status or 1
    completed = re.search(r'^test ' + re.escape(test) + r' \.\.\. ok$', transcript, re.MULTILINE)
    summary = re.search(r'^test result: ok\. 1 passed; 0 failed; 0 ignored;', transcript, re.MULTILINE)
    return 0 if completed and summary else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", choices=TESTS)
    parser.add_argument("--operator-router-config",type=Path,default=routing.default_path('AUGMENTAGENT_MODEL_ROUTER_CONFIG','model-router.json'))
    parser.add_argument("--operator-selection-config",type=Path,default=routing.default_path('AUGMENTAGENT_MODEL_SELECTION_CONFIG','model-selection.json'))
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--vm-policy", type=Path, required=True,
                        help="private disposable native VM policy prepared for this probe")
    parser.add_argument("--build-vm-config", type=Path, help="explicit build runtime; defaults to native policy runtime")
    parser.add_argument("--controller-headroom-mb", type=int, default=256,
                        help="preflight controller allowance, minimum256MiB; final RunScope admission remains authoritative")
    parser.add_argument("--defer-run-admission", action="store_true",
                        help="start only the bounded controller; RunScope must admit the unchanged full run before VM/model dispatch")
    parser.add_argument("--lease-wait-secs", type=float, default=1800,
                        help="wait this long for the host build lease (disk-update.lock + disk-build.lock) so no "
                             "managed or deployment build runs beside the VMs; polled, no ordering guarantee; 0 tries once")
    parser.add_argument("--capacity-settle-secs", type=float, default=120,
                        help="with the lease held, re-read MemAvailable this long for memory freed by a finished build; 0 reads once")
    parser.add_argument("--max-run-secs", type=int, default=14400,
                        help="unit RuntimeMaxSec, which bounds how long the host build lease is held (minimum 60)")
    args = parser.parse_args()
    if args.max_run_secs<60:
        parser.error("--max-run-secs must be at least 60")
    if not (args.lease_wait_secs>=0 and args.capacity_settle_secs>=0):
        parser.error("--lease-wait-secs and --capacity-settle-secs must be non-negative")
    route_evidence,route_environment=routing.snapshot(args.operator_router_config,args.operator_selection_config)
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an existing executable managed test artifact")
    inventory_binary_digest=binary_digest(binary)
    try:
        inventory_digest=verify_test_inventory(binary,TESTS[args.probe])
    except (ValueError,subprocess.TimeoutExpired,OSError) as error:
        parser.error(str(error) if isinstance(error,ValueError) else 'selected binary inventory could not be verified')
    policy = args.vm_policy.resolve(strict=True)
    info = policy.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        parser.error("VM policy must be a private file owned by the current user")
    declaration=json.loads(policy.read_text())
    native_mb=declaration['memory_mb']
    build_config=(args.build_vm_config or Path(declaration['runtime_config'])).resolve(strict=True)
    build_info=build_config.stat()
    if not stat.S_ISREG(build_info.st_mode) or build_info.st_uid!=os.getuid() or build_info.st_mode & 0o077:
        parser.error("build VM configuration must be private")
    try:
        build_mb=reachable_build_memory(args.probe,json.loads(build_config.read_text())['memory_mb'])
    except ValueError as error:
        parser.error(str(error))
    # Host build lease first, memory second: a reading taken while managed
    # builds can still start loses every race. The unit's own builds use the
    # isolated XDG_STATE_HOME below, so holding the host lease cannot deadlock it.
    try:
        lease,budget=admit_host(host_state_dir(os.environ),native_mb,build_mb,args.controller_headroom_mb,
                                args.defer_run_admission,args.lease_wait_secs,args.capacity_settle_secs,
                                read_mem_available,lambda line:print(line,flush=True))
    except LeaseUnavailable as error:
        # Same clean-deferral exit as a busy managed build (storage busy).
        print(f"{parser.prog}: {error}",file=sys.stderr,flush=True)
        return 75
    except ValueError as error:
        parser.error(str(error))
    try:
        return launch(args,binary,inventory_binary_digest,inventory_digest,policy,build_config,
                      budget,route_evidence,route_environment,lease,parser)
    finally:
        lease.release()


def read_mem_available():
    return next((int(line.split()[1])*1024 for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:')),0)


def launch(args,binary,inventory_binary_digest,inventory_digest,policy,build_config,budget,
           route_evidence,route_environment,lease,parser):
    run_memory=budget['run_memory_bytes']
    aggregate_memory=budget['aggregate_ceiling_bytes']
    policy_digest = hashlib.sha256(policy.read_bytes()).hexdigest()
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix="autoship-native-acceptance-", dir="/tmp"))
    for name in ("state", "cache", "builder", "gate"):
        (root / name).mkdir(mode=0o700)
    env = {
        "TMPDIR": "/tmp", "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"), "CARGO_TARGET_DIR": str(root / "builder"),
        "AUGMENTAGENT_GATE_TARGET_DIR": str(root / "gate"),
        "AUGMENTAGENT_AUTOPR_REVIEWERS": "codex",
        "AUGMENTAGENT_AUTOPR_OBSERVATION_CALL_CAP": "32",
        "AUGMENTAGENT_GATE_MIN_FREE_GB": "0", "AUGMENTAGENT_GATE_BUILD_GROWTH_GB": "0",
        "AUGMENTAGENT_NATIVE_VM_POLICY": str(policy),
        "AUGMENTAGENT_BUILD_VM_CONFIG": str(build_config),
        "AUGMENTAGENT_RUN_RESOURCES": "1", "AUGMENTAGENT_RUN_MEMORY_BYTES": str(run_memory),
        "AUGMENTAGENT_RUN_PIDS": "128", "AUGMENTAGENT_RUN_CPU_PERCENT": "100",
        "CARGO_BUILD_JOBS": "1", "CARGO_PROFILE_DEV_DEBUG": "0",
        "CARGO_PROFILE_TEST_DEBUG": "0", "CARGO_INCREMENTAL": "0", "PATH": os.environ["PATH"],
    }
    env.update(route_environment)
    (root/"operator-route.json").write_text(json.dumps(route_evidence,sort_keys=True))
    if args.probe == "issue":
        gh = root / "remote-main-fixture.py"
        gh.write_text('''#!/usr/bin/python3
import subprocess, sys
a = sys.argv[1:]
if len(a) != 4 or a[0] != 'api' or not a[1].startswith('repos/autoship-canary/full-') or not a[1].endswith('/git/ref/heads/main') or a[2:] != ['--jq', '.object.sha']:
    raise SystemExit('unsupported read-only remote-main fixture call')
subprocess.run(['/usr/bin/git', 'rev-parse', 'refs/remotes/origin/main'], check=True)
''')
        gh.chmod(0o700)
        env["GH_BIN"] = str(gh)
    unit = "augmentagent-native-" + root.name.removeprefix("autoship-native-acceptance-")
    command = ["systemd-run", "--user", "--quiet", "--wait", "--pipe", "--collect", f"--unit={unit}"]
    for value in ("Delegate=cpu memory pids", "DelegateSubgroup=daemon", f"MemoryMax={aggregate_memory}", "TasksMax=256", "CPUQuota=200%", f"RuntimeMaxSec={args.max_run_secs}"):
        command += ["-p", value]
    command += [f"--setenv={key}={value}" for key, value in env.items()]
    command += ["/usr/bin/env"]
    for key in ("AUGMENTAGENT_WORKFLOW_DB", "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
                "AUGMENTAGENT_TOOL_AUDIT_LOG", "AUGMENTAGENT_COOLDOWN_FILE", "AUGMENTAGENT_TOKEN_USAGE_LOG",
                "AUGMENTAGENT_AUTOPR_OPENED_FILE", "AUGMENTAGENT_AUTOPR_BASELINE_FILE",
                "AUGMENTAGENT_AUTOPR_UNREVIEWABLE_FILE", "AUGMENTAGENT_AUTOPR_FAIR_FILE",
                "AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE", "AUGMENTAGENT_AUTOPR_HISTORY_FILE",
                "AUGMENTAGENT_PRUNE_TARGET_DIRS", "AUGMENTAGENT_BUILD_STORAGE_PARENT_LEASE",
                "AUGMENTAGENT_BUILD_PARENT_LEASE"):
        command += ["-u", key]
    completion_nonce = uuid.uuid4().hex
    completion_path = root / "test-completion.json"
    command += ["/usr/bin/python3", "-I", "-c", COMPLETION_WRAPPER, str(completion_path), completion_nonce, str(binary), "--exact", TESTS[args.probe], "--ignored", "--nocapture"]
    tested_binary_digest=binary_digest(binary)
    if tested_binary_digest!=inventory_binary_digest:
        parser.error("selected binary changed after inventory validation; retry with a stable managed artifact")
    print(f"Evidence: {root}", flush=True)
    with (root / "transcript.log").open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    try:
        completion = json.loads(completion_path.read_text())
    except (OSError, ValueError):
        completion = None
    exit_code = completion_status(result.returncode, completion, completion_nonce, (root / "transcript.log").read_text(errors="replace"), TESTS[args.probe])
    route_unchanged=routing.unchanged(route_evidence)
    (root / "operator-route-final.json").write_text(json.dumps({"unchanged":route_unchanged,"initial_sha256":hashlib.sha256((root/"operator-route.json").read_bytes()).hexdigest()}))
    if not route_unchanged:
        raise RuntimeError("operator routing changed during probe; result cannot establish one route")
    (root / "result.json").write_text(json.dumps({"probe": args.probe, "test": TESTS[args.probe], "binary": str(binary), "binary_sha256": tested_binary_digest, "test_inventory_sha256": inventory_digest, "vm_policy": str(policy), "vm_policy_sha256": policy_digest, "unit": unit, "exit_code": exit_code, "service_exit_code": result.returncode, "test_completed": completion is not None, "no_production_mutation": True, "resource_preflight": budget, "host_build_lease": {"waited_seconds": round(lease.waited_seconds, 3), "locks": lease.locks}}, indent=2) + "\n")
    print(f"Exit: {exit_code}; transcript: {root / 'transcript.log'}", flush=True)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
