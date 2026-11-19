use super::*;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(unix)]
#[test]
fn responses_forwarder_never_bypasses_required_proxy() -> Result<()> {
    const MODE: &str = "CODEX_TEST_RESPONSES_PROXY_MODE";
    const PROXY: &str = "CODEX_TEST_RESPONSES_PROXY_ADDRESS";
    const ORIGIN: &str = "CODEX_TEST_RESPONSES_ORIGIN_ADDRESS";
    if let Ok(mode) = std::env::var(MODE) {
        codex_required_proxy::install(
            codex_required_proxy::RequiredProxy::new(
                &format!("http://fixture:fixture-secret@{}", std::env::var(PROXY)?),
                &[],
            )?,
            None,
        )?;
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let gateway = codex_command_proxy::spawn(
                    tokio::net::TcpListener::bind("127.0.0.1:0").await?,
                    codex_required_proxy::current,
                )?;
                codex_required_proxy::set_command_proxy(gateway)?;
                tokio::task::spawn_blocking(move || {
                    let client = upstream_client_builder(&Auth::ApiKey("fixture-api-key"))?
                        .connect_timeout(Duration::from_secs(2))
                        .timeout(Duration::from_secs(3))
                        .build()?;
                    let result = client
                        .post(format!("http://{}/v1/responses", std::env::var(ORIGIN)?))
                        .body("{}")
                        .send();
                    if mode == "ok" {
                        assert_eq!(result?.status(), reqwest::StatusCode::OK);
                    } else {
                        assert!(
                            result.is_err()
                                || result.is_ok_and(|response| !response.status().is_success())
                        );
                    }
                    Ok(())
                })
                .await?
            });
    }
    for mode in ["ok", "407", "reset", "unavailable"] {
        let origin = TcpListener::bind("0.0.0.0:0")?;
        let origin_address = origin.local_addr()?;
        let probe = std::net::TcpStream::connect(origin_address)?;
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
                    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") && header.len() < 32 * 1024 {
                        let mut byte = [0];
                        if stream.read(&mut byte)? == 0 {
                            break;
                        }
                        header.push(byte[0]);
                    }
                    let header = String::from_utf8_lossy(&header).to_ascii_lowercase();
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if length > 1024 * 1024 {
                        return Err(std::io::ErrorKind::InvalidData.into());
                    }
                    stream.read_exact(&mut vec![0; length])?;
                    if mode != "reset" {
                        let response: &[u8] = if mode == "407" {
                            b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        } else {
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                        };
                        stream.write_all(response)?;
                    }
                    Ok(Some(header))
                },
            ))
        };
        let mut command = Command::new(std::env::current_exe()?);
        command.args([
            "--exact",
            "required_proxy_tests::responses_forwarder_never_bypasses_required_proxy",
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
            let _ = std::net::TcpStream::connect(proxy_address);
            worker
                .join()
                .map_err(|_| anyhow!("proxy worker panicked"))??
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
            std::io::ErrorKind::WouldBlock,
            "{mode}: direct upstream connection"
        );
        if mode == "ok" || mode == "407" || mode == "reset" {
            assert!(header.as_deref().is_some_and(|header| {
                header.starts_with(&format!("post http://{origin_address}/v1/responses "))
            }));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-secret"));
    }
    Ok(())
}
