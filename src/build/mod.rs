/*!
 * build/mod.rs
 *
 * Implementation of the `build` subsystem for diaryx-cli.
 *
 * Responsibilities:
 * - Load a single Diaryx Markdown entry file (the "entry").
 * - If the entry has `this_file_is_root_index: true`, recursively traverse
 *   `contents` lists of index files (including nested ones) to build a multi-page site.
 * - Otherwise, produce a single-page site for just that file.
 * - Exclude non-public files by default; include them only if `include_nonpublic` is set.
 * - Copy local attachments (images / other files referenced in Markdown) that are not themselves
 *   Diaryx pages.
 * - Emit HTML pages + optional JSON model (if `emit_json`).
 *
 * Design notes:
 * - Minimal validation: we only warn about missing required properties.
 * - Internal Markdown links pointing to other Diaryx pages are rewritten to their generated HTML.
 * - Slug collisions are not yet disambiguated (TODO).
 * - Supports `--flat` mode to avoid creating a `pages/` subdirectory in multi-page builds.
 * - Displays unrecognized frontmatter keys in an “Additional Metadata” section.
 */

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};

static WARNING_COUNT: AtomicUsize = AtomicUsize::new(0);

use crate::BuildOptions;

// --------------------------- Public Entry Point ---------------------------------------------

pub fn run_build(opts: BuildOptions) -> Result<()> {
    let entry = opts.input.clone();
    let root_dir = entry
        .parent()
        .ok_or_else(|| anyhow!("Entry file has no parent directory"))?
        .to_path_buf();

    if opts.verbose {
        eprintln!("[build] entry: {}", entry.display());
    }

    // 1. Collect documents (single or recursive)
    let mut docs = collect_documents(&opts, &entry, &root_dir)?;

    // 2. Build parent/child graph (based on contents arrays)
    link_graph(&mut docs);

    // Register docs globally for alias resolution & later link rewriting
    set_global_docs(&docs);

    // 3. (new) Rewrite internal markdown links in each document's rendered HTML
    rewrite_internal_links(&mut docs, &opts);

    // 4. Copy attachments
    copy_attachments(&opts, &docs, &root_dir)?;

    // 5. Emit site
    emit_site(&opts, &docs)?;

    // Strict mode: fail if any warnings were recorded
    if opts.strict {
        let wc = WARNING_COUNT.load(Ordering::SeqCst);
        if wc > 0 {
            return Err(anyhow!(
                "Strict mode: build failed due to {} warning(s)",
                wc
            ));
        }
    }

    // Always print completion line (stdout) including warning count, regardless of verbose.
    let wc = WARNING_COUNT.load(Ordering::SeqCst);
    println!(
        "[diaryx] build completed -> {} (warnings: {})",
        opts.output.display(),
        wc
    );

    Ok(())
}

// --------------------------- Data Structures -------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct FrontmatterRaw {
    title: Option<String>,
    author: Option<serde_yaml::Value>,
    created: Option<String>,
    updated: Option<String>,
    visibility: Option<serde_yaml::Value>,
    format: Option<serde_yaml::Value>,
    contents: Option<Vec<String>>,
    part_of: Option<serde_yaml::Value>,
    version: Option<String>,
    copying: Option<String>,
    checksums: Option<String>,
    banner: Option<String>,
    language: Option<String>,
    tags: Option<Vec<String>>,
    aliases: Option<Vec<String>>,
    this_file_is_root_index: Option<bool>,
    starred: Option<bool>,
    pinned: Option<bool>,
    // Extension placeholder fields (kept for forward compatibility):
    // mood, coordinates, weather, etc. (Not used in MVP rendering)
}

#[derive(Debug, Clone)]
struct DiaryxDoc {
    id: String,
    abs_path: PathBuf,
    #[allow(dead_code)]
    rel_dir: PathBuf,
    title: String,
    visibility: Vec<String>,
    tags: Vec<String>,
    aliases: Vec<String>,
    is_root_index: bool,
    is_index: bool,
    contents_raw: Vec<String>,
    children: Vec<String>,
    parents: Vec<String>,
    raw_part_of: Vec<String>,
    html: String,
    body_md: String,
    #[allow(dead_code)]
    frontmatter_raw: serde_yaml::Value,
    warnings: Vec<String>,
}

impl DiaryxDoc {
    fn is_public(&self) -> bool {
        self.visibility.iter().any(|v| v == "public")
    }
}

// --------------------------- Collection & Parsing --------------------------------------------

