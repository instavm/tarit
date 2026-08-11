#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;
use vmm_core::vsock_exec::VsockExecChannel;

const MAGIC: &[u8; 4] = b"VEX2";
const VERSION: u8 = 2;
const REQUEST: u8 = 1;
const START: u8 = 2;
const STDOUT: u8 = 3;
const STDERR: u8 = 4;
const EXIT: u8 = 5;

fn socket_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/vsock-exec-tests")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.sock"))
}

fn read_frame(stream: &mut UnixStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 10];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..4], MAGIC);
    assert_eq!(header[4], VERSION);
    let len = u32::from_be_bytes(header[6..10].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).unwrap();
    (header[5], payload)
}

fn write_frame(stream: &mut UnixStream, kind: u8, payload: &[u8]) {
    let mut header = [0u8; 10];
    header[0..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[5] = kind;
    header[6..10].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}

fn request_id_and_command(payload: &[u8]) -> (u64, String) {
    assert!(payload.len() >= 12);
    let request_id = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let len = u32::from_be_bytes(payload[8..12].try_into().unwrap()) as usize;
    let command = String::from_utf8(payload[12..12 + len].to_vec()).unwrap();
    (request_id, command)
}

#[test]
fn channel_execs_chunked_stdout_and_stderr() {
    let socket = socket_path("separate-streams");
    let channel = VsockExecChannel::bind(&socket).unwrap();
    let guest = std::thread::spawn({
        let socket = socket.clone();
        move || {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .write_all(b"VMM_AGENT_READY\nVMM_VSOCK_EXEC_PROTO=2\n")
                .unwrap();
            let (kind, payload) = read_frame(&mut stream);
            assert_eq!(kind, REQUEST);
            let (request_id, command) = request_id_and_command(&payload);
            assert_eq!(command, "echo integration");
            write_frame(&mut stream, START, &request_id.to_be_bytes());
            let mut out = request_id.to_be_bytes().to_vec();
            out.extend_from_slice(b"stdout\n");
            write_frame(&mut stream, STDOUT, &out);
            let mut err = request_id.to_be_bytes().to_vec();
            err.extend_from_slice(b"stderr\n");
            write_frame(&mut stream, STDERR, &err);
            let mut exit = request_id.to_be_bytes().to_vec();
            exit.extend_from_slice(&3i32.to_be_bytes());
            write_frame(&mut stream, EXIT, &exit);
        }
    });

    for _ in 0..20 {
        if channel.is_connected() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (code, stdout, stderr, _) = channel
        .exec("echo integration", Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(code, 3);
    assert_eq!(stdout, "stdout\n");
    assert_eq!(stderr, "stderr\n");
    guest.join().unwrap();
    let _ = std::fs::remove_file(socket);
}

#[test]
fn channel_reconnects_after_ambiguous_timeout() {
    let socket = socket_path("reconnect-timeout");
    let channel = VsockExecChannel::bind(&socket).unwrap();
    let first = std::thread::spawn({
        let socket = socket.clone();
        move || {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .write_all(b"VMM_AGENT_READY\nVMM_VSOCK_EXEC_PROTO=2\n")
                .unwrap();
            let (kind, payload) = read_frame(&mut stream);
            assert_eq!(kind, REQUEST);
            let (request_id, _) = request_id_and_command(&payload);
            write_frame(&mut stream, START, &request_id.to_be_bytes());
            let mut out = request_id.to_be_bytes().to_vec();
            out.extend_from_slice(b"partial");
            write_frame(&mut stream, STDOUT, &out);
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    for _ in 0..20 {
        if channel.is_connected() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let timed_out = channel
        .exec("first", Duration::from_millis(20))
        .unwrap()
        .unwrap();
    assert_eq!(timed_out.0, -1);
    assert_eq!(timed_out.1, "partial");
    first.join().unwrap();

    let second = std::thread::spawn({
        let socket = socket.clone();
        move || {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .write_all(b"VMM_AGENT_READY\nVMM_VSOCK_EXEC_PROTO=2\n")
                .unwrap();
            let (kind, payload) = read_frame(&mut stream);
            assert_eq!(kind, REQUEST);
            let (request_id, command) = request_id_and_command(&payload);
            assert_eq!(command, "second");
            write_frame(&mut stream, START, &request_id.to_be_bytes());
            let mut out = request_id.to_be_bytes().to_vec();
            out.extend_from_slice(b"done\n");
            write_frame(&mut stream, STDOUT, &out);
            let mut exit = request_id.to_be_bytes().to_vec();
            exit.extend_from_slice(&0i32.to_be_bytes());
            write_frame(&mut stream, EXIT, &exit);
        }
    });
    for _ in 0..20 {
        if channel.is_connected() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (code, stdout, stderr, _) = channel
        .exec("second", Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(stdout, "done\n");
    assert!(stderr.is_empty());
    second.join().unwrap();
    let _ = std::fs::remove_file(socket);
}
