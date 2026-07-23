use crate::provider::ImageAttachment;
use anyhow::{Context, Result};
use base64::Engine;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const INSTRUCTION_FILES: [&str; 2] = ["KAMUI.md", "AGENTS.md"];
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_CONTEXT_BYTES: usize = 128 * 1024;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_DIRECTORY_FILES: usize = 50;

/// A prompt after `@` references are expanded: text context inlined, images carried separately.
#[derive(Debug)]
pub struct Expanded {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

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

    pub fn expand_file_references(&self, input: &str) -> Result<Expanded> {
        let references = file_references(input);
        if references.is_empty() {
            return Ok(Expanded {
                text: input.to_string(),
                images: Vec::new(),
            });
        }

        let mut total_bytes = 0;
        let mut context = String::new();
        let mut images = Vec::new();
        for reference in references {
            // Named sources are not paths, so they are resolved before any filesystem lookup.
            let named = match reference.as_str() {
                "diff" => Some(("git diff".to_string(), self.read_git_diff(false)?)),
                "staged" => Some(("git diff --staged".to_string(), self.read_git_diff(true)?)),
                "clipboard" => match read_clipboard()? {
                    ClipboardContent::Text(text) => Some(("clipboard".to_string(), text)),
                    ClipboardContent::Image(image) => {
                        images.push(image);
                        context.push_str(
                            "\n\n<context source=\"clipboard\">(image attached)</context>",
                        );
                        continue;
                    }
                },
                _ => None,
            };
            if let Some((label, content)) = named {
                total_bytes += content.len();
                if total_bytes > MAX_CONTEXT_BYTES {
                    anyhow::bail!("attached context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
                }
                context.push_str(&format!(
                    "\n\n<context source=\"{label}\">\n{content}\n</context>"
                ));
                continue;
            }

            // Images cannot be inlined as text; they travel as attachments on the message.
            if let Some(media_type) = image_media_type(Path::new(&reference)) {
                images.push(read_project_image(&self.root, &reference, media_type)?);
                context.push_str(&format!(
                    "\n\n<context source=\"{reference}\">(image attached)</context>"
                ));
                continue;
            }

            let path = resolve_within_root(&self.root, &reference)?;
            if path.is_dir() {
                let budget = MAX_CONTEXT_BYTES.saturating_sub(total_bytes);
                let (blocks, used) = read_project_directory(&self.root, &path, &reference, budget)?;
                total_bytes += used;
                context.push_str(&blocks);
                continue;
            }

            let content = read_text_file(&path)?;
            total_bytes += content.len();
            if total_bytes > MAX_CONTEXT_BYTES {
                anyhow::bail!("attached context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
            }
            context.push_str(&format!(
                "\n\n<context source=\"{reference}\">\n{content}\n</context>"
            ));
        }

        Ok(Expanded {
            text: format!("{input}\n\nAttached project context:{context}"),
            images,
        })
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

/// Resolve a project-relative path for writing. Unlike `resolve_within_root`, the file itself may
/// not exist yet, but its parent directory must already exist inside the project root.
pub fn resolve_for_write(root: &Path, reference: &str) -> Result<PathBuf> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        anyhow::bail!("path must be relative to the project: {reference}");
    }

    let joined = root.join(relative);
    let parent = joined
        .parent()
        .with_context(|| format!("path has no parent directory: {reference}"))?;
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "the parent directory of {reference} does not exist in {}",
            root.display()
        )
    })?;
    if !parent.starts_with(root) {
        anyhow::bail!("path is outside the project: {reference}");
    }
    let name = joined
        .file_name()
        .with_context(|| format!("path has no file name: {reference}"))?;
    Ok(parent.join(name))
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

/// Attach the text files inside a project directory, honouring `.gitignore` and skipping hidden
/// files. Files are added in path order until the remaining context budget or the file cap runs
/// out; anything left over (too large, binary, or over budget) is reported rather than failing the
/// whole prompt. Returns the rendered context blocks and how many bytes they consumed.
fn read_project_directory(
    root: &Path,
    directory: &Path,
    reference: &str,
    budget: usize,
) -> Result<(String, usize)> {
    let mut paths: Vec<PathBuf> = ignore::WalkBuilder::new(directory)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Honour .gitignore even when the project is not (yet) a git repository.
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();

    let mut blocks = String::new();
    let mut used = 0;
    let mut attached = 0;
    let mut omitted = 0;
    for path in paths {
        if attached >= MAX_DIRECTORY_FILES {
            omitted += 1;
            continue;
        }
        // Binary or oversized files are skipped, not fatal.
        let Ok(content) = read_text_file(&path) else {
            omitted += 1;
            continue;
        };
        if used + content.len() > budget {
            omitted += 1;
            continue;
        }
        used += content.len();
        attached += 1;
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        blocks.push_str(&format!(
            "\n\n<context source=\"{label}\">\n{content}\n</context>"
        ));
    }

    if attached == 0 && omitted == 0 {
        anyhow::bail!("no attachable text files found in {reference}");
    }
    if omitted > 0 {
        blocks.push_str(&format!(
            "\n\n<context source=\"{reference}\">({omitted} more files omitted: binary, too large, or over the context budget)</context>"
        ));
    }
    Ok((blocks, used))
}