fn collect_documents(
    opts: &BuildOptions,
    entry: &PathBuf,
    root_dir: &PathBuf,
) -> Result<Vec<DiaryxDoc>> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    let mut visited: HashMap<PathBuf, DiaryxDoc> = HashMap::new();
    let mut order: Vec<PathBuf> = Vec::new();

    queue.push_back(entry.clone());

    while let Some(path) = queue.pop_front() {
        if visited.contains_key(&path) {
            continue;
        }

        // Skip non-Markdown files early (prevents attempting to UTF-8 decode images/binaries)
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| !e.eq_ignore_ascii_case("md"))
            .unwrap_or(true)
        {
            if opts.verbose {
                eprintln!(
                    "[info] Skipping non-markdown file in traversal: {}",
                    path.display()
                );
            }
            continue;
        }

        // Read file as UTF-8; if it fails (binary or invalid encoding), skip with a warning instead of erroring out.
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) => {
                if opts.verbose {
                    eprintln!(
                        "[warn] Skipping file (non-UTF8 or unreadable): {} ({err})",
                        path.display()
                    );
                }
                WARNING_COUNT.fetch_add(1, Ordering::SeqCst);
                continue;
            }
        };

        let split = split_frontmatter(&raw)
            .with_context(|| format!("Failed splitting frontmatter for {}", path.display()))?;

        let (fm_value, fm_struct) = parse_frontmatter(&split.frontmatter_yaml)?;
        let mut warnings = Vec::new();

        check_required(&fm_struct, &mut warnings);

        let title = fm_struct
            .title
            .clone()
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().to_string());
        let slug = slugify(&title);

        let html = render_markdown(&split.body_md)
            .with_context(|| format!("Markdown render failure: {}", path.display()))?;

        let rel_dir = path
            .parent()
            .map(|p| p.strip_prefix(root_dir).unwrap_or(Path::new("")))
            .unwrap_or(Path::new(""))
            .to_path_buf();

        // Normalize visibility and contents early
        let visibility_norm = normalize_string_or_list(&fm_struct.visibility);
        let contents_norm = normalize_contents(&fm_struct.contents);
        let root_flag = fm_struct.this_file_is_root_index.unwrap_or(false);

        let doc = DiaryxDoc {
            id: slug,
            abs_path: path.clone(),
            rel_dir,
            title,
            visibility: visibility_norm,
            tags: fm_struct.tags.unwrap_or_default(),
            aliases: fm_struct.aliases.unwrap_or_default(),
            is_root_index: root_flag,
            is_index: !contents_norm.is_empty(),
            contents_raw: contents_norm,
            children: vec![],
            parents: vec![],
            raw_part_of: parse_part_of(&fm_struct.part_of),
            html,
            body_md: split.body_md,
            frontmatter_raw: fm_value,
            warnings,
        };

        if !doc.warnings.is_empty() {
            if opts.verbose {
                for w in &doc.warnings {
                    eprintln!("[warn] {}: {}", path.display(), w);
                }
            }
            WARNING_COUNT.fetch_add(doc.warnings.len(), Ordering::SeqCst);
        }

        let is_index = doc.is_index;
        let contents_links = doc.contents_raw.clone();
        let is_root_index = doc.is_root_index;

        visited.insert(path.clone(), doc);
        order.push(path.clone());

        // Only perform recursive traversal if entry is root index chain OR doc is part of that recursion.
        // If the *entry* file itself is not root index we skip recursion entirely.
        if is_index && (is_root_index || entry_metadata_had_root(entry, &visited)) {
            let parent_dir = path.parent().unwrap();
            for raw_link in contents_links {
                if let Some(resolved) = resolve_contents_link(&raw_link, parent_dir) {
                    if !resolved.exists() {
                        if opts.verbose {
                            eprintln!("[warn] contents target not found: {}", resolved.display());
                        }
                        WARNING_COUNT.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    if resolved.is_file() {
                        queue.push_back(resolved);
                    }
                }
            }
        }
    }

    // Preserve traversal order in final vector
    let mut out = Vec::with_capacity(visited.len());
    for p in order {
        if let Some(d) = visited.remove(&p) {
            out.push(d);
        }
    }
    Ok(out)
}

fn entry_metadata_had_root(entry: &Path, visited: &HashMap<PathBuf, DiaryxDoc>) -> bool {
    visited.get(entry).map(|d| d.is_root_index).unwrap_or(false)
}

struct SplitFrontmatter {
    frontmatter_yaml: Option<String>,
    body_md: String,
}

/// Split YAML frontmatter if present at top of file.
/// Returns body (without the frontmatter section).
fn split_frontmatter(raw: &str) -> Result<SplitFrontmatter> {
    let mut lines_iter = raw.lines();
    if lines_iter.next() != Some("---") {
        return Ok(SplitFrontmatter {
            frontmatter_yaml: None,
            body_md: raw.to_string(),
        });
    }
    let mut yaml = Vec::new();
    let mut body = Vec::new();
    let mut in_yaml = true;
    for line in raw.lines().skip(1) {
        if in_yaml {
            if line == "---" {
                in_yaml = false;
                continue;
            }
            yaml.push(line);
        } else {
            body.push(line);
        }
    }
    if in_yaml {
        return Err(anyhow!("Unterminated frontmatter block"));
    }
    Ok(SplitFrontmatter {
        frontmatter_yaml: Some(yaml.join("\n")),
        body_md: body.join("\n"),
    })
}

fn parse_frontmatter(yaml_opt: &Option<String>) -> Result<(serde_yaml::Value, FrontmatterRaw)> {
    if let Some(yaml) = yaml_opt {
        if yaml.trim().is_empty() {
            return Ok((serde_yaml::Value::Null, FrontmatterRaw::default()));
        }
        let value: serde_yaml::Value =
            serde_yaml::from_str(yaml).context("Invalid YAML frontmatter")?;
        let fm_struct: FrontmatterRaw = serde_yaml::from_value(value.clone()).unwrap_or_default();
        Ok((value, fm_struct))
    } else {
        Ok((serde_yaml::Value::Null, FrontmatterRaw::default()))
    }
}

fn check_required(fm: &FrontmatterRaw, warnings: &mut Vec<String>) {
    if fm.title.is_none() {
        warnings.push("Missing required field: title".into());
    }
    if fm.author.is_none() {
        warnings.push("Missing required field: author".into());
    }
    if fm.created.is_none() {
        warnings.push("Missing required field: created".into());
    }
    if fm.updated.is_none() {
        warnings.push("Missing required field: updated".into());
    }
    if fm.visibility.is_none() {
        warnings.push("Missing required field: visibility".into());
    }
    if fm.format.is_none() {
        warnings.push("Missing required field: format".into());
    }
}

/// Parse part_of field (string or sequence) into a Vec<String> of raw link entries.
fn parse_part_of(value: &Option<serde_yaml::Value>) -> Vec<String> {
    use serde_yaml::Value;
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => vec![],
    }
}

/// Extract (alias, target_path) from a markdown link raw snippet.
/// Returns ("", "") if not matched.
fn extract_alias_and_target(raw: &str) -> (String, String) {
    static LINK_RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"^\[([^\]]*)]\(\s*<?([^)>]+)>?\s*\)$").unwrap());
    if let Some(caps) = LINK_RE.captures(raw.trim()) {
        let alias = caps
            .get(1)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        let target = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        return (alias, target);
    }
    (String::new(), String::new())
}

