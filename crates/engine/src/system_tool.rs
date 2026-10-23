//! The operating system's own command-line tools (`vm_stat`, `sysctl`, `ps`, `kill`, `date`, …)
//! by absolute path.
//!
//! Started by bare name, a tool is looked up in every `PATH` folder in turn (35 of them in an
//! agent's shell, `/usr/sbin` 21st), and each try is a process start. When macOS blocks a
//! process's starts (its binary was replaced while it ran, see scripts/frozen-exe.sh), every try
//! is blocked and malware-scanned: one reading became about 60 scans. By path it is one start.

/// `name` in the system's tool folders (`/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`) where it is
/// there, else `name` itself (looked up in `PATH`, as on Windows).
pub fn path(name: &str) -> std::path::PathBuf {
    #[cfg(unix)]
    for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() {
            return p;
        }
    }
    name.into()
}

/// A [`std::process::Command`] for the system tool `name` ([`path`]).
pub fn command(name: &str) -> std::process::Command {
    std::process::Command::new(path(name))
}
