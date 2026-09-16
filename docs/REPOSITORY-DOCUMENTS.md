# Read-only repository documents

`augmentagent repo-docs` reads current documents from configured GitHub repositories and stages originals for Discord delivery. It never checks out or executes repository code, initializes submodules, pushes, or uses the ambient GitHub login.

## Setup

Create a separate SSH deploy key for each repository. Add the public key in the repository's **Settings → Deploy keys** with **Allow write access unchecked**. Verify GitHub reports `read_only: true`. Keep the private key outside the wiki and public checkout, owned by the daemon user with permissions 0600. Verify GitHub's host key in that user's known_hosts file; the CLI requires strict host-key checking and does not enroll keys automatically.

Create `~/.config/augmentagent/repo-docs.json` (or under XDG_CONFIG_HOME), mode 0600:

```json
{
  "sources": {
    "reference-docs": {
      "repository": "example/documentation",
      "branch": "main",
      "key_path": "/absolute/private/path/document-read-key",
      "known_hosts": "/absolute/private/path/known_hosts"
    }
  }
}
```

Use a genuine read-only deploy key: the local CLI cannot determine an arbitrary SSH key's server-side permissions. The provisioning check is essential. Do not reuse the general owner SSH key. Git runs with an empty inherited environment, disabled global/system configuration and hooks, no SSH agent, and only the explicitly configured identity. The query issue helper is also restricted to the Jarvis report repository so it cannot write issues/comments in a document repository. This limits the query tools; it does not revoke credentials used by unrelated self-improvement or issue-publishing tools.

## Commands

```sh
augmentagent repo-docs sources
augmentagent repo-docs list --source reference-docs --prefix reports
augmentagent --wiki-dir ./wiki repo-docs get --source reference-docs --path 'reports/Example report.pdf'
augmentagent --wiki-dir ./wiki gmail get-attachment --account owner@example.com --message-id EXAMPLE_ID --name 'Example report.pdf' --deliver --extract false
```

Query-agent invocations inherit WIKI_ROOT and need no global wiki flag. `list` and `get` fetch the latest configured branch into a fresh private temporary bare repository. Results include the immutable revision SHA and repository commit timestamp; the latter is not a per-file modification date. `get` also reports the exact blob ID and an ATTACH marker. No remote URL, arbitrary Git options, config override or mutation command is accepted from the query agent.

Only regular non-executable document blobs are eligible: PDF, Markdown, text, DOC/DOCX, CSV/TSV, RTF, XLSX and PPTX. Symlinks and submodules are excluded. Original delivery is limited to 8 MiB per file and five files per answer by the existing Discord layer. Fetches time out after 90 seconds. A fresh shallow fetch avoids stale clones but may transfer other files in that repository; use a dedicated document repository.

Files are preserved verbatim in unique private `download-*` directories under the wiki. They remain there for later download and may be included in an existing private wiki backup. They are not automatically deleted. Existing wiki-only attachment containment remains unchanged; arbitrary temporary paths are still refused. The default email download/extraction path remains unchanged without `--deliver`.

Repository documents and attachment contents are untrusted data. Public issues, tests and PRs must use synthetic examples; private source mappings, filenames, document text and live validation receipts stay in private runtime storage.
