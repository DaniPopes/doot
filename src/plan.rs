use crate::store::Store;
use anyhow::Result;
use ignore::gitignore::GitignoreBuilder;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Same,
    Create,
    Overwrite,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub relative_path: PathBuf,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub status: FileStatus,
}

#[derive(Debug)]
pub struct GroupPlan {
    pub group_name: String,
    pub entries: Vec<FileEntry>,
}

impl GroupPlan {
    pub fn has_changes(&self) -> bool {
        self.entries.iter().any(|e| e.status != FileStatus::Same)
    }

    pub fn count_by_status(&self, status: FileStatus) -> usize {
        self.entries.iter().filter(|e| e.status == status).count()
    }
}

#[derive(Debug)]
pub struct Plan {
    pub groups: Vec<GroupPlan>,
}

impl Plan {
    pub fn new() -> Self {
        Self { groups: Vec::new() }
    }

    pub fn add_group(&mut self, group_name: String, entries: Vec<FileEntry>) {
        self.groups.push(GroupPlan {
            group_name,
            entries,
        });
    }

    pub fn has_changes(&self) -> bool {
        self.groups.iter().any(|g| g.has_changes())
    }

    pub fn total_count_by_status(&self, status: FileStatus) -> usize {
        self.groups
            .iter()
            .map(|g| g.count_by_status(status.clone()))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.entries.is_empty())
    }
}

pub struct PlanBuilder<'a> {
    store: &'a dyn Store,
}

impl<'a> PlanBuilder<'a> {
    pub fn new(store: &'a dyn Store) -> Self {
        Self { store }
    }

