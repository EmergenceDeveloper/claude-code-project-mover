use anyhow::{bail, Context};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "claude-code-project-mover")]
#[command(about = "Move Claude Code project data when a project's filesystem path changes")]
struct Cli {
    /// The old project path (e.g. "C:\\Projects\\MyApp")
    old_path: String,

    /// The new project path (e.g. "D:\\Work\\MyApp")
    new_path: String,

    /// Show what would change without doing it
    #[arg(long)]
    dry_run: bool,

    /// If the destination already exists, back it up with --backup suffix and overwrite
    #[arg(long)]
    force: bool,
}

/// Encode a filesystem path into Claude Code's project folder name.
/// e.g. "C:\Projects\MyApp" -> "C--Projects-MyApp"
/// Replaces ':', '\', '/', and ' ' with '-'.
fn encode_path(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            ':' | '\\' | '/' | ' ' => '-',
            _ => c,
        })
        .collect()
}

struct MoveRequest {
    old_path: String,
    new_path: String,
}

struct MoveManifest {
    old_encoded: String,
    new_encoded: String,
    old_folder: PathBuf,
    new_folder: PathBuf,
    old_cwd_escaped: String,
    new_cwd_escaped: String,
}

fn find_projects_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .context("Could not determine home directory (checked USERPROFILE and HOME)")?;
    let projects_dir = PathBuf::from(home).join(".claude").join("projects");
    if !projects_dir.is_dir() {
        bail!("Projects directory not found: {}", projects_dir.display());
    }
    Ok(projects_dir)
}

/// JSON-escape a Windows path: backslashes become double-backslashes.
fn json_escape_path(path: &str) -> String {
    path.replace('\\', "\\\\")
}

fn build_manifest(request: &MoveRequest, force: bool) -> anyhow::Result<MoveManifest> {
    let projects_dir = find_projects_dir()?;
    let old_encoded = encode_path(&request.old_path);
    let new_encoded = encode_path(&request.new_path);
    let old_folder = projects_dir.join(&old_encoded);
    let new_folder = projects_dir.join(&new_encoded);

    if !old_folder.is_dir() {
        bail!(
            "Old project folder not found: {}\nEncoded as: {}",
            old_folder.display(),
            old_encoded
        );
    }
    if new_folder.exists() {
        if force {
            let backup = projects_dir.join(format!("{}--backup", new_encoded));
            if backup.exists() {
                bail!(
                    "Backup folder already exists: {}\nRemove it manually before using --force again",
                    backup.display()
                );
            }
            println!("Backing up existing destination:");
            println!("  {} -> {}", new_folder.display(), backup.display());
            std::fs::rename(&new_folder, &backup)
                .with_context(|| format!(
                    "Failed to back up {} -> {}",
                    new_folder.display(),
                    backup.display()
                ))?;
        } else {
            bail!(
                "New project folder already exists: {}\nEncoded as: {}\nUse --force to back it up and overwrite",
                new_folder.display(),
                new_encoded
            );
        }
    }

    Ok(MoveManifest {
        old_encoded,
        new_encoded,
        old_folder,
        new_folder,
        old_cwd_escaped: json_escape_path(&request.old_path),
        new_cwd_escaped: json_escape_path(&request.new_path),
    })
}

/// Walk the project folder and collect all files that need path transformation.
/// Targets: *.jsonl (top-level and in subagent subdirs), sessions-index.json
fn collect_target_files(folder: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in std::fs::read_dir(folder).context("Failed to read project folder")? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".jsonl") || name == "sessions-index.json" {
                files.push(path);
            }
        } else if path.is_dir() {
            // Check for subagent dirs (UUID folders containing subagents/)
            let subagents = path.join("subagents");
            if subagents.is_dir() {
                for sub_entry in std::fs::read_dir(&subagents)? {
                    let sub_path = sub_entry?.path();
                    if sub_path.is_file()
                        && sub_path
                            .extension()
                            .is_some_and(|ext| ext == "jsonl")
                    {
                        files.push(sub_path);
                    }
                }
            }
        }
    }

    files.sort();
    Ok(files)
}

struct FileResult {
    path: PathBuf,
    replacements: usize,
}

struct MoveReport {
    files_updated: Vec<FileResult>,
}

/// Replace all occurrences of old path references with new ones in file content.
/// Two passes: (1) JSON-escaped cwd paths, (2) encoded folder names.
/// Returns the transformed content and total replacement count.
fn transform_content(content: &str, manifest: &MoveManifest) -> (String, usize) {
    let mut count = 0;

    // Pass 1: Replace JSON-escaped cwd paths (e.g. "C:\\Projects\\MyApp" -> "D:\\Work\\MyApp")
    let cwd_matches = content.matches(&manifest.old_cwd_escaped).count();
    count += cwd_matches;
    let result = content.replace(&manifest.old_cwd_escaped, &manifest.new_cwd_escaped);

    // Pass 2: Replace encoded folder names (e.g. "C--Projects-MyApp" -> "D--Work-MyApp")
    // in trackedFileBackups keys and fullPath values
    let encoded_matches = result.matches(&manifest.old_encoded).count();
    count += encoded_matches;
    let result = result.replace(&manifest.old_encoded, &manifest.new_encoded);

    (result, count)
}

