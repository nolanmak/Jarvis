# Build storage

Use `scripts/cargo-managed.sh` in place of `cargo` for session builds and tests.
It chooses a stable cache per worktree, registers ownership, reclaims eligible
output before building, and holds the build/prune lease. No environment variable
is required. An explicit `CARGO_TARGET_DIR` is supported through this wrapper.
The auto-PR builder and normal updater coordinate storage themselves.

For bootstrapping changes to the storage commands before the deployed binary
supports them, reuse an existing target, keep dev/test debuginfo disabled, check
available space, and do not delete a cache while any build uses it. Resume the
managed wrapper once a supporting binary is available. Do not create another
unregistered target directory to work around a busy cache.

Take rollout backups through `augmentagent deploy snapshot`; do not create raw
database copies or ad hoc rollback directories outside its retention policy.

See `docs/disk-storage.md` for ownership, budgets, rollback protection, and rollout
verification. Follow `CLAUDE.md` for other repository conventions when present.
