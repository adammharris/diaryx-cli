/*!
 * diaryx-core
 *
 * Core library for parsing Diaryx-formatted Markdown files (YAML frontmatter + CommonMark body)
 * and producing static site artifacts (HTML pages, optional attachment copy plan, warnings).
 *
 * This crate is intentionally independent of direct filesystem (`std::fs`) so it can:
 * - Be embedded in a CLI (provide a real FS adapter)
 * - Run in a WebAssembly environment (provide an in-memory virtual FS)
 * - Be unit tested with synthetic file graphs
 *
 * High-Level Flow (build_site):
 * 1. Load entry file (a Diaryx Markdown file).
 * 2. Parse YAML frontmatter; record missing required fields as warnings.
 * 3. If entry (or files reached through traversal) declares `this_file_is_root_index: true`,
 *    recursively walk `contents:` lists (markdown link syntax) to load additional files.
 * 4. Construct parent/child relationships from `contents` arrays.
 * 5. Render Markdown bodies to HTML (using `markdown` crate).
 * 6. Rewrite internal markdown links (.md) in rendered HTML to corresponding .html page names.
 * 7. Produce `BuildArtifacts` containing pages & warnings (and an attachment copy plan hook).
 *
 * Added: minimal metadata HTML rendering (unordered list) is now generated per page (`metadata_html`)
 * so the CLI layer can simply drop it into its outer template and provide CSS externally.
 *
 * NOTE: This is an initial skeleton. Some advanced behaviors from the original CLI (like
 * attachment copying, full logging modes, strict mode erroring, etc.) can be layered on top.
 */

use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
pub use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, VecDeque};
use time::{OffsetDateTime, UtcOffset};

// -------------------------------------------------------------------------------------------------
// Public API Types
// -------------------------------------------------------------------------------------------------

/// Abstract file provider trait. Supply your own implementation for:
/// - Real filesystem (CLI)
/// - In-memory map (browser / WASM)
/// - Test double
pub trait FileProvider {
    /// Return UTF-8 file contents. Implementations may normalize paths (e.g., canonicalize).
    fn read_to_string(&self, path: &str) -> Result<String>;
    /// Returns true if a path exists (file or directory).
    fn exists(&self, path: &str) -> bool;
    /// Returns true if path refers to a "file" (as opposed to directory).
    fn is_file(&self, path: &str) -> bool;
    /// Joins a parent + relative segment into a normalized path string.
    fn join(&self, parent: &str, rel: &str) -> String;
    /// Returns Some(extension_lowercase) for the basename (without dot); else None.
    fn extension_lowercase(&self, path: &str) -> Option<String>;
    /// Returns the directory portion (without trailing slash) or "" for root.
    fn parent(&self, path: &str) -> Option<String>;
    /// Returns just the file name (no directories); may return the entire path if implementation cannot split.
    fn file_name(&self, path: &str) -> Option<String>;
    /// Produce a deterministic relative key suitable for output naming; default: slug of title + ".html" will use slug only.
    fn canonical_display(&self, path: &str) -> String {
        path.to_string()
    }
}

/// Build configuration options.
#[derive(Debug, Clone, Default)]
pub struct CoreBuildOptions {
    /// Include non-public documents (visibility not containing 'public'). If false, only public + entry.
    pub include_nonpublic: bool,
    /// Flat mode: no nested pages/ path. (The caller can choose how to persist pages.)
    pub flat: bool,
    /// If true, warnings are still returned but *not* upgraded to errors here; the caller decides enforcement.
    pub strict: bool,
    /// When true, internal link rewrite will attempt cross-page rewriting. If false, leaves .md links intact.
    pub rewrite_links: bool,
}

/// A single generated page artifact.
#[derive(Debug, Clone, Serialize)]
pub struct PageOutput {
    pub id: String,          // slug
    pub source_path: String, // original input path
    pub file_name: String,   // recommended html file name (e.g. "<slug>.html" or "index.html")
    pub title: String,
    pub html: String,
    pub metadata_html: String, // rendered frontmatter (no outer <html>, CSS added by CLI)
    pub is_root_index: bool,
    pub is_index: bool,
    pub parents: Vec<String>,  // parent slugs
    pub children: Vec<String>, // child slugs
    pub frontmatter: serde_yaml::Value,
    pub warnings: Vec<String>, // warnings local to this page
}

/// (Future) Attachment copy plan.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AttachmentPlanEntry {
    pub source: String,
    pub target: String,
}

/// The result of a build.
#[derive(Debug, Clone, Serialize)]
pub struct BuildArtifacts {
    pub pages: Vec<PageOutput>,
    pub attachments: Vec<AttachmentPlanEntry>,
    pub warnings: Vec<String>, // global + collected per-page (flattened summary)
    pub multi_page: bool,
    pub root_slug: Option<String>,
}

