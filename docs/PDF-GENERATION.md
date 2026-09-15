# PDF generation

`augmentagent doc render-pdf` creates a local, print-ready PDF from Markdown or
plain text and returns an `ATTACH:` marker for the existing Discord delivery
layer. Query mode can invoke this command directly; no rendering service, API
key, database, or network access is required.

## Install

Requires Python 3.11+, ReportLab, Python-Markdown and Liberation fonts. On Ubuntu:

```sh
sudo apt-get install python3-pip fonts-liberation poppler-utils
python3 -m pip install --user -r crates/augmentagent-docs/python/requirements.txt
```

On systems that restrict pip installation into the system interpreter, install
those requirements in a virtual environment and put its `bin` directory on the
daemon's PATH. The command uses `python3` from PATH. The worker is embedded in the
Rust binary, so it does not need the source checkout at runtime. Python ignores
`PYTHONPATH` and does not import modules from the wiki working directory.

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
python3 -m unittest discover -s crates/augmentagent-docs/python -v
cargo test -p augmentagent-docs
cargo test -p augmentagent-cli --test pdf_generation
cargo test -p augmentagent-channel-core ask_opts_pdf_generation --lib
cargo test -p augmentagent-approval-discord attachments:: --lib
```

The CLI integration test renders a real PDF from a fresh wiki directory and
passes it through Discord attachment preparation without sending a message.
