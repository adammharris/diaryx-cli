/*
 * diaryx-cli
 * Main entry point.
 *
 * This binary currently supports the `build` subcommand, which converts a single
 * Diaryx Markdown file (and, if it is a root index, its recursively referenced
 * contents) into a static HTML site.
 *
 * Copyright:
 *   Code: CC-BY-SA-4.0 (adjust later if you decide to separate code/spec licensing)
 */

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
mod build;

/// Diaryx CLI – utilities for working with Diaryx-formatted Markdown files.
///
/// Current focus: `build` subcommand.
/// Future: `schema`, `validate`, `watch`, exports, etc.
#[derive(Parser, Debug)]
#[command(name = "diaryx", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build a static HTML site from a single Diaryx file.
    ///
    /// If the file has `this_file_is_root_index: true`, recursively traverse its `contents`
    /// lists (and nested index files) to build a multi-page site. Otherwise, produce a single
    /// page site for just that file (plus attachments).
    Build(BuildArgs),
}

/// Arguments for the `build` subcommand.
#[derive(Args, Debug)]
struct BuildArgs {
    /// Entry Diaryx Markdown file (required).
    #[arg(long, value_name = "FILE")]
    input: PathBuf,

    /// Output directory (will be created or replaced).
    #[arg(long, default_value = "./site", value_name = "DIR")]
    output: PathBuf,

    /// Include non-public files (those whose visibility does NOT include `public`).
    /// By default only public-visible files are emitted.
    #[arg(long)]
    include_nonpublic: bool,

    /// Emit an intermediate JSON model (diaryx-data.json).
    #[arg(long)]
    emit_json: bool,

    /// Emit all pages directly in the output directory (no pages/ subfolder in multi-page mode).
    #[arg(long)]
    flat: bool,

    /// Verbose logging (prints warnings/progress to stderr).
    #[arg(long)]
    verbose: bool,

    /// Treat warnings as errors (fail the build if any warning occurs).
    #[arg(long)]
    strict: bool,
}

/// Public-facing build options passed to the build layer.
/// (Kept minimal here; expand as the build subsystem grows.)
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub include_nonpublic: bool,
    pub emit_json: bool,
    pub flat: bool,
    pub verbose: bool,
    pub strict: bool,
}

impl BuildOptions {
    fn from_args(a: &BuildArgs) -> Result<Self> {
        if !a.input.exists() {
            bail!("Input file does not exist: {}", a.input.display());
        }
        if !a.input.is_file() {
            bail!("Input must be a file: {}", a.input.display());
        }
        Ok(Self {
            input: a
                .input
                .canonicalize()
                .with_context(|| "Failed to canonicalize input path")?,
            output: a.output.clone(),
            include_nonpublic: a.include_nonpublic,
            emit_json: a.emit_json,
            flat: a.flat,
            verbose: a.verbose,
            strict: a.strict,
        })
    }
}

// --- Build module interface (to be implemented in src/build/...) -----------------------------
// We declare a minimal interface the rest of the codebase should implement.
// This keeps main.rs stable even as internal module structure evolves.
mod build_interface {
    use super::BuildOptions;
    use anyhow::Result;

    /// Placeholder trait / function signatures for the actual build implementation.
    /// The real implementation should live under `src/build/` and expose a `run_build` function.
    #[allow(unused_variables)]
    pub fn run_build(opts: BuildOptions) -> Result<()> {
        crate::build::run_build(opts)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Build(args) => {
            let opts = BuildOptions::from_args(&args)?;
            if opts.verbose {
                eprintln!(
                    "[diaryx] Building site\n  input: {}\n  output: {}\n  include_nonpublic: {}\n  emit_json: {}\n  flat: {}\n  strict: {}",
                    opts.input.display(),
                    opts.output.display(),
                    opts.include_nonpublic,
                    opts.emit_json,
                    opts.flat,
                    opts.strict
                );
            }
            // Call into the (to-be-implemented) build system.
            if let Err(e) = build_interface::run_build(opts.clone()) {
                if opts.verbose {
                    eprintln!("[diaryx] build failed: {:#}", e);
                }
                return Err(e);
            }
            if opts.verbose {
                eprintln!("[diaryx] build complete");
            }
        }
    }

    Ok(())
}