/// Build the site from a single entry file path.
pub fn build_site(
    entry: &str,
    opts: CoreBuildOptions,
    fs: &impl FileProvider,
) -> Result<BuildArtifacts> {
    // 1. Collect all documents (recursive if root index pattern)
    let mut warnings_global = Vec::new();
    let mut docs = collect_documents(entry, &opts, fs, &mut warnings_global)?;

    // 2. Link graph (parents / children)
    link_graph(&mut docs, fs);

    // 3. Filter by visibility (always keep entry)
    let entry_abs = entry.to_string();
    let _ = docs
        .iter()
        .find(|d| d.abs_path == entry_abs)
        .map(|d| d.id.clone())
        .ok_or_else(|| anyhow!("Entry path not loaded: {entry_abs}"))?;

    if !opts.include_nonpublic {
        docs.retain(|d| d.is_public() || d.abs_path == entry_abs);
    }

    if docs.is_empty() {
        return Err(anyhow!(
            "No documents after filtering. Ensure visibility includes 'public' or enable include_nonpublic."
        ));
    }

    // 4. Render HTML (already done in parse step) + rewrite links if requested
    if opts.rewrite_links {
        rewrite_internal_links(&mut docs, &opts);
    }

    // 5. Determine root / multipage
    let multi_page = docs.iter().any(|d| d.is_root_index) && docs.len() > 1;
    let root_slug = docs.iter().find(|d| d.is_root_index).map(|d| d.id.clone());

    // 5b. Attachment/resource discovery & rewriting (non-.md relative links)
    // This scans each rendered HTML body for src/href attributes pointing to relative,
    // non-.md files (images, PDFs, etc.), assigns them a unique target under assets/,
    // rewrites the HTML to point there (adjusting for nested layout), and produces an
    // attachment copy plan.
    let attachments = {
        use once_cell::sync::Lazy;
        use regex::Regex;
        static RES_REF: Lazy<Regex> =
            Lazy::new(|| Regex::new(r#"(?i)(src|href)="([^"]+)""#).unwrap());
        use std::collections::{HashMap, HashSet};
        let mut source_to_target: HashMap<String, String> = HashMap::new();
        let mut used_names: HashSet<String> = HashSet::new();

        for doc in docs.iter_mut() {
            // Fast skip if no candidate attributes
            if !doc.html.contains("src=\"") && !doc.html.contains("href=\"") {
                continue;
            }
            let parent_dir = std::path::Path::new(&doc.abs_path)
                .parent()
                .unwrap_or(std::path::Path::new(""));

            let mut new_html = String::with_capacity(doc.html.len());
            let mut last = 0;

            for cap in RES_REF.captures_iter(&doc.html) {
                let m = cap.get(0).unwrap();
                let attr_name = cap.get(1).unwrap().as_str();
                let val = cap.get(2).unwrap().as_str();

                // Write portion before this attribute match
                new_html.push_str(&doc.html[last..m.start()]);

                // Filter out values we do NOT treat as attachments
                if val.is_empty()
                    || val.starts_with('#')
                    || val.starts_with('/')
                    || val.starts_with("data:")
                    || val.starts_with("mailto:")
                    || val.contains("://")
                {
                    new_html.push_str(m.as_str());
                    last = m.end();
                    continue;
                }

                // Strip query / fragment for resolution, retain original for replacement basis
                let core_val = val.split(|c| c == '?' || c == '#').next().unwrap_or(val);
                let lower = core_val.to_ascii_lowercase();
                if lower.ends_with(".md") {
                    // Skip markdown page links (already handled by internal link rewriting)
                    new_html.push_str(m.as_str());
                    last = m.end();
                    continue;
                }

                // Decode simple %20 for filesystem lookup
                let decoded = core_val.replace("%20", " ");
                let abs_path_buf = parent_dir.join(&decoded);
                let abs_path_string = abs_path_buf.to_string_lossy().to_string();

                if !abs_path_buf.exists() {
                    doc.warnings
                        .push(format!("Attachment not found: {}", abs_path_string));
                    new_html.push_str(m.as_str());
                    last = m.end();
                    continue;
                }
                if abs_path_buf.is_dir() {
                    doc.warnings.push(format!(
                        "Attachment path is directory (skipped): {}",
                        abs_path_string
                    ));
                    new_html.push_str(m.as_str());
                    last = m.end();
                    continue;
                }

                // Map / reuse target
                let target_rel = if let Some(existing) = source_to_target.get(&abs_path_string) {
                    existing.clone()
                } else {
                    // Assign new unique name under assets/
                    let mut base_name = abs_path_buf
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("attachment")
                        .to_string();

                    if !used_names.insert(base_name.clone()) {
                        // Collision: append -N before extension
                        let (stem, ext) = if let Some((s, e)) = base_name.rsplit_once('.') {
                            (s.to_string(), format!(".{}", e))
                        } else {
                            (base_name.clone(), "".to_string())
                        };
                        let mut counter = 1;
                        loop {
                            let candidate = format!("{}-{}{}", stem, counter, ext);
                            if used_names.insert(candidate.clone()) {
                                base_name = candidate;
                                break;
                            }
                            counter += 1;
                        }
                    }
                    let rel = format!("assets/{}", base_name);
                    source_to_target.insert(abs_path_string.clone(), rel.clone());
                    rel
                };

                // Compute path relative to page output location
                let mut final_path = target_rel.clone();
                if multi_page && !opts.flat && !doc.is_root_index {
                    // child page lives under pages/
                    final_path = format!("../{}", final_path);
                }
                // Re-encode spaces minimally (only spaces)
                let encoded = final_path.replace(' ', "%20");

                // Emit rewritten attribute
                new_html.push_str(attr_name);
                new_html.push_str("=\"");
                new_html.push_str(&encoded);
                new_html.push('"');

                last = m.end();
            }
            // Tail
            new_html.push_str(&doc.html[last..]);
            doc.html = new_html;
        }

        // Convert mapping to plan
        let mut plan: Vec<AttachmentPlanEntry> = source_to_target
            .into_iter()
            .map(|(source, target)| AttachmentPlanEntry { source, target })
            .collect();
        plan.sort_by(|a, b| a.target.cmp(&b.target));
        plan
    };

    // 6. Produce PageOutput
    let mut all_pages = Vec::new();
    let mut aggregated: Vec<String> = warnings_global.clone();
    for d in docs.into_iter() {
        aggregated.extend(d.warnings.iter().cloned());
        let file_name = if multi_page {
            if d.is_root_index {
                "index.html".to_string()
            } else {
                format!("{}.html", d.id)
            }
        } else {
            // Single page site => always index.html
            "index.html".to_string()
        };
        all_pages.push(PageOutput {
            id: d.id,
            source_path: d.abs_path,
            file_name,
            title: d.title,
            html: d.html,
            metadata_html: build_metadata_html(
                &d.frontmatter,
                d.is_root_index,
                d.is_index,
                multi_page,
                opts.flat,
                &d.children,
                &d.parents,
                &d.raw_part_of,
                root_slug.as_deref(),
                &d.child_aliases,
                &d.parent_aliases,
            ),
            is_root_index: d.is_root_index,
            is_index: d.is_index,
            parents: d.parents,
            children: d.children,
            frontmatter: d.frontmatter,
            warnings: d.warnings,
        });
    }

    Ok(BuildArtifacts {
        pages: all_pages,
        attachments,
        warnings: aggregated,
        multi_page,
        root_slug,
    })
}

// -------------------------------------------------------------------------------------------------
// Internal Document Model
// -------------------------------------------------------------------------------------------------

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
    tags: Option<Vec<String>>,
    aliases: Option<Vec<String>>,
    this_file_is_root_index: Option<bool>,
    reachable: Option<serde_yaml::Value>,
    // Additional fields ignored for now
}

#[derive(Debug, Clone)]
struct Doc {
    id: String,
    abs_path: String,
    title: String,
    visibility: Vec<String>,
    #[allow(dead_code)]
    tags: Vec<String>,
    #[allow(dead_code)]
    aliases: Vec<String>,
    is_root_index: bool,
    is_index: bool,
    contents_raw: Vec<String>,
    raw_part_of: Vec<String>,
    children: Vec<String>,
    parents: Vec<String>,
    child_aliases: HashMap<String, String>,  // slug -> alias
    parent_aliases: HashMap<String, String>, // slug -> alias
    html: String,
    frontmatter: serde_yaml::Value,
    warnings: Vec<String>,
    #[allow(dead_code)]
    body_md: String,
}

impl Doc {
    fn is_public(&self) -> bool {
        self.visibility.iter().any(|v| v == "public")
    }
}

// -------------------------------------------------------------------------------------------------
// Collection / Parsing
// -------------------------------------------------------------------------------------------------

fn collect_documents(
    entry: &str,
    _opts: &CoreBuildOptions,
    fs: &impl FileProvider,
    warnings_global: &mut Vec<String>,
) -> Result<Vec<Doc>> {
    let mut queue = VecDeque::new();
    let mut visited: HashMap<String, Doc> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    queue.push_back(entry.to_string());

    while let Some(path) = queue.pop_front() {
        if visited.contains_key(&path) {
            continue;
        }
        if !fs.exists(&path) {
            warnings_global.push(format!("Entry or referenced path missing: {path}"));
            continue;
        }
        if !fs.is_file(&path) {
            warnings_global.push(format!("Skipping non-file path: {path}"));
            continue;
        }

        // Skip non-markdown
        if let Some(ext) = fs.extension_lowercase(&path) {
            if ext != "md" {
                // Non-diaryx file – skip silently (user-level decision to add warning if needed)
                continue;
            }
        } else {
            // No extension, assume not Diaryx
            continue;
        }

        let raw = match fs.read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                warnings_global.push(format!("Failed to read {path}: {e}"));
                continue;
            }
        };

        let split =
            split_frontmatter(&raw).with_context(|| format!("Split frontmatter failed: {path}"))?;
        let (fm_val, fm_struct) = parse_frontmatter(&split.frontmatter_yaml)?;
        let mut doc_warnings = Vec::new();
        check_required(&fm_struct, &mut doc_warnings, &path);

        let title = fm_struct
            .title
            .clone()
            .unwrap_or_else(|| fs.file_name(&path).unwrap_or_else(|| path.clone()));
        // Derive slug from filename stem instead of title to ensure stable cross-file linking / alias resolution
        // (prevents mismatch when title differs from physical filename used in links)
        let slug = {
            let fname = fs.file_name(&path).unwrap_or_else(|| title.clone());
            // strip extension if present
            let stem = fname
                .rsplit_once('.')
                .map(|(s, _)| s.to_string())
                .unwrap_or(fname);
            slugify(&stem)
        };

        let html = render_markdown(&split.body_md)
            .with_context(|| format!("Markdown render failure: {path}"))?;

        let visibility = normalize_string_or_list(&fm_struct.visibility);
        let contents_norm = normalize_contents(&fm_struct.contents);
        let is_root = fm_struct.this_file_is_root_index.unwrap_or(false);

        let doc = Doc {
            id: slug,
            abs_path: path.clone(),
            title,
            visibility,
            tags: fm_struct.tags.unwrap_or_default(),
            aliases: fm_struct.aliases.unwrap_or_default(),
            is_root_index: is_root,
            is_index: !contents_norm.is_empty(),
            contents_raw: contents_norm,
            raw_part_of: parse_part_of(&fm_struct.part_of),
            children: Vec::new(),
            parents: Vec::new(),
            child_aliases: HashMap::new(),
            parent_aliases: HashMap::new(),
            html,
            frontmatter: fm_val,
            warnings: doc_warnings,
            body_md: split.body_md,
        };

        let is_index = doc.is_index;
        let is_root_index = doc.is_root_index;
        let contents_links = doc.contents_raw.clone();

        order.push(path.clone());
        visited.insert(path.clone(), doc);

        // Recursion gate
        if is_index && (is_root_index || entry_metadata_had_root(entry, &visited)) {
            if let Some(parent_dir) = fs.parent(&path) {
                for raw_link in contents_links {
                    if let Some(resolved) = resolve_contents_link(&raw_link, &parent_dir, fs) {
                        if fs.exists(&resolved) && fs.is_file(&resolved) {
                            queue.push_back(resolved);
                        } else {
                            warnings_global.push(format!(
                                "contents target not found or not a file: {} (from {})",
                                resolved, path
                            ));
                        }
                    } else {
                        warnings_global.push(format!(
                            "Could not parse contents entry '{}' in {}",
                            raw_link, path
                        ));
                    }
                }
            }
        }
    }

    // Preserve traversal ordering
    let mut out = Vec::with_capacity(visited.len());
    for p in order {
        if let Some(d) = visited.remove(&p) {
            out.push(d);
        }
    }
    Ok(out)
}

