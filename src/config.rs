//! `config.toml` (settings) and `credentials.toml` (secrets) under the Saddle
//! config directory: `$XDG_CONFIG_HOME/saddle`, falling back to `~/.config/saddle`
//! on every platform, macOS included.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const CONFIG_FILE: &str = "config.toml";
pub const CREDENTIALS_FILE: &str = "credentials.toml";

const CONFIG_EXAMPLE: &str = r#"model = "claude-sonnet-4-5"
api = "messages"            # or "responses"
base_url = "https://litellm.example.internal""#;

const CREDENTIALS_EXAMPLE: &str = r#"[inference]
api_key = "sk-...""#;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot locate the Saddle config directory: neither $XDG_CONFIG_HOME nor $HOME is set")]
    NoConfigDir,

    #[error("{} not found at {}\n\nCreate it with at least:\n\n{example}\n\nSee the README for the full schema.", kind.name(), path.display())]
    NotFound {
        kind: FileKind,
        path: PathBuf,
        example: &'static str,
    },

    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid {}:\n{message}", path.display())]
    Parse { path: PathBuf, message: String },

    #[error("invalid {}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Config,
    Credentials,
}

impl FileKind {
    fn name(self) -> &'static str {
        match self {
            FileKind::Config => "config file",
            FileKind::Credentials => "credentials file",
        }
    }

    fn example(self) -> &'static str {
        match self {
            FileKind::Config => CONFIG_EXAMPLE,
            FileKind::Credentials => CREDENTIALS_EXAMPLE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiFamily {
    /// Anthropic Messages API (`/v1/messages`).
    Messages,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
}

impl fmt::Display for ApiFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ApiFamily::Messages => "messages",
            ApiFamily::Responses => "responses",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub api: ApiFamily,
    pub base_url: String,
    #[serde(default)]
    pub lsp: BTreeMap<String, LspServer>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub batching: BatchingConfig,
    #[serde(default)]
    pub search: SearchConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions without the leading dot, e.g. `["rs"]`.
    pub file_types: Vec<String>,
    /// Files whose presence marks a workspace root for this server, e.g. `["Cargo.toml"]`.
    #[serde(default)]
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Writable paths outside any project (toolchain caches). Absolute or `~/`-prefixed.
    #[serde(default = "default_global_writable")]
    pub writable: Vec<String>,
    /// Keyed by project root (absolute or `~/`-prefixed).
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectSandbox>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            writable: default_global_writable(),
            projects: BTreeMap::new(),
        }
    }
}

fn default_global_writable() -> Vec<String> {
    vec!["~/.cargo".into()]
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSandbox {
    /// Writable directories inside the project, relative to its root.
    pub writable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BatchingConfig {
    /// Trailing quiet period after the last file change before a batch is delivered.
    pub quiet_ms: u64,
    /// Upper bound on how long a batch may keep growing.
    pub max_window_ms: u64,
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            quiet_ms: 1_000,
            max_window_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchConfig {
    pub enabled: bool,
    pub num_results: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            num_results: 5,
        }
    }
}

impl Config {
    pub fn parse(source: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(source).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string().trim_end().to_owned(),
        })?;
        config.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_owned(),
            message,
        })?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("`model` must not be empty".into());
        }
        if !(self.base_url.starts_with("https://") || self.base_url.starts_with("http://")) {
            return Err(format!(
                "`base_url` must start with https:// or http:// (got {:?})",
                self.base_url
            ));
        }
        for (language, server) in &self.lsp {
            if server.command.trim().is_empty() {
                return Err(format!("`lsp.{language}.command` must not be empty"));
            }
            if server.file_types.is_empty() {
                return Err(format!("`lsp.{language}.file_types` must not be empty"));
            }
        }
        if self.batching.quiet_ms > self.batching.max_window_ms {
            return Err("`batching.quiet_ms` must not exceed `batching.max_window_ms`".into());
        }
        if !(1..=100).contains(&self.search.num_results) {
            return Err("`search.num_results` must be between 1 and 100".into());
        }
        Ok(())
    }

    /// Writable carve-outs for agent shell commands in `project_root`: the global
    /// list, plus the project's configured list, or `target` for a Cargo project
    /// without an explicit entry.
    pub fn writable_paths(&self, project_root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .sandbox
            .writable
            .iter()
            .map(|p| expand_tilde(p, home))
            .collect();

        let entry = self
            .sandbox
            .projects
            .iter()
            .find(|(root, _)| {
                let root = expand_tilde(root, home);
                std::fs::canonicalize(&root).unwrap_or(root) == project_root
            })
            .map(|(_, project)| project);

        match entry {
            Some(project) => paths.extend(project.writable.iter().map(|p| project_root.join(p))),
            None if project_root.join("Cargo.toml").is_file() => {
                paths.push(project_root.join("target"))
            }
            None => {}
        }
        paths
    }
}

/// A secret string. Never printed: `Debug` is redacted and there is no `Display`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub inference: ApiKey,
    #[serde(default)]
    pub exa: Option<ApiKey>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    pub api_key: Secret,
}

