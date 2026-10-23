use crate::system_tool;

#[cfg(unix)]
#[test]
fn system_tools_resolve_to_absolute_paths() {
    // sysinfo's readings and the hosts' liveness check: one process start each, no PATH search.
    for name in ["ps", "kill", "date"] {
        let p = system_tool::path(name);
        assert!(p.is_absolute() && p.is_file(), "{name} -> {}", p.display());
    }
}

#[test]
fn unknown_tools_fall_back_to_the_bare_name() {
    assert_eq!(system_tool::path("no-such-tool-effectcraft"), std::path::PathBuf::from("no-such-tool-effectcraft"));
}
