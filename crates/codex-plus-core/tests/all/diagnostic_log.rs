use serde_json::{Value, json};

/// 并发写诊断日志时每条记录各占一行：以前 `writeln!` 分两次写，
/// 别的线程的记录会插到记录和换行之间，两条挤成一行、都解析不了。
#[test]
fn concurrent_diagnostic_records_stay_one_per_line() {
    // 诊断日志路径是进程级状态，与其它测试模块串行。
    let _state = crate::common::global_state_lock();
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("codex-plus.log");
    codex_plus_core::diagnostic_log::set_diagnostic_log_path_for_tests(Some(log_path.clone()));
    let threads: Vec<_> = (0..8)
        .map(|thread| {
            std::thread::spawn(move || {
                for index in 0..200 {
                    codex_plus_core::diagnostic_log::append_diagnostic_log(
                        "test.concurrent_record",
                        json!({ "thread": thread, "index": index, "pad": "x".repeat(200) }),
                    )
                    .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    codex_plus_core::diagnostic_log::set_diagnostic_log_path_for_tests(None);

    let contents = std::fs::read_to_string(&log_path).unwrap();
    let ours = contents
        .lines()
        .filter(|line| line.contains("test.concurrent_record"))
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("每行都要是一条完整记录：{error}\n{line}"))
        })
        .count();
    assert_eq!(ours, 8 * 200);
}