impl Credentials {
    pub fn parse(source: &str, path: &Path) -> Result<Self, ConfigError> {
        // toml's Display quotes the offending source line, which may hold a key.
        let credentials: Credentials = toml::from_str(source).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: match e.span() {
                Some(span) => {
                    let (line, column) = line_column(source, span.start);
                    format!("line {line}, column {column}: {}", e.message())
                }
                None => e.message().to_owned(),
            },
        })?;

        let mut keys = vec![("inference.api_key", &credentials.inference.api_key)];
        if let Some(exa) = &credentials.exa {
            keys.push(("exa.api_key", &exa.api_key));
        }
        for (name, key) in keys {
            if key.expose().trim().is_empty() {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    message: format!("`{name}` must not be empty"),
                });
            }
        }
        Ok(credentials)
    }
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[derive(Debug)]
pub struct Loaded {
    pub dir: PathBuf,
    pub config: Config,
    pub credentials: Credentials,
}

/// Loads both files from the directory selected by the current environment.
pub fn load() -> Result<Loaded, ConfigError> {
    let dir = config_dir(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )?;
    load_from(&dir)
}

pub fn load_from(dir: &Path) -> Result<Loaded, ConfigError> {
    let config_path = dir.join(CONFIG_FILE);
    let config = Config::parse(&read(&config_path, FileKind::Config)?, &config_path)?;
    let credentials_path = dir.join(CREDENTIALS_FILE);
    let credentials = Credentials::parse(
        &read(&credentials_path, FileKind::Credentials)?,
        &credentials_path,
    )?;
    Ok(Loaded {
        dir: dir.to_owned(),
        config,
        credentials,
    })
}

fn read(path: &Path, kind: FileKind) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NotFound {
                kind,
                path: path.to_owned(),
                example: kind.example(),
            }
        } else {
            ConfigError::Read {
                path: path.to_owned(),
                source,
            }
        }
    })
}

/// Per the XDG spec, a relative `$XDG_CONFIG_HOME` is ignored.
pub fn config_dir(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, ConfigError> {
    if let Some(xdg) = xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Ok(xdg.join("saddle"));
    }
    match home.filter(|h| !h.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join(".config").join("saddle")),
        None => Err(ConfigError::NoConfigDir),
    }
}

