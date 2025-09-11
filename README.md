---
title: diaryx-cli
author: Adam Harris
created: 2025-09-08T12:00:00-6:00
updated: 2025-09-10T22:20:00-6:00
visibility: public
format: "[CommonMark (Markdown)](https://spec.commonmark.org/0.31.2/)"
reachable: "[diaryx-cli git repo](https://github.com/adammharris/diaryx-cli)"
version: v0.2.0-alpha
---

# diaryx-cli

`diaryx-cli` is a Rust command‑line tool for turning Diaryx-formatted Markdown files into a small static HTML site.
It works on a *single entry file* (a Diaryx file). If that file is a **root index** (`this_file_is_root_index: true`), it recursively traverses the `contents` lists of index files it references to build a multi-page site. Otherwise it produces a single-page site for just that file (plus its local attachments).

This is an early MVP focusing on the `build` subcommand. Additional subcommands (like `import`, `validate`, and `watch`) can be layered in later.

---

## Key Concepts

Diaryx file:
- A Markdown file with YAML frontmatter.
- Required properties: `title`, `author`, `created`, `updated`, `visibility`, `format`.
- Optional: `contents`, `part_of`, `tags`, `aliases`, flags like `this_file_is_root_index`, etc.

Root Index behavior:
- If the entry file has `this_file_is_root_index: true`, it is treated as the site root.
- Any file with a `contents` list is considered an “index” node; its listed files are recursively loaded.
- Cycles are avoided with a visited set.

Single File behavior:
- If the entry file is not a root index, only that file is rendered to `index.html`.
- Attachments referenced by relative links are copied.

Visibility & publishing:
- By default, *non-public* content (anything whose `visibility` does not include `public`) is excluded.
- To include non-public files in the output you must opt in with `--include-nonpublic`.

---

## Current Status (MVP)

Implemented:
- `build` command
- Single-entry or recursive multi-page render
- Minimal frontmatter parsing + required field warnings
- Markdown → HTML via `markdown` crate
- Basic parent/child graph from `contents`
- Simple templating (inline HTML scaffolding)
- Attachment copying (images, documents, etc. referenced in Markdown)
- Optional JSON export of the model (`--emit-json`)
- Opt-in inclusion of non-public entries (`--include-nonpublic`)

Not yet implemented (planned):
- Rich validation (schema-based)
- Internal link rewriting inside body content
- Search index
- Watch mode / live rebuild
- Redaction (e.g., remove health/location metadata)
- Slug collision disambiguation
- Theming system / pluggable templates
- Extension dashboards (health, location, weather visualizations)
- WASM core library export
- Versioned spec support
- Proper logging framework
- Comprehensive tests

---

## Installation

(After repository bootstrap)

Clone and build:
    git clone https://github.com/your-org/diaryx-cli
    cd diaryx-cli
    cargo build --release
    # Binary at target/release/diaryx

(You can also run with `cargo run -- <args>` for development.)

Rust Version:
- Targeting stable Rust (Edition 2021).
- No nightly-only features in MVP.

---

## Usage

Basic (single non-index file):
    diaryx build --input ./notes/Entry.md --output ./site

Recursive (root index):
    diaryx build --input ./spec/Diaryx\ Writing\ Specification.md --output ./site

Include non-public documents:
    diaryx build --input ./spec/Diaryx\ Writing\ Specification.md --output ./site --include-nonpublic

Emit intermediate JSON model:
    diaryx build --input ./spec/Diaryx\ Writing\ Specification.md --output ./site --emit-json

Verbose logging (future: more detail):
    diaryx build --input ./Entry.md --output ./site --verbose

Flags summary (current):
- `--input <file>`: REQUIRED. Path to a single Diaryx Markdown file (entry point).
- `--output <dir>`: Output directory (default: `./site`).
- `--include-nonpublic`: Opt-in to include files whose `visibility` does not contain `public`.
- `--emit-json`: Write `diaryx-data.json` (doc metadata model).
- `--verbose`: Emit extra warnings / progress info (rudimentary for now).

Exit codes:
- 0: success
- Non-zero: unrecoverable parse or IO error (missing file, unreadable YAML, etc.)

---

## Output Structure

For a root index traversal:
    site/
      index.html                 (root index file)
      <slug>.html                (child pages)
      css/style.css
      attachments/...            (mirrored relative paths)
      diaryx-data.json (optional)

For a single non-root file:
    site/
      index.html
      css/style.css
      attachments/... (if any)
      diaryx-data.json (optional)

---

## Slug Generation

