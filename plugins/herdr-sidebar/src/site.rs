//! "Open in Browser": view a file — an HTML page with its relative CSS, JS
//! and images, an SVG, a PDF, an image — in the browser of the machine an
//! ssh / ET session came from. UI-free; the context menu calls [`start`],
//! which runs this binary again as `--serve-site <file> <bind-ip>`.
//!
//! The file's FOLDER is served (so `<link href="style.css">` resolves), every
//! response inline with its content type, and every request reads from disk,
//! so editing the page and refreshing the browser shows the change.
//!
//! Same safety as the download server (see `serve.rs`): bound only to the
//! address the client used, a 128-bit token in every path, plus no path —
//! `..`, an absolute one, or a symlink — can reach outside the folder. It
//! exits after [`IDLE`] without a request.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::serve::{parse_request_line, random_token, token_matches};

pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// Spawn the background server for `file`'s folder, bound to `bind_ip`, and
/// return the URL of `file` itself.
pub fn start(file: &Path, bind_ip: &str) -> io::Result<String> {
    crate::serve::spawn_server("--serve-site", file, bind_ip)
}

/// Files a browser shows better than the terminal preview: pages, PDFs,
/// images, audio and video. Code and plain text are not on the list.
pub fn is_browser_document(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "html"
            | "htm"
            | "xhtml"
            | "pdf"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "bmp"
            | "ico"
            | "mp4"
            | "webm"
            | "mp3"
            | "wav"
            | "ogg"
    )
}

