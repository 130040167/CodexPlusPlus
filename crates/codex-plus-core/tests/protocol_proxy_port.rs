//! 使用独立读取进程，避免 OnceLock 或进程级环境变量掩盖端口迁移回归。
#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CHILD_MODE: &str = "CODEX_PLUS_TEST_PROXY_PORT_MODE";

#[test]
fn proxy_port_reader_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let path = codex_plus_core::paths::default_app_state_dir().join("protocol-proxy-port");
    let saved = std::fs::read_to_string(&path).unwrap();
    let preferred: u16 = saved.parse().unwrap();
    let selected = codex_plus_core::protocol_proxy::protocol_proxy_port();
    if mode == "healthy" {
        assert_eq!(selected, preferred, "健康的跨进程代理端口不能迁移");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    } else {
        assert_ne!(selected, preferred, "真正被其他服务占用时应选择可用端口");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            selected.to_string()
        );
    }
    assert_eq!(
        codex_plus_core::protocol_proxy::protocol_proxy_port(),
        selected
    );
}

fn check_running_service(mode: &str, status: &str, body: &str) {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let path = temp.path().join("protocol-proxy-port");
    std::fs::write(&path, port.to_string()).unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let thread_requests = requests.clone();
    let server = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    // Windows runner 的端口探测可能只连接后立即断开，不应终止 fixture。
                    if request.is_empty() {
                        continue;
                    }
                    assert!(
                        request.starts_with(b"GET /backend/status HTTP/1.1\r\n"),
                        "unexpected fixture request: {:?}",
                        String::from_utf8_lossy(&request)
                    );
                    thread_requests.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    // 显式复现无 HTTP 请求的连接，验证它不会阻止后续真实健康检查。
    drop(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "proxy_port_reader_child", "--nocapture"])
        .env(CHILD_MODE, mode)
        .env("CODEX_PLUS_STATE_DIR", temp.path())
        .env_remove("CODEX_PLUS_PROTOCOL_PROXY_PORT")
        .output();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests.load(Ordering::Relaxed) > 0,
        "必须实际探测 helper 身份"
    );
}

#[test]
fn healthy_existing_proxy_keeps_saved_port_across_processes() {
    check_running_service(
        "healthy",
        "200 OK",
        r#"{"status":"ok","transport":"http-helper","version":"1.9.0"}"#,
    );
}

#[test]
fn unrelated_service_does_not_keep_the_occupied_port() {
    check_running_service("foreign", "200 OK", r#"{"status":"ok"}"#);
}

#[test]
fn unhealthy_helper_does_not_keep_the_occupied_port() {
    check_running_service(
        "unhealthy",
        "503 Service Unavailable",
        r#"{"status":"ok","transport":"http-helper","version":"1.9.0"}"#,
    );
}

#[test]
fn malformed_helper_response_does_not_keep_the_occupied_port() {
    check_running_service("malformed", "200 OK", "invalid json");
}
