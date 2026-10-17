use epsh::error::ExitStatus;
use epsh::eval::Shell;
use std::sync::{Arc, Mutex};

#[test]
fn embedding_status_is_visible_to_the_next_expansion() {
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(out.clone()).build();
    shell.set_exit_status(ExitStatus::from(37));
    assert_eq!(shell.run_script("echo $?"), 0);
    assert_eq!(*out.lock().unwrap(), b"37\n");
}
