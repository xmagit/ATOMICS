use super::*;
use std::process::{Command, Stdio};

type Requests = Arc<Mutex<Vec<(String, String, Vec<u8>)>>>;

pub(crate) struct Server {
    pub root: crate::test_support::TempDir,
    pub work: PathBuf,
    pub bare: PathBuf,
    pub base: String,
    pub seen: Requests,
    pub default_branch: Arc<Mutex<String>>,
    stopped: Arc<AtomicBool>,
}

impl Server {
    pub fn start() -> Self {
        Self::start_with_nonblocking_peer(false)
    }

    fn start_with_nonblocking_peer(nonblocking_peer: bool) -> Self {
        let root = crate::test_support::tempdir().unwrap();
        let work = root.path().join("work");
        let group = root.path().join("group");
        let bare = group.join("repo.git");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&group).unwrap();
        git(&work, &["init", "-qb", "main"]);
        std::fs::write(work.join("a"), "base\n").unwrap();
        git(&work, &["add", "."]);
        git(
            &work,
            &["-c", "user.name=Contributor", "-c", "user.email=contributor@example.invalid", "commit", "-qm", "base"],
        );
        git(&group, &["clone", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
        git(&bare, &["config", "http.receivepack", "true"]);
        git(&work, &["remote", "add", "origin", "https://github.com/group/repo.git"]);
        git(&work, &["checkout", "-qb", "task"]);
        std::fs::write(work.join("a"), "task\n").unwrap();
        git(&work, &["add", "."]);
        git(
            &work,
            &["-c", "user.name=Contributor", "-c", "user.email=contributor@example.invalid", "commit", "-qm", "task"],
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let default_branch = Arc::new(Mutex::new("main".into()));
        let (stop, record, branch, project) =
            (stopped.clone(), seen.clone(), default_branch.clone(), root.path().to_path_buf());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if nonblocking_peer {
                            stream.set_nonblocking(true).unwrap();
                        }
                        let (record, branch, project) = (record.clone(), branch.clone(), project.clone());
                        std::thread::spawn(move || serve(stream, &project, record, branch));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { root, work, bare, base, seen, default_branch, stopped }
    }

    pub fn commit_large_blob(&self) {
        use ring::rand::SecureRandom;
        let mut bytes = vec![0; 3 * 1024 * 1024];
        ring::rand::SystemRandom::new().fill(&mut bytes).unwrap();
        std::fs::write(self.work.join("large.bin"), bytes).unwrap();
        git(&self.work, &["add", "large.bin"]);
        git(
            &self.work,
            &[
                "-c",
                "user.name=Contributor",
                "-c",
                "user.email=contributor@example.invalid",
                "commit",
                "-qm",
                "large blob",
            ],
        );
    }

    pub fn assert_large_push(&self, relay: &Service) {
        let scope = Scope::from_project(&self.work, true).unwrap();
        let grant = relay.grant_github("large-push", "account-one", scope, false, None).unwrap().unwrap();
        let run = |destination: &str| {
            Command::new("git")
                .arg("-C")
                .arg(&self.work)
                .args(["-c", "http.postBuffer=1048576", "push", "origin", destination])
                .envs(&grant.secrets)
                .output()
                .unwrap()
        };
        let main = git(&self.bare, &["rev-parse", "main"]);
        assert!(!run("task:main").status.success());
        assert_eq!(git(&self.bare, &["rev-parse", "main"]), main);
        assert!(
            !self
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _, body)| path.ends_with("/git-receive-pack") && body.len() > 1024 * 1024),
            "refused updates never reach upstream"
        );
        let pushed = run("task");
        assert!(pushed.status.success(), "{}", String::from_utf8_lossy(&pushed.stderr));
        assert_eq!(git(&self.bare, &["rev-parse", "task"]), git(&self.work, &["rev-parse", "task"]));
        let seen = self.seen.lock().unwrap();
        assert!(
            seen.iter().any(|(path, _, body)| path.ends_with("/git-receive-pack") && body == b"0000"),
            "Git's empty probe reached upstream"
        );
        assert!(
            seen.iter().any(|(path, _, body)| path.ends_with("/git-receive-pack") && body.len() > 1024 * 1024),
            "the streamed pack exceeds postBuffer"
        );
        relay.revoke(&grant);
    }

