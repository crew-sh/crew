//! Two ushers on one state directory would run the same detached jobs twice.
use crew_usher::config::Config;
use crew_usher::server::{IdleSource, Paths, Server, ServerError};

fn config(dir: &std::path::Path) -> Config {
    Config {
        watched_paths: vec![],
        min_free_disk_gib: Default::default(),
        tmp_root: dir.join("jobtmp").display().to_string(),
        ..Config::default()
    }
}

#[test]
fn a_second_usher_on_the_same_state_dir_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let first = Paths::in_state(dir.path().join("a.sock"), dir.path());
    let second = Paths::in_state(dir.path().join("b.sock"), dir.path());
    let live = Server::start(config(dir.path()), &first, IdleSource::Off, false).unwrap();
    match Server::start(config(dir.path()), &second, IdleSource::Off, false) {
        Err(ServerError::StateLocked(_)) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(!second.socket.exists(), "the refused usher bound a socket");
    live.stop();
    Server::start(config(dir.path()), &second, IdleSource::Off, false)
        .expect("the lock is released when the first usher stops")
        .stop();
}

#[test]
fn a_socket_in_an_existing_directory_leaves_its_mode_alone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let paths = Paths::in_state(dir.path().join("u.sock"), &dir.path().join("state"));
    Server::start(config(dir.path()), &paths, IdleSource::Off, false)
        .unwrap()
        .stop();
    let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
}
