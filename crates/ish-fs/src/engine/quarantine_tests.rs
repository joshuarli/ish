use super::*;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

#[test]
#[ignore = "subprocess fixture for journal interruption"]
fn crash_child() {
    let Some(root) = std::env::var_os("ISH32_ENGINE_CRASH_ROOT") else { return; };
    let root = PathBuf::from(root);
    let boundary = std::env::var("ISH32_ENGINE_CRASH_BOUNDARY").unwrap();
    let replacement = boundary.starts_with("replacement_");
    let operation = if boundary == "before_publish" { Operation::Copy } else { Operation::Move };
    let request = Request { operation, operands: crate::Operands::Exact { source: "source".into(), destination: "destination".into() }, options: crate::Options { collision: if replacement { crate::Collision::Replace } else { crate::Collision::Error }, ..crate::Options::default() }, context: crate::Context { cwd: root.clone(), home: None, scratch: root } };
    HOOK.with(|hook| *hook.borrow_mut() = Some(Arc::new(move |point| {
        if !replacement { force_exdev(point)?; }
        if point == boundary {
            println!("ISH32_READY");
            std::io::stdout().flush().unwrap();
            loop { std::thread::park(); }
        }
        Ok(())
    })));
    execute(&request, &Cancellation::default(), &|_: Event| Ok(()));
    panic!("crash boundary was not reached");
}

fn kill_at(fixture: &Fixture, boundary: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap()).args(["--exact", "engine::tests::quarantine_tests::crash_child", "--ignored", "--nocapture"]).env("ISH32_ENGINE_CRASH_ROOT", &fixture.0).env("ISH32_ENGINE_CRASH_BOUNDARY", boundary).stdout(Stdio::piped()).spawn().unwrap();
    let pipe = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut pipe = std::io::BufReader::new(pipe);
        let mut line = String::new();
        while pipe.read_line(&mut line).unwrap() != 0 {
            if line.contains("ISH32_READY") { sender.send(()).unwrap(); break; }
            line.clear();
        }
    });
    let ready = receiver.recv_timeout(std::time::Duration::from_secs(20));
    if boundary.starts_with("replacement_") && ready.is_ok() {
        let parent = fs::File::open(&fixture.0).unwrap();
        assert!(fdu_posix::quarantine::Session::check_recovery(parent.as_fd()).unwrap_err().to_string().contains("another deletion"));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    reader.join().unwrap();
    ready.unwrap();
}

#[test]
fn killed_cross_filesystem_move_keeps_destination_and_recovery_never_deletes() {
    for boundary in ["journal_intent_published", "journal_captured", "journal_capture_parent_synced", "journal_capture_directory_synced", "journal_before_unlink", "journal_unlinked", "journal_removal_directory_synced", "journal_before_parent_sync", "journal_parent_synced", "journal_intent_removed", "journal_intent_removal_synced"] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"transferred").unwrap();
        fs::hard_link(fixture.0.join("source"), fixture.0.join("alias")).unwrap();
        kill_at(&fixture, boundary);
        assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"transferred");
        let (outcome, events) = fixture.run(&fixture.request(), |_| Ok(()));
        assert_eq!(outcome.status(), 1);
        assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("interrupted deletion recovered"))));
        let removed = matches!(boundary, "journal_unlinked" | "journal_removal_directory_synced" | "journal_before_parent_sync" | "journal_parent_synced" | "journal_intent_removed" | "journal_intent_removal_synced");
        assert_eq!(fixture.0.join("source").exists(), !removed, "{boundary}");
        assert_eq!(fs::read(fixture.0.join("alias")).unwrap(), b"transferred");
        assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"transferred");
        let root = fs::File::open(&fixture.0).unwrap();
        fdu_posix::quarantine::Session::check_recovery(root.as_fd()).unwrap();
    }
}

#[test]
fn killed_stage_is_reported_and_retained_without_publication_or_deletion() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"unpublished").unwrap();
    kill_at(&fixture, "before_publish");
    let stage = staged_entry(&fixture.0);
    assert_eq!(fs::read(&stage).unwrap(), b"unpublished");
    let (outcome, events) = fixture.run(&fixture.request(), |_| Ok(()));
    assert_eq!(outcome.status(), 1);
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("unpublished staging object") && diagnostic.message.contains(".fdu-quarantine-v2-"))));
    assert_eq!(fs::read(&stage).unwrap(), b"unpublished");
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"unpublished");
    assert!(!fixture.0.join("destination").exists());
}