    pub fn upstreams(&self) -> HashMap<String, String> {
        [("github.com".into(), self.base.clone()), ("api.github.com".into(), self.base.clone())].into()
    }

    pub fn relay(&self) -> Service {
        let mut relay = Service::with_upstreams(
            self.root.path().join("relay"),
            Keychain::in_folder(self.root.path().join("keychain").as_path()),
            self.upstreams(),
        );
        relay.broker = None;
        let store = crate::credentials::Store::local(relay.keychain().clone());
        store.connect_github_account("account-one", "ghp_CANARY_ACCOUNT_ONE").unwrap();
        store.connect_github_account("account-two", "ghp_CANARY_ACCOUNT_TWO").unwrap();
        relay
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// Keep request reads fragmented and occasionally delayed, independent of TCP buffer sizes.
struct SmallReads {
    stream: TcpStream,
    prefix: Vec<u8>,
    total: usize,
    next_pause: usize,
    error: Option<io::ErrorKind>,
}

impl SmallReads {
    fn new(stream: TcpStream) -> Self {
        // BSD accept can inherit O_NONBLOCK from the polling listener. These
        // fixture request readers use blocking I/O, regardless of that inheritance.
        stream.set_nonblocking(false).unwrap();
        Self { stream, prefix: Vec::new(), total: 0, next_pause: 64 * 1024, error: None }
    }

    fn rejected(&self, reason: impl std::fmt::Debug) -> ! {
        let end = self.prefix.windows(4).position(|bytes| bytes == b"\r\n\r\n").map(|at| at + 4);
        let (head, body) = self.prefix.split_at(end.unwrap_or(self.prefix.len()));
        let head = String::from_utf8_lossy(head)
            .lines()
            .map(|line| {
                if line.split_once(':').is_some_and(|(name, _)| name.eq_ignore_ascii_case("authorization")) {
                    "authorization: <redacted>"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "fake Git upstream rejected request: {reason:?}; bytes_read={}, io_error={:?}\n{head}\nfirst 200 body bytes: {:?}",
            self.total,
            self.error,
            String::from_utf8_lossy(&body[..body.len().min(200)])
        );
    }
}

impl Read for SmallReads {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.total >= self.next_pause {
            std::thread::sleep(Duration::from_millis(1));
            self.next_pause += 64 * 1024;
        }
        let count = out.len().min(113);
        match self.stream.read(&mut out[..count]) {
            Ok(read) => {
                self.total += read;
                let keep = read.min((MAX_HEAD + 200).saturating_sub(self.prefix.len()));
                self.prefix.extend_from_slice(&out[..keep]);
                Ok(read)
            }
            Err(error) => {
                self.error = Some(error.kind());
                Err(error)
            }
        }
    }
}

fn upstream_request(stream: TcpStream, after_head: impl FnOnce(&TcpStream)) -> Option<(Head, Vec<u8>, TcpStream)> {
    stream.set_read_timeout(Some(crate::test_support::scaled_timeout(Duration::from_secs(10)))).unwrap();
    let mut reader = BufReader::with_capacity(257, SmallReads::new(stream));
    let head = read_head(&mut reader).unwrap_or_else(|error| reader.get_ref().rejected(error))?;
    after_head(&reader.get_ref().stream);
    let body = read_body(&mut reader, &head).unwrap_or_else(|error| reader.get_ref().rejected(error));
    Some((head, body, reader.into_inner().stream))
}

fn serve(stream: TcpStream, root: &std::path::Path, record: Requests, branch: Arc<Mutex<String>>) {
    let Some((head, body, mut writer)) = upstream_request(stream, |_| {}) else { return };
    record.lock().unwrap().push((
        head.target.clone(),
        head.header("authorization").unwrap_or_default().into(),
        body.clone(),
    ));
    let (path, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
    if path.starts_with("/repos/") {
        let payload = if path == "/repos/group/repo" {
            serde_json::json!({"default_branch": *branch.lock().unwrap()})
        } else if head.method == "GET" {
            serde_json::json!([])
        } else {
            serde_json::json!({"html_url":"https://github.com/group/repo/pull/1","number":1})
        };
        let bytes = serde_json::to_vec(&payload).unwrap();
        write!(
            writer,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .unwrap();
        writer.write_all(&bytes).unwrap();
        return;
    }
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", &head.method)
        .env("CONTENT_TYPE", head.header("content-type").unwrap_or_default())
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_USER", "connected")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let output = child.wait_with_output().unwrap();
    let boundary = output.stdout.windows(4).position(|s| s == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8_lossy(&output.stdout[..boundary]);
    let status = headers.lines().find_map(|s| s.strip_prefix("Status: ")).unwrap_or("200 OK");
    write!(
        writer,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
        output.stdout.len() - boundary - 4
    )
    .unwrap();
    for line in headers.lines().filter(|l| !l.starts_with("Status:")) {
        write!(writer, "{line}\r\n").unwrap();
    }
    writer.write_all(b"\r\n").unwrap();
    writer.write_all(&output.stdout[boundary + 4..]).unwrap();
}

#[test]
fn github_plain_git_push_fetch_scope_default_branch_and_canary() {
    let server = Server::start();
    let relay = server.relay();
    let scope = Scope::from_project(&server.work, false).unwrap();
    let grant = relay.grant_github("task-session", "account-two", scope, false, None).unwrap().unwrap();
    let run = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&server.work).args(args).envs(&grant.secrets).output().unwrap()
    };
    let pushed = run(&["push"]);
    assert!(pushed.status.success(), "{}", String::from_utf8_lossy(&pushed.stderr));
    assert_eq!(git(&server.bare, &["rev-parse", "task"]), git(&server.work, &["rev-parse", "task"]));
    assert!(run(&["fetch", "origin"]).status.success());
    assert!(!run(&["push", "origin", "task:main"]).status.success());
    assert!(!run(&["push", "https://github.com/other/repo.git", "task"]).status.success());
    *server.default_branch.lock().unwrap() = "task".into();
    std::fs::write(server.work.join("a"), "next\n").unwrap();
    git(&server.work, &["add", "."]);
    git(
        &server.work,
        &["-c", "user.name=Contributor", "-c", "user.email=contributor@example.invalid", "commit", "-qm", "next"],
    );
    assert!(!run(&["push", "origin", "task"]).status.success());
    use base64::Engine;
    let auth =
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("x-access-token:ghp_CANARY_ACCOUNT_TWO"));
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(_, header, _)| header == &auth || header == "Bearer ghp_CANARY_ACCOUNT_TWO")
    );
    let mut readable = format!("{:?}{:?}{:?}", grant.secrets, pushed, grant);
    readable += &std::fs::read_to_string(server.root.path().join("relay/relay.log")).unwrap();
    readable += &std::fs::read_to_string(server.work.join(".git/config")).unwrap();
    assert!(!readable.contains("CANARY"));
    relay.revoke(&grant);
    assert!(!run(&["fetch", "origin"]).status.success());
}

#[test]
fn github_paths_and_packet_parser_fail_closed() {
    let scope = Scope { repository: "group/repo".into(), pulls: false, refuse_default_branch: true };
    for (method, target, allowed) in [
        ("GET", "/github.com/group/repo.git/info/refs?service=git-upload-pack", true),
        ("POST", "/github.com/group/repo.git/git-upload-pack", true),
        ("POST", "/github.com/group/repo.git/git-receive-pack", true),
        ("GET", "/github.com/group/repo.git/info/refs?service=other", false),
        ("GET", "/github.com/group/other.git/info/refs?service=git-upload-pack", false),
        ("GET", "/api.github.com/user", false),
        ("POST", "/api.github.com/repos/group/repo/pulls", false),
        ("GET", "/github.com/group/repo.git/config", false),
        ("POST", "/github.com/group/repo.git/git-receive-pack?x=1", false),
    ] {
        assert_eq!(scope.allows(method, &Route::parse(target).unwrap()), allowed, "{target}");
    }
    let scope = Scope { pulls: true, ..scope };
    assert!(scope.allows("POST", &Route::parse("/api.github.com/repos/group/repo/pulls").unwrap()));
    assert!(scope.allows("PATCH", &Route::parse("/api.github.com/repos/group/repo/pulls/1").unwrap()));
    assert!(!scope.allows("PUT", &Route::parse("/api.github.com/repos/group/repo/pulls/1/merge").unwrap()));
    assert!(!scope.allows("POST", &Route::parse("/api.github.com/repos/other/repo/pulls").unwrap()));
    for (reference, allowed) in [
        ("refs/heads/task", true),
        ("refs/heads/main", false),
        ("refs/tags/release", false),
        ("refs/heads/main/other", true),
    ] {
        let line = format!("{} {} {reference}\0report-status\n", "0".repeat(40), "1".repeat(40));
        let packet = format!("{:04x}{line}0000PACK", line.len() + 4);
        assert_eq!(receive_pack_allowed(packet.as_bytes(), "main"), allowed);
    }
    for malformed in ["", "0001", "ffffinvalid", "000bpush-cert0000"] {
        assert!(!receive_pack_allowed(malformed.as_bytes(), "main"));
    }
}

#[test]
fn github_account_scope_and_generic_secret_grants_do_not_leak_or_broaden_it() {
    let fake = Server::start();
    let service = fake.relay();
    let store = crate::credentials::Store::local(service.keychain().clone());
    let spec = account_spec("account-one").unwrap();
    store.remove_secret(&spec.name).unwrap();
    store
        .add_secret(
            atomics_core::secrets::Secret { scope: atomics_core::secrets::Scope::Workspace("daily".into()), ..spec },
            "ghp_CANARY_ACCOUNT_ONE",
        )
        .unwrap();
    assert!(service.grant_project_github("other", "account-one", &fake.work, Some("other"), false, None).is_err());
    let generic =
        service.grant_secrets("generic", Some("daily"), Some(fake.work.to_str().unwrap()), false, None).unwrap();
    assert!(generic.is_none(), "GitHub accounts require a repository grant, never a broad environment credential");
    let grant =
        service.grant_project_github("daily", "account-one", &fake.work, Some("daily"), false, None).unwrap().unwrap();
    assert!(!format!("{:?}", grant.secrets).contains("CANARY"));
    service.revoke(&grant);
}

#[test]
fn github_large_push_streams_after_the_empty_probe_through_the_relay() {
    let server = Server::start_with_nonblocking_peer(true);
    server.commit_large_blob();
    server.assert_large_push(&server.relay());
}

#[test]
fn fake_upstream_waits_for_a_body_on_an_inherited_nonblocking_socket() {
    use std::os::fd::AsRawFd;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    {
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        client.write_all(b"POST /repos/group/repo HTTP/1.1\r\nContent-Length: 5\r\n\r\n").unwrap();
        let mut reader = BufReader::new(SmallReads::new(peer));
        // Reintroduce the old BSD-inherited flag with the body still in flight:
        // the blocking parser then reports the exact failure seen on the Mac.
        reader.get_mut().stream.set_nonblocking(true).unwrap();
        let head = read_head(&mut reader).unwrap().unwrap();
        assert_eq!(read_body(&mut reader, &head).unwrap_err(), (400, "malformed request body"));
        assert_eq!(reader.get_ref().error, Some(io::ErrorKind::WouldBlock));
    }
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    peer.set_nonblocking(true).unwrap();
    client.write_all(b"POST /repos/group/repo HTTP/1.1\r\nContent-Length: 5\r\n\r\n").unwrap();
    let (parsed, ready) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        upstream_request(peer, |socket| {
            // SAFETY: F_GETFL only reads flags from this live fixture descriptor.
            let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
            assert!(
                flags >= 0 && flags & libc::O_NONBLOCK == 0,
                "fake upstream still has nonblocking flags: {flags:#x}"
            );
            parsed.send(()).unwrap();
        })
        .unwrap()
        .1
    });
    ready.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10))).unwrap();
    for bytes in [b"h".as_slice(), b"el", b"lo"] {
        client.write_all(bytes).unwrap();
        std::thread::yield_now();
    }
    assert_eq!(worker.join().unwrap(), b"hello");
}

