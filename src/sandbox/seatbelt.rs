//! Seatbelt profile for `sandbox-exec`. Everything is allowed (reads, network,
//! process control) except writing outside the carve-outs, creating hard links
//! (Seatbelt checks paths, so a link would be a writable alias of a project
//! file), moving the carve-outs themselves, and reading hidden files. Paths go
//! in as `-D` parameters, so they need no quoting inside the profile.

use std::ffi::OsString;
use std::fmt::Write;

use super::Policy;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const DEVICES: [&str; 5] = [
    "/dev/null",
    "/dev/zero",
    "/dev/dtracehelper",
    "/dev/tty",
    "/dev/stdout",
];

pub fn profile(policy: &Policy) -> String {
    let writable = writable(policy);
    let mut profile = String::from("(version 1)\n(allow default)\n");
    profile.push_str("(deny file-write*\n  (require-not\n    (require-any\n");
    for (i, _) in writable.iter().enumerate() {
        let _ = writeln!(profile, "      (subpath (param \"WRITABLE_{i}\"))");
    }
    for device in DEVICES {
        let _ = writeln!(profile, "      (literal \"{device}\")");
    }
    profile.push_str("      (subpath \"/dev/fd\"))))\n");
    profile.push_str("(deny file-link)\n");
    for (i, _) in writable.iter().enumerate() {
        let _ = writeln!(
            profile,
            "(deny file-write-unlink (literal (param \"WRITABLE_{i}\")))"
        );
    }
    for (i, _) in policy.hidden.iter().enumerate() {
        let _ = writeln!(
            profile,
            "(deny file-read* (literal (param \"HIDDEN_{i}\")))"
        );
    }
    profile
}

fn writable(policy: &Policy) -> Vec<&std::path::Path> {
    let mut paths = vec![policy.scratch.as_path()];
    paths.extend(policy.writable.iter().map(|p| p.as_path()));
    paths
}

pub fn args(policy: &Policy, command: &[OsString]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-p".into(), profile(policy).into()];
    for (i, path) in writable(policy).into_iter().enumerate() {
        let mut define = OsString::from(format!("-DWRITABLE_{i}="));
        define.push(path);
        args.push(define);
    }
    for (i, path) in policy.hidden.iter().enumerate() {
        let mut define = OsString::from(format!("-DHIDDEN_{i}="));
        define.push(path);
        args.push(define);
    }
    args.push("--".into());
    args.extend(command.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::policy;

    #[test]
    fn profile_denies_writes_outside_carve_outs() {
        let profile = profile(&policy());
        assert_eq!(
            profile,
            r#"(version 1)
(allow default)
(deny file-write*
  (require-not
    (require-any
      (subpath (param "WRITABLE_0"))
      (subpath (param "WRITABLE_1"))
      (subpath (param "WRITABLE_2"))
      (literal "/dev/null")
      (literal "/dev/zero")
      (literal "/dev/dtracehelper")
      (literal "/dev/tty")
      (literal "/dev/stdout")
      (subpath "/dev/fd"))))
(deny file-link)
(deny file-write-unlink (literal (param "WRITABLE_0")))
(deny file-write-unlink (literal (param "WRITABLE_1")))
(deny file-write-unlink (literal (param "WRITABLE_2")))
(deny file-read* (literal (param "HIDDEN_0")))
"#
        );
    }

    #[test]
    fn args_bind_parameters() {
        let command = ["/bin/sh", "-c", "ls"].map(OsString::from);
        let args = args(&policy(), &command);
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args[0], "-p");
        assert_eq!(
            &args[2..],
            [
                "-DWRITABLE_0=/tmp/saddle-x",
                "-DWRITABLE_1=/src/proj/target",
                "-DWRITABLE_2=/home/u/.cargo",
                "-DHIDDEN_0=/home/u/.config/saddle/credentials.toml",
                "--",
                "/bin/sh",
                "-c",
                "ls"
            ]
        );
    }
}
