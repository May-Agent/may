use super::DaemonPaths;

#[test]
fn a_linked_home_keeps_its_spelling_for_the_socket_and_resolves_for_files() {
    let real = tempfile::tempdir().unwrap();
    let links = tempfile::tempdir().unwrap();
    let linked = links.path().join("home");
    std::os::unix::fs::symlink(real.path(), &linked).unwrap();

    let paths = DaemonPaths::for_home(linked.clone()).unwrap();

    assert_eq!(paths.socket(), linked.join(".may").join("daemon.sock"));
    assert_eq!(
        paths.root(),
        dunce::canonicalize(real.path()).unwrap().join(".may")
    );
}
