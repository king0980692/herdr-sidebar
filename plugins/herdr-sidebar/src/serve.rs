//! "Download via Browser": a short-lived HTTP server that hands one file (or
//! a folder, as a streamed .tar.gz) to the browser of the machine an ssh / ET
//! session came from. UI-free; the context menu calls [`start`], which runs
//! this binary again as `--serve-download <path> <bind-ip>` in the background
//! so the transfer outlives the sidebar.
//!
//! Safety, since this opens a listener:
//! - it binds ONLY the address the client itself used to reach this machine
//!   (SSH_CONNECTION's server ip, e.g. a Tailscale 100.x), never 0.0.0.0;
//! - the URL carries a 128-bit random token, and any other path is a 404;
//! - it exits [`AFTER_DOWNLOAD`] after the first complete transfer (browsers
//!   sometimes fetch twice), or after [`LIFETIME`] if nobody comes.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
pub const AFTER_DOWNLOAD: Duration = Duration::from_secs(30);

/// Spawn the background server for `path`, bound to `bind_ip`, and return
/// the download URL it prints once it is listening.
pub fn start(path: &Path, bind_ip: &str) -> io::Result<String> {
    spawn_server("--serve-download", path, bind_ip)
}

/// Run this binary again as `<flag> <path> <bind-ip>` in its own process
/// group and return the URL it prints once it is listening.
pub(crate) fn spawn_server(flag: &str, path: &Path, bind_ip: &str) -> io::Result<String> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg(flag)
        .arg(path)
        .arg(bind_ip)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Its own process group: closing the sidebar's pty hangs up only the
    // foreground group, so the transfer survives the sidebar closing.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    let url = line.trim();
    if url.starts_with("http://") {
        Ok(url.to_string())
    } else {
        let _ = child.kill();
        Err(io::Error::other("server did not start"))
    }
}

/// What gets sent: a regular file as itself, a folder as `<name>.tar.gz`.
#[derive(Debug, PartialEq, Eq)]
enum Payload {
    File(PathBuf),
    Folder(PathBuf),
}

impl Payload {
    fn of(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            Ok(Self::Folder(path.to_path_buf()))
        } else if meta.is_file() {
            Ok(Self::File(path.to_path_buf()))
        } else {
            Err(io::Error::other("not a file or folder"))
        }
    }

    fn download_name(&self) -> String {
        let (Self::File(path) | Self::Folder(path)) = self;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        match self {
            Self::File(_) => name,
            Self::Folder(_) => format!("{name}.tar.gz"),
        }
    }
}

/// `--serve-download <path> <bind-ip>`: print the URL on stdout, then serve
/// until done. Runs in the background process [`start`] spawned.
pub fn run(path: &Path, bind_ip: &str) -> io::Result<()> {
    let payload = Payload::of(path)?;
    let listener = TcpListener::bind((bind_ip, 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token()?;
    let name = payload.download_name();
    let host = if bind_ip.contains(':') {
        format!("[{bind_ip}]")
    } else {
        bind_ip.to_string()
    };
    let url = format!("http://{host}:{port}/{token}/{}", percent_encode(&name));
    {
        let mut out = io::stdout().lock();
        writeln!(out, "{url}")?;
        out.flush()?;
    }
    serve(listener, &token, &payload, LIFETIME, AFTER_DOWNLOAD)
}

fn serve(
    listener: TcpListener,
    token: &str,
    payload: &Payload,
    lifetime: Duration,
    after_download: Duration,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let started = Instant::now();
    let done = Arc::new(AtomicBool::new(false));
    let mut done_at: Option<Instant> = None;
    loop {
        if started.elapsed() >= lifetime {
            return Ok(());
        }
        if done_at.is_none() && done.load(Ordering::SeqCst) {
            done_at = Some(Instant::now());
        }
        if done_at.is_some_and(|at| at.elapsed() >= after_download) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let token = token.to_string();
                let payload = match payload {
                    Payload::File(p) => Payload::File(p.clone()),
                    Payload::Folder(p) => Payload::Folder(p.clone()),
                };
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    if handle(stream, &token, &payload).unwrap_or(false) {
                        done.store(true, Ordering::SeqCst);
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// One request. `Ok(true)` when a full body was sent.
fn handle(mut stream: TcpStream, token: &str, payload: &Payload) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 || head.len() > 16 * 1024 {
            return Ok(false);
        }
        head.extend_from_slice(&buf[..n]);
    }
    let request = String::from_utf8_lossy(&head);
    let (method, target) = parse_request_line(&request).unwrap_or(("", ""));
    let wanted = matches!(method, "GET" | "HEAD") && token_matches(target, token);
    if !wanted {
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(false);
    }
    let name = payload.download_name();
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback(&name),
        percent_encode(&name)
    );
    match payload {
        Payload::File(path) => {
            let mut file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nContent-Disposition: {disposition}\r\nConnection: close\r\n\r\n"
            )?;
            if method == "HEAD" {
                return Ok(false);
            }
            let sent = io::copy(&mut file, &mut stream)?;
            Ok(sent == len)
        }
        Payload::Folder(path) => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Disposition: {disposition}\r\nConnection: close\r\n\r\n"
            )?;
            if method == "HEAD" {
                return Ok(false);
            }
            let parent = path.parent().unwrap_or(Path::new("/"));
            let base = path.file_name().unwrap_or(path.as_os_str());
            let mut tar = Command::new("tar")
                .arg("-czf")
                .arg("-")
                .arg("-C")
                .arg(parent)
                .arg(base)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            let mut out = tar
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("no tar output"))?;
            io::copy(&mut out, &mut stream)?;
            Ok(tar.wait()?.success())
        }
    }
}

