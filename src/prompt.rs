//! The system prompt. Its text lives in `prompts/system.md` so it can be
//! replaced wholesale; `{project_root}` is substituted at startup.

use std::path::Path;

const SYSTEM: &str = include_str!("prompts/system.md");

pub fn system(project_root: &Path) -> String {
    SYSTEM.replace("{project_root}", &project_root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_the_inversion_and_the_root() {
        let prompt = system(Path::new("/src/project"));
        assert!(prompt.contains("cannot change anything in the codebase"));
        assert!(prompt.contains("`/src/project`"));
        assert!(!prompt.contains("{project_root}"));
    }
}
