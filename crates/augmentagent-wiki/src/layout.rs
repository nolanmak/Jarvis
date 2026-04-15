//! On-disk layout for the wiki. Paths only — no IO for mutations.

use std::path::PathBuf;

use crate::slug::slug_from_email;

/// Resolved paths for a wiki rooted at a given directory.
///
/// The wiki root is expected to exist; this struct doesn't create it.
/// Callers that want to lazily bootstrap an empty wiki can call
/// [`WikiLayout::bootstrap`] once at startup.
#[derive(Debug, Clone)]
pub struct WikiLayout {
    pub root: PathBuf,
}

impl WikiLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn index(&self) -> PathBuf {
        self.root.join("index.md")
    }

    pub fn log(&self) -> PathBuf {
        self.root.join("log.md")
    }

    pub fn people_dir(&self) -> PathBuf {
        self.root.join("people")
    }

    pub fn threads_dir(&self) -> PathBuf {
        self.root.join("threads")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// Page path for a sender email address. Deterministic.
    pub fn person_page(&self, email: &str) -> PathBuf {
        self.people_dir()
            .join(format!("{}.md", slug_from_email(email)))
    }

    /// Page path for a thread id.
    pub fn thread_page(&self, thread_id: &str) -> PathBuf {
        self.threads_dir()
            .join(format!("{}.md", sanitize(thread_id)))
    }

    /// Page path for a project slug (caller-provided, already normalized).
    pub fn project_page(&self, slug: &str) -> PathBuf {
        self.projects_dir().join(format!("{}.md", sanitize(slug)))
    }

    /// Ensure the directory tree exists and seed index.md + log.md if missing.
    /// Idempotent — safe to call every startup.
    pub fn bootstrap(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.people_dir())?;
        std::fs::create_dir_all(self.threads_dir())?;
        std::fs::create_dir_all(self.projects_dir())?;
        if !self.index().exists() {
            std::fs::write(
                self.index(),
                "# Wiki Index\n\n*Auto-maintained by AugmentAgent. Do not edit by hand.*\n\n## People\n\n## Threads\n\n## Projects\n",
            )?;
        }
        if !self.log().exists() {
            std::fs::write(
                self.log(),
                "# Wiki Log\n\n*Append-only. One entry per ingest event, reverse-chronological.*\n\n",
            )?;
        }
        Ok(())
    }
}

/// Collapse any path-unsafe characters to `_`. Used for thread ids and project slugs.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn td() -> (TempDir, WikiLayout) {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        (d, layout)
    }

    #[test]
    fn bootstrap_creates_dirs_and_seed_files() {
        let (_d, layout) = td();
        layout.bootstrap().unwrap();
        assert!(layout.people_dir().is_dir());
        assert!(layout.threads_dir().is_dir());
        assert!(layout.projects_dir().is_dir());
        assert!(layout.index().is_file());
        assert!(layout.log().is_file());
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let (_d, layout) = td();
        layout.bootstrap().unwrap();
        let index_before = std::fs::read_to_string(layout.index()).unwrap();
        std::fs::write(layout.index(), "custom content").unwrap();
        layout.bootstrap().unwrap();
        // Second bootstrap should NOT overwrite existing index.
        let index_after = std::fs::read_to_string(layout.index()).unwrap();
        assert_eq!(index_after, "custom content");
        let _ = index_before;
    }

    #[test]
    fn person_page_is_deterministic() {
        let (_d, layout) = td();
        let a = layout.person_page("Jeremy Doe <jeremy.doe@example.com>");
        let b = layout.person_page("Jeremy Doe <jeremy.doe@example.com>");
        assert_eq!(a, b);
        assert_eq!(
            a.file_name().unwrap().to_string_lossy(),
            "jeremy_doe_at_example_com.md"
        );
    }

    #[test]
    fn thread_page_sanitizes() {
        let (_d, layout) = td();
        let p = layout.thread_page("abc/123:weird");
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "abc_123_weird.md");
    }

    #[test]
    fn project_page_sanitizes() {
        let (_d, layout) = td();
        let p = layout.project_page("Q2 launch!");
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "Q2_launch_.md");
    }
}
