# Relocate managed build output

`AUGMENTAGENT_BUILD_VOLUME` selects an existing, user-owned build volume.
Without component overrides it selects these dedicated directories:

| Output | Directory below the volume | Specific override |
| --- | --- | --- |
| Gate Cargo target | `augmentagent-gate-target` | `AUGMENTAGENT_GATE_TARGET_DIR` |
| Interactive session targets | `augmentagent-session-targets` | `AUGMENTAGENT_BUILD_PARENT` |
| Independent verifier targets | `augmentagent-verifier-targets` | None |
| Native VM build scratch | `codex-vm` | `AUGMENTAGENT_BUILD_SCRATCH_DIR` |

`CARGO_TARGET_DIR` still selects a specific managed build target. Existing
component overrides win independently; moving the gate alone does not move
verifiers away from the volume. With no volume setting, legacy defaults remain.
The wrapper reads its shell environment, so export the setting in interactive
shells as well as placing it in the worker's `.env`.

`augmentagent disk relocate --to /owned/volume --dry-run` prints a JSON plan,
including the setting, destinations, source paths, retirement candidates and
required/available bytes. Dry-run creates no directories, tags, records or
locks. Destination validation refuses a symlink (including a symlink ancestor),
a directory owned by another user, a non-writable directory, the same filesystem
as all source outputs, or less than the normal admission floor plus growth
headroom. It uses the destination filesystem's available bytes.

Without `--dry-run`, the command enrolls the new dedicated session/verifier
parents and gate using the existing ownership registry. It records retirement
intentions for old registered Cargo caches on other filesystems. It never edits
`.env`, restarts services, copies artifacts or deletes old artifacts itself.
Normal prune checks the original cache identity, registration, active configured
paths, physical target leases and running processes before reclaiming retired
profiles, including recently used idle profiles. Busy caches wait for a later
pass. Tags/ownership records can remain after their rebuildable profiles are
reclaimed. Unknown and foreign directories are not relocation candidates.
Admission still checks every filesystem actually receiving build output,
including separately mounted or symlinked profiles; a larger unrelated volume
cannot satisfy its floor. `disk status` includes the resolved gate, session,
verifier and VM scratch paths in its filesystem labels.

## Legion production rollout (Nolan/orchestrator only)

These steps are deliberately **not** executed by the issue lane or by CI.
The production updater deploys `main` every five minutes. Coordinate this change
with a normal updater deployment/restart; do not restart the worker directly.
An environment-only edit does not itself force a rebuild/restart when the
updater's built commit is already current. If this PR has already deployed,
apply the setting ahead of the next normal code deployment and wait for that
updater restart. Keep the deployment checkout clean.

1. After merge, confirm the installed binary supports `disk relocate --help`.
   Use the deployed checkout and binary for every command below, not a lane's
   test binary. Take the policy-managed rollback snapshot:

   ```sh
   augmentagent deploy snapshot
   ```

   If storage refuses the snapshot, report it to the owner and stop. Do not
   copy databases, override storage floors, delete snapshots or unpin them.

2. Record a baseline **before** editing config or pruning:

   ```sh
   augmentagent disk status --json > /tmp/build-volume-before.json
   python3 -c 'import os; s=os.statvfs("/mnt/build"); print(s.f_bavail*s.f_frsize)'
   augmentagent disk relocate --to /mnt/store --dry-run
   ```

   Save the integer as `BEFORE_FREE_BYTES` in the operator shell. `/mnt/store`
   already belongs to the deployment user; no mount, chown or symlink is
   needed. Inspect the plan's sources and retirement candidates.

3. In production `.env`, set exactly:

   ```dotenv
   AUGMENTAGENT_BUILD_VOLUME=/mnt/store
   ```

   Remove/unset the existing
   `AUGMENTAGENT_GATE_TARGET_DIR=/mnt/build/augmentagent-gate-target`.
   Remove any old-volume `AUGMENTAGENT_BUILD_PARENT`,
   `AUGMENTAGENT_BUILD_SCRATCH_DIR` and interactive `CARGO_TARGET_DIR`
   overrides unless keeping that specific output on its old filesystem is
   intentional. Preserve unrelated settings. Export the volume in the operator
   shell and unset those obsolete exported overrides too. Existing old parent
   and target registrations allow relocation discovery even after setting the
   new volume.

4. From the deployed checkout, enroll destinations and retirement records:

   ```sh
   export AUGMENTAGENT_BUILD_VOLUME=/mnt/store
   augmentagent disk relocate --to /mnt/store --dry-run
   augmentagent disk relocate --to /mnt/store
   ```

   Let the **normal updater** perform the coordinated deployment/restart and
   load the new `.env`. Its live-build/owner-turn safeguards stay in force.
   New builds recreate cold artifacts on `/mnt/store`; old builders retain their
   leases and finish in their original caches. VM scratch follows the new
   setting for future sessions; this command does not adopt/delete arbitrary
   old VM images or bypass native-session cleanup proofs.

5. Run a normal prune after old builders become idle (or wait for housekeeping):

   ```sh
   augmentagent disk prune
   augmentagent disk status --json > /tmp/build-volume-after.json
   python3 scripts/verify-build-volume.py --volume /mnt/store --old /mnt/build \
     --before-free-bytes "$BEFORE_FREE_BYTES" --status-json /tmp/build-volume-after.json
   ```

   The verifier is read-only. It requires gate/session/verifier labels on the
   `/mnt/store` device and a measured rise in `/mnt/build` free bytes. If busy
   caches or concurrent writers mask reclamation, keep the evidence and retry
   after an idle prune; do not delete leased caches. Record the command output,
   updater restart evidence, before/after status and free-byte values on
   **issue #2158 after merge**, outside CI. This live acceptance remains the
   production owner's responsibility.

## Rollback

Restore the prior `AUGMENTAGENT_BUILD_VOLUME` value (or unset it) and the prior
component overrides in `.env` and operator shells. Coordinate the next normal
updater deployment/restart. Active configured gate/session/verifier paths are
protected from retirement even if their earlier retirement records remain.
Old artifacts may already have been pruned: allow managed builds to recreate
cold output in the restored locations, subject to those filesystems' admission
headroom. Do not move caches back by hand or promise immediate admission on
`/mnt/build` if it is still full. Newly idle caches remain registered for normal
retention; they can be retired with a validated reverse relocation when the
restored destination has sufficient space. Rollback snapshots are restored
only through the supported deploy commands and their retention policy.
