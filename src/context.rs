use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

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
            let relative = Path::new(&reference);
            if relative.is_absolute() {
                anyhow::bail!("@file path must be relative to the project: {reference}");
            }

            let path = self.root.join(relative).canonicalize().with_context(|| {
                format!(
                    "could not read @{reference} relative to {}",
                    self.root.display()
                )
            })?;
            if !path.starts_with(&self.root) {
                anyhow::bail!("@file path is outside the project: {reference}");
            }
            if !path.is_file() {
                anyhow::bail!("@file path is not a file: {reference}");
            }

            let content = read_text_file(&path)?;
            total_bytes += content.len();
            if total_bytes > MAX_CONTEXT_BYTES {
                anyhow::bail!("@file context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
            }
            context.push_str(&format!(
                "\n\n<file path=\"{}\">\n{}\n</file>",
                relative.display(),
                content
            ));
        }

        Ok(format!("{input}\n\nAttached project files:{context}"))
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

        assert_eq!(prompt.matches("<file path=").count(), 1);
        assert!(prompt.contains("fn main() {}"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_prompts_without_references_unchanged() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.expand_file_references("hello").unwrap(), "hello");
        fs::remove_dir_all(root).unwrap();
    }
}
