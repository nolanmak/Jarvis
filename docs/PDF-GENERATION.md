# PDF generation

`augmentagent doc render-pdf` creates a local, print-ready PDF from Markdown or
plain text and returns an `ATTACH:` marker for the existing Discord delivery
layer. Query mode can invoke this command directly; no rendering service, API
key, database, or network access is required.

## Install

Requires Python 3.11+, ReportLab, Python-Markdown and Liberation fonts. On Ubuntu:

```sh
sudo apt-get install python3-pip fonts-liberation poppler-utils
python3 scripts/pdf-runtime.py
python3 scripts/pdf-runtime.py --check
```

Deployment and service installation provision a private virtual environment at
`${XDG_DATA_HOME:-$HOME/.local/share}/augmentagent/pdf-runtime`. The complete
Python dependency set is pinned in `crates/augmentagent-docs/python/requirements.txt`.
The installer needs pip 22.3+ and builds a replacement before atomically switching
the active runtime. Re-running repairs missing packages or a broken interpreter.
Old runtime directories are retained so in-flight renders can finish.

The service startup wrapper performs an offline health check, including a real
render and font loading, before accepting work. Startup logs an actionable error
without taking unrelated channels offline. A failed provision prevents an
updater restart and withholds the deployment stamp for retry.
`AUGMENTAGENT_PDF_PYTHON` can explicitly select another provisioned interpreter;
the installer checks that interpreter without modifying it. Otherwise the CLI
uses the managed runtime regardless of PATH or wiki working directory. The worker
is embedded in Rust; Python ignores user site-packages, PYTHONPATH and cwd imports.

The font files must be available in `/usr/share/fonts/truetype/liberation/`.
`pdftotext` (poppler-utils) is needed for the verification suite, not rendering.
Missing packages or fonts produce an actionable error and no output PDF.

## CLI

```sh
export WIKI_ROOT=/absolute/path/to/wiki
augmentagent doc render-pdf deliverables/packet.md --out deliverables/packet.pdf --json
```

Paths are resolved relative to `WIKI_ROOT`, irrespective of the shell's current
directory. Absolute paths are accepted only inside that root. Input must have a
`.md`, `.markdown` or `.txt` extension. Omitting `--out` uses the input's name with
`.pdf`. The destination directory must already exist. Existing files are never
overwritten; use a new filename for a revision.

The JSON receipt contains `path`, `bytes` and `attach`. Only a successful render
returns a marker. Query mode summarizes the document and includes the marker in
its final reply, for example `ATTACH: deliverables/packet.pdf`. The Discord layer
retains its wiki-scope check, five-file limit and 8 MiB limit per attachment.

Markdown headings, paragraphs, emphasis, lists, tables, block quotes, code and
source URLs are supported, with page numbers and automatic pagination on US
Letter pages. Liberation fonts cover Latin, Greek and Cyrillic text, including
common typographic punctuation; this is not a full CJK/emoji renderer. Images are
represented by alt text, not embedded; raw HTML is literal text. The renderer
never fetches linked resources or interprets document text as executable markup.

Input is capped at 1 MiB, output at 8 MiB, and rendering at 30 seconds. Completed
PDFs are published without overwriting existing files; failures leave no partial
PDF at the requested path.

## Verify

```sh
~/.local/share/augmentagent/pdf-runtime/bin/python3 -m unittest discover -s crates/augmentagent-docs/python -v
python3 -m unittest discover -s scripts/tests -p pdf_runtime_test.py -v
cargo test -p augmentagent-docs
cargo test -p augmentagent-cli --test pdf_generation
cargo test -p augmentagent-channel-core ask_opts_pdf_generation --lib
cargo test -p augmentagent-approval-discord attachments:: --lib
```

The CLI integration test renders a real PDF from a fresh wiki directory and
passes it through Discord attachment preparation without sending a message.

## October 7, 2026 regression (#1433)

The host's apt history records `/usr/bin/unattended-upgrade` removing
`python3-markdown` 3.5.2-1 at 06:31:54 on October 7. The unattended-upgrades log
confirms it among successfully auto-removed GObject development dependencies at
06:32:01. ReportLab remained in the user's Python 3.12 site-packages. There is no
need to infer a Python upgrade: the package removal directly explains the import
failure.

CI installed requirements before rendering, while the production updater only
built Rust and never provisioned renderer dependencies. Thus its rendering tests
passed without testing the production installation path. CI now uses that same
installer, tests missing-dependency detection and repair, and exercises the CLI
against the managed runtime from a separate wiki directory. OS removal of
`python3-markdown` no longer affects the renderer. Host Python and Liberation
fonts remain system prerequisites; the preflight detects their loss.
