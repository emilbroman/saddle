//! The curated environment for sandboxed commands: what toolchains need to
//! find themselves, without anything that looks like a secret.

use std::ffi::{OsStr, OsString};

const NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CC",
    "CXX",
    "AR",
    "LD",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "DEVELOPER_DIR",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "IN_NIX_SHELL",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "VIRTUAL_ENV",
];

const PREFIXES: &[&str] = &["LC_", "NIX_", "CARGO_", "RUSTUP_", "RUST", "PKG_CONFIG"];

/// Words that mark a variable as a secret, matched against `_`-separated parts.
const SECRET_WORDS: &[&str] = &[
    "KEY",
    "APIKEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
];

/// Fixed settings: no colour codes, pagers or prompts in captured output, and
/// no opportunistic git index writes (they would fail on the read-only `.git`).
const FIXED: &[(&str, &str)] = &[
    ("TERM", "dumb"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// Filters `vars` down to the allowlist plus `extra` names, drops secrets
/// (even from `extra`), and adds the fixed settings.
pub fn environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[String],
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| allowed(name, extra) && !looks_secret(name))
        })
        .filter(|(name, _)| !FIXED.iter().any(|(fixed, _)| OsStr::new(fixed) == name))
        .collect();
    env.extend(FIXED.iter().map(|(k, v)| (k.into(), v.into())));
    env.sort();
    env
}

fn allowed(name: &str, extra: &[String]) -> bool {
    NAMES.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || extra.iter().any(|e| e == name)
}

fn looks_secret(name: &str) -> bool {
    name.to_ascii_uppercase()
        .split('_')
        .any(|word| SECRET_WORDS.contains(&word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_toolchain_vars_and_drops_secrets() {
        let vars = [
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("LC_ALL", "C"),
            ("NIX_CFLAGS_COMPILE", "-I"),
            ("CARGO_HOME", "/c"),
            ("CARGO_REGISTRY_TOKEN", "t"),
            ("RUSTFLAGS", "-D"),
            ("OPENAI_API_KEY", "sk"),
            ("EXA_API_KEY", "x"),
            ("AWS_SECRET_ACCESS_KEY", "a"),
            ("GITHUB_TOKEN", "g"),
            ("SSH_AUTH_SOCK", "/s"),
            ("EDITOR", "hx"),
            ("MY_TOOL", "1"),
            ("MY_PASSWORD", "p"),
            ("TERM", "xterm-kitty"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = environment(vars, &["MY_TOOL".into(), "MY_PASSWORD".into()]);
        let names: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "CARGO_HOME",
                "GIT_OPTIONAL_LOCKS",
                "GIT_PAGER",
                "GIT_TERMINAL_PROMPT",
                "HOME",
                "LC_ALL",
                "MY_TOOL",
                "NIX_CFLAGS_COMPILE",
                "PAGER",
                "PATH",
                "RUSTFLAGS",
                "TERM",
            ]
        );
        assert!(env.contains(&("TERM".into(), "dumb".into())));
    }
}