    pub fn build_import(
        &self,
        group_dir: &Path,
        resolved_path: &Path,
        ignore_file: &Path,
    ) -> Result<Vec<FileEntry>> {
        let mut entries = Vec::new();

        let mut builder = WalkBuilder::new(resolved_path);
        builder.standard_filters(false);
        let mut ignore = GitignoreBuilder::new(resolved_path);
        if ignore_file.exists() {
            if let Some(error) = ignore.add(ignore_file) {
                return Err(error.into());
            }
        }
        let ignore = ignore.build()?;
        builder.filter_entry(move |entry| {
            entry.depth() == 0
                || !ignore
                    .matched(
                        entry.path(),
                        entry.file_type().is_some_and(|ft| ft.is_dir()),
                    )
                    .is_ignore()
        });
        let walker = builder.build();

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let full_path = entry.path();
            let relative = full_path.strip_prefix(resolved_path)?;

            let destination = group_dir.join(relative);
            let status = self.compute_status(full_path, &destination);

            entries.push(FileEntry {
                relative_path: relative.to_path_buf(),
                source: full_path.to_path_buf(),
                destination,
                status,
            });
        }

        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(entries)
    }

    pub fn build_export(&self, group_dir: &Path, resolved_path: &Path) -> Result<Vec<FileEntry>> {
        let mut entries = Vec::new();

        let walker = WalkBuilder::new(group_dir)
            .standard_filters(false)
            .add_custom_ignore_filename(".dootignore")
            .build();

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let full_path = entry.path();
            let relative = full_path.strip_prefix(group_dir)?;

            let destination = resolved_path.join(relative);
            let status = self.compute_status(full_path, &destination);

            entries.push(FileEntry {
                relative_path: relative.to_path_buf(),
                source: full_path.to_path_buf(),
                destination,
                status,
            });
        }

        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(entries)
    }

    fn compute_status(&self, source: &Path, destination: &Path) -> FileStatus {
        if !self.store.exists(destination) {
            FileStatus::Create
        } else if self.store.compare(source, destination).unwrap_or(false) {
            FileStatus::Same
        } else {
            FileStatus::Overwrite
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn import_matches_nested_allowlist_relative_to_source() {
        let temp = tempfile::tempdir().unwrap();
        let group = temp.path().join("repo/config");
        let source = temp.path().join("system/config");
        fs::create_dir_all(&group).unwrap();
        fs::create_dir_all(source.join("zed")).unwrap();
        fs::write(source.join("zed/settings.json"), b"{}").unwrap();
        fs::write(source.join("zed/auth.json"), b"secret").unwrap();
        fs::write(source.join("unrelated"), b"ignored").unwrap();
        let ignore = group.join(".dootignore");
        fs::write(&ignore, "*\n!zed/\n!zed/settings.json\n").unwrap();

        let store = MockStore::new();
        let entries = PlanBuilder::new(&store)
            .build_import(&group, &source, &ignore)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].relative_path, Path::new("zed/settings.json"));
    }

    #[test]
    fn import_without_ignore_file_includes_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("config"), b"content").unwrap();
        let group = temp.path().join("repo");
        let store = MockStore::new();
        let entries = PlanBuilder::new(&store)
            .build_import(&group, &source, &group.join(".dootignore"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].relative_path, Path::new("config"));
    }

    #[test]
    fn import_rejects_invalid_ignore_patterns() {
        let temp = tempfile::tempdir().unwrap();
        let ignore = temp.path().join(".dootignore");
        fs::write(&ignore, "{unclosed\n").unwrap();
        let store = MockStore::new();
        assert!(PlanBuilder::new(&store)
            .build_import(temp.path(), temp.path(), &ignore)
            .is_err());
    }

    struct MockStore {
        files: HashMap<PathBuf, Vec<u8>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }

        fn with_file(mut self, path: &str, content: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), content.to_vec());
            self
        }
    }

    impl Store for MockStore {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn read(&self, path: &Path) -> Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("File not found"))
        }

        fn write(&self, _path: &Path, _content: &[u8]) -> Result<()> {
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }

        fn remove(&self, _path: &Path) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn plan_tracks_changes_across_groups() {
        let mut plan = Plan::new();

        plan.add_group(
            "group1".to_string(),
            vec![FileEntry {
                relative_path: PathBuf::from("file1"),
                source: PathBuf::from("/src/file1"),
                destination: PathBuf::from("/dst/file1"),
                status: FileStatus::Same,
            }],
        );

        plan.add_group(
            "group2".to_string(),
            vec![FileEntry {
                relative_path: PathBuf::from("file2"),
                source: PathBuf::from("/src/file2"),
                destination: PathBuf::from("/dst/file2"),
                status: FileStatus::Create,
            }],
        );

        assert!(plan.has_changes());
        assert_eq!(plan.total_count_by_status(FileStatus::Same), 1);
        assert_eq!(plan.total_count_by_status(FileStatus::Create), 1);
    }

    #[test]
    fn plan_with_no_changes() {
        let mut plan = Plan::new();
        plan.add_group(
            "group".to_string(),
            vec![FileEntry {
                relative_path: PathBuf::from("file"),
                source: PathBuf::from("/src/file"),
                destination: PathBuf::from("/dst/file"),
                status: FileStatus::Same,
            }],
        );

        assert!(!plan.has_changes());
    }

    #[test]
    fn status_create_when_destination_missing() {
        let store = MockStore::new().with_file("/src/file", b"content");
        let builder = PlanBuilder::new(&store);

        let status = builder.compute_status(Path::new("/src/file"), Path::new("/dst/file"));
        assert_eq!(status, FileStatus::Create);
    }

    #[test]
    fn status_same_when_content_matches() {
        let store = MockStore::new()
            .with_file("/src/file", b"content")
            .with_file("/dst/file", b"content");
        let builder = PlanBuilder::new(&store);

        let status = builder.compute_status(Path::new("/src/file"), Path::new("/dst/file"));
        assert_eq!(status, FileStatus::Same);
    }

    #[test]
    fn status_overwrite_when_content_differs() {
        let store = MockStore::new()
            .with_file("/src/file", b"new content")
            .with_file("/dst/file", b"old content");
        let builder = PlanBuilder::new(&store);

        let status = builder.compute_status(Path::new("/src/file"), Path::new("/dst/file"));
        assert_eq!(status, FileStatus::Overwrite);
    }
}
