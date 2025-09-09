use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use diaryx_core::{CoreBuildOptions, PageOutput, build_site};
use serde_json::json;

use crate::BuildOptions;

/// Adapter build module
///
/// This module bridges the CLI-specific concerns (real filesystem, output directory layout,
/// CSS emission, JSON model emission, verbose / strict handling) to the pure core library
/// (`diaryx-core`), which performs parsing, traversal, link rewriting, and HTML body rendering.
///
/// High-level steps:
/// 1. Invoke `diaryx_core::build_site` with a filesystem shim.
/// 2. Adjust internal links for directory layout differences (multi-page nested vs flat).
/// 3. Wrap raw HTML bodies in a full document shell (metadata rows minimal for now).
/// 4. Emit pages to disk (respecting flat vs nested).
/// 5. Optionally emit a JSON model.
/// 6. Enforce `--strict` (treat warnings as errors).
/// 7. Print a completion line (always) including warning count.
pub fn run_build(opts: BuildOptions) -> Result<()> {
    let real_fs = RealFs;
    let entry_str = opts
        .input
        .to_str()
        .ok_or_else(|| anyhow!("Non-UTF8 entry path"))?
        .to_string();

    let core_opts = CoreBuildOptions {
        include_nonpublic: opts.include_nonpublic,
        flat: opts.flat,
        strict: opts.strict,
        rewrite_links: true,
    };

    if opts.verbose {
        eprintln!("[build] core build start");
    }
    let mut artifacts =
        build_site(&entry_str, core_opts, &real_fs).with_context(|| "Core build failed")?;

    // (Removed adjust_links_for_nested_layout: core now emits layout-aware links)

    // Site emission
    if opts.output.exists() {
        fs::remove_dir_all(&opts.output)
            .with_context(|| format!("Failed removing {}", opts.output.display()))?;
    }
    fs::create_dir_all(&opts.output)
        .with_context(|| format!("Failed creating {}", opts.output.display()))?;
    fs::create_dir_all(opts.output.join("css"))?;

    fs::write(opts.output.join("css/style.css"), DEFAULT_CSS.as_bytes())
        .context("Writing CSS failed")?;

    // Page writing
    if artifacts.multi_page {
        if opts.flat {
            // Root index becomes index.html, others <slug>.html
            for page in &artifacts.pages {
                let html_doc = wrap_full_html(page, artifacts.multi_page, opts.flat);
                let out_name = &page.file_name; // already computed in core
                fs::write(opts.output.join(out_name), html_doc)
                    .with_context(|| format!("Failed writing page {}", out_name))?;
            }
        } else {
            // Nested: root index at output/index.html, others under /pages
            let pages_dir = opts.output.join("pages");
            fs::create_dir_all(&pages_dir)
                .with_context(|| format!("Failed creating {}", pages_dir.display()))?;
            for page in &artifacts.pages {
                let html_doc = wrap_full_html(page, artifacts.multi_page, opts.flat);
                if page.is_root_index {
                    fs::write(opts.output.join("index.html"), html_doc)
                        .context("Failed writing root index.html")?;
                } else {
                    let fname = page
                        .file_name
                        .strip_prefix("index.")
                        .map(|_| format!("{}.html", page.id))
                        .unwrap_or_else(|| page.file_name.clone());
                    fs::write(pages_dir.join(fname), html_doc)
                        .with_context(|| "Failed writing nested page")?;
                }
            }
        }
    } else {
        // Single page => only one page artifact, designated index.html
        let page = artifacts.pages.first().unwrap();
        let html_doc = wrap_full_html(page, false, opts.flat);
        fs::write(opts.output.join("index.html"), html_doc)
            .context("Failed writing single index.html")?;
    }

    // Optional JSON model
    if opts.emit_json {
        let pages_json: Vec<_> = artifacts
            .pages
            .iter()
            .map(|p| {
                json!({
                  "id": p.id,
                  "title": p.title,
                  "file_name": p.file_name,
                  "is_root_index": p.is_root_index,
                  "is_index": p.is_index,
                  "parents": p.parents,
                  "children": p.children,
                  "warnings": p.warnings,
                  "frontmatter": p.frontmatter, // raw YAML value -> serialized JSON
                })
            })
            .collect();

        let model = json!({
          "multi_page": artifacts.multi_page,
          "root_slug": artifacts.root_slug,
          "pages": pages_json,
          "warnings": artifacts.warnings,
        });
        fs::write(
            opts.output.join("diaryx-data.json"),
            serde_json::to_string_pretty(&model).unwrap(),
        )
        .context("Failed writing diaryx-data.json")?;
    }

    let warning_count = artifacts.warnings.len();

    if opts.verbose {
        if warning_count > 0 {
            eprintln!(
                "[warn] {} warning(s) encountered during build:",
                warning_count
            );
            for w in &artifacts.warnings {
                eprintln!("  - {}", w);
            }
        } else {
            eprintln!("[build] no warnings");
        }
    }

    if opts.strict && warning_count > 0 {
        // Fail after emitting artifacts (mirrors prior behavior; change policy if you prefer pre-emission fail)
        return Err(anyhow!(
            "Strict mode: build failed due to {} warning(s)",
            warning_count
        ));
    }

    // Always print final completion line with warning count
    println!(
        "[diaryx] build completed -> {} (warnings: {})",
        opts.output.display(),
        warning_count
    );

    Ok(())
}

