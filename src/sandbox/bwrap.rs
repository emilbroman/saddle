//! Bubblewrap arguments. The host filesystem is mounted read-only (recursively,
//! submounts included), then the scratch directory over `/tmp`, the project
//! read-only again (in case it lives under `/tmp`), the carve-outs writable on
//! top, and `/dev/null` over hidden files. Bind mounts are separate mount
//! points, so hard links from the project into a writable mount fail with
//! `EXDEV`.

use std::ffi::OsString;
use std::path::Path;

use super::{Policy, ProcMount};

/// Where the scratch directory appears inside the sandbox.
pub const SANDBOX_TMP: &str = "/tmp";

pub fn args(policy: &Policy, proc: ProcMount, cwd: &Path, command: &[OsString]) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |items: &[&dyn AsRef<std::ffi::OsStr>]| {
        args.extend(items.iter().map(|item| item.as_ref().to_owned()));
    };
    push(&[&"--ro-bind", &"/", &"/", &"--dev", &"/dev"]);
    match proc {
        ProcMount::Fresh => push(&[&"--proc", &"/proc"]),
        ProcMount::HostReadOnly => push(&[&"--ro-bind", &"/proc", &"/proc"]),
    }
    push(&[&"--bind", &policy.scratch, &SANDBOX_TMP]);
    push(&[&"--ro-bind", &policy.project_root, &policy.project_root]);
    for path in &policy.writable {
        push(&[&"--bind", path, path]);
    }
    for path in &policy.hidden {
        push(&[&"--ro-bind", &"/dev/null", path]);
    }
    push(&[
        &"--unshare-all",
        &"--share-net",
        &"--die-with-parent",
        &"--chdir",
        &cwd,
        &"--",
    ]);
    args.extend(command.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::policy;

    #[test]
    fn mount_order_and_flags() {
        let command = ["/bin/sh", "-c", "ls"].map(OsString::from);
        let args = args(
            &policy(),
            ProcMount::Fresh,
            Path::new("/src/proj/src"),
            &command,
        );
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args.join(" "),
            "--ro-bind / / --dev /dev --proc /proc \
             --bind /tmp/saddle-x /tmp \
             --ro-bind /src/proj /src/proj \
             --bind /src/proj/target /src/proj/target \
             --bind /home/u/.cargo /home/u/.cargo \
             --ro-bind /dev/null /home/u/.config/saddle/credentials.toml \
             --unshare-all --share-net --die-with-parent --chdir /src/proj/src \
             -- /bin/sh -c ls"
        );
    }

    #[test]
    fn host_proc_fallback() {
        let args = args(&policy(), ProcMount::HostReadOnly, Path::new("/"), &[]);
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(&args[5..8], ["--ro-bind", "/proc", "/proc"]);
    }
}
