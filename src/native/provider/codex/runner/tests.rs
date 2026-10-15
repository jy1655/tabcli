use super::*;

#[test]
fn codex_command_failure_distinguishes_creation_from_possible_delivery() {
    let suspended = CodexCommandFailure::before_resume(anyhow::anyhow!("containment failed"), true);
    assert!(!suspended.delivery_may_have_started);

    let already_running =
        CodexCommandFailure::before_resume(anyhow::anyhow!("containment failed"), false);
    assert!(already_running.delivery_may_have_started);
}

#[test]
fn codex_command_output_reader_enforces_its_byte_limit() {
    use std::io::Write;

    assert_eq!(MAX_NATIVE_QUEUE_OUTPUT_BYTES, 1_048_576);
    let mut exact = tempfile::tempfile().unwrap();
    exact.set_len(MAX_NATIVE_QUEUE_OUTPUT_BYTES as u64).unwrap();
    let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
    let (output, truncated) = read_capped_output(&mut exact, &mut remaining).unwrap();
    assert_eq!(output.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES);
    assert!(!truncated);
    assert_eq!(remaining, 0);

    let mut oversized = tempfile::tempfile().unwrap();
    oversized
        .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES + 1) as u64)
        .unwrap();
    let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
    let (output, truncated) = read_capped_output(&mut oversized, &mut remaining).unwrap();
    assert_eq!(output.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES);
    assert!(truncated);
    assert_eq!(remaining, 0);

    let mut stdout = tempfile::tempfile().unwrap();
    stdout
        .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 + 1) as u64)
        .unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    stderr
        .set_len((MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 + 1) as u64)
        .unwrap();
    let mut remaining = MAX_NATIVE_QUEUE_OUTPUT_BYTES;
    let (stdout, stdout_truncated) = read_capped_output(&mut stdout, &mut remaining).unwrap();
    let (stderr, stderr_truncated) = read_capped_output(&mut stderr, &mut remaining).unwrap();
    assert!(stdout.len() + stderr.len() <= MAX_NATIVE_QUEUE_OUTPUT_BYTES);
    assert!(stdout_truncated || stderr_truncated);
    assert_eq!(stdout.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 + 1);
    assert!(!stdout_truncated);
    assert_eq!(stderr.len(), MAX_NATIVE_QUEUE_OUTPUT_BYTES / 2 - 1);
    assert!(stderr_truncated);
    assert_eq!(remaining, 0);

    let mut empty = tempfile::tempfile().unwrap();
    let mut remaining = 0;
    let (output, truncated) = read_capped_output(&mut empty, &mut remaining).unwrap();
    assert!(output.is_empty());
    assert!(!truncated);
    assert_eq!(remaining, 0);

    let mut one_byte = tempfile::tempfile().unwrap();
    one_byte.write_all(b"x").unwrap();
    let (output, truncated) = read_capped_output(&mut one_byte, &mut remaining).unwrap();
    assert!(output.is_empty());
    assert!(truncated);
    assert_eq!(remaining, 0);

    let mut prefix = tempfile::tempfile().unwrap();
    prefix.write_all(b"abcd").unwrap();
    let mut remaining = 3;
    let (output, truncated) = read_capped_output(&mut prefix, &mut remaining).unwrap();
    assert_eq!(output, b"abc");
    assert!(truncated);
    assert_eq!(remaining, 0);
}
