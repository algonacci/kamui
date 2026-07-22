use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const INSTRUCTION_FILES: [&str; 2] = ["KAMUI.md", "AGENTS.md"];
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_CONTEXT_BYTES: usize = 128 * 1024;

pub struct ProjectContext {
    root: PathBuf,
    instructions: Option<(String, String)>,
}

impl ProjectContext {
    pub fn discover() -> Result<Self> {
        Self::from_root(std::env::current_dir().context("could not determine working directory")?)
    }

    fn from_root(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to access {}", root.display()))?;
        let instructions = INSTRUCTION_FILES
            .iter()
            .find_map(|name| {
                let path = root.join(name);
                path.is_file().then_some((*name, path))
            })
            .map(|(name, path)| read_text_file(&path).map(|content| (name.to_string(), content)))
            .transpose()?;

        Ok(Self { root, instructions })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn instruction_name(&self) -> Option<&str> {
        self.instructions.as_ref().map(|(name, _)| name.as_str())
    }

    pub fn system_message(&self) -> Option<String> {
        self.instructions.as_ref().map(|(name, content)| {
            format!(
                "Follow the project instructions from {name} for this conversation:\n\n{content}"
            )
        })
    }

    pub fn expand_file_references(&self, input: &str) -> Result<String> {
        let references = file_references(input);
        if references.is_empty() {
            return Ok(input.to_string());
        }

        let mut total_bytes = 0;
        let mut context = String::new();
        for reference in references {
            let (label, content) = match reference.as_str() {
                "diff" => ("git diff".to_string(), self.read_git_diff(false)?),
                "staged" => ("git diff --staged".to_string(), self.read_git_diff(true)?),
                _ => (
                    reference.clone(),
                    read_project_file(&self.root, &reference)?,
                ),
            };

            total_bytes += content.len();
            if total_bytes > MAX_CONTEXT_BYTES {
                anyhow::bail!("attached context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
            }
            context.push_str(&format!(
                "\n\n<context source=\"{label}\">\n{content}\n</context>"
            ));
        }

        Ok(format!("{input}\n\nAttached project context:{context}"))
    }

    fn read_git_diff(&self, staged: bool) -> Result<String> {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args(["diff", "--no-ext-diff", "--no-color"]);
        if staged {
            command.arg("--cached");
        }

        let output = command.output().context("failed to run git diff")?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("git diff failed: {error}");
        }
        let diff = String::from_utf8(output.stdout).context("git diff output is not UTF-8")?;
        Ok(if diff.is_empty() {
            "(no changes)".to_string()
        } else {
            diff
        })
    }
}

fn file_references(input: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    input
        .split_whitespace()
        .filter_map(|word| {
            let reference = word.strip_prefix('@')?.trim_matches(|character: char| {
                matches!(character, ',' | ';' | ':' | ')' | ']' | '}')
            });
            (!reference.is_empty() && seen.insert(reference.to_string()))
                .then(|| reference.to_string())
        })
        .collect()
}

/// Resolve a project-relative reference to a real path inside the project root, rejecting absolute
/// paths and anything that escapes the root once symlinks are resolved. Shared by `@file` expansion
/// and the read-only tools so path safety lives in one place.
fn resolve_within_root(root: &Path, reference: &str) -> Result<PathBuf> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        anyhow::bail!("path must be relative to the project: {reference}");
    }

    let path = root.join(relative).canonicalize().with_context(|| {
        format!(
            "could not access {reference} relative to {}",
            root.display()
        )
    })?;
    if !path.starts_with(root) {
        anyhow::bail!("path is outside the project: {reference}");
    }

    Ok(path)
}

/// Read a UTF-8 text file identified by a project-relative reference.
pub fn read_project_file(root: &Path, reference: &str) -> Result<String> {
    let path = resolve_within_root(root, reference)?;
    if !path.is_file() {
        anyhow::bail!("path is not a file: {reference}");
    }

    read_text_file(&path)
}

