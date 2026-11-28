# Working across Auto Ship and Jarvis

The public [Auto Ship repository](https://github.com/nolanmak/autoship) owns the submission types, repository normalization and durable workflow ID functions in the autoship-contract crate. Change those definitions upstream. Jarvis owns admission transactions, the worker and all delivery policy until those components are explicitly migrated.

Jarvis consumes the commit recorded in Cargo.lock. Builds and CI should use the lockfile. To adopt an upstream change, run `cargo update -p autoship-contract`, inspect the lockfile diff, and open a Jarvis PR. The existing monthly Cargo Dependabot configuration also covers this dependency.

For a coordinated change, point the dependency at the public feature branch in a draft Jarvis PR and update the lockfile. After the upstream PR lands, restore the branch to main and resolve the lockfile again before merging the Jarvis change. Run the submission tests against that final revision.

Move one component per handoff. Immediately before merge, compare the affected Jarvis paths with current main, port any intervening fixes upstream, and rerun integration checks. Remove the old implementation in the same PR that starts consuming the shared crate.