pub(crate) fn parse_request_line(request: &str) -> Option<(&str, &str)> {
    let mut parts = request.lines().next()?.split_whitespace();
    Some((parts.next()?, parts.next()?))
}

/// The path must be `/<token>/…` exactly; compared without early exit.
pub(crate) fn token_matches(target: &str, token: &str) -> bool {
    let Some(rest) = target.strip_prefix('/') else {
        return false;
    };
    let given = rest.split('/').next().unwrap_or("");
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

pub(crate) fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `filename="…"` for browsers that ignore `filename*`: ASCII only, no quotes.
fn ascii_fallback(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-serve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fetch(port: u16, path: &str) -> Vec<u8> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_the_file_only_under_its_token_then_exits() {
        let dir = temp("file");
        let file = dir.join("報告 v1.txt");
        std::fs::write(&file, b"hello").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let payload = Payload::of(&file).unwrap();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "abc123",
                &payload,
                Duration::from_secs(10),
                Duration::from_millis(200),
            )
        });

        let denied = String::from_utf8(fetch(port, "/wrong/x")).unwrap();
        assert!(denied.starts_with("HTTP/1.1 404"), "{denied}");

        let ok = fetch(port, "/abc123/anything");
        let text = String::from_utf8_lossy(&ok);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("Content-Length: 5"), "{text}");
        assert!(
            text.contains("filename*=UTF-8''%E5%A0%B1%E5%91%8A%20v1.txt"),
            "{text}"
        );
        assert!(text.ends_with("hello"), "{text}");

        // A completed download ends the server after the grace period.
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn folders_go_out_as_tar_gz() {
        let dir = temp("dir");
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        std::fs::write(dir.join("proj/a.txt"), b"x").unwrap();
        let payload = Payload::of(&dir.join("proj")).unwrap();
        assert_eq!(payload.download_name(), "proj.tar.gz");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "tok",
                &payload,
                Duration::from_secs(10),
                Duration::from_millis(200),
            )
        });
        let body = fetch(port, "/tok/proj.tar.gz");
        let split = body.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&body[split..split + 2], &[0x1f, 0x8b], "gzip magic");
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unvisited_server_times_out() {
        let dir = temp("idle");
        std::fs::write(dir.join("f"), b"x").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let payload = Payload::of(&dir.join("f")).unwrap();
        let started = Instant::now();
        serve(
            listener,
            "t",
            &payload,
            Duration::from_millis(300),
            AFTER_DOWNLOAD,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_check_is_exact() {
        assert!(token_matches("/abc/x.txt", "abc"));
        assert!(!token_matches("/abcd/x.txt", "abc"));
        assert!(!token_matches("/ab/x.txt", "abc"));
        assert!(!token_matches("abc/x", "abc"));
    }
}