/// List the entries of a project-relative directory. Directories are shown with a trailing slash and
/// sorted first; the `.git` directory is skipped to reduce noise. Use `.` for the project root.
pub fn list_project_directory(root: &Path, reference: &str) -> Result<String> {
    let path = resolve_within_root(root, reference)?;
    if !path.is_dir() {
        anyhow::bail!("path is not a directory: {reference}");
    }

    let mut entries: Vec<(bool, String)> = Vec::new();
    for entry in fs::read_dir(&path).with_context(|| format!("could not list {reference}"))? {
        let entry = entry.with_context(|| format!("could not read an entry in {reference}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        entries.push((is_dir, name));
    }
    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    // Directories first, then files, each alphabetically.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let listing = entries
        .into_iter()
        .map(|(is_dir, name)| if is_dir { format!("{name}/") } else { name })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(listing)
}

fn read_text_file(path: &Path) -> Result<String> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        anyhow::bail!("{} exceeds the 64 KiB file limit", path.display());
    }
    fs::read_to_string(path)
        .with_context(|| format!("{} is not a readable UTF-8 text file", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn project() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-context-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn loads_kamui_instructions_before_agents() {
        let root = project();
        fs::write(root.join("KAMUI.md"), "Use Rust.").unwrap();
        fs::write(root.join("AGENTS.md"), "Use Go.").unwrap();

        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), Some("KAMUI.md"));
        assert!(context.system_message().unwrap().contains("Use Rust."));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_each_file_reference_once() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context
            .expand_file_references("Explain @src/main.rs and @src/main.rs")
            .unwrap();

        assert_eq!(prompt.matches("<context source=").count(), 1);
        assert!(prompt.contains("fn main() {}"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_staged_git_diff() {
        let root = project();
        fs::write(root.join("file.txt"), "hello\n").unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context.expand_file_references("Review @staged").unwrap();

        assert!(prompt.contains("<context source=\"git diff --staged\">"));
        assert!(prompt.contains("+hello"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_prompts_without_references_unchanged() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.expand_file_references("hello").unwrap(), "hello");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();
        let prompt = format!("Read @{}", root.join("main.rs").display());

        let error = context.expand_file_references(&prompt).unwrap_err();
        assert!(error.to_string().contains("relative"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_directory_references() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let error = context.expand_file_references("Read @src").unwrap_err();
        assert!(error.to_string().contains("not a file"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_missing_files() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert!(context.expand_file_references("Read @nope.rs").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_files_over_the_size_limit() {
        let root = project();
        fs::write(root.join("big.txt"), vec![b'a'; 65 * 1024]).unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let error = context.expand_file_references("Read @big.txt").unwrap_err();
        assert!(error.to_string().contains("64 KiB"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_unstaged_git_diff() {
        let root = project();
        fs::write(root.join("file.txt"), "hello\n").unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        // Modify the tracked file so it differs from the index without a commit.
        fs::write(root.join("file.txt"), "hello\nworld\n").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context.expand_file_references("Review @diff").unwrap();

        assert!(prompt.contains("<context source=\"git diff\">"));
        assert!(prompt.contains("+world"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn falls_back_to_agents_when_kamui_absent() {
        let root = project();
        fs::write(root.join("AGENTS.md"), "Use Go.").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), Some("AGENTS.md"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_no_instructions_when_none_present() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), None);
        assert!(context.system_message().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_references_strip_punctuation_and_deduplicate() {
        let refs = file_references("see @a.rs, and @b.rs; also @a.rs and a bare @");
        assert_eq!(refs, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn lists_directory_entries_within_the_project() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("a.txt"), "x").unwrap();
        let canonical = root.canonicalize().unwrap();

        let listing = list_project_directory(&canonical, ".").unwrap();

        assert!(listing.contains("src/"));
        assert!(listing.contains("a.txt"));
        assert!(listing.find("src/").unwrap() < listing.find("a.txt").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_rejects_a_file_path() {
        let root = project();
        fs::write(root.join("a.txt"), "x").unwrap();
        let canonical = root.canonicalize().unwrap();

        let error = list_project_directory(&canonical, "a.txt").unwrap_err();
        assert!(error.to_string().contains("not a directory"));
        fs::remove_dir_all(root).unwrap();
    }
}
