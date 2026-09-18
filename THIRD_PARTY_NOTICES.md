# Third-party components

The root ISC license covers original Jarvis code. Dependencies retain their
own licenses; consult the Cargo and npm lockfiles and upstream notices.

## Removed grocery integration

The unused grocery integration has been removed from the current source tree,
including the Giant PRISM client and browser helper previously attributed to
DSado88/Grocery. Redistribution permission was not established, so those
components are not part of this source release.

Older Git commits and pull-request diffs can still contain the removed code.
Removing it from the current tree does not remove those historical copies;
see [the publication checklist](docs/PUBLISH.md) before promoting the repository.

## Optional 9Router service

The model-account installer downloads and builds [9Router](https://github.com/decolua/9router), licensed under the [MIT license](https://github.com/decolua/9router/blob/17c4cc76877bd1755030a8414f8d0083f48dcccf/LICENSE). Its source and transitive dependencies retain their respective licenses in the installed runtime. The pinned dependency lock is in `sidecars/9router/`.