fn dry_run(manifest: &MoveManifest) -> anyhow::Result<()> {
    println!("=== DRY RUN ===\n");
    println!("Folder rename:");
    println!("  {} -> {}", manifest.old_folder.display(), manifest.new_folder.display());
    println!();

    let files = collect_target_files(&manifest.old_folder)?;
    println!("Files to transform: {}", files.len());

    let mut total_replacements = 0;
    for file in &files {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read {}", file.display()))?;
        let (_, count) = transform_content(&content, manifest);
        if count > 0 {
            let relative = file.strip_prefix(&manifest.old_folder).unwrap_or(file);
            println!("  {} ({} replacements)", relative.display(), count);
        }
        total_replacements += count;
    }

    println!("\nTotal replacements: {}", total_replacements);
    Ok(())
}

fn execute_move(manifest: &MoveManifest) -> anyhow::Result<MoveReport> {
    let files = collect_target_files(&manifest.old_folder)?;
    let mut results = Vec::new();

    // Transform all file contents first (safe: folder still at old name if this fails)
    for file in &files {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read {}", file.display()))?;
        let (transformed, count) = transform_content(&content, manifest);
        if count > 0 {
            std::fs::write(file, transformed)
                .with_context(|| format!("Failed to write {}", file.display()))?;
        }
        results.push(FileResult {
            path: file.to_owned(),
            replacements: count,
        });
    }

    // Rename the folder last
    std::fs::rename(&manifest.old_folder, &manifest.new_folder)
        .with_context(|| format!(
            "Failed to rename {} -> {}",
            manifest.old_folder.display(),
            manifest.new_folder.display()
        ))?;

    Ok(MoveReport {
        files_updated: results,
    })
}

fn print_report(report: &MoveReport, manifest: &MoveManifest) {
    let total: usize = report.files_updated.iter().map(|f| f.replacements).sum();
    let changed_count = report.files_updated.iter().filter(|f| f.replacements > 0).count();

    println!("Done!\n");
    println!("Folder renamed: {} -> {}",
        manifest.old_folder.display(),
        manifest.new_folder.display(),
    );
    println!("Files updated: {} / {}", changed_count, report.files_updated.len());
    for f in &report.files_updated {
        if f.replacements > 0 {
            let relative = f.path.strip_prefix(&manifest.old_folder).unwrap_or(&f.path);
            println!("  {} ({} replacements)", relative.display(), f.replacements);
        }
    }
    println!("Total replacements: {}", total);
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let request = MoveRequest {
        old_path: cli.old_path,
        new_path: cli.new_path,
    };
    let manifest = build_manifest(&request, cli.force)?;

    if cli.dry_run {
        dry_run(&manifest)?;
    } else {
        let report = execute_move(&manifest)?;
        print_report(&report, &manifest);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> MoveManifest {
        MoveManifest {
            old_encoded: "C--Projects-MyApp".to_owned(),
            new_encoded: "D--Work-MyApp".to_owned(),
            old_folder: PathBuf::from("unused"),
            new_folder: PathBuf::from("unused"),
            old_cwd_escaped: r"C:\\Projects\\MyApp".to_owned(),
            new_cwd_escaped: r"D:\\Work\\MyApp".to_owned(),
        }
    }

    #[test]
    fn test_encode_path() {
        assert_eq!(
            encode_path(r"C:\Projects\MyApp"),
            "C--Projects-MyApp"
        );
        assert_eq!(
            encode_path(r"C:\Users\dev\Code\MyProject"),
            "C--Users-dev-Code-MyProject"
        );
        assert_eq!(
            encode_path(r"D:\Work\client-sites\acme-corp"),
            "D--Work-client-sites-acme-corp"
        );
        assert_eq!(
            encode_path(r"C:\Projects"),
            "C--Projects"
        );
        assert_eq!(
            encode_path(r"D:\Work\MyApp"),
            "D--Work-MyApp"
        );
    }

    #[test]
    fn test_transform_cwd() {
        let m = test_manifest();
        let input = r#"{"type":"user","cwd":"C:\\Projects\\MyApp","message":"hello"}"#;
        let (output, count) = transform_content(input, &m);
        assert_eq!(
            output,
            r#"{"type":"user","cwd":"D:\\Work\\MyApp","message":"hello"}"#
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn test_transform_encoded_folder_name() {
        let m = test_manifest();
        let input = r#""C:\\Users\\dev\\.claude\\projects\\C--Projects-MyApp\\memory\\MEMORY.md""#;
        let (output, count) = transform_content(input, &m);
        assert_eq!(
            output,
            r#""C:\\Users\\dev\\.claude\\projects\\D--Work-MyApp\\memory\\MEMORY.md""#
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn test_transform_multiple_occurrences() {
        let m = test_manifest();
        let input = r#"{"cwd":"C:\\Projects\\MyApp"}
{"cwd":"C:\\Projects\\MyApp"}
{"path":"C--Projects-MyApp\\file"}"#;
        let (_, count) = transform_content(input, &m);
        assert_eq!(count, 3);
    }

    #[test]
    fn test_transform_no_matches() {
        let m = test_manifest();
        let input = r#"{"type":"user","message":"nothing relevant here"}"#;
        let (output, count) = transform_content(input, &m);
        assert_eq!(output, input);
        assert_eq!(count, 0);
    }
}
