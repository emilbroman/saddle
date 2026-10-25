//! Mapping files to configured language servers and their workspace roots.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::LspServer;

#[derive(Debug, Clone, Default)]
pub struct Languages {
    servers: BTreeMap<String, LspServer>,
}

impl Languages {
    pub fn new(servers: &BTreeMap<String, LspServer>) -> Self {
        Self {
            servers: servers.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.servers.keys().map(String::as_str)
    }

    pub fn server(&self, language: &str) -> Option<&LspServer> {
        self.servers.get(language)
    }

    /// The language whose `file_types` lists the path's extension or its whole
    /// file name (for e.g. `Makefile`). The first language in name order wins.
    pub fn for_path(&self, path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        let extension = path.extension().and_then(|e| e.to_str());
        self.servers
            .iter()
            .find(|(_, server)| {
                server
                    .file_types
                    .iter()
                    .any(|t| Some(t.as_str()) == extension || t == name)
            })
            .map(|(language, _)| language.as_str())
    }
}

/// The topmost directory between `file` and `project_root` (inclusive) that
/// contains one of `markers`, or `project_root` when none does. Picking the
/// topmost one keeps a Cargo workspace on a single server rather than one per
/// member crate.
pub fn workspace_root(project_root: &Path, file: &Path, markers: &[String]) -> PathBuf {
    let mut found = None;
    for dir in file.ancestors().skip(1) {
        if !dir.starts_with(project_root) {
            break;
        }
        if markers.iter().any(|m| dir.join(m).exists()) {
            found = Some(dir);
        }
    }
    found.unwrap_or(project_root).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(file_types: &[&str]) -> LspServer {
        LspServer {
            command: "ls".into(),
            args: vec![],
            file_types: file_types.iter().map(|s| s.to_string()).collect(),
            root_markers: vec![],
        }
    }

    #[test]
    fn matches_extension_and_file_name() {
        let mut map = BTreeMap::new();
        map.insert("rust".to_string(), server(&["rs"]));
        map.insert("make".to_string(), server(&["mk", "Makefile"]));
        map.insert("toml".to_string(), server(&["toml"]));
        let languages = Languages::new(&map);
        assert_eq!(
            languages.for_path(Path::new("/p/src/main.rs")),
            Some("rust")
        );
        assert_eq!(languages.for_path(Path::new("/p/Makefile")), Some("make"));
        assert_eq!(languages.for_path(Path::new("/p/rules.mk")), Some("make"));
        assert_eq!(languages.for_path(Path::new("/p/Cargo.toml")), Some("toml"));
        assert_eq!(languages.for_path(Path::new("/p/README.md")), None);
    }

    #[test]
    fn first_language_wins_on_overlap() {
        let mut map = BTreeMap::new();
        map.insert("b".to_string(), server(&["x"]));
        map.insert("a".to_string(), server(&["x"]));
        assert_eq!(Languages::new(&map).for_path(Path::new("f.x")), Some("a"));
    }

    #[test]
    fn workspace_root_is_topmost_marker_inside_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::create_dir_all(root.join("tools/b/src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("crates/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("crates/a/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("tools/b/Cargo.toml"), "").unwrap();
        let markers = vec!["Cargo.toml".to_string()];

        let file = root.join("crates/a/src/lib.rs");
        assert_eq!(workspace_root(root, &file, &markers), root.join("crates"));
        let file = root.join("tools/b/src/main.rs");
        assert_eq!(workspace_root(root, &file, &markers), root.join("tools/b"));
        let file = root.join("docs/x.rs");
        assert_eq!(workspace_root(root, &file, &markers), root);
        assert_eq!(workspace_root(root, &file, &[]), root);

        // A marker above the project root is not considered.
        let project = root.join("tools");
        let file = project.join("loose.rs");
        assert_eq!(workspace_root(&project, &file, &markers), project);
    }
}