/// Real filesystem implementation of the core FileProvider.
struct RealFs;

impl diaryx_core::FileProvider for RealFs {
    fn read_to_string(&self, path: &str) -> Result<String> {
        Ok(fs::read_to_string(path).with_context(|| format!("Failed to read {}", path))?)
    }
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }
    fn is_file(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }
    fn join(&self, parent: &str, rel: &str) -> String {
        if parent.is_empty() {
            PathBuf::from(rel).to_string_lossy().to_string()
        } else {
            let mut p = PathBuf::from(parent);
            p.push(rel);
            p.to_string_lossy().to_string()
        }
    }
    fn extension_lowercase(&self, path: &str) -> Option<String> {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
    }
    fn parent(&self, path: &str) -> Option<String> {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
    }
    fn file_name(&self, path: &str) -> Option<String> {
        Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
    }
}

/// Adjust internal links produced by core rewriting to account for a nested layout (pages/).
/// Core rewrites as if everything is in one directory:
/// - root index: index.html
/// - other pages: <slug>.html
/// When we move non-root pages into pages/<slug>.html we must fix:
/// - In root page: links to <slug>.html -> pages/<slug>.html
/// - In non-root page content: links to index.html -> ../index.html

/// Wrap the core-rendered HTML content inside a full HTML document + metadata header.
/// This is intentionally minimal; you can later replicate the full rich metadata grid.
fn wrap_full_html(page: &PageOutput, multi_page: bool, flat: bool) -> String {
    // Desired minimal layout:
    // 1. Metadata (already HTML from core: page.metadata_html, includes converted markdown links & contents links)
    // 2. Line break (semantic separation via <hr /> or simple margin in CSS)
    // 3. Content body
    //
    // Removed: Title <h1>, relationship blocks (Part Of / Contents duplicates) and duplicate contents list.
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\" />");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\" />");
    out.push_str("<title>");
    html_esc_append(&mut out, &page.title);
    out.push_str("</title><link rel=\"stylesheet\" href=\"");
    if multi_page && !flat && !page.is_root_index {
        out.push_str("../css/style.css");
    } else {
        out.push_str("css/style.css");
    }
    out.push_str("\" /></head><body>");
    out.push_str("<header class=\"page-header\">");
    out.push_str(&page.metadata_html);
    // Do not include h1 title before content. title is already in metadata, and many markdown files include a title in the first h1 block.
    //out.push_str("<h1>");
    //html_esc_append(&mut out, &page.title);
    //out.push_str("</h1>");
    out.push_str("</header><main class=\"content\">");
    out.push_str(&page.html);
    out.push_str("</main></body></html>");
    out
}

fn html_esc_append(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

// (Removed: render_frontmatter_list – now using core metadata_html)

// (Removed: convert_markdown_links_inline & pretty_timestamp – handled in core)

/// Inline YAML value rendering (previous implementation).
// (Removed: yaml_value_inline – metadata formatting done in core)

/// Escape text nodes.
// (Removed: html_escape_text / html_escape_attr – no longer needed in CLI layer)

/// Convert a YAML value into a concise single-line string.

// Helper adapter for closure capturing
fn out_by_ref<'a>(s: &'a mut String) -> &'a mut String {
    s
}

// Simplified CSS (subset of earlier styling). Extend as needed.
const DEFAULT_CSS: &str = r#"
:root {
  --bg: #ffffff;
  --fg: #1d1f21;
  --accent: #0a6d3d;
  --border: #e2e2e2;
  --code-bg: #f5f5f5;
  color-scheme: light;
  font-size: 16px;
}

* { box-sizing: border-box; }

body {
  margin: 0;
  font-family: system-ui,-apple-system,Segoe UI,Roboto,Ubuntu,sans-serif;
  background: var(--bg);
  color: var(--fg);
  line-height: 1.55;
  padding: 0 1.2rem 2.5rem;
  max-width: 64rem;
  margin-left: auto;
  margin-right: auto;
}

a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }

header.page-header {
  border-bottom: 1px solid var(--border);
  padding: 1.4rem 0 .75rem;
  margin-bottom: 1rem;
}

h1 {
  font-size: 1.9rem;
  margin: 0 0 .8rem;
  line-height: 1.2;
}



.content img {
  max-width: 100%;
  height: auto;
}

code, pre {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  background: var(--code-bg);
  font-size: .9rem;
}

pre {
  padding: .75rem;
  overflow: auto;
  border-radius: 4px;
}

table {
  border-collapse: collapse;
  margin: 1rem 0;
  width: 100%;
  font-size: .9rem;
}

table th, table td {
  border: 1px solid var(--border);
  padding: .4rem .6rem;
  text-align: left;
  vertical-align: top;
}

blockquote {
  margin: 1rem 0;
  padding: .5rem 1rem;
  border-left: 4px solid var(--accent);
  background: #f6fcf9;
}

hr {
  border: 0;
  border-top: 1px solid var(--border);
  margin: 2rem 0;
}
"#;
