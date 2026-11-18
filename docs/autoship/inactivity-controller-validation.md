# Native inactivity controller validation

Normal tests use real temporary Git repositories and durable stores with injected
budgets/clocks. They cover expiry, independent waits, duplicate progress, lost leases,
terminal state, input holds, unpaid inspection and merge observation. The native
renewal loop retains the production 60-second interval.

The delegated-service test uses a real supervised Python process and detached child,
protected workflow audit, declared cgroup resources and the actual renewal/finish path.
It injects a 25 ms renewal interval and short scope budget after kernel admission,
checks descendant disappearance and kernel quiescence, proves the lease is still held,
then finishes and checks zero model calls plus durable retry eligibility. It does not
invoke a provider or use production workflow data.

The case is excluded from generic runners that lack delegated cgroups. Run it explicitly
in an isolated transient user service after compiling in the same worktree; never use
the daemon service. Continue to use the managed wrapper and your owned target cache:

```sh
systemd-run --user --wait --pipe --collect \
  -p 'Delegate=cpu memory pids' -p DelegateSubgroup=daemon \
  -p MemoryMax=1G -p TasksMax=128 -p CPUQuota=100% \
  --working-directory="$PWD" \
  /usr/bin/env -u AUGMENTAGENT_NATIVE_VM_POLICY \
  CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  AUGMENTAGENT_RUN_RESOURCES=1 AUGMENTAGENT_RUN_MEMORY_BYTES=67108864 \
  AUGMENTAGENT_RUN_PIDS=32 AUGMENTAGENT_RUN_CPU_PERCENT=50 \
  scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent \
  --features augmentagent-channel-core/test-isolation \
  self_improve::workflow::idle_controller::tests::renewal_stops_real_hung_descendants_before_releasing_lease \
  -- --ignored --exact --nocapture --test-threads=1
```

This sets policy only for the transient fixture. It does not install a daemon drop-in,
restart the worker, alter merge allowance or confirm production restoration.