fn entry_metadata_had_root(entry: &str, visited: &HashMap<String, Doc>) -> bool {
    visited.get(entry).map(|d| d.is_root_index).unwrap_or(false)
}

struct SplitFrontmatter {
    frontmatter_yaml: Option<String>,
    body_md: String,
}

fn split_frontmatter(raw: &str) -> Result<SplitFrontmatter> {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
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

fn check_required(fm: &FrontmatterRaw, warnings: &mut Vec<String>, path: &str) {
    if fm.title.is_none() {
        warnings.push(format!("Missing required field: title ({path})"));
    }
    if fm.author.is_none() {
        warnings.push(format!("Missing required field: author ({path})"));
    }
    if fm.created.is_none() {
        warnings.push(format!("Missing required field: created ({path})"));
    }
    if fm.updated.is_none() {
        warnings.push(format!("Missing required field: updated ({path})"));
    }
    if fm.visibility.is_none() {
        warnings.push(format!("Missing required field: visibility ({path})"));
    }
    if fm.format.is_none() {
        warnings.push(format!("Missing required field: format ({path})"));
    }
    // reachable: required, but can be any non-empty scalar, sequence, or mapping value.
    // Treat missing, null, empty string, or empty sequence as "missing".
    if match &fm.reachable {
        None => true,
        Some(serde_yaml::Value::Null) => true,
        Some(serde_yaml::Value::String(s)) => s.trim().is_empty(),
        Some(serde_yaml::Value::Sequence(seq)) => seq.is_empty(),
        _ => false,
    } {
        warnings.push(format!("Missing required field: reachable ({path})"));
    }
}

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

// -------------------------------------------------------------------------------------------------
// Graph Linking
// -------------------------------------------------------------------------------------------------

fn link_graph(docs: &mut [Doc], fs: &impl FileProvider) {
    // Build quick lookup: abs_path -> (index, slug)
    let mut path_to_index: HashMap<String, usize> = HashMap::new();
    for (i, d) in docs.iter().enumerate() {
        path_to_index.insert(d.abs_path.clone(), i);
    }
    for i in 0..docs.len() {
        if !docs[i].is_index {
            continue;
        }
        let parent_dir = match fs.parent(&docs[i].abs_path) {
            Some(p) => p,
            None => "".to_string(),
        };
        let entries = docs[i].contents_raw.clone();
        for raw_link in entries {
            if let Some(abs) = resolve_contents_link(&raw_link, &parent_dir, fs) {
                if let Some(child_idx) = docs.iter().position(|d| d.abs_path == abs) {
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

    // After structural links, derive alias maps from original frontmatter link strings
    // Child aliases
    for i in 0..docs.len() {
        if !docs[i].is_index || docs[i].contents_raw.is_empty() {
            continue;
        }
        let parent_dir = fs.parent(&docs[i].abs_path).unwrap_or_default();
        for raw in docs[i].contents_raw.clone() {
            if let Some((alias, _target)) = extract_md_link_parts_raw(&raw) {
                if alias.is_empty() {
                    continue;
                }
                // Resolve target to slug
                if let Some(abs) = resolve_contents_link(&raw, &parent_dir, fs) {
                    if let Some(idx) = docs.iter().position(|d| d.abs_path == abs) {
                        let slug = docs[idx].id.clone();
                        docs[i].child_aliases.insert(slug, alias);
                    }
                }
            }
        }
    }

    // Parent aliases from raw_part_of
    for i in 0..docs.len() {
        if docs[i].raw_part_of.is_empty() {
            continue;
        }
        let parent_dir = fs.parent(&docs[i].abs_path).unwrap_or_default();
        for raw in docs[i].raw_part_of.clone() {
            if let Some((alias, _target)) = extract_md_link_parts_raw(&raw) {
                if alias.is_empty() {
                    continue;
                }
                if let Some(abs) = resolve_contents_link(&raw, &parent_dir, fs) {
                    if let Some(idx) = docs.iter().position(|d| d.abs_path == abs) {
                        let slug = docs[idx].id.clone();
                        docs[i].parent_aliases.insert(slug, alias);
                    }
                }
            }
        }
    }
}

fn resolve_contents_link(raw: &str, parent_dir: &str, fs: &impl FileProvider) -> Option<String> {
    static LINK_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\[[^\]]*]\(\s*<?([^)>]+)>?\s*\)").unwrap());
    let caps = LINK_RE.captures(raw)?;
    let target = caps.get(1)?.as_str().trim();
    let first = fs.join(parent_dir, target);
    if fs.exists(&first) {
        return Some(first);
    }
    // Add .md if missing extension
    if fs.extension_lowercase(target).is_none() && !target.ends_with('/') {
        let appended = format!("{target}.md");
        let with_md = fs.join(parent_dir, &appended);
        if fs.exists(&with_md) {
            return Some(with_md);
        }
    }
    Some(first) // Return best-effort path (even if missing) so caller can warn
}

// -------------------------------------------------------------------------------------------------
// Markdown Rendering & Link Rewriting
// -------------------------------------------------------------------------------------------------

fn render_markdown(src: &str) -> Result<String> {
    let opts = markdown::Options::default();
    markdown::to_html_with_options(src, &opts).map_err(|e| anyhow!("Markdown render error: {e}"))
}

/// Update doc.html in-place rewriting internal .md links.
fn rewrite_internal_links(docs: &mut [Doc], opts: &CoreBuildOptions) {
    if docs.is_empty() {
        return;
    }
    let has_root = docs.iter().any(|d| d.is_root_index);
    let multi_page = has_root && docs.len() > 1;
    let by_basename: HashMap<String, (String, bool)> = docs
        .iter()
        .map(|d| {
            let name = d
                .abs_path
                .rsplit('/')
                .next()
                .unwrap_or(&d.abs_path)
                .to_string();
            (name, (d.id.clone(), d.is_root_index))
        })
        .collect();

    static HREF_MD: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"href="([^"]+?\.(?i:md)(?:[?#][^"]*)?)""#).unwrap());

    for doc in docs.iter_mut() {
        if !doc.html.to_ascii_lowercase().contains(".md") {
            continue;
        }
        let current_is_root = doc.is_root_index;
        let mut new_html = String::with_capacity(doc.html.len());
        let mut last = 0;
        for cap in HREF_MD.captures_iter(&doc.html) {
            let m = cap.get(0).unwrap();
            let url = cap.get(1).unwrap().as_str();
            let core = url.split(&['?', '#'][..]).next().unwrap_or(url);
            let basename = core.rsplit('/').next().unwrap_or(core);
            let basename_norm = basename.replace("%20", " ");
            let mapping = by_basename.get(&basename_norm);
            if let Some((target_slug, target_is_root)) = mapping {
                let new_href = if multi_page && !opts.flat {
                    // Nested layout (root at top-level, children under pages/)
                    if current_is_root {
                        if *target_is_root {
                            "index.html".into()
                        } else {
                            // root -> child
                            format!("pages/{}.html", target_slug)
                        }
                    } else {
                        // Current doc is a child (lives in pages/)
                        if *target_is_root {
                            // child -> root
                            "../index.html".into()
                        } else {
                            // child -> sibling
                            format!("{}.html", target_slug)
                        }
                    }
                } else if multi_page {
                    // Flat multi-page: everything at one level
                    if *target_is_root {
                        "index.html".into()
                    } else {
                        format!("{}.html", target_slug)
                    }
                } else {
                    // Single-page build: all internal links point to index.html
                    "index.html".into()
                };
                let mut suffix = "";
                if let Some(idx) = url.find(|c| c == '?' || c == '#') {
                    suffix = &url[idx..];
                }
                new_html.push_str(&doc.html[last..m.start()]);
                new_html.push_str("href=\"");
                new_html.push_str(&new_href);
                new_html.push_str(suffix);
                new_html.push('"');
                last = m.end();
            }
        }
        new_html.push_str(&doc.html[last..]);
        doc.html = new_html;
    }

    if opts.strict {
        // Placeholder for future strict validation of fully rewritten links.
    }
}

// -------------------------------------------------------------------------------------------------
// Utilities
// -------------------------------------------------------------------------------------------------

fn slugify(s: &str) -> String {
    static NON_ALNUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let lower = s.to_ascii_lowercase();
    let replaced = NON_ALNUM.replace_all(&lower, "-");
    replaced.trim_matches('-').to_string()
}

#[allow(dead_code)]
fn humanize_timestamp(raw: &str) -> String {
    if raw.is_empty() || !raw.contains('T') {
        return raw.to_string();
    }
    if let Ok(dt) =
        OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
    {
        let date_fmt =
            time::format_description::parse("[month repr:long] [day padding:zero], [year]")
                .unwrap_or_else(|_| {
                    time::format_description::parse("[year]-[month]-[day]").unwrap()
                });
        let date_str = dt.format(&date_fmt).unwrap_or_else(|_| raw.to_string());
        let hour = dt.hour();
        let (h12, ampm) = match hour {
            0 => (12, "am"),
            1..=11 => (hour, "am"),
            12 => (12, "pm"),
            _ => (hour - 12, "pm"),
        };
        let minute = dt.minute();
        let offset: UtcOffset = dt.offset();
        let hours = offset.whole_hours();
        let tz_label = if hours == 0 {
            "UTC".to_string()
        } else {
            format!("UTC{:+}", hours)
        };
        return format!(
            "{}, {:02}:{:02}{} ({})",
            date_str, h12, minute, ampm, tz_label
        );
    }
    raw.to_string()
}
/// Build minimal metadata HTML (unordered list). Caller supplies CSS.
/// Includes special formatting for created / updated if present.
fn build_metadata_html(
    frontmatter: &serde_yaml::Value,
    is_root_index: bool,
    is_index: bool,
    multi_page: bool,
    flat: bool,
    children: &[String],
    parents: &[String],
    raw_part_of: &[String],
    root_slug: Option<&str>,
    child_alias_map: &HashMap<String, String>,
    parent_alias_map: &HashMap<String, String>,
) -> String {
    use serde_yaml::Value;
    let mapping = match frontmatter {
        Value::Mapping(m) => m,
        _ => return String::new(),
    };
    if mapping.is_empty() {
        return String::new();
    }

    // 1. Gather original key insertion order (preserve author intent)
    // serde_yaml::Mapping preserves insertion order in iteration
    let mut ordered: Vec<(&String, &Value)> = Vec::new();
    for (k, v) in mapping {
        if let Value::String(s) = k {
            ordered.push((s, v));
        }
    }

    // Ensure 'reachable' renders last regardless of YAML insertion order
    {
        let mut reachable_entry: Option<(&String, &Value)> = None;
        ordered.retain(|(k, v)| {
            if *k == "reachable" {
                reachable_entry = Some((*k, *v));
                false
            } else {
                true
            }
        });
        if let Some(r) = reachable_entry {
            ordered.push(r);
        }
    }

    // 2. Prepare alias-aware child link rendering.
    // We attempt to recover alias text from the original 'contents' sequence of markdown links:
    // "[Alias](target.md)" so that the metadata view shows human-friendly labels.
    // Build a map: normalized_target_basename -> alias.
    let mut alias_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if is_index {
        if let Some(contents_val) = mapping.get(&Value::String("contents".to_string())) {
            if let Value::Sequence(seq) = contents_val {
                for item in seq {
                    if let Value::String(s) = item {
                        if let Some((alias, target)) = extract_md_link_parts_raw(s) {
                            if !alias.is_empty() {
                                let norm = normalize_target_basename_raw(&target);
                                alias_map.insert(norm, alias);
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. Child links (alias text if available) with layout-aware hrefs
    let mut child_links: Vec<String> = Vec::new();
    if is_index && !children.is_empty() {
        for slug in children {
            // Normalized key used when deriving aliases from raw contents link targets
            let md_key = format!("{slug}.md");
            // Precedence:
            // 1. Explicit child_alias_map (slug -> alias) from graph phase
            // 2. Raw contents alias_map (normalized basename -> alias)
            // 3. Fallback to slug
            let label = child_alias_map
                .get(slug)
                .cloned()
                .or_else(|| alias_map.get(&md_key).cloned())
                .unwrap_or_else(|| slug.to_string());
            let href = if multi_page {
                if flat {
                    format!("{slug}.html")
                } else if is_root_index {
                    format!("pages/{slug}.html")
                } else {
                    format!("{slug}.html")
                }
            } else {
                // single-page build (all content together)
                "index.html".to_string()
            };
            child_links.push(format!(
                "<a href=\"{}\">{}</a>",
                href,
                html_escape_text(&label)
            ));
        }
    }

    // 3b. Parent (part_of) alias extraction & links
    // Build map from normalized basename -> alias extracted directly from raw_part_of
    let mut raw_parent_alias_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if !raw_part_of.is_empty() {
        for raw in raw_part_of {
            if let Some((alias, target)) = extract_md_link_parts_raw(raw) {
                if !alias.is_empty() {
                    let norm = normalize_target_basename_raw(&target);
                    raw_parent_alias_map.insert(norm, alias);
                }
            }
        }
    }
    let mut parent_links: Vec<String> = Vec::new();
    if !parents.is_empty() {
        for parent_slug in parents {
            let md_key = format!("{parent_slug}.md");
            // Precedence:
            // 1. parent_aliases (slug -> alias) from graph phase
            // 2. raw_parent_alias_map (normalized target -> alias)
            // 3. fallback to slug
            let label = parent_alias_map
                .get(parent_slug)
                .cloned()
                .or_else(|| parent_alias_map.get(&md_key).cloned())
                .or_else(|| raw_parent_alias_map.get(&md_key).cloned())
                .unwrap_or_else(|| parent_slug.to_string());
            let is_root_parent = root_slug.map(|r| r == parent_slug).unwrap_or(false);

            let href = if multi_page && !flat {
                if is_root_index {
                    if is_root_parent {
                        "index.html".to_string()
                    } else {
                        format!("pages/{parent_slug}.html")
                    }
                } else if is_root_parent {
                    "../index.html".to_string()
                } else {
                    format!("../pages/{parent_slug}.html")
                }
            } else if multi_page {
                if is_root_parent {
                    "index.html".to_string()
                } else {
                    format!("{parent_slug}.html")
                }
            } else {
                "index.html".to_string()
            };

            parent_links.push(format!(
                "<a href=\"{}\">{}</a>",
                href,
                html_escape_text(&label)
            ));
        }
    }
    // Include non-structural part_of targets (aliases) not present in structural parents.
    // These are entries in raw_part_of whose targets were not discovered via contents traversal.
    if !raw_parent_alias_map.is_empty() {
        // Build a set of already linked slugs (derive slug from filename stem).
        let mut existing: std::collections::HashSet<String> = parents.iter().cloned().collect();
        // Also capture slugs already pushed (in case of duplicates)
        for link in &parent_links {
            // crude extraction: href=".../slug.html" or href="slug.html"
            if let Some(href_start) = link.find("href=\"") {
                let rest = &link[href_start + 6..];
                if let Some(end_q) = rest.find('"') {
                    let target = &rest[..end_q];
                    // pull final component without extension
                    let slug = target
                        .rsplit('/')
                        .next()
                        .unwrap_or(target)
                        .strip_suffix(".html")
                        .unwrap_or(target)
                        .to_string();
                    existing.insert(slug);
                }
            }
        }
        for (norm, alias_label) in &raw_parent_alias_map {
            // norm like "alpha.md"
            let stem = norm.strip_suffix(".md").unwrap_or(norm).to_string();
            let slug_candidate = slugify(&stem);
            if existing.contains(&slug_candidate) {
                continue;
            }
            // Derive href using same layout rules
            let href = if multi_page && !flat {
                if is_root_index {
                    // root page linking "up" to a non-structural parent (treat as pages/)
                    format!("pages/{}.html", slug_candidate)
                } else {
                    // child page linking to another non-root parent
                    format!("../pages/{}.html", slug_candidate)
                }
            } else if multi_page {
                format!("{}.html", slug_candidate)
            } else {
                "index.html".to_string()
            };
            parent_links.push(format!(
                "<a href=\"{}\">{}</a>",
                href,
                html_escape_text(alias_label)
            ));
            existing.insert(slug_candidate);
        }
    }

    static MD_LINK_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?x)\[([^\]]+)\]\(([^)]+)\)").unwrap());

    let mut out = String::new();
    out.push_str("<ul class=\"metadata\">");

    for (k, v) in ordered {
        out.push_str("<li><strong>");
        html_esc_simple(&mut out, k);
        out.push_str(":</strong> ");

        // contents: emit alias-aware links (replace raw value)
        if *k == "contents" {
            if !child_links.is_empty() {
                // For nested layout: adjust root vs child link prefixes.
                if multi_page && !flat {
                    let mut adjusted: Vec<String> = Vec::with_capacity(child_links.len());
                    if is_root_index {
                        // Links already have pages/ prefix (set earlier) so just reuse.
                        adjusted = child_links.clone();
                    } else {
                        // Child page: remove any leading "pages/" (should not be present) to keep sibling links relative.
                        for link in &child_links {
                            // link format: <a href="...">label</a>
                            if let Some(start) = link.find("href=\"") {
                                let prefix_cut = &link[start + 6..];
                                if let Some(end_quote) = prefix_cut.find('"') {
                                    let href = &prefix_cut[..end_quote];
                                    let label_start = link.find('>').unwrap_or(0) + 1;
                                    let label_end = link.rfind('<').unwrap_or(link.len());
                                    let label = &link[label_start..label_end];
                                    // If href begins with "pages/", strip it.
                                    let adjusted_href = if href.starts_with("pages/") {
                                        &href[6..]
                                    } else {
                                        href
                                    };
                                    adjusted.push(format!(
                                        "<a href=\"{}\">{}</a>",
                                        adjusted_href, label
                                    ));
                                } else {
                                    adjusted.push(link.clone());
                                }
                            } else {
                                adjusted.push(link.clone());
                            }
                        }
                    }
                    out.push_str(&adjusted.join("<br/>"));
                } else {
                    out.push_str(&child_links.join("<br/>"));
                }
            } else {
                let rendered = inline_yaml(v);
                push_maybe_md_links(&mut out, &rendered, &MD_LINK_RE);
            }
            out.push_str("</li>");
            continue;
        }

        // part_of: emit alias-aware parent links
        if *k == "part_of" {
            if !parent_links.is_empty() {
                out.push_str(&parent_links.join("<br/>"));
            } else {
                let rendered = inline_yaml(v);
                push_maybe_md_links(&mut out, &rendered, &MD_LINK_RE);
            }
            out.push_str("</li>");
            continue;
        }

        // timestamps
        if (*k == "created" || *k == "updated") && v.as_str().is_some() {
            if let Some(s) = v.as_str() {
                let pretty = humanize_timestamp(s);
                html_esc_simple(&mut out, &pretty);
                out.push_str("</li>");
                continue;
            }
        }

        // General value (with markdown link conversion)
        let rendered = inline_yaml(v);
        push_maybe_md_links(&mut out, &rendered, &MD_LINK_RE);
        out.push_str("</li>");
    }

    out.push_str("</ul>");
    out
}

/// If the string contains markdown links, convert them to HTML anchors (escaping text & href);
/// otherwise escape the whole string.
fn push_maybe_md_links(out: &mut String, s: &str, re: &Regex) {
    if !re.is_match(s) {
        html_esc_simple(out, s);
        return;
    }
    let mut last = 0;
    for cap in re.captures_iter(s) {
        let m = cap.get(0).unwrap();
        // text before match
        if m.start() > last {
            html_esc_simple(out, &s[last..m.start()]);
        }
        let text = &cap[1];
        let href = &cap[2];
        out.push_str("<a href=\"");
        html_esc_simple(out, href);
        out.push_str("\">");
        html_esc_simple(out, text);
        out.push_str("</a>");
        last = m.end();
    }
    if last < s.len() {
        html_esc_simple(out, &s[last..]);
    }
}
fn inline_yaml(v: &serde_yaml::Value) -> String {
    use serde_yaml::Value;
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        Value::Sequence(seq) => {
            if seq.is_empty() {
                "[]".into()
            } else {
                seq.iter().map(inline_yaml).collect::<Vec<_>>().join(", ")
            }
        }
        Value::Mapping(map) => {
            if map.is_empty() {
                "{}".into()
            } else {
                let mut kvs: Vec<String> = Vec::new();
                for (k, vv) in map {
                    if let Value::String(ks) = k {
                        kvs.push(format!("{}={}", ks, inline_yaml(vv)));
                    }
                }
                kvs.join("; ")
            }
        }
        Value::Tagged(tag) => format!("!{} {}", tag.tag, inline_yaml(&tag.value)),
    }
}
fn html_esc_simple(out: &mut String, s: &str) {
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

/// Stand‑alone helpers (moved out of build_metadata_html to avoid nested fn declarations)
fn extract_md_link_parts_raw(raw: &str) -> Option<(String, String)> {
    static LINK_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^\[([^\]]*)]\(\s*<?([^)>]+)>?\s*\)$").unwrap());
    let caps = LINK_RE.captures(raw.trim())?;
    let alias = caps
        .get(1)
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();
    let target = caps
        .get(2)
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();
    Some((alias, target))
}

fn normalize_target_basename_raw(t: &str) -> String {
    let last = t.rsplit('/').next().unwrap_or(t);
    if last.to_ascii_lowercase().ends_with(".md") {
        last.to_string()
    } else {
        format!("{}.md", last)
    }
}

fn html_escape_text(s: &str) -> String {
    let mut out = String::new();
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
    out
}

// -------------------------------------------------------------------------------------------------
// (Optional) WASM bindings (behind "wasm" feature)
// -------------------------------------------------------------------------------------------------

#[cfg(feature = "wasm")]
mod wasm_bindings {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use wasm_bindgen::prelude::*;

    // Simple in-memory FS for WASM usage
    pub struct InMemoryFs {
        files: HashMap<String, String>,
    }

    impl InMemoryFs {
        pub fn new(files: HashMap<String, String>) -> Self {
            Self { files }
        }
        fn normalize(path: &str) -> String {
            // Very light normalization; real impl might collapse ../ .
            path.replace('\\', "/")
        }
    }

    impl FileProvider for InMemoryFs {
        fn read_to_string(&self, path: &str) -> Result<String> {
            let p = Self::normalize(path);
            self.files
                .get(&p)
                .cloned()
                .ok_or_else(|| anyhow!("File not found: {p}"))
        }
        fn exists(&self, path: &str) -> bool {
            let p = Self::normalize(path);
            self.files.contains_key(&p)
        }
        fn is_file(&self, path: &str) -> bool {
            self.exists(path)
        }
        fn join(&self, parent: &str, rel: &str) -> String {
            if parent.is_empty() {
                Self::normalize(rel)
            } else {
                let mut base = parent.trim_end_matches('/').to_string();
                base.push('/');
                base.push_str(rel.trim_start_matches('/'));
                Self::normalize(&base)
            }
        }
        fn extension_lowercase(&self, path: &str) -> Option<String> {
            let p = Self::normalize(path);
            p.rsplit('/')
                .next()
                .and_then(|f| f.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()))
        }
        fn parent(&self, path: &str) -> Option<String> {
            let p = Self::normalize(path);
            match p.rsplit_once('/') {
                Some((dir, _)) if !dir.is_empty() => Some(dir.to_string()),
                _ => Some(String::new()),
            }
        }
        fn file_name(&self, path: &str) -> Option<String> {
            let p = Self::normalize(path);
            Some(p.rsplit('/').next().unwrap_or(&p).to_string())
        }
    }

    #[derive(Deserialize)]
    struct WasmInput {
        entry: String,
        files: HashMap<String, String>,
        #[serde(default)]
        include_nonpublic: bool,
        #[serde(default)]
        flat: bool,
        #[serde(default)]
        strict: bool,
        #[serde(default = "default_true")]
        rewrite_links: bool,
    }

    fn default_true() -> bool {
        true
    }

    #[derive(Serialize)]
    struct WasmOutput {
        pages: Vec<super::PageOutput>,
        warnings: Vec<String>,
        multi_page: bool,
        root_slug: Option<String>,
    }

    #[wasm_bindgen]
    pub fn build_diaryx(payload_json: String) -> Result<String, JsValue> {
        let input: WasmInput = serde_json::from_str(&payload_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid JSON: {e}")))?;
        let fs = InMemoryFs::new(input.files);
        let opts = CoreBuildOptions {
            include_nonpublic: input.include_nonpublic,
            flat: input.flat,
            strict: input.strict,
            rewrite_links: input.rewrite_links,
        };
        let artifacts = build_site(&input.entry, opts, &fs)
            .map_err(|e| JsValue::from_str(&format!("Build error: {e}")))?;
        let out = WasmOutput {
            pages: artifacts.pages,
            warnings: artifacts.warnings,
            multi_page: artifacts.multi_page,
            root_slug: artifacts.root_slug,
        };
        serde_json::to_string(&out).map_err(|e| JsValue::from_str(&format!("Serialize error: {e}")))
    }
}

// -------------------------------------------------------------------------------------------------
// Tests (basic smoke)
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFs {
        map: HashMap<String, String>,
    }
    impl TestFs {
        fn new(map: &[(&str, &str)]) -> Self {
            Self {
                map: map
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
    }
    impl FileProvider for TestFs {
        fn read_to_string(&self, path: &str) -> Result<String> {
            self.map
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("not found: {path}"))
        }
        fn exists(&self, path: &str) -> bool {
            self.map.contains_key(path)
        }
        fn is_file(&self, path: &str) -> bool {
            self.exists(path)
        }
        fn join(&self, parent: &str, rel: &str) -> String {
            if parent.is_empty() {
                rel.to_string()
            } else {
                format!(
                    "{}/{}",
                    parent.trim_end_matches('/'),
                    rel.trim_start_matches('/')
                )
            }
        }
        fn extension_lowercase(&self, path: &str) -> Option<String> {
            path.rsplit('/')
                .next()
                .and_then(|f| f.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()))
        }
        fn parent(&self, path: &str) -> Option<String> {
            match path.rsplit_once('/') {
                Some((dir, _)) => Some(dir.to_string()),
                None => Some(String::new()),
            }
        }
        fn file_name(&self, path: &str) -> Option<String> {
            Some(path.rsplit('/').next().unwrap_or(path).to_string())
        }
    }

    // Helper to extract all hrefs from a snippet
    fn hrefs(html: &str) -> Vec<String> {
        let mut v = Vec::new();
        let re = regex::Regex::new(r#"href="([^"]+)""#).unwrap();
        for cap in re.captures_iter(html) {
            v.push(cap[1].to_string());
        }
        v
    }

    // Helper: assert that every expected href appears at least once
    fn assert_hrefs_contains(html: &str, expected: &[&str]) {
        let list = hrefs(html);
        for e in expected {
            assert!(
                list.iter().any(|h| h == e),
                "Expected href '{}' not found in {:?}",
                e,
                list
            );
        }
    }

    #[test]
    fn smoke_single() {
        let fs = TestFs::new(&[(
            "entry.md",
            r#"---
title: Test Doc
author: Someone
created: 2025-08-25T10:00:00Z
updated: 2025-08-25T10:00:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
---
Hello **world**.
"#,
        )]);
        let artifacts = build_site(
            "entry.md",
            CoreBuildOptions {
                rewrite_links: true,
                ..Default::default()
            },
            &fs,
        )
        .expect("build ok");
        assert_eq!(artifacts.pages.len(), 1);
        assert!(artifacts.pages[0].html.contains("<strong>world</strong>"));
        assert_eq!(artifacts.pages[0].file_name, "index.html");
        // metadata should contain the CommonMark link converted to anchor
        assert!(
            artifacts.pages[0]
                .metadata_html
                .contains(r#"<a href="https://spec.commonmark.org/">CommonMark</a>"#)
        );
    }

    #[test]
    fn flat_alias_and_part_of_links() {
        // Flat multi-page: no pages/ prefix
        let fs = TestFs::new(&[
            (
                "root.md",
                r#"---
title: Root Title
author: A
created: 2025-08-25T10:00:00Z
updated: 2025-08-25T10:00:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
this_file_is_root_index: true
contents:
  - "[First Child](child-one.md)"
  - "[Second Child](child-two)"
---
Root body
"#,
            ),
            (
                "child-one.md",
                r#"---
title: First Child
author: A
created: 2025-08-25T10:01:00Z
updated: 2025-08-25T10:01:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
part_of:
  - "[Root Alias](root.md)"
---
Child one body referencing [Second](child-two.md) and [Root](root.md).
"#,
            ),
            (
                "child-two.md",
                r#"---
title: Second Child
author: A
created: 2025-08-25T10:02:00Z
updated: 2025-08-25T10:02:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
part_of: "[Root Alias](root.md)"
---
Child two body referencing [First](child-one.md) and [Root](root.md).
"#,
            ),
        ]);
        let artifacts = build_site(
            "root.md",
            CoreBuildOptions {
                rewrite_links: true,
                flat: true,
                ..Default::default()
            },
            &fs,
        )
        .expect("build ok");
        assert_eq!(artifacts.pages.len(), 3);
        let root = artifacts.pages.iter().find(|p| p.is_root_index).unwrap();
        // Root metadata should show alias labels, not slugs
        assert!(
            root.metadata_html
                .contains(r#"<a href="child-one.html">First Child</a>"#),
            "Flat root should link to child-one.html with alias label: got {}",
            root.metadata_html
        );
        assert!(
            root.metadata_html
                .contains(r#"<a href="child-two.html">Second Child</a>"#),
            "Flat root should link to child-two.html with alias label: got {}",
            root.metadata_html
        );
        // Child part_of alias link
        let child_one = artifacts
            .pages
            .iter()
            .find(|p| p.title == "First Child")
            .unwrap();
        assert!(
            child_one
                .metadata_html
                .contains(r#"<a href="index.html">Root Alias</a>"#),
            "Flat child_one part_of should show alias 'Root Alias': {}",
            child_one.metadata_html
        );
        // Body link rewrite (flat)
        assert!(child_one.html.contains(r#"href="child-two.html""#));
    }

    #[test]
    fn nested_alias_and_layout_links() {
        // Nested multi-page layout (flat=false)
        let fs = TestFs::new(&[
            (
                "index.md",
                r#"---
title: Root
author: P
created: 2025-08-25T10:00:00Z
updated: 2025-08-25T10:00:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
this_file_is_root_index: true
contents:
  - "[Alpha Entry](alpha.md)"
  - "[Beta Entry](beta)"
---
Root body linking to [Alpha](alpha.md) and [Beta](beta.md).
"#,
            ),
            (
                "alpha.md",
                r#"---
title: Alpha Entry
author: P
created: 2025-08-25T10:01:00Z
updated: 2025-08-25T10:01:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
part_of: "[Root Label](index.md)"
---
Alpha body linking back to [Root](index.md) and to [Beta](beta.md).
"#,
            ),
            (
                "beta.md",
                r#"---
title: Beta Entry
author: P
created: 2025-08-25T10:02:00Z
updated: 2025-08-25T10:02:00Z
visibility: public
format: "[CommonMark](https://spec.commonmark.org/)"
part_of:
  - "[Root Label](index.md)"
  - "[Alpha Alias](alpha.md)"
---
Beta body linking back to [Root](index.md) and to [Alpha](alpha.md).
"#,
            ),
        ]);
        let artifacts = build_site(
            "index.md",
            CoreBuildOptions {
                rewrite_links: true,
                flat: false,
                ..Default::default()
            },
            &fs,
        )
        .expect("build ok");
        assert_eq!(artifacts.pages.len(), 3);
        let root = artifacts.pages.iter().find(|p| p.is_root_index).unwrap();
        // Root metadata: pages/ prefix for children
        assert!(
            root.metadata_html
                .contains(r#"<a href="pages/alpha.html">Alpha Entry</a>"#),
            "Nested root should have pages/alpha.html alias link (slug from filename stem): {}",
            root.metadata_html
        );
        assert!(
            root.metadata_html
                .contains(r#"<a href="pages/beta.html">Beta Entry</a>"#),
            "Nested root should have pages/beta.html alias link (slug from filename stem): {}",
            root.metadata_html
        );
        // Alpha metadata: part_of root ../index OR index? (Since alpha is nested; root link should be ../index.html in body, index.html in metadata alias depends on design)
        let alpha = artifacts
            .pages
            .iter()
            .find(|p| p.title == "Alpha Entry")
            .unwrap();
        assert!(
            alpha
                .metadata_html
                .contains(r#"<a href="../index.html">Root Label</a>"#),
            "Nested alpha part_of should use alias 'Root Label': {}",
            alpha.metadata_html
        );
        // Alpha body link to Beta sibling (nested) should be beta.html (slug from filename stem)
        assert!(alpha.html.contains(r#"href="beta.html""#));
        // Root body link to Alpha should have pages/ prefix (filename-stem slug)
        assert!(root.html.contains(r#"href="pages/alpha.html""#));
        // Beta metadata part_of includes Root Label and Alpha Alias
        let beta = artifacts
            .pages
            .iter()
            .find(|p| p.title == "Beta Entry")
            .unwrap();
        assert!(
            beta.metadata_html
                .contains(r#"<a href="../index.html">Root Label</a>"#),
            "Nested beta part_of should use alias 'Root Label': {}",
            beta.metadata_html
        );
        assert!(
            beta.metadata_html
                .contains(r#"<a href="../pages/alpha.html">Alpha Alias</a>"#),
            "Nested beta part_of should link to alpha alias with ../pages/ prefix (filename stem slug): {}",
            beta.metadata_html
        );
        // Beta body link back to Root
        assert!(beta.html.contains(r#"href="../index.html""#));
    }
}