fn normalize_string_or_list(v: &Option<serde_yaml::Value>) -> Vec<String> {
    match v {
        Some(serde_yaml::Value::String(s)) => vec![s.trim().to_string()],
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

/// Normalize contents: accept Option<Vec<String>> and strip empties / trim.
fn normalize_contents(c: &Option<Vec<String>>) -> Vec<String> {
    match c {
        Some(list) => list
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec![],
    }
}

// --------------------------- Graph Linking ---------------------------------------------------

fn link_graph(docs: &mut [DiaryxDoc]) {
    // Map absolute path -> index in docs for potential lookups
    // (Currently not needed for path resolution because we already decided which docs to load)
    let mut slug_index: HashMap<String, usize> = HashMap::new();
    for (i, d) in docs.iter().enumerate() {
        slug_index.insert(d.id.clone(), i);
    }

    // Build parent/child relationships using contents_raw slugs or filename resolution
    // Re-resolve each contents entry relative to parent directory.
    // For simpler MVP, we attempt to match by file name or by slug if collision.
    // (We stored only raw link strings; now we must parse the target path again.)
    for i in 0..docs.len() {
        if !docs[i].is_index {
            continue;
        }
        let parent_dir = docs[i].abs_path.parent().unwrap();
        let entries = docs[i].contents_raw.clone();
        for raw_link in entries {
            if let Some(abs) = resolve_contents_link(&raw_link, parent_dir) {
                // Find matching doc by absolute path -> slug
                if let Some(child_idx) = docs
                    .iter()
                    .position(|d| d.abs_path == abs.canonicalize().unwrap_or(abs.clone()))
                {
                    let child_slug = docs[child_idx].id.clone();
                    if !docs[i].children.contains(&child_slug) {
                        docs[i].children.push(child_slug.clone());
                    }
                    if !docs[child_idx].parents.contains(&docs[i].id) {
                        docs[child_idx].parents.push(docs[i].id.clone());
                    }
                }
            }
        }
    }
}

/// Resolve a contents link line of the form `[Alias](path.md)` / `[Alias](<File Name.md>)`.
fn resolve_contents_link(raw: &str, parent_dir: &Path) -> Option<PathBuf> {
    // Regex capturing target inside link parentheses (angle brackets optional).
    static LINK_RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"\[[^\]]*]\(\s*<?([^)>]+)>?\s*\)").unwrap());

    let caps = LINK_RE.captures(raw)?;
    let target = caps.get(1)?.as_str().trim();
    // First attempt: as-is
    let first = parent_dir.join(target);
    if first.exists() {
        return Some(first);
    }
    // If missing extension, try appending .md
    if std::path::Path::new(target).extension().is_none() {
        let with_md = parent_dir.join(format!("{target}.md"));
        if with_md.exists() {
            return Some(with_md);
        }
    }
    Some(first) // return original even if it doesn't exist; upstream will warn
}

// --------------------------- Markdown Rendering ----------------------------------------------

fn render_markdown(src: &str) -> Result<String> {
    let opts = markdown::Options {
        // Adjust options if needed later (tables/footnotes are auto-detected by crate version).
        ..markdown::Options::default()
    };
    markdown::to_html_with_options(src, &opts).map_err(|e| anyhow!("Markdown render error: {e}"))
}

// --------------------------- Attachment Copying ----------------------------------------------

fn copy_attachments(opts: &BuildOptions, docs: &[DiaryxDoc], root_dir: &Path) -> Result<()> {
    let attachments_dir = opts.output.join("attachments");
    if !attachments_dir.exists() {
        fs::create_dir_all(&attachments_dir)
            .with_context(|| "Failed to create attachments directory")?;
    }

    let doc_paths: HashSet<PathBuf> = docs.iter().map(|d| d.abs_path.clone()).collect();

    for doc in docs {
        let links = extract_inline_links(&doc.body_md);
        for link in links {
            if is_external(&link) {
                continue;
            }
            // Resolve relative to doc directory
            let abs = doc.abs_path.parent().unwrap().join(&link);
            if !abs.exists() || abs.is_dir() {
                continue;
            }
            // Skip if the linked file is itself a Diaryx page we rendered
            if doc_paths.contains(&abs.canonicalize().unwrap_or(abs.clone())) {
                continue;
            }

            // Mirror path relative to the root directory (entry parent). If that fails, flatten.
            let rel = match abs.strip_prefix(root_dir) {
                Ok(r) => r.to_path_buf(),
                Err(_) => PathBuf::from(abs.file_name().unwrap()),
            };
            let target = attachments_dir.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create attachment parent: {}", parent.display())
                })?;
            }
            fs::copy(&abs, &target).with_context(|| {
                format!(
                    "Failed to copy attachment {} -> {}",
                    abs.display(),
                    target.display()
                )
            })?;
        }
    }

    Ok(())
}

fn extract_inline_links(md: &str) -> Vec<String> {
    // Capture both images and standard links (excluding reference-style; basic MVP).
    // Image: ![alt](path)
    // Link: [text](path)
    // We ignore trailing title part for simplicity.
    static IMG_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"!\[[^\]]*]\(\s*<?([^)\s>]+)[^)]*>\s*\)").unwrap()
    });
    static LINK_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        // Revised pattern: optional leading '!' captured in group 1 (to distinguish images),
        // actual target path captured in group 2. This avoids unsupported lookbehind.
        Regex::new(r"(!)?\[[^\]]*]\(\s*<?([^)\s>]+)[^)]*>\s*\)").unwrap()
    });

    let mut out = Vec::new();
    for cap in IMG_RE.captures_iter(md) {
        out.push(cap[1].to_string());
    }
    for cap in LINK_RE.captures_iter(md) {
        // If group 1 is '!' it's an image (already handled by IMG_RE); skip to avoid duplicates.
        if cap.get(1).map(|m| m.as_str()) == Some("!") {
            continue;
        }
        out.push(cap[2].to_string());
    }
    out
}

fn is_external(link: &str) -> bool {
    let l = link.to_ascii_lowercase();
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("mailto:")
        || l.starts_with("data:")
        || l.starts_with("javascript:")
}

// --------------------------- Site Emission ---------------------------------------------------