Slugs are derived from the `title`:
- Lowercased
- Non-alphanumeric sequences → single `-`
- Leading/trailing `-` trimmed

Potential improvements:
- Detect collisions and append suffixes
- Option for stable hash-based slugging

---

## Attachment Handling

Rules:
- Any relative link or image (e.g. `![Alt](images/photo.jpg)` or `[Doc](docs/file.pdf)`) that resolves to a local file and is NOT one of the discovered Diaryx pages is treated as an attachment.
- Copied into `attachments/...` preserving relative layout relative to the directory of the entry file’s parent (current heuristic).
- External URLs (`http://`, `https://`, `mailto:`, `data:`) are ignored.

Future:
- Configurable destination directory
- MIME type filtering
- Size limit / skip large attachments by default

---

## Security / Privacy Considerations

Default exclusion of non-public items helps avoid accidental publishing.
When you use `--include-nonpublic`, you accept responsibility for ensuring sensitive content is safe to publish.
Future redaction features will allow selective removal (e.g., health metrics, coordinates).

---

## Internal Architecture (MVP)

Pipeline (build command):
1. Read entry file (YAML frontmatter + Markdown body).
2. If root index: recursively queue `contents` targets; continue until exhaustion or cycle.
3. Parse + store docs in memory (struct `DiaryxDoc`).
4. Derive parent/child relationships.
5. Render each doc’s Markdown to HTML (`markdown` crate).
6. Copy attachments.
7. Emit HTML pages + optional JSON model.

Core modules (planned layout):
- `build/options.rs` (CLI -> internal)
- `build/frontmatter.rs` (split)
- `build/collect.rs` (recursive gather)
- `build/parse.rs` (structs + minimal transforms)
- `build/graph.rs` (parent/child linking)
- `build/attachments.rs` (asset copying)
- `build/render.rs` (Markdown → HTML)
- `build/templates.rs` (inline HTML assembly)
- `build/write.rs` (filesystem emission)

---

## JSON Model (Preview)

`diaryx-data.json` (structure may evolve):
    {
      "docs": [
        {
          "id": "diaryx-writing-specification",
          "title": "Diaryx Writing Specification",
            "visibility": ["public"],
            "tags": [],
            "children": ["diaryx-optional-properties-device-info", "..."],
            "parents": []
        }
      ]
    }

Future additions:
- Created/updated timestamps
- Path info
- Extension property presence
- Graph summary
- Build metadata (timestamp, version)

---

## Roadmap (Proposed)

Short-term:
- Internal link rewriting (body links pointing to other Diaryx files → correct `.html`).
- Proper timestamp parsing + validation.
- Enriched metadata panel (created/updated/timezone).
- Basic search index (client-side JSON).

Medium:
- `validate` subcommand (schema + rules).
- `schema` subcommand (JSON Schema export).
- Watch mode (`--watch`) incremental rebuilds.
- Redaction flags (`--redact health,location`).
- Theming system (pluggable templates / HTML partials).
- Plugin hooks (extension property visualizations).

Long-term:
- WASM library for browser-based validation + rendering.
- ePub / PDF export.
- Versioned site builds (multi-version spec hosting).
- Integration tests (golden output snapshots).
- Performance optimizations (parallel render, caching).

---

## Contributing

1. Fork & clone.
2. Create a feature branch.
3. Run tests (once they exist).
4. Submit PR with clear description / rationale.

Coding guidelines:
- Favor explicit, small modules.
- Keep public APIs minimal until stable.
- Add doc comments (`///`) for externally visible types/functions.

---

## License

The CLI source code (unless stated otherwise) is released under the license declared in `Cargo.toml` (currently MIT — may revisit for code vs. spec text separation).
The Diaryx specification text it processes may have its own license declared via the `copying` field; the tool does not alter or remove those notices—ensure compliance when publishing.

---

## FAQ

Q: Why default to excluding non-public content?
A: To prevent accidental leaking of private journaling content. Inclusion becomes a deliberate action via `--include-nonpublic`.

Q: Does it validate the full Diaryx spec now?
A: Not yet. Only presence of basic required fields triggers warnings. A future `validate` subcommand + JSON Schema enforcement will tighten this.

Q: Why start with a simplistic template instead of a templating engine?
A: Fast bootstrap; once the data model stabilizes we can swap to a template engine (e.g. MiniJinja) without breaking user workflows.

---

## Support / Feedback

Open an issue with:
- Exact command run
- Expected vs actual behavior
- Sample frontmatter/body (redact private data)
- `--verbose` output if relevant

---

Happy writing!
