# Vendored browser scripts

These files are served from `/vendor/` and loaded by `views/partials/header.ejs`.
They are copied unmodified from the published npm release tarballs and are not
listed in `package.json` (spec D9). `test/vendorHtmx.test.js` checks each file
against the hash below, and the header's `integrity` attributes must match.

| File | Package | Version | SRI |
| --- | --- | --- | --- |
| `htmx.min.js` | htmx.org | 2.0.4 | `sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+` |
| `sse.min.js` | htmx-ext-sse | 2.2.4 | `sha384-A986SAtodyH8eg8x8irJnYUk7i9inVQqYigD6qZ9evobksGNIXfeFvDwLSHcp31N` |

Sources (tarball sha512 integrity verified against the npm registry):

- `https://registry.npmjs.org/htmx.org/-/htmx.org-2.0.4.tgz`, file `package/dist/htmx.min.js`
- `https://registry.npmjs.org/htmx-ext-sse/-/htmx-ext-sse-2.2.4.tgz`, file `package/dist/sse.min.js`

Both packages are licensed under the BSD Zero Clause License (0BSD).

## Updating

1. Download the new tarball from the registry and check its `dist.integrity`.
2. Copy the minified file here unmodified.
3. Recompute the hash: `openssl dgst -sha384 -binary <file> | base64`.
4. Update the table above and the `integrity` attribute in `views/partials/header.ejs`.