#[test]
fn apple_curl_push_write_pattern_has_exact_length_framing_upstream() {
    use base64::Engine as _;
    use std::collections::BTreeMap;

    let sizes: Vec<usize> = std::iter::once(65_159)
        .chain(std::iter::repeat_n(65_521, 32))
        .chain(std::iter::repeat_n(65_524, 14))
        .chain(std::iter::repeat_n(8192, 8))
        .chain([730, 1946])
        .collect();
    let mut body: Vec<u8> = (0..sizes.iter().sum::<usize>()).map(|at| (at % 251) as u8).collect();
    let command = format!(
        "{} {} refs/heads/task\0report-status-v2 side-band-64k quiet object-format=sha1 agent=git/2.50.1-Darwin\n",
        "0".repeat(40),
        "1".repeat(40)
    );
    let prefix = format!("{:04x}{command}0000PACK", command.len() + 4);
    body[..prefix.len()].copy_from_slice(prefix.as_bytes());
    let mut pieces = Vec::new();
    let mut at = 0;
    for (index, size) in sizes.into_iter().enumerate() {
        let mut piece = if index == 0 {
            format!("{size:x}\r\n").into_bytes()
        } else if index <= 32 {
            // Exercise size lines sent separately from their data and trailing CRLF.
            pieces.push(format!("{size:08x}\r\n").into_bytes());
            Vec::new()
        } else {
            format!("{size:04x}\r\n").into_bytes()
        };
        piece.extend_from_slice(&body[at..at + size]);
        piece.extend_from_slice(b"\r\n");
        pieces.push(piece);
        at += size;
    }
    pieces.push(b"0\r\n\r\n".to_vec());
    assert_eq!(pieces.len(), 90);
    let mut counts = BTreeMap::new();
    for piece in &pieces {
        *counts.entry(piece.len()).or_insert(0) += 1;
    }
    assert_eq!(
        counts,
        BTreeMap::from([(65_167, 1), (65_523, 32), (65_532, 14), (8200, 8), (10, 32), (738, 1), (1954, 1), (5, 1)])
    );
    assert!(pieces[0].starts_with(b"fe87\r\n"));

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = std::thread::spawn(move || {
        let mut observed = Vec::new();
        for _ in 0..2 {
            let (peer, _) = listener.accept().unwrap();
            peer.set_nonblocking(true).unwrap(); // Model the BSD listener inheritance on Linux too.
            peer.set_read_timeout(Some(crate::test_support::scaled_timeout(Duration::from_secs(30)))).unwrap();
            let mut reader = BufReader::with_capacity(31, SmallReads::new(peer));
            // Independent wire capture: neither read_head nor read_body is used here.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                reader.read_exact(&mut byte).unwrap_or_else(|error| reader.get_ref().rejected(error));
                head.push(byte[0]);
                assert!(head.len() <= MAX_HEAD);
            }
            let text = std::str::from_utf8(&head).unwrap();
            let lengths: Vec<usize> = text
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
                .collect();
            assert_eq!(lengths.len(), 1, "unexpected upstream framing: {text}");
            assert!(lengths[0] <= MAX_BODY as usize);
            let mut bytes = vec![0; lengths[0]];
            reader.read_exact(&mut bytes).unwrap_or_else(|error| reader.get_ref().rejected(error));
            reader
                .into_inner()
                .stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            observed.push((head, bytes));
        }
        observed
    });
    let dir = crate::test_support::tempdir().unwrap();
    let relay = Relay::start(
        0,
        dir.path().join("relay.log"),
        HashMap::from([("github.com".into(), format!("http://{address}"))]),
    )
    .unwrap();
    let token = "ghp_FORWARDING_FIXTURE";
    let scope = Scope { repository: "group/repo".into(), pulls: false, refuse_default_branch: false };
    let placeholder =
        relay.grant("apple-push", Arc::new(Credential::github("account-one", token.into(), scope).unwrap()));
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, relay.port())).unwrap();
    stream.set_nodelay(true).unwrap();
    stream.set_read_timeout(Some(crate::test_support::scaled_timeout(Duration::from_secs(30)))).unwrap();
    let mut client = BufReader::new(stream);
    let headers = format!(
        "POST /github.com/group/repo.git/git-receive-pack HTTP/1.1\r\nAuthorization: Bearer {placeholder}\r\nAccept-Encoding: deflate, gzip\r\nContent-Type: application/x-git-receive-pack-request\r\n"
    );
    client.get_mut().write_all(format!("{headers}Content-Length: 4\r\n\r\n0000").as_bytes()).unwrap();
    fn response(client: &mut BufReader<TcpStream>) {
        let mut status = String::new();
        client.read_line(&mut status).unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        loop {
            let mut line = String::new();
            assert_ne!(client.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
        assert!(super::super::fake::chunk(client).is_empty());
    }
    response(&mut client);
    client.get_mut().write_all(format!("{headers}Transfer-Encoding: chunked\r\n\r\n").as_bytes()).unwrap();
    for piece in pieces {
        client.get_mut().write_all(&piece).unwrap();
        std::thread::yield_now();
    }
    response(&mut client);
    for ((head, bytes), expected) in upstream.join().unwrap().into_iter().zip([b"0000".as_slice(), body.as_slice()]) {
        let text = std::str::from_utf8(&head).unwrap();
        assert_eq!(text.lines().next(), Some("POST /group/repo.git/git-receive-pack HTTP/1.1"));
        let headers: Vec<_> = text
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        assert_eq!(headers.len(), 5, "all upstream headers: {text}");
        assert_eq!(
            headers.into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from([
                ("host".into(), address.to_string()),
                ("content-length".into(), expected.len().to_string()),
                ("content-type".into(), "application/x-git-receive-pack-request".into()),
                ("accept-encoding".into(), "identity".into()),
                (
                    "authorization".into(),
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
                    )
                ),
            ])
        );
        assert_eq!(bytes, expected, "the complete decoded body must reach upstream unchanged");
    }
}

#[test]
fn github_only_the_exact_empty_probe_is_allowed_without_commands() {
    for default in ["main", ""] {
        assert!(receive_pack_allowed(b"0000", default));
        for body in [b"".as_slice(), b"00000000", b"0000PACK", b"0000\n", b"0000ref-change"] {
            assert!(!receive_pack_allowed(body, default), "{body:?}");
        }
    }
}

#[test]
fn github_shallow_push_commands_remain_unsupported() {
    let shallow = format!("shallow {}\n", "1".repeat(40));
    let update = format!("{} {} refs/heads/task\0report-status\n", "0".repeat(40), "2".repeat(40));
    let packet = format!("{:04x}{shallow}{:04x}{update}0000PACK", shallow.len() + 4, update.len() + 4);
    assert!(!receive_pack_allowed(packet.as_bytes(), "main"));
}
