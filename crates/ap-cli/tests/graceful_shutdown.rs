//! Integration test verifying that the real compiled `audiophile-player.exe` binary:
//! 1. Starts and prints its startup line.
//! 2. Stays alive cleanly until interrupted.
//! 3. Handles a real Windows `CTRL_BREAK_EVENT` gracefully, prints stop output, and exits 0.

#[cfg(windows)]
mod windows_signal_test {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CTRL_BREAK_EVENT: u32 = 1;

    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
    }

    #[test]
    fn binary_starts_stays_running_and_stops_gracefully_on_ctrl_break() {
        let exe = env!("CARGO_BIN_EXE_audiophile-player");
        let mut child = Command::new(exe)
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn audiophile-player.exe");

        let stdout = child.stdout.take().expect("stdout pipe");
        let mut reader = BufReader::new(stdout);

        // 1. Verify startup message is emitted immediately
        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("failed reading startup line");
        assert!(
            first_line.contains("audiophile-player") && first_line.contains("started"),
            "unexpected startup line: {first_line:?}"
        );

        // 2. Verify process remains running cleanly without exiting on its own
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().expect("try_wait failed").is_none(),
            "process exited prematurely before receiving interrupt signal"
        );

        // 3. Send a real Windows CTRL_BREAK_EVENT to the child's process group
        let pid = child.id();
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        assert_ne!(sent, 0, "GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT) failed");

        // 4. Wait for graceful exit (within 5 seconds) and verify stop messages + exit code 0
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(st) = child.try_wait().expect("try_wait failed") {
                break st;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("audiophile-player did not exit gracefully within 5s after CTRL_BREAK_EVENT");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let mut rest = String::new();
        std::io::Read::read_to_string(&mut reader, &mut rest).expect("failed reading remaining stdout");

        assert!(
            status.success(),
            "expected exit code 0 on graceful stop, got {status:?}; output: {rest:?}"
        );
        assert!(
            rest.contains("CTRL_BREAK_EVENT") && rest.contains("stopped cleanly"),
            "missing graceful stop output: {rest:?}"
        );
    }
}
