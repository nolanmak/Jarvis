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

## Optional OmniRoute service

The explicit gateway installer downloads the pinned [OmniRoute npm package](https://www.npmjs.com/package/omniroute), licensed under [MIT](https://github.com/diegosouzapw/OmniRoute/blob/v3.8.52/LICENSE). The version and sha512 integrity are in `scripts/omniroute.pin`. Its dependencies retain their respective licenses in the installed runtime; no gateway runtime is vendored into this repository.
