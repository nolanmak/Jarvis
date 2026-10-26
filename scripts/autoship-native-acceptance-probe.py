#!/usr/bin/env python3
"""Explicit paid native probes, one at a time, with retained isolated evidence.

Build the test binary through cargo-managed.sh first. This launcher never builds,
changes the production daemon, or publishes to GitHub. The adoption probe uses
real Git/native models but simulated GitHub; the issue probe simulates only its
remote-main identity lookup. An explicit private VM policy supplies declared
provider credentials; no required-workflow host transport fallback is allowed.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import stat
from pathlib import Path
import subprocess
import tempfile


_spec=importlib.util.spec_from_file_location('probe_routing',Path(__file__).with_name('native-probe-routing.py'))
routing=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(routing)


TESTS = {
    "issue": "self_improve::native_acceptance::live_native_disposable_scope_build_verify_review_and_seed",
    "adoption": "self_improve::adoption_acceptance::live_native_source_less_adoption_repairs_reviews_and_merges",
    "adjudicator": "self_improve::native_acceptance::live_native_final_adjudicator_resolves_synthetic_contradiction",
    "writer": "self_improve::native_acceptance::live_native_writer_inventory_replay_1538",
    "routed_transport": "self_improve::native_route_acceptance::live_native_operator_routed_transport",
}


def reachable_build_memory(probe, declared_build_mb):
    # Native writer replay and contradiction probes invoke the supplied-evidence
    # adjudicator, which clears tools before fresh_review_scope. Their Python
    # fixture observation runs in the controller's bounded command scope.
    # Routed transport uses the production configured constructor with no Bash.
    # Issue/adoption exercise builder and repair paths with guarded VM builds.
    roles={'issue':'builder','adoption':'builder','writer':'reviewer','adjudicator':'reviewer','routed_transport':'reviewer'}
    if set(roles)!=set(TESTS) or probe not in roles:
        raise ValueError('probe capabilities must be explicitly classified')
    if roles[probe]=='reviewer':
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
    args = parser.parse_args()
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
    available=next((int(line.split()[1])*1024 for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:')),0)
    try:
        budget=resource_budget(native_mb,build_mb,available,args.controller_headroom_mb,args.defer_run_admission)
    except ValueError as error:
        parser.error(str(error))
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
    for value in ("Delegate=cpu memory pids", "DelegateSubgroup=daemon", f"MemoryMax={aggregate_memory}", "TasksMax=256", "CPUQuota=200%"):
        command += ["-p", value]
    command += [f"--setenv={key}={value}" for key, value in env.items()]
    command += ["/usr/bin/env"]
    for key in ("AUGMENTAGENT_WORKFLOW_DB", "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
                "AUGMENTAGENT_TOOL_AUDIT_LOG", "AUGMENTAGENT_COOLDOWN_FILE", "AUGMENTAGENT_TOKEN_USAGE_LOG",
                "AUGMENTAGENT_AUTOPR_OPENED_FILE", "AUGMENTAGENT_AUTOPR_BASELINE_FILE",
                "AUGMENTAGENT_AUTOPR_UNREVIEWABLE_FILE", "AUGMENTAGENT_AUTOPR_FAIR_FILE",
                "AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE", "AUGMENTAGENT_AUTOPR_HISTORY_FILE",
                "AUGMENTAGENT_PRUNE_TARGET_DIRS", "AUGMENTAGENT_BUILD_STORAGE_PARENT_LEASE"):
        command += ["-u", key]
    command += [str(binary), "--exact", TESTS[args.probe], "--ignored", "--nocapture"]
    tested_binary_digest=binary_digest(binary)
    if tested_binary_digest!=inventory_binary_digest:
        parser.error("selected binary changed after inventory validation; retry with a stable managed artifact")
    print(f"Evidence: {root}", flush=True)
    with (root / "transcript.log").open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    route_unchanged=routing.unchanged(route_evidence)
    (root / "operator-route-final.json").write_text(json.dumps({"unchanged":route_unchanged,"initial_sha256":hashlib.sha256((root/"operator-route.json").read_bytes()).hexdigest()}))
    if not route_unchanged:
        raise RuntimeError("operator routing changed during probe; result cannot establish one route")
    (root / "result.json").write_text(json.dumps({"probe": args.probe, "test": TESTS[args.probe], "binary": str(binary), "binary_sha256": tested_binary_digest, "test_inventory_sha256": inventory_digest, "vm_policy": str(policy), "vm_policy_sha256": policy_digest, "unit": unit, "exit_code": result.returncode, "no_production_mutation": True, "resource_preflight": budget}, indent=2) + "\n")
    print(f"Exit: {result.returncode}; transcript: {root / 'transcript.log'}", flush=True)
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