#[test]
fn replacement_is_journaled_before_exchange_and_recovers_without_deletion() {
    for boundary in ["replacement_intent_published", "replacement_published", "replacement_captured", "replacement_unlinked"] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"new source").unwrap();
        fs::write(fixture.0.join("destination"), b"old destination").unwrap();
        kill_at(&fixture, boundary);
        let parent = fs::File::open(&fixture.0).unwrap();
        let recovery = fdu_posix::quarantine::Session::check_recovery(parent.as_fd());
        assert!(recovery.is_err(), "{boundary}");
        if boundary == "replacement_intent_published" {
            assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"new source");
            assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"old destination");
        } else {
            assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"new source");
            if boundary != "replacement_unlinked" { assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"old destination"); }
        }
    }
}

#[test]
fn replacement_setup_refusal_and_injected_errors_preserve_truthful_state() {
    for boundary in ["before_replace", "replacement_intent_published", "replacement_published", "replacement_captured"] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"new source").unwrap();
        fs::write(fixture.0.join("destination"), b"old destination").unwrap();
        let mut request = fixture.request(); request.options.collision = crate::Collision::Replace;
        let (outcome, events) = fixture.run(&request, move |point| if point == boundary { Err(io::Error::other("replacement interrupted")) } else { Ok(()) });
        assert_eq!(outcome.status(), 1, "{boundary}");
        let published = matches!(boundary, "replacement_published" | "replacement_captured");
        assert_eq!(fs::read(fixture.0.join("source")).unwrap(), if published { b"old destination".as_slice() } else { b"new source".as_slice() });
        assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), if published { b"new source".as_slice() } else { b"old destination".as_slice() });
        assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == if published { OperandState::SourcePartiallyRetired } else { OperandState::FailedBeforePublication })), "{events:?}");
    }
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"new source").unwrap(); fs::write(fixture.0.join("destination"), b"old destination").unwrap();
    let parent = fs::File::open(&fixture.0).unwrap();
    let held = Arc::new(Mutex::new(None));
    let acquire = held.clone();
    let mut request = fixture.request(); request.options.collision = crate::Collision::Replace;
    assert_eq!(fixture.run(&request, move |point| {
        if point == "before_replace" { *acquire.lock().unwrap() = Some(fdu_posix::quarantine::Session::start(parent.as_fd())?); }
        Ok(())
    }).0.status(), 1);
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"new source");
    assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"old destination");
    held.lock().unwrap().take().unwrap().finish().unwrap();
}

#[test]
fn reached_nested_recovery_roots_block_copy_and_move_retirement() {
    for destination in [false, true] {
        for interrupted in [false, true] {
            let fixture = Fixture::new();
            fs::create_dir_all(fixture.0.join("source/deep")).unwrap();
            fs::write(fixture.0.join("source/deep/file"), b"source bytes").unwrap();
            let nested = fixture.0.join(if destination { "destination/deep" } else { "source/deep" });
            fs::create_dir_all(&nested).unwrap();
            if interrupted {
                let parent = fs::File::open(&nested).unwrap();
                let session = fdu_posix::quarantine::Session::start(parent.as_fd()).unwrap();
                let stage = session.stage_name(); session.register_stage(&stage).unwrap();
                fs::write(nested.join(session.name()).join(stage), b"interrupted bytes").unwrap();
            } else { fs::create_dir(nested.join(".fdu-quarantine-v999-unknown")).unwrap(); }
            let mut request = fixture.request(); request.operation = Operation::Copy; request.options.merge = destination;
            let (outcome, events) = fixture.run(&request, |_| Ok(()));
            assert_eq!(outcome.status(), 1, "{events:?}");
            assert_eq!(fs::read(fixture.0.join("source/deep/file")).unwrap(), b"source bytes");
            assert!(!fixture.0.join("destination/deep/file").exists());
        }
    }
}

