//! Without a terminal, the dashboard says what's wrong instead of failing
//! with a bare OS error.

use std::process::{Command, Stdio};

#[test]
fn without_a_terminal_it_says_so_and_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_diskwatch"))
        .arg("--no-config")
        .stdin(Stdio::null())
        .output()
        .expect("run diskwatch");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("needs a terminal") && err.contains("--diag"),
        "{err}"
    );
    assert!(out.stdout.is_empty(), "nothing drawn into the pipe");
}
