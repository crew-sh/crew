//! A state file the usher cannot read is kept aside and reported, never read as "nothing queued".
use crew_proto::{JobPhase, JobRequest, JobsFile, prost::Message};
use crew_usher::config::Config;
use crew_usher::server::{IdleSource, Paths, Server};

fn start(dir: &std::path::Path) -> (Server, Paths) {
    let cfg = Config {
        watched_paths: vec![],
        min_free_disk_gib: Default::default(),
        tmp_root: dir.join("jobtmp").display().to_string(),
        ..Config::default()
    };
    let paths = Paths::in_state(dir.join("usher.sock"), dir);
    (
        Server::start(cfg, &paths, IdleSource::Off, false).unwrap(),
        paths,
    )
}

fn corrupt_copies(dir: &std::path::Path, stem: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{stem}.corrupt-"))
        })
        .collect()
}

#[test]
fn a_cut_off_job_file_is_moved_aside_with_a_log_line() {
    let dir = tempfile::tempdir().unwrap();
    let record = crew_proto::DetachedJob {
        request: Some(JobRequest {
            id: "queued-bench".into(),
            argv: vec!["true".into()],
            ..JobRequest::default()
        }),
        phase: JobPhase::Queued as i32,
        ..Default::default()
    };
    let whole = JobsFile {
        version: 2,
        jobs: vec![record],
    }
    .encode_to_vec();
    let jobs = dir.path().join("usher-jobs.pb");
    std::fs::write(&jobs, &whole[..whole.len() - 4]).unwrap();

    let (server, _) = start(dir.path());
    server.stop();

    let kept = corrupt_copies(dir.path(), "usher-jobs.pb");
    assert_eq!(kept.len(), 1, "the unreadable file must be kept");
    assert_eq!(std::fs::read(&kept[0]).unwrap(), whole[..whole.len() - 4]);
    let log = std::fs::read_to_string(dir.path().join("usher.log")).unwrap();
    assert!(
        log.contains("ERROR") && log.contains("usher-jobs.pb"),
        "{log}"
    );
}

#[test]
fn a_job_file_from_a_newer_usher_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let future = JobsFile {
        version: 99,
        jobs: vec![],
    }
    .encode_to_vec();
    std::fs::write(dir.path().join("usher-jobs.pb"), &future).unwrap();
    let (server, _) = start(dir.path());
    server.stop();
    let kept = corrupt_copies(dir.path(), "usher-jobs.pb");
    assert_eq!(kept.len(), 1);
    assert_eq!(std::fs::read(&kept[0]).unwrap(), future);
}

#[test]
fn a_damaged_sizes_file_is_set_aside_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("usher-sizes.pb"), b"\x0a\xff\xff\xff").unwrap();
    let (server, _) = start(dir.path());
    server.stop();
    assert_eq!(corrupt_copies(dir.path(), "usher-sizes.pb").len(), 1);
    let log = std::fs::read_to_string(dir.path().join("usher.log")).unwrap();
    assert!(log.contains("usher-sizes.pb"), "{log}");
}

#[test]
fn a_job_file_written_by_the_previous_usher_restores_its_queued_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let id = "5cbd0f7a-0000-4000-8000-000000000001";
    let log = dir.path().join("jobs").join(format!("{id}.log"));
    let record = crew_proto::DetachedJob {
        request: Some(JobRequest {
            id: id.into(),
            kind: crew_proto::JobKind::Bench as i32,
            argv: vec!["true".into()],
            cwd: "/".into(),
            detach: true,
            ..JobRequest::default()
        }),
        phase: JobPhase::Queued as i32,
        queued_at_unix_ms: 1_700_000_000_000,
        log_path: log.display().to_string(),
        status_path: log.with_extension("status").display().to_string(),
        ..Default::default()
    };
    let v1 = JobsFile {
        version: 1,
        jobs: vec![record],
    };
    std::fs::write(dir.path().join("usher-jobs.pb"), v1.encode_to_vec()).unwrap();

    let (server, paths) = start(dir.path());
    let client = crew_usher_client::Client::new(&paths.socket);
    client.set_owner(crew_usher_client::OwnerArg::Here).unwrap();
    let queued = client.jobs().unwrap();
    server.stop();
    let job = queued
        .jobs
        .iter()
        .find(|j| j.id == id)
        .expect("the queued job came back");
    assert_eq!(job.queued_at_unix_ms, 1_700_000_000_000);
    assert!(corrupt_copies(dir.path(), "usher-jobs.pb").is_empty());
    let rewritten = JobsFile::decode(
        std::fs::read(dir.path().join("usher-jobs.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(rewritten.jobs.len(), 1);
}
