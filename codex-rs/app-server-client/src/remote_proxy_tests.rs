use super::*;
use std::error::Error;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(unix)]
#[test]
fn remote_websocket_never_bypasses_required_proxy() -> Result<(), Box<dyn Error>> {
    const MODE: &str = "CODEX_TEST_REMOTE_PROXY_MODE";
    const PROXY: &str = "CODEX_TEST_REMOTE_PROXY_ADDRESS";
    const ORIGIN: &str = "CODEX_TEST_REMOTE_ORIGIN_ADDRESS";
    if let Ok(mode) = std::env::var(MODE) {
        let scheme = if mode == "tls" { "https" } else { "http" };
        codex_required_proxy::install(
            codex_required_proxy::RequiredProxy::new(
                &format!(
                    "{scheme}://fixture:fixture-secret@{}",
                    std::env::var(PROXY)?
                ),
                &[],
            )?,
            None,
        )?;
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(connect_websocket_endpoint(
                format!("ws://{}/rpc", std::env::var(ORIGIN)?),
                None,
            ));
        assert!(
            result.is_err(),
            "failed proxy must never become a direct connection"
        );
        return Ok(());
    }
    for mode in ["407", "reset", "unavailable", "tls"] {
        let origin = TcpListener::bind("0.0.0.0:0")?;
        let origin_address = origin.local_addr()?;
        let probe = TcpStream::connect(origin_address)?;
        drop(origin.accept()?);
        drop(probe);
        origin.set_nonblocking(true)?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let proxy_address = listener.local_addr()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let worker = if mode == "unavailable" {
            drop(listener);
            None
        } else {
            let stopped = Arc::clone(&stopped);
            Some(std::thread::spawn(
                move || -> std::io::Result<Option<String>> {
                    let (mut stream, _) = listener.accept()?;
                    if stopped.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    if mode == "tls" {
                        stream.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")?;
                        return Ok(Some("TLS handshake".into()));
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") && header.len() < 32 * 1024 {
                        let mut byte = [0];
                        if stream.read(&mut byte)? == 0 {
                            break;
                        }
                        header.push(byte[0]);
                    }
                    if mode == "407" {
                        stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")?;
                    }
                    Ok(Some(String::from_utf8_lossy(&header).to_ascii_lowercase()))
                },
            ))
        };
        let mut command = Command::new(std::env::current_exe()?);
        command.args([
            "--exact",
            "remote::proxy_tests::remote_websocket_never_bypasses_required_proxy",
        ]);
        for (key, _) in std::env::vars_os() {
            if key
                .to_str()
                .is_some_and(codex_required_proxy::is_command_proxy_env_key)
            {
                command.env_remove(key);
            }
        }
        let output = command
            .env(MODE, mode)
            .env(PROXY, proxy_address.to_string())
            .env(ORIGIN, origin_address.to_string())
            .output()?;
        stopped.store(true, Ordering::Release);
        let header = if let Some(worker) = worker {
            let _ = TcpStream::connect(proxy_address);
            worker.join().map_err(|_| "proxy worker panicked")??
        } else {
            None
        };
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            origin.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock,
            "{mode}: direct origin connection"
        );
        if mode == "407" || mode == "reset" {
            assert!(header.as_deref().is_some_and(|header| header.starts_with(&format!("connect {origin_address} "))), "proxy was not used");
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-secret"));
    }
    Ok(())
}