#[test]
fn merge_child_rename_sync_failure_reports_partial_source_retirement() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap(); fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/a"), b"a").unwrap(); fs::write(fixture.0.join("source/b"), b"b").unwrap();
    let mut request = fixture.request(); request.options.merge = true; request.options.durable = true;
    let (outcome, events) = fixture.run(&request, |point| if point == "destination_directory_sync" { Err(io::Error::other("child rename sync failure")) } else { Ok(()) });
    assert_eq!(outcome.status(), 1);
    assert!(fixture.0.join("source").is_dir());
    assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(fixture.0.join("destination")).unwrap().count(), 1);
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourcePartiallyRetired)), "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourceRemovedSyncFailed)));
}

#[test]
fn reached_nested_recovery_roots_block_retirement_after_publication() {
    for destination in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("source/deep")).unwrap();
        fs::write(fixture.0.join("source/deep/file"), b"source bytes").unwrap();
        let nested = fixture.0.join(if destination { "destination/deep" } else { "source/deep" });
        let (outcome, events) = fixture.run(&fixture.request(), move |point| {
            force_exdev(point)?;
            if point == "before_retire" { fs::create_dir(nested.join(".fdu-quarantine-v999-unknown")).unwrap(); }
            Ok(())
        });
        assert_eq!(outcome.status(), 1);
        assert_eq!(fs::read(fixture.0.join("source/deep/file")).unwrap(), b"source bytes");
        assert_eq!(fs::read(fixture.0.join("destination/deep/file")).unwrap(), b"source bytes");
        assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("unknown quarantine version"))), "{events:?}");
        assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::DestinationPublishedSourceRetained)));
    }
}

#[test]
fn copying_dot_and_parent_keeps_the_original_parent_anchor() {
    for name in [".", ".."] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("tree/child")).unwrap();
        fs::write(fixture.0.join("tree/file"), b"source bytes").unwrap();
        let mut request = fixture.request();
        request.operation = Operation::Copy;
        request.context.cwd = fixture.0.join(if name == "." { "tree" } else { "tree/child" });
        request.operands = crate::Operands::Exact { source: name.into(), destination: fixture.0.join("destination") };
        let (outcome, _) = fixture.run(&request, |_| Ok(()));
        assert_eq!(outcome.status(), 0, "source {name}");
        assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"source bytes");
        assert_eq!(fs::read(fixture.0.join("tree/file")).unwrap(), b"source bytes");
        assert!(fixture.0.join("destination/child").is_dir());
    }
}

#[test]
fn restored_hardlink_versions_allow_later_aliases_and_reject_external_changes() {
    for mutate in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("source")).unwrap();
        fs::create_dir(fixture.0.join("out")).unwrap();
        fs::write(fixture.0.join("source/a"), b"original").unwrap();
        fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/b")).unwrap();
        let restored_source = fixture.0.join("source/a");
        let mut request = fixture.request();
        request.operands = crate::Operands::Into { directory: "out".into(), first: "source/a".into(), rest: vec!["source/b".into()] };
        let first_capture = std::sync::atomic::AtomicBool::new(true);
        let (outcome, events) = fixture.run(&request, move |point| {
            force_exdev(point)?;
            if point == "after_capture" && first_capture.swap(false, Ordering::AcqRel) { return Err(io::Error::other("first capture interrupted")); }
            if mutate && point == "journal_restored" { fs::write(&restored_source, b"externally changed").unwrap(); }
            Ok(())
        });
        assert_eq!(outcome.status(), 1);
        assert_eq!(fs::read(fixture.0.join("out/a")).unwrap(), b"original");
        let states = events.iter().filter_map(|event| match event { Event::Operand(operand) => Some(operand.state), _ => None }).collect::<Vec<_>>();
        assert_eq!(states[0], OperandState::DestinationPublishedSourceRetained);
        if mutate {
            assert_eq!(states[1], OperandState::FailedBeforePublication);
            assert_eq!(fs::read(fixture.0.join("source/b")).unwrap(), b"externally changed");
            assert!(!fixture.0.join("out/b").exists());
        } else {
            assert_eq!(states[1], OperandState::Moved);
            assert!(!fixture.0.join("source/b").exists());
            assert_eq!(fs::read(fixture.0.join("source/a")).unwrap(), b"original");
            assert_eq!(fs::metadata(fixture.0.join("out/a")).unwrap().ino(), fs::metadata(fixture.0.join("out/b")).unwrap().ino());
        }
    }
}
