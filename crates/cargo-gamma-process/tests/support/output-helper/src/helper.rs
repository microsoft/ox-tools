// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

thread_local! {
    static HELD: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
    static CONNECTIONS: std::cell::RefCell<Vec<std::net::TcpStream>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn main() {
    let mut code = 0;
    for argument in std::env::args().skip(1) {
        let Some(directive) = argument.strip_prefix("--gamma-step=") else {
            continue;
        };
        if let Some(status) = step(directive) {
            code = status;
            break;
        }
    }
    std::process::exit(code);
}

fn step(directive: &str) -> Option<i32> {
    let (name, payload) = directive.split_once(':').unwrap_or((directive, ""));
    match name {
        "sleep" => {
            let ms = payload.parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            None
        }
        "exit" => Some(payload.parse().unwrap_or(0)),
        "connect" => {
            let connection = std::net::TcpStream::connect(payload).expect("the test listener accepts the helper");
            CONNECTIONS.with(|connections| connections.borrow_mut().push(connection));
            None
        }
        "flood" => {
            use std::io::Write as _;
            let count: usize = payload.parse().expect("a byte count");
            let text = vec![b'x'; count];
            std::io::stdout().write_all(&text).expect("captured stdout");
            std::io::stdout().write_all(b"diagnostic-tail").expect("stdout tail");
            std::io::stderr().write_all(&text).expect("captured stderr");
            std::io::stderr().write_all(b"diagnostic-tail").expect("stderr tail");
            None
        }
        "raw-output" => {
            fn emit(mut output: impl std::io::Write, mut count: usize) {
                let mut chunk = [0_u8; 8192];
                for (index, byte) in chunk.iter_mut().enumerate() {
                    *byte = b'a' + (index % 26) as u8;
                }
                while count > 0 {
                    let written = count.min(chunk.len());
                    output.write_all(&chunk[..written]).expect("raw captured output");
                    count -= written;
                }
            }
            let (stdout, stderr) = payload.split_once(':').expect("stdout and stderr byte counts");
            emit(std::io::stdout().lock(), stdout.parse().expect("stdout byte count"));
            emit(std::io::stderr().lock(), stderr.parse().expect("stderr byte count"));
            None
        }
        "touch" => {
            let _ = std::fs::write(payload, b"");
            None
        }
        "wait" => {
            if payload.is_empty() {
                return Some(98);
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !std::path::Path::new(payload).exists() {
                if std::time::Instant::now() >= deadline {
                    return Some(98);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
        "spawn" => {
            launch(payload, false);
            None
        }
        "flee" => {
            launch(payload, true);
            None
        }
        "eat" => {
            let mib: usize = payload.parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..mib {
                let mut block = vec![0_u8; 1024 * 1024];
                for at in (0..block.len()).step_by(4096) {
                    block[at] = 1;
                }
                held.push(block);
            }
            HELD.with(|slot| slot.borrow_mut().extend(held));
            None
        }
        _ => Some(97),
    }
}

fn launch(payload: &str, own_group: bool) {
    let executable = if cfg!(target_os = "linux") {
        std::path::PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().expect("the helper knows its own path")
    };
    let mut child = std::process::Command::new(executable);
    for inner in payload.split('|') {
        let _ = child.arg(format!("--gamma-step={inner}"));
    }
    #[cfg(unix)]
    if own_group {
        let _ = child.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = own_group;
    let _spawned = child.spawn().expect("the helper can start another copy of itself");
}
