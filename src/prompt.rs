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
    fn includes_the_interaction_contract_and_tool_families() {
        for phrase in [
            "highest level of intent",
            "Never provide function bodies",
            "No confirmation question",
            "Be critical, not compliant",
            "Own a wrong suggestion plainly",
            "Run checks and tests proactively",
            "Earlier context may have been removed",
            "<file-changes>",
            "<diagnostic-changes>",
            "<git-event>",
            "review by default",
            "no_comment",
            "read_file",
            "lsp_diagnostics",
            "search_web",
            "shell_wait",
        ] {
            assert!(SYSTEM.contains(phrase), "missing prompt guidance: {phrase}");
        }
    }

    #[test]
    fn states_the_inversion_and_the_root() {
        let prompt = system(Path::new("/src/project"));
        assert!(prompt.contains("cannot change anything in the codebase"));
        assert!(prompt.contains("`/src/project`"));
        assert!(!prompt.contains("{project_root}"));
    }
}
