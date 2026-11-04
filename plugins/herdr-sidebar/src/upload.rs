//! "Upload here": the reverse of "Download via Browser". A short-lived HTTP
//! server serves an upload page to the browser of the machine an ssh / ET
//! session came from; files dropped there are saved into one folder here.
//! UI-free; the context menu calls [`start`], which runs this binary again as
//! `--serve-upload <dir> <bind-ip>` in the background.
//!
//! Same safety as the download server (see `serve.rs`): bound only to the
//! address the client used, a 128-bit token in every path, plus:
//! - a file is written to a hidden `.part` file and renamed into place only
//!   once complete; an existing file is never overwritten (`name copy.ext`);
//! - only the bare file name the browser sends is used: no `/`, `\`, `..`;
//! - the server exits on the page's Done button, or after [`IDLE`] without an
//!   upload.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::serve::{parse_request_line, random_token, token_matches};

pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// Spawn the background upload server for `dir`, bound to `bind_ip`, and
/// return the page's URL.
pub fn start(dir: &Path, bind_ip: &str) -> io::Result<String> {
    crate::serve::spawn_server("--serve-upload", dir, bind_ip)
}

/// Touched after every saved upload, so a sidebar showing the folder can
/// refresh its tree.
pub fn stamp_path() -> PathBuf {
    crate::rundir::dir("upload").join("stamp")
}