fn emit_site(opts: &BuildOptions, docs_all: &[DiaryxDoc]) -> Result<()> {
    // Always include the entry file even if it is not public (visibility missing / non-public).
    // Only filter out other non-public docs unless --include_nonpublic is supplied.
    let entry_abs = &opts.input;

    let docs: Vec<&DiaryxDoc> = if opts.include_nonpublic {
        docs_all.iter().collect()
    } else {
        docs_all
            .iter()
            .filter(|d| d.is_public() || d.abs_path == *entry_abs)
            .collect()
    };

    // Warn about excluded non-public documents when not opting in.
    if !opts.include_nonpublic {
        let excluded: Vec<&DiaryxDoc> = docs_all
            .iter()
            .filter(|d| !d.is_public() && d.abs_path != *entry_abs)
            .collect();
        if !excluded.is_empty() {
            if opts.verbose {
                eprintln!(
                    "[warn] excluded {} non-public document(s). Use --include_nonpublic to include them.",
                    excluded.len()
                );
            }
            WARNING_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    if docs.is_empty() {
        return Err(anyhow!(
            "No documents to emit. The entry file was not public and no public documents were found.\n\
             Add `visibility: public` to the entry file or rerun with --include-nonpublic."
        ));
    }

    if opts.output.exists() {
        fs::remove_dir_all(&opts.output)
            .with_context(|| format!("Failed removing {}", opts.output.display()))?;
    }
    fs::create_dir_all(&opts.output)
        .with_context(|| format!("Failed creating {}", opts.output.display()))?;
    fs::create_dir_all(opts.output.join("css"))?;

    fs::write(opts.output.join("css/style.css"), DEFAULT_CSS).context("Failed writing CSS")?;

    // Determine root doc if any
    let root_idx = docs.iter().position(|d| d.is_root_index);
    let multi_page = root_idx.is_some() && docs.len() > 1;

    if multi_page && !opts.flat {
        // Standard (nested) multi-page mode: emit into pages/ plus top-level index.html
        let pages_dir = opts.output.join("pages");
        fs::create_dir_all(&pages_dir)
            .with_context(|| format!("Failed creating {}", pages_dir.display()))?;
        for doc in &docs {
            let is_root = doc.is_root_index;
            let page_html = render_document(
                doc, is_root, /*in_pages_dir*/ true, /*flat_mode*/ false,
            );
            let out_path = pages_dir.join(format!("{}.html", doc.id));
            fs::write(&out_path, &page_html)
                .with_context(|| format!("Failed writing page for {}", doc.title))?;
        }
        // Root index at top-level
        let root_doc = docs.iter().find(|d| d.is_root_index).unwrap_or(&docs[0]);
        let root_page = render_document(root_doc, true, false, false);
        fs::write(opts.output.join("index.html"), root_page)
            .context("Failed writing index.html")?;
    } else if multi_page && opts.flat {
        // Flat multi-page mode: everything at the root (no pages/ directory)
        for doc in &docs {
            let is_root = doc.is_root_index;
            let page_html = render_document(
                doc, is_root, /*in_pages_dir*/ false, /*flat_mode*/ true,
            );
            let name = if is_root {
                "index".to_string()
            } else {
                doc.id.clone()
            };
            fs::write(opts.output.join(format!("{name}.html")), page_html)
                .with_context(|| format!("Failed writing page for {}", doc.title))?;
        }
    } else {
        // Single (even if not root) -> index.html
        let html = render_document(docs[0], docs[0].is_root_index, false, false);
        fs::write(opts.output.join("index.html"), html)
            .with_context(|| format!("Failed writing page for {}", docs[0].title))?;
    }

    if opts.emit_json {
        let model = SerializableModel::from_docs(&docs);
        let json = serde_json::to_string_pretty(&model).context("Serializing JSON model failed")?;
        fs::write(opts.output.join("diaryx-data.json"), json)
            .context("Failed writing diaryx-data.json")?;
    }

    Ok(())
}

// --------------------------- Rendering -------------------------------------------------------

fn render_document(
    doc: &DiaryxDoc,
    _is_root: bool,
    in_pages_subdir: bool,
    flat_mode: bool,
) -> String {
    use regex::Regex;
    use serde_yaml::Value;

    // Helper: parse a single markdown link "[text](url)" -> (text,url)
    fn parse_md_link(s: &str) -> Option<(String, String)> {
        static RE: once_cell::sync::Lazy<Regex> =
            once_cell::sync::Lazy::new(|| Regex::new(r#"^\[([^\]]+)\]\(([^)]+)\)$"#).unwrap());
        let caps = RE.captures(s.trim())?;
        Some((
            caps.get(1)?.as_str().to_string(),
            caps.get(2)?.as_str().to_string(),
        ))
    }

    // Extract frontmatter mapping (for recognized + additional keys)
    let map_opt = match &doc.frontmatter_raw {
        Value::Mapping(m) => Some(m),
        _ => None,
    };

    // Pull & render core fields
    let title_display = doc.title.clone();
    let author_display = map_opt
        .and_then(|m| m.get(&Value::String("author".into())))
        .map(render_value)
        .unwrap_or_else(|| "—".into());

    let created_raw = map_opt
        .and_then(|m| m.get(&Value::String("created".into())))
        .map(render_value)
        .unwrap_or_else(|| "—".into());
    let updated_raw = map_opt
        .and_then(|m| m.get(&Value::String("updated".into())))
        .map(render_value)
        .unwrap_or_else(|| "—".into());

    let created_human = humanize_timestamp_friendly(&created_raw);
    let updated_human = humanize_timestamp_friendly(&updated_raw);

    let visibility_display = if doc.visibility.is_empty() {
        "—".into()
    } else {
        // prefer first token if single-value semantics; otherwise comma-join
        if doc.visibility.len() == 1 {
            esc(&doc.visibility[0])
        } else {
            esc(&doc.visibility.join(", "))
        }
    };

    // Format: convert markdown link -> anchor if possible (handle single or list)
    let format_value = map_opt
        .and_then(|m| m.get(&Value::String("format".into())))
        .map(render_value)
        .unwrap_or_else(|| "—".into());
    let format_html = match parse_md_link(&strip_quotes(&format_value)) {
        Some((text, url)) => format!("<a href=\"{}\">{}</a>", esc(&url), esc(&text)),
        None => esc(&format_value),
    };

    // Copying (license)
    let copying_html = map_opt
        .and_then(|m| m.get(&Value::String("copying".into())))
        .map(render_value)
        .map(|v| {
            if let Some((t, u)) = parse_md_link(&strip_quotes(&v)) {
                format!("<a href=\"{}\">{}</a>", esc(&u), esc(&t))
            } else if v.starts_with("http://") || v.starts_with("https://") {
                format!("<a href=\"{0}\">{0}</a>", esc(&v))
            } else {
                esc(&v)
            }
        })
        .unwrap_or_else(|| "—".into());

    // Version
    let version_html = map_opt
        .and_then(|m| m.get(&Value::String("version".into())))
        .map(render_value)
        .unwrap_or_else(|| "—".into());

    // Root index flag display
    let root_index_flag = if doc.is_root_index { "true" } else { "—" };

    // Tags (if present)
    let tags_html = if doc.tags.is_empty() {
        String::new()
    } else {
        esc(&doc.tags.join(", "))
    };

    // Build contents (collapsible) inside metadata
    let mut contents_block = String::new();
    if doc.is_index && !doc.children.is_empty() {
        let mut alias_map = std::collections::HashMap::<String, String>::new();
        if let Some(m) = map_opt {
            if let Some(raw_contents) = m.get(&Value::String("contents".into())) {
                if let Value::Sequence(seq) = raw_contents {
                    for v in seq {
                        if let Value::String(s) = v {
                            let (alias, target) = extract_alias_and_target(s);
                            if target.is_empty() {
                                continue;
                            }
                            let base = normalized_basename(&target);
                            for child_slug in &doc.children {
                                if let Some(child_doc) = lookup_doc_by_slug(child_slug) {
                                    if let Some(child_name) =
                                        child_doc.abs_path.file_name().and_then(|x| x.to_str())
                                    {
                                        if file_name_matches(child_name, &base) && !alias.is_empty()
                                        {
                                            alias_map.insert(child_slug.clone(), alias.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        contents_block.push_str(
            "<div class=\"meta-item\"><dt>Contents</dt><dd><details><summary>View</summary><pre>",
        );
        for slug in &doc.children {
            let label = alias_map
                .get(slug)
                .cloned()
                .unwrap_or_else(|| slug.to_string());
            let href = if flat_mode {
                format!("{}.html", slug)
            } else if in_pages_subdir {
                format!("{}.html", slug)
            } else {
                format!("pages/{}.html", slug)
            };
            contents_block.push_str("<a href=\"");
            contents_block.push_str(&href);
            contents_block.push_str("\">");
            contents_block.push_str(&esc(&label));
            contents_block.push_str("</a>\n");
        }
        contents_block.push_str("</pre></details></dd></div>");
    }

    // Build part_of block (parents) if any
    let mut part_of_html = String::new();
    if !doc.parents.is_empty() {
        // Map parent slug -> alias (if provided)
        let mut parent_alias_map = std::collections::HashMap::<String, String>::new();
        for raw in &doc.raw_part_of {
            let (alias, target) = extract_alias_and_target(raw);
            if target.is_empty() {
                continue;
            }
            let base = normalized_basename(&target);
            for parent_slug in &doc.parents {
                if let Some(parent_doc) = lookup_doc_by_slug(parent_slug) {
                    if let Some(parent_name) =
                        parent_doc.abs_path.file_name().and_then(|x| x.to_str())
                    {
                        if file_name_matches(parent_name, &base) && !alias.is_empty() {
                            parent_alias_map.insert(parent_slug.clone(), alias.clone());
                        }
                    }
                }
            }
        }
        // Assemble anchors
        let mut links: Vec<String> = Vec::new();
        for parent_slug in &doc.parents {
            if let Some(parent_doc) = lookup_doc_by_slug(parent_slug) {
                let label = parent_alias_map
                    .get(parent_slug)
                    .cloned()
                    .unwrap_or_else(|| parent_doc.title.clone());
                let href = if parent_doc.is_root_index {
                    if flat_mode {
                        "index.html".to_string()
                    } else if in_pages_subdir {
                        "../index.html".to_string()
                    } else {
                        "index.html".to_string()
                    }
                } else if flat_mode {
                    format!("{}.html", parent_slug)
                } else if in_pages_subdir {
                    format!("{}.html", parent_slug)
                } else {
                    format!("pages/{}.html", parent_slug)
                };
                links.push(format!("<a href=\"{}\">{}</a>", esc(&href), esc(&label)));
            }
        }
        if !links.is_empty() {
            part_of_html = links.join("<br/>");
        }
    }

    // Presence checks for optional keys
    let copying_present = map_opt
        .and_then(|m| m.get(&Value::String("copying".into())))
        .is_some();
    let version_present = map_opt
        .and_then(|m| m.get(&Value::String("version".into())))
        .is_some();
    let root_flag_present = map_opt
        .and_then(|m| m.get(&Value::String("this_file_is_root_index".into())))
        .is_some();

    // CSS prefix for stylesheet
    let css_prefix = if in_pages_subdir && !flat_mode {
        "../"
    } else {
        ""
    };

    // Begin HTML
    let mut out = String::new();
    out.push_str(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\" />\n<title>",
    );
    out.push_str(&esc(&title_display));
    out.push_str("</title>\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n<link rel=\"stylesheet\" href=\"");
    out.push_str(css_prefix);
    out.push_str("css/style.css\" />\n</head>\n<body>\n<header class=\"page-header\">\n  <div class=\"meta-block\">\n    <dl class=\"meta-grid\">");

    fn meta_row(out: &mut String, label: &str, value: &str) {
        out.push_str("<div class=\"meta-item\"><dt>");
        out.push_str(label);
        out.push_str("</dt><dd>");
        out.push_str(value);
        out.push_str("</dd></div>");
    }

    // Ordered metadata
    meta_row(&mut out, "Title", &esc(&title_display));
    meta_row(&mut out, "Author", &author_display);
    meta_row(&mut out, "Created", &created_human);
    meta_row(&mut out, "Updated", &updated_human);
    meta_row(&mut out, "Visibility", &visibility_display);
    meta_row(&mut out, "Format", &format_html);
    if !part_of_html.is_empty() {
        meta_row(&mut out, "Part Of", &part_of_html);
    }
    if !contents_block.is_empty() {
        out.push_str(&contents_block);
    }
    if copying_present {
        meta_row(&mut out, "Copying", &copying_html);
    }
    if doc.is_root_index && root_flag_present {
        meta_row(&mut out, "this_file_is_root_index", "true");
    }
    if version_present {
        meta_row(&mut out, "Version", &version_html);
    }
    if !tags_html.is_empty() {
        meta_row(&mut out, "Tags", &tags_html);
    }

    // Additional metadata (unrecognized keys) appended
    let extra_meta_html = build_additional_metadata(&doc.frontmatter_raw);
    out.push_str(&extra_meta_html);

    out.push_str("</dl>\n  </div>\n</header>\n<main class=\"content\">\n");
    // Content body (original markdown)
    out.push_str(&doc.html);
    out.push_str("\n</main>\n</body>\n</html>\n");
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_value(v: &serde_yaml::Value) -> String {
    use serde_yaml::Value;
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => strip_quotes(s).to_string(),
        Value::Sequence(seq) => {
            if seq.is_empty() {
                return "[]".into();
            }
            let parts: Vec<String> = seq.iter().map(render_value).collect();
            parts.join(", ")
        }
        Value::Mapping(map) => {
            if map.is_empty() {
                return "{}".into();
            }
            let mut inner = String::new();
            inner.push('{');
            let mut first = true;
            for (k, val) in map {
                if !first {
                    inner.push_str(", ");
                }
                first = false;
                let key_render = match k {
                    Value::String(s) => strip_quotes(s).to_string(),
                    other => format!("{:?}", other),
                };
                inner.push_str(&esc(&key_render));
                inner.push_str(": ");
                inner.push_str(&render_value(val));
            }
            inner.push('}');
            inner
        }
        Value::Tagged(tag) => {
            let tag_name = esc(&format!("{:?}", tag.tag));
            let val_str = render_value(&tag.value);
            format!("!{} {}", tag_name, val_str)
        }
    }
}
/// Produce a human-friendly timestamp (YYYY-MM-DD HH:MM) if input looks like RFC3339; otherwise return original.

/// Friendly timestamp like "August 28, 2025, 01:17pm (UTC-0)" or fallback to original.
fn strip_quotes(s: &str) -> &str {
    let trimmed = s.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}
fn humanize_timestamp_friendly(raw: &str) -> String {
    use time::{OffsetDateTime, UtcOffset};
    if raw.trim().is_empty() || !raw.contains('T') {
        return raw.to_string();
    }
    if let Ok(dt) =
        OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
    {
        // Date portion
        let date_fmt =
            time::format_description::parse("[month repr:long] [day padding:zero], [year]")
                .unwrap_or_else(|_| {
                    time::format_description::parse("[year]-[month]-[day]").unwrap()
                });
        let date_str = dt.format(&date_fmt).unwrap_or_else(|_| raw.to_string());
        // Time portion (12h)
        let hour12 = {
            let h = dt.hour();
            let h12 = match h {
                0 => 12,
                1..=12 => h,
                _ => h - 12,
            };
            h12
        };
        let minute = dt.minute();
        let ampm = if dt.hour() < 12 { "am" } else { "pm" };
        // Timezone short
        let offset: UtcOffset = dt.offset();
        let total_minutes = offset.whole_minutes();
        let hours = total_minutes / 60;
        let tz_label = match hours {
            0 => "UTC-0".to_string(),
            _ => format!("UTC{:+}", hours),
        };
        return format!(
            "{}, {:02}:{:02}{} ({})",
            date_str, hour12, minute, ampm, tz_label
        );
    }
    raw.to_string()
}
/// Build additional metadata rows for any frontmatter keys not already shown.
fn build_additional_metadata(root: &serde_yaml::Value) -> String {
    use serde_yaml::Value;
    let mut out = String::new();
    let recognized = [
        "title",
        "author",
        "created",
        "updated",
        "visibility",
        "format",
        "tags",
        "contents",
        "this_file_is_root_index",
        "part_of",
        "version",
        "copying",
        "checksums",
        "banner",
        "language",
        "aliases",
        "starred",
        "pinned",
    ];
    let recognized_set: std::collections::HashSet<&str> = recognized.into_iter().collect();
    if let Value::Mapping(map) = root {
        let mut extra_rows = Vec::new();
        for (k, v) in map {
            if let Value::String(key) = k {
                if recognized_set.contains(key.as_str()) {
                    continue;
                }
                extra_rows.push((key, render_value(v)));
            } else {
                // Non-string key – skip
            }
        }
        if !extra_rows.is_empty() {
            // Insert heading cell
            out.push_str("<div class=\"meta-section-separator\"></div>");
            for (k, val) in extra_rows {
                out.push_str("<div class=\"meta-item\"><dt>");
                out.push_str(&esc(k));
                out.push_str("</dt><dd>");
                out.push_str(&val);
                out.push_str("</dd></div>");
            }
        }
    }
    out
}

// --------------------------- JSON Model ------------------------------------------------------

#[derive(serde::Serialize)]
struct SerializableDoc<'a> {
    id: &'a str,
    title: &'a str,
    visibility: &'a [String],
    tags: &'a [String],
    aliases: &'a [String],
    parents: &'a [String],
    children: &'a [String],
    is_root_index: bool,
    is_index: bool,
}

#[derive(serde::Serialize)]
struct SerializableModel<'a> {
    docs: Vec<SerializableDoc<'a>>,
}

impl<'a> SerializableModel<'a> {
    fn from_docs(docs: &[&'a DiaryxDoc]) -> Self {
        let mut out = Vec::with_capacity(docs.len());
        for d in docs {
            out.push(SerializableDoc {
                id: &d.id,
                title: &d.title,
                visibility: &d.visibility,
                tags: &d.tags,
                aliases: &d.aliases,
                parents: &d.parents,
                children: &d.children,
                is_root_index: d.is_root_index,
                is_index: d.is_index,
            });
        }
        Self { docs: out }
    }
}

// --------------------------- Utilities -------------------------------------------------------

fn slugify(s: &str) -> String {
    static NON_ALNUM: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let lower = s.to_ascii_lowercase();
    let replaced = NON_ALNUM.replace_all(&lower, "-");
    replaced.trim_matches('-').to_string()
}

/*
 * Rewrites internal markdown links (those pointing to .md files) in each document's rendered HTML
 * to point at their generated HTML equivalents, respecting multi-page / flat modes and the
 * position (root vs page) of the current document.
 *
 * Strategy (lightweight / regex-based):
 * 1. Build a mapping from original source file basename -> (slug, is_root_index).
 * 2. Detect multi_page (root index with more than one doc).
 * 3. For each doc.html, regex match href="...md" (case-insensitive). Extract the target basename.
 * 4. If basename matches a known doc, compute new relative href:
 *      - multi_page && !flat:
 *          current=root, target=root        => index.html
 *          current=root, target=other       => pages/<slug>.html
 *          current=page, target=root        => ../index.html
 *          current=page, target=other       => <slug>.html
 *      - flat OR !multi_page:
 *          target=root  => index.html
 *          target=other => <slug>.html
 * 5. Replace only the URL inside the href attribute, preserving any fragment/query (rare for .md).
 *
 * Limitations:
 * - Does not currently percent-decode filenames (expects href uses original basename or encoded spaces).
 * - If two different basenames exist (name collision), the first discovered wins (TODO: collision handling).
 */
fn rewrite_internal_links(docs: &mut [DiaryxDoc], opts: &BuildOptions) {
    use regex::Regex;
    if docs.is_empty() {
        return;
    }

    set_global_docs(docs);
    let root_idx = docs.iter().position(|d| d.is_root_index);
    let multi_page = root_idx.is_some() && docs.len() > 1;
    let flat_mode = opts.flat;

    // Map basename (exact as on disk) to (slug, is_root)
    let mut by_basename: std::collections::HashMap<String, (String, bool)> =
        std::collections::HashMap::new();
    for d in docs.iter() {
        if let Some(name) = d.abs_path.file_name().and_then(|s| s.to_str()) {
            by_basename
                .entry(name.to_string())
                .or_insert((d.id.clone(), d.is_root_index));
        }
    }

    // Case-insensitive .md pattern
    static HREF_MD: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r#"href="([^"]+?\.(?i:md)(?:[?#][^"]*)?)""#).unwrap()
    });

    // Track unmatched for optional diagnostics
    let mut unmatched: Vec<String> = Vec::new();

    for doc in docs.iter_mut() {
        if !doc.html.to_ascii_lowercase().contains(".md") {
            continue;
        }
        let mut new_html = String::with_capacity(doc.html.len());
        let mut last_end = 0;
        for cap in HREF_MD.captures_iter(&doc.html) {
            let m = cap.get(0).unwrap();
            let url_full = cap.get(1).unwrap().as_str();

            // Split off any ? or #
            let core = url_full.split(&['?', '#'][..]).next().unwrap_or(url_full);
            let basename_raw = core.rsplit('/').next().unwrap_or(core);

            // Normalize for lookup: decode %20 -> space
            let basename_norm = basename_raw.replace("%20", " ");

            // We only stored canonical basenames (with original case). Try exact first,
            // then try a fallback insensitive match if needed.
            let mut mapping = by_basename.get(&basename_norm);

            if mapping.is_none() {
                // Try case-insensitive fallback
                if let Some((k, v)) = by_basename
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&basename_norm))
                {
                    mapping = Some(v);
                }
            }

            if let Some((target_slug, target_is_root)) = mapping {
                // Build new href
                let new_href = if multi_page && !flat_mode {
                    let current_is_root = doc.is_root_index;
                    if current_is_root {
                        if *target_is_root {
                            "index.html".to_string()
                        } else {
                            format!("pages/{}.html", target_slug)
                        }
                    } else {
                        if *target_is_root {
                            "../index.html".to_string()
                        } else {
                            format!("{}.html", target_slug)
                        }
                    }
                } else {
                    if *target_is_root {
                        "index.html".to_string()
                    } else {
                        format!("{}.html", target_slug)
                    }
                };

                // Preserve suffix
                let mut suffix = "";
                if let Some(idx) = url_full.find(|c| c == '?' || c == '#') {
                    suffix = &url_full[idx..];
                }

                new_html.push_str(&doc.html[last_end..m.start()]);
                new_html.push_str("href=\"");
                new_html.push_str(&new_href);
                new_html.push_str(suffix);
                new_html.push('"');
                last_end = m.end();
            } else {
                // Unmatched internal .md-like link
                unmatched.push(url_full.to_string());
            }
        }
        new_html.push_str(&doc.html[last_end..]);
        doc.html = new_html;
    }

    if !unmatched.is_empty() {
        if opts.verbose {
            for u in &unmatched {
                eprintln!("[warn] Unmatched internal markdown link (left as-is): {u}");
            }
        }
        WARNING_COUNT.fetch_add(unmatched.len(), Ordering::SeqCst);
    }
}

// --------------------------- Styling ---------------------------------------------------------

static GLOBAL_DOCS: once_cell::sync::Lazy<
    std::sync::RwLock<std::collections::HashMap<String, DiaryxDoc>>,
> = once_cell::sync::Lazy::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
static GLOBAL_SLUG_MAP: once_cell::sync::Lazy<
    std::sync::RwLock<std::collections::HashMap<String, String>>,
> = once_cell::sync::Lazy::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
static GLOBAL_BASENAME_MAP: once_cell::sync::Lazy<
    std::sync::RwLock<std::collections::HashMap<String, String>>,
> = once_cell::sync::Lazy::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

fn set_global_docs(docs: &[DiaryxDoc]) {
    let mut map = GLOBAL_DOCS.write().unwrap();
    map.clear();
    let mut slug_map = GLOBAL_SLUG_MAP.write().unwrap();
    slug_map.clear();
    let mut base_map = GLOBAL_BASENAME_MAP.write().unwrap();
    base_map.clear();
    for d in docs {
        map.insert(d.id.clone(), d.clone_light());
        slug_map.insert(d.id.clone(), d.id.clone());
        if let Some(name) = d.abs_path.file_name().and_then(|s| s.to_str()) {
            base_map.insert(name.to_string(), d.id.clone());
        }
    }
}

fn clone_doc_shallow(doc: &DiaryxDoc) -> DiaryxDoc {
    // Shallow clone for registry (exclude heavy HTML/body to save memory)
    DiaryxDoc {
        id: doc.id.clone(),
        abs_path: doc.abs_path.clone(),
        rel_dir: doc.rel_dir.clone(),
        title: doc.title.clone(),
        visibility: doc.visibility.clone(),
        tags: doc.tags.clone(),
        aliases: doc.aliases.clone(),
        is_root_index: doc.is_root_index,
        is_index: doc.is_index,
        contents_raw: doc.contents_raw.clone(),
        children: doc.children.clone(),
        parents: doc.parents.clone(),
        raw_part_of: doc.raw_part_of.clone(),
        html: String::new(),
        body_md: String::new(),
        frontmatter_raw: serde_yaml::Value::Null,
        warnings: vec![],
    }
}

// Provide a method to create the shallow clone (impl block)
impl DiaryxDoc {
    fn clone_light(&self) -> DiaryxDoc {
        clone_doc_shallow(self)
    }
}

fn lookup_doc_by_slug(slug: &str) -> Option<DiaryxDoc> {
    GLOBAL_DOCS.read().ok()?.get(slug).cloned()
}

fn normalized_basename(target: &str) -> String {
    let p = std::path::Path::new(target);
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or(target);
    if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{name}.md")
    }
}

fn file_name_matches(actual: &str, requested_norm: &str) -> bool {
    if actual == requested_norm {
        return true;
    }
    // Allow space vs %20 equivalence
    let alt = requested_norm.replace("%20", " ");
    actual == alt
}

const DEFAULT_CSS: &str = r#"
:root {
  --bg: #ffffff;
  --fg: #1d1f21;
  --accent: #0a6d3d;
  --border: #e2e2e2;
  --code-bg: #f5f5f5;
  --badge-bg: #e0f5ec;
  --badge-fg: #0a6d3d;
  color-scheme: light;
}

* { box-sizing: border-box; }

body {
  font-family: system-ui,-apple-system,Segoe UI,Roboto,Ubuntu,sans-serif;
  margin: 0;
  padding: 0 1.2rem 2rem;
  background: var(--bg);
  color: var(--fg);
  line-height: 1.55;
  max-width: 62rem;
  margin-left: auto;
  margin-right: auto;
}

header {
  padding: 1.4rem 0 .5rem;
  border-bottom: 1px solid var(--border);
  margin-bottom: 1rem;
}

h1,h2,h3,h4 {
  line-height: 1.2;
  font-weight: 600;
}

h1 { font-size: 2rem; margin: 0 0 .75rem; }
h2 { margin-top: 2.2rem; }

p { margin: .9rem 0; }

a {
  color: var(--accent);
  text-decoration: none;
}
a:hover { text-decoration: underline; }

.meta {
  font-size: .85rem;
  background: #fafafa;
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: .75rem 1rem;
  margin: 1rem 0 1.5rem;
}
.meta ul { list-style: none; margin: 0; padding: 0; }
.meta li { margin: .25rem 0; }

.root-badge {
  display: inline-block;
  background: var(--badge-bg);
  color: var(--badge-fg);
  font-size: .75rem;
  padding: .25rem .55rem;
  border-radius: 4px;
  margin: 0;
}

.content pre {
  background: var(--code-bg);
  padding: .9rem 1rem;
  overflow: auto;
  border-radius: 6px;
  font-size: .9rem;
}

code {
  background: var(--code-bg);
  padding: 2px 5px;
  border-radius: 4px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  font-size: .85rem;
}

img { max-width: 100%; height: auto; }

table {
  border-collapse: collapse;
  margin: 1rem 0;
  width: 100%;
  font-size: .9rem;
}
table th, table td {
  border: 1px solid var(--border);
  padding: .4rem .5rem;
  text-align: left;
}

/* Metadata header layout */
.page-header h1 { margin: 0 0 .4rem; font-size:2.05rem; }
.page-header .root-badge { margin-left:.6rem; vertical-align:middle; }
.meta-block { margin-top:.75rem; }
.meta-grid {
  display:grid;
  grid-template-columns:repeat(auto-fit,minmax(180px,1fr));
  gap:.75rem 1.25rem;
  margin:0;
  padding:0;
  position:relative;
}
.meta-section-separator {
  grid-column:1/-1;
  height:1px;
  background:var(--border);
  margin:.35rem 0 .15rem;
  opacity:.65;
}
.meta-item dt {
  margin:0 0 .15rem;
  font-size:.70rem;
  font-weight:600;
  text-transform:uppercase;
  letter-spacing:.07em;
  color:#555;
}
.meta-item dd {
  margin:0;
  font-size:.9rem;
  line-height:1.2;
  word-break:break-word;
}
.contents-nav { margin:1.75rem 0 1.5rem; }
.contents-title { font-size:1.15rem; margin:.2rem 0 .6rem; }
.contents-nav ol { margin:.25rem 0 0 1.2rem; padding:0; }
.contents-nav li { margin:.25rem 0; }
.contents-nav a { text-decoration:none; }
.contents-nav a:hover { text-decoration:underline; }
"#;

// --------------------------- Tests (Basic) ---------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  Multiple --- Spaces "), "multiple-spaces");
        assert_eq!(slugify("Title_With_Underscore"), "title-with-underscore");
    }

    #[test]
    fn split_no_frontmatter() {
        let s = "Hello\nWorld";
        let split = split_frontmatter(s).unwrap();
        assert!(split.frontmatter_yaml.is_none());
        assert_eq!(split.body_md, "Hello\nWorld");
    }

    #[test]
    fn split_with_frontmatter() {
        let s = "---\ntitle: Test\n---\nBody";
        let split = split_frontmatter(s).unwrap();
        assert_eq!(
            split.frontmatter_yaml.as_ref().unwrap().trim(),
            "title: Test"
        );
        assert_eq!(split.body_md.trim(), "Body");
    }
}
