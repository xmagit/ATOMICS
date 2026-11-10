use super::*;
use std::process::{Command, Stdio};

pub(crate) struct Server {
    pub root: crate::test_support::TempDir,
    pub work: PathBuf,
    pub bare: PathBuf,
    pub base: String,
    pub seen: Arc<Mutex<Vec<(String, String, Vec<u8>)>>>,
    pub default_branch: Arc<Mutex<String>>,
    stopped: Arc<AtomicBool>,
}

impl Server {
    pub fn start() -> Self {
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
                        let (record, branch, project) = (record.clone(), branch.clone(), project.clone());
                        std::thread::spawn(move || serve(stream, &project, record, branch));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { root, work, bare, base, seen, default_branch, stopped }
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

fn serve(
    stream: TcpStream,
    root: &std::path::Path,
    record: Arc<Mutex<Vec<(String, String, Vec<u8>)>>>,
    branch: Arc<Mutex<String>>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut reader = BufReader::new(stream);
    let Some(head) = read_head(&mut reader).unwrap() else {
        return;
    };
    let body = read_body(&mut reader, &head).unwrap();
    record.lock().unwrap().push((
        head.target.clone(),
        head.header("authorization").unwrap_or_default().into(),
        body.clone(),
    ));
    let (path, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
    let mut writer = reader.into_inner();
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
    for malformed in ["", "0000", "0001", "ffffinvalid", "000bpush-cert0000"] {
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