/// Recognize an attachable image by extension, returning its MIME type.
fn image_media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Read a project image and encode it for transport. Uses the same containment checks as text.
fn read_project_image(
    root: &Path,
    reference: &str,
    media_type: &'static str,
) -> Result<ImageAttachment> {
    let path = resolve_within_root(root, reference)?;
    if !path.is_file() {
        anyhow::bail!("path is not a file: {reference}");
    }
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "{reference} exceeds the {} MiB image limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(ImageAttachment {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// What the clipboard held: text if any, otherwise a pasted image (e.g. a screenshot).
enum ClipboardContent {
    Text(String),
    Image(ImageAttachment),
}

/// Read the operating system clipboard, preferring text and falling back to image data so a
/// screenshot can be pasted directly. Errors clearly when the clipboard is unavailable (e.g. a
/// headless session) or holds neither.
fn read_clipboard() -> Result<ClipboardContent> {
    let mut clipboard =
        arboard::Clipboard::new().context("could not access the system clipboard")?;

    if let Ok(text) = clipboard.get_text()
        && !text.trim().is_empty()
    {
        return Ok(ClipboardContent::Text(text));
    }

    match clipboard.get_image() {
        Ok(image) => Ok(ClipboardContent::Image(encode_png(
            image.width,
            image.height,
            &image.bytes,
        )?)),
        Err(_) => anyhow::bail!("the clipboard holds no text or image"),
    }
}

/// Encode raw RGBA pixels as a PNG image attachment.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<ImageAttachment> {
    let width = u32::try_from(width).context("clipboard image is too wide")?;
    let height = u32::try_from(height).context("clipboard image is too tall")?;

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("failed to encode the clipboard image")?;
        writer
            .write_image_data(rgba)
            .context("failed to encode the clipboard image")?;
    }
    if png.len() as u64 > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "the clipboard image exceeds the {} MiB image limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }

    Ok(ImageAttachment {
        media_type: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(png),
    })
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

        assert_eq!(prompt.text.matches("<context source=").count(), 1);
        assert!(prompt.text.contains("fn main() {}"));
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

        assert!(
            prompt
                .text
                .contains("<context source=\"git diff --staged\">")
        );
        assert!(prompt.text.contains("+hello"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_prompts_without_references_unchanged() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(
            context.expand_file_references("hello").unwrap().text,
            "hello"
        );
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
    fn attaches_a_directory_and_honours_ignore_rules() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/keep.rs"), "fn keep() {}").unwrap();
        fs::write(root.join("src/skip.rs"), "fn skip() {}").unwrap();
        fs::write(root.join("src/.gitignore"), "skip.rs\n").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("Review @src").unwrap();

        assert!(expanded.text.contains("fn keep() {}"));
        // The ignored file and the hidden .gitignore itself are left out.
        assert!(!expanded.text.contains("fn skip() {}"));
        assert!(!expanded.text.contains(".gitignore"));
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

        assert!(prompt.text.contains("<context source=\"git diff\">"));
        assert!(prompt.text.contains("+world"));
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
    fn attaches_an_image_reference_as_an_attachment() {
        let root = project();
        fs::write(root.join("shot.png"), b"fake-png-bytes").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("look at @shot.png").unwrap();

        assert_eq!(expanded.images.len(), 1);
        assert_eq!(expanded.images[0].media_type, "image/png");
        assert!(!expanded.images[0].data.is_empty());
        // The text notes the attachment but does not inline the bytes.
        assert!(expanded.text.contains("shot.png"));
        assert!(!expanded.text.contains("fake-png-bytes"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_images_over_the_size_limit() {
        let root = project();
        fs::write(
            root.join("big.jpg"),
            vec![0u8; (MAX_IMAGE_BYTES + 1) as usize],
        )
        .unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let error = context
            .expand_file_references("see @big.jpg")
            .unwrap_err()
            .to_string();

        assert!(error.contains("image limit"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn encodes_clipboard_pixels_into_a_png_attachment() {
        // A single opaque red pixel.
        let attachment = encode_png(1, 1, &[255, 0, 0, 255]).unwrap();

        assert_eq!(attachment.media_type, "image/png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(attachment.data)
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n"); // PNG magic number
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