pub fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if path == "~" {
            return home.to_owned();
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PathBuf {
        PathBuf::from("/cfg/config.toml")
    }

    const FULL: &str = r#"
model = "claude-sonnet-4-5"
api = "responses"
base_url = "https://litellm.bb3.internal"

[lsp.rust]
command = "rust-analyzer"
file_types = ["rs"]
root_markers = ["Cargo.toml"]

[sandbox]
writable = ["~/.cargo", "/opt/cache"]

[sandbox.projects."~/src/saddle"]
writable = ["target", "build"]

[batching]
quiet_ms = 500
max_window_ms = 10000

[search]
enabled = false
num_results = 8
"#;

    #[test]
    fn parses_full_config() {
        let config = Config::parse(FULL, &p()).unwrap();
        assert_eq!(config.model, "claude-sonnet-4-5");
        assert_eq!(config.api, ApiFamily::Responses);
        assert_eq!(config.base_url, "https://litellm.bb3.internal");
        let rust = &config.lsp["rust"];
        assert_eq!(rust.command, "rust-analyzer");
        assert!(rust.args.is_empty());
        assert_eq!(rust.file_types, ["rs"]);
        assert_eq!(rust.root_markers, ["Cargo.toml"]);
        assert_eq!(config.sandbox.writable, ["~/.cargo", "/opt/cache"]);
        assert_eq!(
            config.sandbox.projects["~/src/saddle"].writable,
            ["target", "build"]
        );
        assert_eq!(config.batching.quiet_ms, 500);
        assert_eq!(config.batching.max_window_ms, 10_000);
        assert!(!config.search.enabled);
        assert_eq!(config.search.num_results, 8);
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let config = Config::parse(CONFIG_EXAMPLE, &p()).unwrap();
        assert_eq!(config.api, ApiFamily::Messages);
        assert!(config.lsp.is_empty());
        assert_eq!(config.sandbox, SandboxConfig::default());
        assert_eq!(config.batching, BatchingConfig::default());
        assert_eq!(config.search, SearchConfig::default());
    }

    #[test]
    fn missing_required_key_is_reported() {
        let err = Config::parse("model = \"m\"\napi = \"messages\"\n", &p()).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(msg.contains("base_url"), "{msg}");
        assert!(msg.contains("/cfg/config.toml"), "{msg}");
    }

    #[test]
    fn malformed_toml_is_reported_with_location() {
        let err = Config::parse("model = \"m\nbase_url = 3", &p()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains("line 1"), "{err}");
    }

    #[test]
    fn unknown_api_family_is_rejected() {
        let src = CONFIG_EXAMPLE.replace("\"messages\"", "\"chat\"");
        let err = Config::parse(&src, &p()).unwrap_err().to_string();
        assert!(err.contains("chat"), "{err}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let src = format!("{CONFIG_EXAMPLE}\nmodle = \"typo\"\n");
        let err = Config::parse(&src, &p()).unwrap_err().to_string();
        assert!(err.contains("modle"), "{err}");
    }

    #[test]
    fn semantic_validation() {
        let cases = [
            (CONFIG_EXAMPLE.replace("https://", "ftp://"), "base_url"),
            (CONFIG_EXAMPLE.replace("claude-sonnet-4-5", " "), "model"),
            (
                format!("{CONFIG_EXAMPLE}\n[lsp.rust]\ncommand = \"ra\"\nfile_types = []\n"),
                "lsp.rust.file_types",
            ),
            (
                format!("{CONFIG_EXAMPLE}\n[batching]\nquiet_ms = 5\nmax_window_ms = 1\n"),
                "batching.quiet_ms",
            ),
            (
                format!("{CONFIG_EXAMPLE}\n[search]\nnum_results = 0\n"),
                "search.num_results",
            ),
        ];
        for (src, needle) in cases {
            let err = Config::parse(&src, &p()).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn credentials_parse_and_redact() {
        let src = "[inference]\napi_key = \"sk-very-secret\"\n[exa]\napi_key = \"exa-secret\"\n";
        let creds = Credentials::parse(src, &p()).unwrap();
        assert_eq!(creds.inference.api_key.expose(), "sk-very-secret");
        assert_eq!(creds.exa.as_ref().unwrap().api_key.expose(), "exa-secret");
        let debug = format!("{creds:?}");
        assert!(!debug.contains("secret"), "{debug}");
    }

    #[test]
    fn exa_key_is_optional() {
        let creds = Credentials::parse(CREDENTIALS_EXAMPLE, &p()).unwrap();
        assert!(creds.exa.is_none());
    }

    #[test]
    fn credentials_errors_never_echo_secrets() {
        let cases = [
            "[inference]\napi_key = sk-unquoted-secret\n",
            "[inference]\napi_key = \"sk-secret\"\nextra = \"sk-other-secret\"\n",
            "[inference]\napi_key = [\"sk-secret-in-array\"]\n",
        ];
        for src in cases {
            let err = Credentials::parse(src, &p()).unwrap_err().to_string();
            assert!(!err.contains("secret"), "{err}");
            assert!(err.contains("line"), "{err}");
        }
    }

    #[test]
    fn missing_or_empty_inference_key() {
        let err = Credentials::parse("[exa]\napi_key = \"x\"\n", &p()).unwrap_err();
        assert!(err.to_string().contains("inference"), "{err}");
        let err = Credentials::parse("[inference]\napi_key = \"\"\n", &p()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn load_reports_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_from(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NotFound {
                kind: FileKind::Config,
                ..
            }
        ));
        assert!(err.to_string().contains("base_url"), "{err}");

        std::fs::write(dir.path().join(CONFIG_FILE), CONFIG_EXAMPLE).unwrap();
        let err = load_from(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NotFound {
                kind: FileKind::Credentials,
                ..
            }
        ));

        std::fs::write(dir.path().join(CREDENTIALS_FILE), CREDENTIALS_EXAMPLE).unwrap();
        let loaded = load_from(dir.path()).unwrap();
        assert_eq!(loaded.config.model, "claude-sonnet-4-5");
    }

    #[test]
    fn config_dir_resolution() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            config_dir(os("/xdg"), os("/home/u")).unwrap(),
            PathBuf::from("/xdg/saddle")
        );
        assert_eq!(
            config_dir(None, os("/home/u")).unwrap(),
            PathBuf::from("/home/u/.config/saddle")
        );
        assert_eq!(
            config_dir(os("relative"), os("/home/u")).unwrap(),
            PathBuf::from("/home/u/.config/saddle")
        );
        assert_eq!(
            config_dir(os(""), os("/home/u")).unwrap(),
            PathBuf::from("/home/u/.config/saddle")
        );
        assert!(matches!(
            config_dir(None, None),
            Err(ConfigError::NoConfigDir)
        ));
    }

    #[test]
    fn writable_paths_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let project = home.join("src/saddle");
        let other = home.join("src/other");
        let plain = home.join("src/plain");
        for dir in [&project, &other, &plain] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(other.join("Cargo.toml"), "").unwrap();

        let config = Config::parse(FULL, &p()).unwrap();
        let h = Some(home.as_path());
        assert_eq!(
            config.writable_paths(&project, h),
            [
                home.join(".cargo"),
                PathBuf::from("/opt/cache"),
                project.join("target"),
                project.join("build"),
            ]
        );
        assert_eq!(
            config.writable_paths(&other, h),
            [
                home.join(".cargo"),
                PathBuf::from("/opt/cache"),
                other.join("target")
            ]
        );
        assert_eq!(
            config.writable_paths(&plain, h),
            [home.join(".cargo"), PathBuf::from("/opt/cache")]
        );
    }
}
