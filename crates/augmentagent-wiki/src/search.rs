//! Bounded literal discovery of context pages. Hits locate pages; callers must
//! read the source before treating its contents as evidence.
use serde::Serialize;
use std::{io::Read, path::Path};

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub paths: Vec<String>,
    pub incomplete: bool,
}

pub fn search(root: &Path, query: &str) -> std::io::Result<SearchResult> {
    let root = root.canonicalize()?;
    let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Wiki search requires a topic",
        ));
    }
    let mut pending: Vec<_> = ["about", "people", "projects", "threads"]
        .iter()
        .map(|p| root.join(p))
        .collect();
    let mut result = SearchResult {
        paths: Vec::new(),
        incomplete: false,
    };
    let mut entries = 0;
    let mut bytes = 0;
    while let Some(path) = pending.pop() {
        entries += 1;
        if entries > 5_000 || bytes >= 8_000_000 || result.paths.len() >= 20 {
            result.incomplete = true;
            break;
        }
        // Do not follow symlinks, including directories escaping the wiki.
        let metadata = match path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            match std::fs::read_dir(&path) {
                Ok(children) => {
                    let mut paths = Vec::new();
                    for child in children.take(5_001) {
                        match child {
                            Ok(c) => paths.push(c.path()),
                            Err(_) => result.incomplete = true,
                        }
                    }
                    paths.sort();
                    pending.extend(paths.into_iter().rev());
                }
                Err(_) => result.incomplete = true,
            }
        } else if metadata.is_file() && path.extension().is_some_and(|x| x == "md") {
            let mut raw = Vec::new();
            match std::fs::File::open(&path).and_then(|f| f.take(128_001).read_to_end(&mut raw)) {
                Ok(_) => {
                    bytes += raw.len();
                    if raw.len() > 128_000 {
                        raw.truncate(128_000);
                        result.incomplete = true;
                    }
                    let relative = path
                        .strip_prefix(&root)
                        .expect("scoped path")
                        .to_string_lossy()
                        .into_owned();
                    let haystack =
                        format!("{}\n{}", relative, String::from_utf8_lossy(&raw)).to_lowercase();
                    if terms.iter().all(|term| haystack.contains(term)) {
                        result.paths.push(relative);
                    }
                }
                Err(_) => result.incomplete = true,
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_content_without_returning_bodies_or_following_links() {
        let root = tempfile::tempdir().unwrap();
        for d in ["about", "people", "projects", "threads"] {
            std::fs::create_dir(root.path().join(d)).unwrap();
        }
        std::fs::write(
            root.path().join("projects/launch.md"),
            "The cobalt deadline is Friday.",
        )
        .unwrap();
        std::fs::write(root.path().join("private.md"), "cobalt").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.path().join("private.md"),
            root.path().join("projects/link.md"),
        )
        .unwrap();
        let result = search(root.path(), "COBALT deadline").unwrap();
        assert_eq!(result.paths, ["projects/launch.md"]);
        assert!(!result.incomplete);
        assert!(search(root.path(), " ").is_err());
    }
}