/// `file`'s link on an already-running server for its folder, if that
/// server still answers: `base` is the server's `http://host:port/<token>`.
pub fn reuse_link(base: &str, file: &Path) -> Option<String> {
    let authority = base.strip_prefix("http://")?.split('/').next()?;
    let addr: std::net::SocketAddr = authority.parse().ok()?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    let name = file.file_name()?.to_string_lossy().into_owned();
    Some(format!("{base}/{}", encode_path(&name)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where a folder's server keeps its port and token, so a restarted server
/// comes back on the SAME link and a browser tab left open keeps working.
fn saved_path(root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    crate::rundir::dir("site").join(format!("{:016x}", hasher.finish()))
}

fn load_saved(root: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(saved_path(root)).ok()?;
    let (port, token) = text.trim().split_once(' ')?;
    let token = token.trim();
    (token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Some((port.parse().ok()?, token.to_string())))
        .flatten()
}

/// The `http://ip:port/<token>` base a previous server for `folder` used,
/// for the sidebar to try before starting a new one.
pub fn saved_base(folder: &Path, bind_ip: &str) -> Option<String> {
    let folder = folder.canonicalize().ok()?;
    let (port, token) = load_saved(&folder)?;
    let host = if bind_ip.contains(':') {
        format!("[{bind_ip}]")
    } else {
        bind_ip.to_string()
    };
    Some(format!("http://{host}:{port}/{token}"))
}

/// `--serve-site <file> <bind-ip>`: print the file's URL, then serve its
/// folder until idle. Runs in the background process [`start`] spawned.
pub fn run(file: &Path, bind_ip: &str) -> io::Result<()> {
    let file = file.canonicalize()?;
    let root = file
        .parent()
        .ok_or_else(|| io::Error::other("file has no folder"))?
        .to_path_buf();
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::other("no file name"))?;
    // Same port and token as last time when the port is free; else fresh.
    let saved = load_saved(&root);
    let (listener, token) = match saved.as_ref().and_then(|(port, token)| {
        TcpListener::bind((bind_ip, *port))
            .ok()
            .map(|l| (l, token.clone()))
    }) {
        Some(reused) => reused,
        None => (TcpListener::bind((bind_ip, 0))?, random_token()?),
    };
    let port = listener.local_addr()?.port();
    let saved_file = saved_path(&root);
    if let Some(dir) = saved_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&saved_file, format!("{port} {token}"));
    let host = if bind_ip.contains(':') {
        format!("[{bind_ip}]")
    } else {
        bind_ip.to_string()
    };
    {
        let mut out = io::stdout().lock();
        writeln!(out, "http://{host}:{port}/{token}/{}", encode_path(&name))?;
        out.flush()?;
    }
    serve(listener, &token, &root, IDLE)
}

/// Files this server has handed out; their newest mtime is what an open page
/// watches to know it should reload.
type Served = Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>;

fn serve(listener: TcpListener, token: &str, root: &Path, idle: Duration) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let last = Arc::new(AtomicU64::new(now_ms()));
    let served: Served = Arc::default();
    loop {
        if now_ms().saturating_sub(last.load(Ordering::SeqCst)) >= idle.as_millis() as u64 {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Any request — the open page's reload check included — keeps
                // the server alive.
                last.store(now_ms(), Ordering::SeqCst);
                let _ = stream.set_nonblocking(false);
                let token = token.to_string();
                let root = root.to_path_buf();
                let served = Arc::clone(&served);
                std::thread::spawn(move || {
                    let _ = handle(stream, &token, &root, &served);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Newest modification time (ms) among the files served so far.
fn signature(served: &Served) -> u128 {
    let paths = served.lock().map(|s| s.clone()).unwrap_or_default();
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .max()
        .unwrap_or(0)
}

/// Injected into every HTML page: once a second, ask whether any file the
/// page loaded changed on disk, and reload if so. The first answer is the
/// baseline (taken after the page's own CSS / JS / images have loaded).
const LIVE_RELOAD: &str = r#"<script>(()=>{const u=location.pathname.split('/').slice(0,2).join('/')+'/__live';let s=null;async function t(){try{const v=await (await fetch(u,{cache:'no-store'})).text();if(s===null)s=v;else if(v!==s){location.reload();return}}catch(e){}setTimeout(t,1000)}addEventListener('load',()=>setTimeout(t,500))})();</script>"#;

fn with_live_reload(html: Vec<u8>) -> Vec<u8> {
    let lower = String::from_utf8_lossy(&html).to_lowercase();
    match lower.rfind("</body>") {
        // Lowercasing keeps byte offsets for the ASCII tag being searched.
        Some(at) if lower.len() == html.len() => {
            let mut out = html[..at].to_vec();
            out.extend_from_slice(LIVE_RELOAD.as_bytes());
            out.extend_from_slice(&html[at..]);
            out
        }
        _ => {
            let mut out = html;
            out.extend_from_slice(LIVE_RELOAD.as_bytes());
            out
        }
    }
}

fn handle(mut stream: TcpStream, token: &str, root: &Path, served: &Served) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 || head.len() > 16 * 1024 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let request = String::from_utf8_lossy(&head).into_owned();
    let (method, target) = parse_request_line(&request).unwrap_or(("", ""));
    let allowed = matches!(method, "GET" | "HEAD") && token_matches(target, token);
    if allowed
        && target
            .split('?')
            .next()
            .is_some_and(|p| p.ends_with("/__live"))
    {
        let body = signature(served).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        return stream.write_all(body.as_bytes());
    }
    let found = allowed.then(|| resolve(root, target)).flatten();
    let Some(path) = found else {
        let body = b"not found";
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        return stream.write_all(body);
    };
    if let Ok(mut set) = served.lock() {
        set.insert(path.clone());
    }
    let kind = content_type(&path);
    let body = if kind.starts_with("text/html") {
        with_live_reload(std::fs::read(&path)?)
    } else {
        std::fs::read(&path)?
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if method == "GET" {
        stream.write_all(&body)?;
    }
    Ok(())
}

/// The file a request names inside `root`, or `None`: the path after the
/// token, percent-decoded, query dropped, made only of normal components,
/// and — after following symlinks — still inside `root`. A folder serves
/// its `index.html`.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let rest = target.strip_prefix('/')?.split_once('/').map(|(_, r)| r)?;
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let decoded = crate::upload::percent_decode(rest);
    let relative = Path::new(&decoded);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let mut path = root.join(relative).canonicalize().ok()?;
    if !path.starts_with(root) {
        return None;
    }
    if path.is_dir() {
        path = path.join("index.html");
    }
    path.is_file().then_some(path)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "xml" => "application/xml",
        "txt" | "md" | "csv" | "log" | "rs" | "py" | "sh" | "toml" | "yaml" | "yml" => {
            "text/plain; charset=utf-8"
        }
        _ => "application/octet-stream",
    }
}

/// Percent-encode a file name for a URL path (keeps it one segment).
pub fn encode_path(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("hs-site-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("www/css")).unwrap();
        std::fs::write(base.join("www/index.html"), "<link href=css/a.css>").unwrap();
        std::fs::write(base.join("www/css/a.css"), "body{}").unwrap();
        std::fs::write(base.join("secret.txt"), "nope").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("secret.txt"), base.join("www/link.txt")).unwrap();
        base.canonicalize().unwrap()
    }

    fn get(port: u16, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_the_folder_inline_and_nothing_outside_it() {
        let base = site("serve");
        let root = base.join("www");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_root = root.clone();
        std::thread::spawn(move || serve(listener, "tok", &server_root, Duration::from_secs(30)));

        let page = get(port, "/tok/index.html");
        assert!(
            page.starts_with("HTTP/1.1 200") && page.contains("text/html"),
            "{page}"
        );
        assert!(!page.contains("attachment"), "inline, not a download");
        assert!(get(port, "/tok/css/a.css?v=2").contains("text/css"));
        assert!(
            get(port, "/tok/").contains("<link href=css/a.css>"),
            "folder → index.html"
        );
        for escape in [
            "/tok/../secret.txt",
            "/tok/%2E%2E/secret.txt",
            "/tok//etc/passwd",
            "/tok/link.txt",
            "/bad/index.html",
        ] {
            assert!(get(port, escape).starts_with("HTTP/1.1 404"), "{escape}");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn pages_get_live_reload_and_the_check_follows_edits() {
        let base = site("live");
        let root = base.join("www");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_root = root.clone();
        std::thread::spawn(move || serve(listener, "tok", &server_root, Duration::from_secs(30)));

        let page = get(port, "/tok/index.html");
        assert!(page.contains("__live"), "script injected: {page}");
        let css = get(port, "/tok/css/a.css");
        assert!(!css.contains("__live"), "only HTML is touched");
        let body = |r: String| r.rsplit("\r\n\r\n").next().unwrap().to_string();
        let before = body(get(port, "/tok/__live"));
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(root.join("css/a.css"), "body{color:red}").unwrap();
        let after = body(get(port, "/tok/__live"));
        assert_ne!(
            before, after,
            "editing a loaded asset changes the signature"
        );
        assert!(get(port, "/bad/__live").starts_with("HTTP/1.1 404"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn live_reload_goes_before_the_closing_body_tag() {
        let out =
            String::from_utf8(with_live_reload(b"<html><BODY>x</BODY></html>".to_vec())).unwrap();
        assert!(
            out.find("__live").unwrap() < out.find("</BODY>").unwrap(),
            "{out}"
        );
        let bare = String::from_utf8(with_live_reload(b"<p>x".to_vec())).unwrap();
        assert!(bare.ends_with("</script>"));
    }

    #[test]
    fn documents_go_to_the_browser_code_does_not() {
        for doc in ["a.html", "B.PDF", "x.svg", "p.png", "v.mp4"] {
            assert!(is_browser_document(Path::new(doc)), "{doc}");
        }
        for code in ["main.rs", "README.md", "a.txt", "Makefile", "x.json"] {
            assert!(!is_browser_document(Path::new(code)), "{code}");
        }
    }

    #[test]
    fn a_live_server_is_reused_and_a_dead_one_is_not() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}/tok");
        assert_eq!(
            reuse_link(&base, Path::new("/x/報 1.html")).as_deref(),
            Some(format!("{base}/%E5%A0%B1%201.html").as_str())
        );
        drop(listener);
        assert_eq!(reuse_link(&base, Path::new("/x/a.html")), None);
    }

    #[test]
    fn an_idle_site_stops() {
        let base = site("idle");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let started = std::time::Instant::now();
        serve(listener, "t", &base.join("www"), Duration::from_millis(300)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(base);
    }
}