/// `--serve-upload <dir> <bind-ip>`: print the URL on stdout, then serve
/// until Done or idle. Runs in the background process [`start`] spawned.
pub fn run(dir: &Path, bind_ip: &str) -> io::Result<()> {
    if !dir.is_dir() {
        return Err(io::Error::other("not a folder"));
    }
    let listener = TcpListener::bind((bind_ip, 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token()?;
    let host = if bind_ip.contains(':') {
        format!("[{bind_ip}]")
    } else {
        bind_ip.to_string()
    };
    {
        let mut out = io::stdout().lock();
        writeln!(out, "http://{host}:{port}/{token}/")?;
        out.flush()?;
    }
    serve(listener, &token, dir, IDLE)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn serve(listener: TcpListener, token: &str, dir: &Path, idle: Duration) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let done = Arc::new(AtomicBool::new(false));
    let last_activity = Arc::new(AtomicU64::new(now_ms()));
    loop {
        if done.load(Ordering::SeqCst) {
            // Let the Done response reach the browser first.
            std::thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        let quiet = now_ms().saturating_sub(last_activity.load(Ordering::SeqCst));
        if quiet >= idle.as_millis() as u64 {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let token = token.to_string();
                let dir = dir.to_path_buf();
                let done = Arc::clone(&done);
                let last_activity = Arc::clone(&last_activity);
                std::thread::spawn(move || {
                    let _ = handle(stream, &token, &dir, &done, &last_activity);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn handle(
    mut stream: TcpStream,
    token: &str,
    dir: &Path,
    done: &AtomicBool,
    last_activity: &AtomicU64,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    // Read the head; whatever arrives after it is the start of the body.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 || buf.len() > 32 * 1024 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let body_start = buf[head_end..].to_vec();
    let (method, target) = parse_request_line(&head).unwrap_or(("", ""));
    if !token_matches(target, token) {
        return respond(&mut stream, "404 Not Found", "text/plain", b"not found");
    }
    let rest = target
        .strip_prefix('/')
        .and_then(|t| t.split_once('/'))
        .map(|(_, rest)| rest)
        .unwrap_or("");
    match (method, rest) {
        ("GET", "") => {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string());
            let page = PAGE.replace("{{FOLDER}}", &html_escape(&name));
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
            )
        }
        ("POST", "done") => {
            done.store(true, Ordering::SeqCst);
            respond(&mut stream, "200 OK", "application/json", b"{\"ok\":true}")
        }
        ("PUT", rest) if rest.starts_with("f/") => {
            let Some(name) = safe_name(&percent_decode(&rest[2..])) else {
                return respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    b"bad file name",
                );
            };
            let Some(len) = content_length(&head) else {
                return respond(
                    &mut stream,
                    "411 Length Required",
                    "text/plain",
                    b"length required",
                );
            };
            last_activity.store(now_ms(), Ordering::SeqCst);
            match save(&mut stream, dir, &name, body_start, len, last_activity) {
                Ok(saved) => {
                    let _ = touch(&stamp_path());
                    let body = format!("{{\"name\":\"{}\"}}", json_escape(&saved));
                    respond(&mut stream, "200 OK", "application/json", body.as_bytes())
                }
                Err(err) => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain",
                    err.to_string().as_bytes(),
                ),
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Stream `len` body bytes (the first of them already read) into a hidden
/// `.part` file, then rename it to a free name. Returns the saved name.
fn save(
    stream: &mut TcpStream,
    dir: &Path,
    name: &str,
    first: Vec<u8>,
    len: u64,
    last_activity: &AtomicU64,
) -> io::Result<String> {
    let part = dir.join(format!(".{name}.{}.part", random_token()?));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        let first_len = (first.len() as u64).min(len);
        file.write_all(&first[..first_len as usize])?;
        let mut remaining = len - first_len;
        let mut chunk = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(chunk.len() as u64) as usize;
            let n = stream.read(&mut chunk[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "upload cut short",
                ));
            }
            file.write_all(&chunk[..n])?;
            remaining -= n as u64;
            last_activity.store(now_ms(), Ordering::SeqCst);
        }
        file.sync_all()
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&part);
        return Err(err);
    }
    let target = crate::actions::free_copy_name(dir, name);
    std::fs::rename(&part, &target)?;
    Ok(target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default())
}

fn touch(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, now_ms().to_string())
}

fn content_length(head: &str) -> Option<u64> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

/// The bare file name only: the last path component, never empty, `.`,
/// `..`, or a hidden `.part` look-alike; control characters dropped.
fn safe_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = last.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim().to_string();
    (!name.is_empty() && name != "." && name != "..").then_some(name)
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The whole page: no external resources. Files go up one at a time as raw
/// `PUT …/f/<name>` bodies, each with its own progress bar.
const PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Upload to {{FOLDER}}</title>
<style>
:root{--bg:#f6f6f4;--fg:#1d1d1b;--muted:#6b6b66;--line:#d9d9d4;--accent:#2f6fde;--ok:#2e8540;--bad:#c0392b;--card:#fff}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ececea;--muted:#9a9a94;--line:#33332f;--accent:#6c9cf0;--ok:#5fb36f;--bad:#e06c5c;--card:#1f1f1d}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}
main{max-width:640px;margin:0 auto;padding:40px 16px}
h1{font-size:20px;margin:0 0 4px}p.sub{margin:0 0 24px;color:var(--muted)}
#drop{border:2px dashed var(--line);border-radius:12px;padding:40px 16px;text-align:center;background:var(--card);cursor:pointer;transition:border-color .15s}
#drop.over{border-color:var(--accent)}#drop strong{color:var(--accent)}
ul{list-style:none;padding:0;margin:20px 0}li{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:10px 12px;margin-bottom:8px}
.row{display:flex;justify-content:space-between;gap:12px}.name{overflow-wrap:anywhere}.state{color:var(--muted);white-space:nowrap}
.bar{height:4px;background:var(--line);border-radius:2px;margin-top:8px;overflow:hidden}.bar i{display:block;height:100%;width:0;background:var(--accent)}
li.ok .state{color:var(--ok)}li.ok .bar i{background:var(--ok)}li.bad .state{color:var(--bad)}
button{font:inherit;padding:8px 16px;border-radius:8px;border:1px solid var(--line);background:var(--card);color:var(--fg);cursor:pointer}
#closed{display:none;color:var(--muted)}
</style></head><body><main>
<h1>Upload to <span>{{FOLDER}}</span></h1>
<p class="sub">Files land in this folder on the remote machine. Existing files are never overwritten.</p>
<div id="drop"><strong>Choose files</strong> or drop them here<input id="pick" type="file" multiple hidden></div>
<ul id="list"></ul>
<button id="done">Done — stop the upload server</button>
<p id="closed">Upload server stopped. You can close this tab.</p>
</main><script>
const base=location.pathname.replace(/\/?$/,'/');
const drop=document.getElementById('drop'),pick=document.getElementById('pick'),list=document.getElementById('list');
let queue=Promise.resolve();
drop.onclick=()=>pick.click();
pick.onchange=()=>{add(pick.files);pick.value=''};
drop.ondragover=e=>{e.preventDefault();drop.classList.add('over')};
drop.ondragleave=()=>drop.classList.remove('over');
drop.ondrop=e=>{e.preventDefault();drop.classList.remove('over');add(e.dataTransfer.files)};
function add(files){for(const f of files){const li=row(f.name);queue=queue.then(()=>send(f,li))}}
function row(name){const li=document.createElement('li');li.innerHTML='<div class="row"><span class="name"></span><span class="state">waiting</span></div><div class="bar"><i></i></div>';li.querySelector('.name').textContent=name;list.appendChild(li);return li}
function send(f,li){return new Promise(res=>{const x=new XMLHttpRequest(),st=li.querySelector('.state'),bar=li.querySelector('.bar i');
x.open('PUT',base+'f/'+encodeURIComponent(f.name));
x.upload.onprogress=e=>{if(e.lengthComputable){const p=Math.round(e.loaded/e.total*100);bar.style.width=p+'%';st.textContent=p+'%'}};
x.onload=()=>{if(x.status==200){li.className='ok';bar.style.width='100%';const n=JSON.parse(x.responseText).name;st.textContent=n==f.name?'saved':'saved as '+n}else{li.className='bad';st.textContent='failed: '+x.responseText}res()};
x.onerror=()=>{li.className='bad';st.textContent='failed (server gone?)';res()};
st.textContent='0%';x.send(f)})}
document.getElementById('done').onclick=async()=>{await queue;try{await fetch(base+'done',{method:'POST'})}catch(e){}
document.getElementById('done').style.display='none';drop.style.display='none';document.getElementById('closed').style.display='block'};
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-upload-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(port: u16, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(raw).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    fn put(port: u16, path: &str, body: &[u8]) -> String {
        let mut raw = format!(
            "PUT {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        request(port, &raw)
    }

    #[test]
    fn uploads_land_in_the_folder_without_clobbering_then_done_stops_it() {
        let dir = temp("save");
        std::fs::write(dir.join("a.txt"), "old").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_dir = dir.clone();
        let server = std::thread::spawn(move || {
            serve(listener, "tok", &server_dir, Duration::from_secs(30))
        });

        let page = request(port, b"GET /tok/ HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            page.starts_with("HTTP/1.1 200") && page.contains("Upload to"),
            "{page}"
        );
        assert!(request(port, b"GET /nope/ HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 404"));

        let big = vec![7u8; 300_000];
        assert!(put(port, "/tok/f/%E5%A0%B1%E5%91%8A.bin", &big).contains("\"name\":\"報告.bin\""));
        assert_eq!(std::fs::read(dir.join("報告.bin")).unwrap(), big);
        // An existing name is kept; the upload gets a free one.
        assert!(put(port, "/tok/f/a.txt", b"new").contains("a copy.txt"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old");
        // A path in the name is cut to its last component.
        assert!(put(port, "/tok/f/..%2F..%2Fescape.txt", b"x").contains("\"name\":\"escape.txt\""));
        assert!(dir.join("escape.txt").exists());
        assert!(!dir.parent().unwrap().join("escape.txt").exists());

        assert!(
            request(
                port,
                b"POST /tok/done HTTP/1.1\r\nContent-Length: 0\r\n\r\n"
            )
            .starts_with("HTTP/1.1 200")
        );
        server.join().unwrap().unwrap();
        // No half-written .part files are left behind.
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".part"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_idle_server_stops() {
        let dir = temp("idle");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let started = std::time::Instant::now();
        serve(listener, "t", &dir, Duration::from_millis(300)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_are_reduced_to_a_safe_last_component() {
        assert_eq!(safe_name("a/b/c.txt").as_deref(), Some("c.txt"));
        assert_eq!(safe_name("C:\\x\\y.doc").as_deref(), Some("y.doc"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("dir/"), None);
        assert_eq!(percent_decode("a%20b%E5%A0%B1"), "a b報");
    }
}
