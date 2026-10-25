//! Landlock fallback for Linux hosts without unprivileged user namespaces.
//! The Saddle binary re-executes itself in a helper mode that restricts every
//! filesystem write right to the carve-outs and then execs the shell, which
//! inherits the restriction. Landlock rules attach to inodes, so aliases of
//! the project (symlinks, bind mounts) are covered too.
//!
//! ABI 3 (Linux 6.2) is required, so that truncation is handled as well.
//! Unlike Bubblewrap, Landlock cannot hide individual files from reads.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use landlock::{
    ABI, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};

const REQUIRED: ABI = ABI::V3;

/// Runs the helper: `<n> <writable>... <program> <args>...`. Never returns.
pub fn exec_helper(args: Vec<OsString>) -> ! {
    let error = match parse(args) {
        Ok((writable, program, rest)) => match restrict(&writable) {
            Ok(()) => std::process::Command::new(&program)
                .args(rest)
                .exec()
                .to_string(),
            Err(e) => format!("cannot enforce Landlock: {e}"),
        },
        Err(e) => e,
    };
    eprintln!("saddle landlock helper: {error}");
    std::process::exit(126);
}

type Parsed = (Vec<PathBuf>, OsString, Vec<OsString>);

fn parse(args: Vec<OsString>) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let count: usize = args
        .next()
        .and_then(|n| n.to_str()?.parse().ok())
        .ok_or("missing writable path count")?;
    let writable: Vec<PathBuf> = args.by_ref().take(count).map(PathBuf::from).collect();
    if writable.len() != count {
        return Err("truncated writable path list".into());
    }
    let program = args.next().ok_or("missing program")?;
    Ok((writable, program, args.collect()))
}

fn restrict(writable: &[PathBuf]) -> Result<(), String> {
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_write(REQUIRED))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules(writable, AccessFs::from_write(REQUIRED)))
        .map_err(|e| e.to_string())?
        .restrict_self()
        .map_err(|e| e.to_string())?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!("ruleset only {:?}", status.ruleset));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_helper_arguments() {
        let args = ["2", "/a", "/b", "/bin/sh", "-c", "ls"].map(OsString::from);
        let (writable, program, rest) = parse(args.to_vec()).unwrap();
        assert_eq!(writable, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(program, "/bin/sh");
        assert_eq!(rest, ["-c", "ls"]);
        assert!(parse(["3", "/a"].map(OsString::from).to_vec()).is_err());
        assert!(parse(["x"].map(OsString::from).to_vec()).is_err());
        assert!(parse(["0"].map(OsString::from).to_vec()).is_err());
    }
}
