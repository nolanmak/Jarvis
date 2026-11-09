# Native credential round-trip component (#1936)

Scoped C0 producer/consumer repair under #1918/#1917/#1703. The existing refresh
can commit 1162285 bytes from a readable 900150-byte original, then its 1048576-byte
reader refuses the result. No live credential/issuer was used. Rust declaration
readiness currently accepts up to the generic 2 MiB file limit, unlike launch
seeding. This component fixes those concrete byte-bound inconsistencies without
changing grants, account identity, approved rotating fields, scope or source/
resource/input/native retirement authority. Full C0/C2 and restoration remain open.

Q1. Keep the supported encoded credential maximum exactly 1048576 bytes. Define
one Python constant and use it for every controlled credential read/write path;
Rust readiness independently enforces the same boundary through behavior tests.
Check actual bytes returned as well as the existing regular/owner/private/single-
link stat checks: a grow-between-stat-and-read result cannot exceed the limit.
Do not raise the limit or truncate/delete metadata to make a refresh fit.

Q2. Direct guest-export refresh already obtains a private candidate bounded to
that limit. Require strict UTF-8 bytes (no UTF-16/32 autodetection), valid Unicode scalar
strings, and unambiguous standard JSON (reject duplicate keys at any depth
and nonfinite numbers), then apply the unchanged existing account/scope/
identity/nonrotating-field validation. If changed, publish those exact validated
candidate bytes, which are already readable by the next seed. This preserves
metadata and avoids expansion from re-encoding UTF-8 as ASCII escape sequences.
An unchanged decoded credential still causes no write; an independently changed
controlled credential still wins through the existing digest check. This is not
permission to accept a different native export or resume a conversation.

Q3. Checkpoint refresh constructs the updated dictionary using existing rules,
then encodes compact UTF-8 JSON and checks its final encoded byte length before
recording result_digest, replacing the auth file or declaring committed. Encoding
failure/nonfinite output/oversize refuses; no partial/truncated metadata is saved.
The original issuer response and original_digest/received_at_ms must already be
persisted as the existing received journal BEFORE constructed-credential
validation/encoding refusal. Preserve existing response-envelope limits and
transport-encoding checks: rejected/oversized raw envelopes stay pending/uncertain,
not falsely received/committed or permission for another issuer request.
It remains received without result_digest on an oversized result. Neither the
old token nor a copied digest is permission for another issuer request.

Q4. Preserve the existing recovery order. Replaying received uses the same exact
response/lineage/time, with no network or new expiry timestamp; unsupported
output keeps refusing without auth mutation or committed status. begin must run
that recovery before it could send again. Successful auth replacement followed
by failed committed publication still uses its original result_digest to settle
without reissuing rotation. A committed journal does not make a missing, malformed
or oversized current credential usable; bounded validation must still occur.
Independent owner credential changes retain the existing lineage/refusal rules;
this component creates no new operator recovery or metadata-dropping shortcut.

Q5. The gateway already checkpoints successful issuer rotation before forwarding
headers. Preserve/test that real branch with an oversized constructed result:
received response survives; no success headers/data/end reach the guest, no
committed journal appears, and later begin refuses before any new issuer send.
Existing credential-use serialization and checkpoint settlement remain unchanged.
A persistent received hold is explicit unsupported/uncertain state, not successful
native completion or a release of storage/model/workflow debt.

Q6. Rust credentials_at must validate every declared auth_file as private,
readable, JSON and <=1048576 bytes before returning a usable/refresh-authorized
declaration, including environment-key routes with a file (the launcher seeds
any declared file). Preserve environment-key-only routes with no auth_file.
These are read-only declaration facts, live_verified remains false; no file,
credential contents, hash-based execution grant or secret enters public status.
Already oversized legacy files are invalid/held, not silently read with a larger
cap, truncated, restored or made valid by their old committed journal.

## Test-first and completion

- Define Python behavioral RED through actual checkpoint begin/sent/commit/read,
both providers, and UTF-8 direct-export re-encoding overflow; distinguish new helper
absence from existing false success. Define Rust readiness exact/one-byte-over
and environment/file fixtures first and reproduce its false usable declaration.
- Exact limit fits; one-byte over refuses before target mutation; all static
metadata and account/scope constraints survive. Cover actual read-size race,
ambiguous/nonstandard candidate rejection, Unicode, both providers and unchanged
inputs. Controlled synthetic files only, no live issuer/provider/credential.
- Reopen received and committed journals, preserve original response/time and
block old-token retries; inject failure at received/result/auth/committed writes.
Exercise actual gateway success-header boundary with scripted socket/issuer.
- Run relevant Python native/gateway/IO/provisioning suites and real Rust readiness
suite using scripts/cargo-managed.sh. Record skips and exact tested SHA, required
hosted checks and independent code approval. Original snapshot/updater rollout
and full #1703 live CLI/Discord/shipment/fault/soak remain separate obligations.

This closed component requires fresh-reader and complete seam review before a
narrow implementation exception. It does not complete whole native growth,
filesystem/inode/quota, durable accounting/migration, all-writer custody or the
productive owner constructor.

## Review record

Fresh-reader and all 17 mapped seam checks are complete. Rechecks resolved the
UTF-16/32 autodetection mismatch and distinguished raw response-envelope refusal
from constructed-credential validation after durable receipt. No remaining
concrete scoped contract finding was reported. This is design review, not
independent code approval, full C0 closure or deployed acceptance.
