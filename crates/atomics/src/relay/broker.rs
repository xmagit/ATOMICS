//! VM secret boundary. Only this process opens the private catalog. The use socket can relay
//! requests; peer credentials restrict grant management and exposure to the host UID. Storage
//! mutations use a separate socket, protected by both filesystem modes and peer credentials.
use super::*;
use atomics_core::secrets::Secret;
use serde::{Deserialize, Serialize};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::Path;

const MAX_CONTROL: u64 = 256 * 1024;

/// Runs only in the administrator's SSH process (or sudo in a single trust domain). Neither
/// the host nor a control socket is involved. Only mutation requests can enter this bridge.
pub fn admin_stdin() -> Result<()> {
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    std::io::stdin().lock().take(MAX_CONTROL + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= MAX_CONTROL as usize, "request too large");
    let request: Request = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid admin input"))?;
    anyhow::ensure!(
        matches!(request, Request::Set { .. } | Request::Remove { .. } | Request::SetGithubPolicy { .. }),
        "not an admin mutation"
    );
    let client = Client { socket: "/run/atomics-secret-broker/admin.sock".into() };
    // Lima can accept SSH just before systemd has brought the new broker up after a boot.
    // Both allowed mutations are idempotent, including an acknowledgement lost during restart.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client.call_ref(&request) {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct Options {
    #[arg(long, default_value = "/var/lib/atomics-secret-broker")]
    state_dir: PathBuf,
    #[arg(long, default_value = "/run/atomics-secret-broker/use.sock")]
    use_socket: PathBuf,
    #[arg(long, default_value = "/run/atomics-secret-broker/admin.sock")]
    admin_socket: PathBuf,
    #[arg(long)]
    host_uid: u32,
    #[arg(long)]
    admin_uid: u32,
    /// Dedicated group permitted to connect to the use socket. Defaults to this process's group.
    #[arg(long)]
    use_group: Option<String>,
}

// No Debug: Set carries a secret. No generic get/read/copy operation exists.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List,
    SetGithubPolicy { name: String, refuse_default_branch: bool },
    Set { spec: Secret, value: String },
    Remove { name: String },
    Grant { name: String, session: String, placeholder: String },
    GithubGrant { account: String, session: String, placeholder: String, scope: github::Scope },
    Revoke { placeholder: String },
    Expose { name: String, session: String },
    Relay,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    secrets: Vec<Secret>,
    // Only Expose, on the use socket, can populate this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exposed: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn configured() -> Option<Self> {
        std::env::var_os("ATOMICS_SECRET_BROKER_SOCKET").map(|socket| Self { socket: socket.into() })
    }

    fn connect(&self, request: &Request) -> Result<UnixStream> {
        let mut stream = UnixStream::connect(&self.socket).context("connecting to the secret broker")?;
        stream.set_read_timeout(Some(if matches!(request, Request::Relay) {
            Duration::from_secs(15 * 60)
        } else {
            Duration::from_secs(5)
        }))?;
        stream.set_write_timeout(Some(IDLE))?;
        let bytes = zeroize::Zeroizing::new(serde_json::to_vec(request)?);
        stream.write_all(&bytes)?;
        stream.write_all(b"\n")?;
        Ok(stream)
    }

    fn call(&self, request: Request) -> Result<Reply> {
        self.call_ref(&request)
    }

    fn call_ref(&self, request: &Request) -> Result<Reply> {
        let stream = self.connect(request)?;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        BufReader::new(stream).take(MAX_CONTROL).read_until(b'\n', &mut bytes)?;
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid broker reply"))?;
        anyhow::ensure!(reply.ok, "Secret broker refused the operation (details in its private journal)");
        Ok(reply)
    }

    pub fn list(&self) -> Result<Vec<Secret>> {
        Ok(self.call(Request::List)?.secrets)
    }

    pub fn set(&self, spec: Secret, value: &str) -> Result<()> {
        self.admin().call(Request::Set { spec, value: value.into() })?;
        Ok(())
    }

    pub fn github_policy(&self, name: &str, refuse_default_branch: bool) -> Result<()> {
        self.admin().call(Request::SetGithubPolicy { name: name.into(), refuse_default_branch })?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        self.admin().call(Request::Remove { name: name.into() })?;
        Ok(())
    }

    fn admin(&self) -> Self {
        Self {
            socket: std::env::var_os("ATOMICS_SECRET_BROKER_ADMIN_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| self.socket.with_file_name("admin.sock")),
        }
    }

    pub(super) fn grant(&self, name: &str, session: &str, placeholder: &str) -> Result<()> {
        self.call(Request::Grant { name: name.into(), session: session.into(), placeholder: placeholder.into() })?;
        Ok(())
    }

    pub(super) fn grant_github(
        &self,
        account: &str,
        session: &str,
        placeholder: &str,
        scope: github::Scope,
    ) -> Result<()> {
        self.call(Request::GithubGrant {
            account: account.into(),
            session: session.into(),
            placeholder: placeholder.into(),
            scope,
        })?;
        Ok(())
    }

    pub(super) fn revoke(&self, placeholder: &str) -> Result<()> {
        self.call(Request::Revoke { placeholder: placeholder.into() })?;
        Ok(())
    }

    pub(super) fn expose(&self, name: &str, session: &str) -> Result<String> {
        self.call(Request::Expose { name: name.into(), session: session.into() })?
            .exposed
            .ok_or_else(|| anyhow::anyhow!("broker did not expose a value"))
    }

    pub(super) fn forward(&self, head: &Head, body: Vec<u8>, writer: &mut impl Write) -> io::Result<(u16, bool)> {
        let mut socket = self.connect(&Request::Relay).map_err(io::Error::other)?;
        write!(socket, "{} {} HTTP/1.1\r\n", head.method, head.target)?;
        for (name, value) in &head.headers {
            if !["connection", "content-length", "transfer-encoding", "expect"]
                .iter()
                .any(|h| name.eq_ignore_ascii_case(h))
            {
                write!(socket, "{name}: {value}\r\n")?;
            }
        }
        write!(socket, "Connection: close\r\nContent-Length: {}\r\n\r\n", body.len())?;
        socket.write_all(&body)?;
        let mut socket = BufReader::new(socket);
        let mut status = String::new();
        socket.by_ref().take(1024).read_line(&mut status)?;
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| io::Error::other("invalid broker response"))?;
        writer.write_all(status.as_bytes())?;
        io::copy(&mut socket, writer)?;
        writer.flush()?;
        // The broker deliberately closes after one streamed HTTP response.
        Ok((code, false))
    }
}

struct Broker {
    store: crate::credentials::Store,
    engine: Arc<Inner>,
    state: PathBuf,
    host_uid: u32,
    admin_uid: u32,
    edits: Mutex<()>,
    hosts_dirty: AtomicBool,
}

impl Broker {
    fn hosts(&self) -> Result<()> {
        // A failed projection must be retried even if the catalog mutation was already saved.
        self.hosts_dirty.store(true, Ordering::Relaxed);
        self.write_hosts()?;
        self.hosts_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn write_hosts(&self) -> Result<()> {
        let secrets = self.store.secrets()?;
        let mut hosts: Vec<_> = secrets.iter().filter_map(|spec| spec.host.clone()).collect();
        if secrets.iter().any(|s| s.name.starts_with(github::ACCOUNT_PREFIX)) {
            hosts.push("api.github.com".into());
        }
        hosts.sort();
        hosts.dedup();
        // Every replacement wakes root's DNS reconciliation. A restart, a rotated value, or
        // another rule for an existing host must leave this inode and its content unchanged.
        let file = self.state.join("rule-hosts.json");
        let content = serde_json::to_string(&hosts)?;
        match std::fs::read(&file) {
            Ok(bytes) if bytes == content.as_bytes() => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // A root-owned path unit consumes only this validated, value-free projection.
        write_private(&self.state, &file, &content)?;
        Ok(())
    }

    fn control(&self, request: Request, admin: bool, uid: u32) -> Result<Reply> {
        let _edit = self.edits.lock().unwrap();
        let mut reply = Reply { ok: true, ..Reply::default() };
        if admin {
            anyhow::ensure!(uid == 0 || uid == self.admin_uid, "admin peer refused");
            match request {
                Request::List => reply.secrets = self.store.secrets()?,
                Request::SetGithubPolicy { name, refuse_default_branch } => {
                    self.store.set_github_policy(&name, refuse_default_branch)?;
                    self.engine
                        .grants
                        .lock()
                        .unwrap()
                        .retain(|_, grant| grant.credential.label != format!("secret:{name}"));
                }
                Request::Set { spec, mut value } => {
                    use zeroize::Zeroize;
                    let label = format!("secret:{}", spec.name);
                    let result = self.store.sync_secret_rule(spec, &value);
                    value.zeroize();
                    let changed = result?;
                    if changed {
                        self.engine.grants.lock().unwrap().retain(|_, grant| grant.credential.label != label);
                    }
                    if changed || self.hosts_dirty.load(Ordering::Relaxed) {
                        self.hosts()?;
                    }
                    eprintln!("secret-broker: secret set");
                }
                Request::Remove { name } => {
                    let changed = self.store.secrets()?.iter().any(|spec| spec.name == name);
                    if changed {
                        self.store.remove_secret(&name)?;
                    }
                    self.engine
                        .grants
                        .lock()
                        .unwrap()
                        .retain(|_, grant| grant.credential.label != format!("secret:{name}"));
                    if changed || self.hosts_dirty.load(Ordering::Relaxed) {
                        self.hosts()?;
                    }
                    eprintln!("secret-broker: secret removed");
                }
                _ => anyhow::bail!("operation unavailable on admin socket"),
            }
        } else {
            if matches!(request, Request::List) {
                anyhow::ensure!(uid == 0 || uid == self.host_uid || uid == self.admin_uid, "metadata peer refused");
                reply.secrets = self.store.secrets()?;
                return Ok(reply);
            }
            anyhow::ensure!(uid == 0 || uid == self.host_uid, "host peer refused");
            match request {
                Request::List => reply.secrets = self.store.secrets()?,
                Request::Grant { name, session, placeholder } => {
                    anyhow::ensure!(valid_session(&session) && valid_placeholder(&placeholder), "invalid grant");
                    let (spec, value) = self
                        .store
                        .secret_values()?
                        .into_iter()
                        .find(|(spec, _)| {
                            spec.name == name && spec.host.is_some() && !name.starts_with(github::ACCOUNT_PREFIX)
                        })
                        .ok_or_else(|| anyhow::anyhow!("unknown relayed secret"))?;
                    // This immutable snapshot stays exclusively in the broker. An in-flight
                    // request cannot acquire a replacement secret under an older rule.
                    let credential = Arc::new(Credential::secret(&spec, value.to_string()));
                    let mut grants = self.engine.grants.lock().unwrap();
                    anyhow::ensure!(
                        grants.len() < 4096 && !grants.contains_key(&placeholder),
                        "grant limit or collision"
                    );
                    grants.insert(placeholder, Grant { session, credential, replies: Arc::default() });
                }
                Request::GithubGrant { account, session, placeholder, mut scope } => {
                    anyhow::ensure!(valid_session(&session) && valid_placeholder(&placeholder), "invalid grant");
                    let spec = github::account_spec(&account)?;
                    let (stored_spec, value) = self
                        .store
                        .secret_values()?
                        .into_iter()
                        .find(|(s, _)| {
                            s.name == spec.name
                                && s.host == spec.host
                                && s.auth == spec.auth
                                && s.path_prefix == spec.path_prefix
                        })
                        .ok_or_else(|| anyhow::anyhow!("unknown GitHub account"))?;
                    scope.refuse_default_branch = stored_spec.refuse_default_branch;
                    let credential = Arc::new(Credential::github(&account, value.to_string(), scope)?);
                    let mut grants = self.engine.grants.lock().unwrap();
                    anyhow::ensure!(
                        grants.len() < 4096 && !grants.contains_key(&placeholder),
                        "grant limit or collision"
                    );
                    grants.insert(placeholder, Grant { session, credential, replies: Arc::default() });
                }
                Request::Revoke { placeholder } => {
                    self.engine.grants.lock().unwrap().remove(&placeholder);
                }
                Request::Expose { name, session } => {
                    anyhow::ensure!(valid_session(&session), "invalid session");
                    let (_, value) = self
                        .store
                        .secret_values()?
                        .into_iter()
                        .find(|(spec, _)| spec.name == name && spec.host.is_none())
                        .ok_or_else(|| anyhow::anyhow!("only exposed secrets may leave the broker"))?;
                    eprintln!("secret-broker: EXPOSED name={name} session={session}");
                    reply.exposed = Some(value.to_string());
                }
                _ => anyhow::bail!("operation unavailable on use socket"),
            }
        }
        Ok(reply)
    }

    fn serve(&self, socket: UnixStream, admin: bool, slot: &mut Slot) -> Result<()> {
        socket.set_write_timeout(Some(IDLE))?;
        let uid = peer_uid(&socket)?;
        anyhow::ensure!(uid == 0 || uid == self.admin_uid || (!admin && uid == self.host_uid), "broker peer refused");
        let mut reader =
            BufReader::new(BrokerRead { stream: socket.try_clone()?, deadline: Some(Instant::now() + AUTH_DEADLINE) });
        let mut writer = socket;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        reader.by_ref().take(MAX_CONTROL).read_until(b'\n', &mut bytes)?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid broker request"))?;
        if matches!(request, Request::Relay) && !admin {
            // A peer admitted to this socket may present a placeholder; the engine accepts
            // only a host-issued mapping.
            let head = match read_head(&mut reader) {
                Ok(Some(head)) => head,
                Ok(None) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    refuse(&mut writer, 400, "malformed request")?;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            // A complete Relay marker is not authentication: require a valid placeholder
            // before allowing an idle/streaming request to occupy an authenticated slot.
            if self.engine.matching_grant(&head).is_some() {
                reader.get_mut().authenticate(slot)?;
            }
            self.engine.exchange(head, &mut reader, &mut writer)?;
        } else {
            reader.get_mut().authenticate(slot)?;
            let reply = self.control(request, admin, uid).unwrap_or_else(|_| {
                eprintln!("secret-broker: control operation refused uid={uid} admin={admin}");
                Reply::default()
            });
            let encoded = zeroize::Zeroizing::new(serde_json::to_vec(&reply)?);
            writer.write_all(&encoded)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// The JSON preamble and, for Relay, the HTTP headers share one absolute deadline. A peer
/// trickling bytes cannot renew it. Authenticated streams retain the regular relay timeout.
struct BrokerRead {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl BrokerRead {
    fn authenticate(&mut self, slot: &mut Slot) -> Result<()> {
        anyhow::ensure!(slot.authenticate(), "broker authenticated connection limit");
        self.deadline = None;
        self.stream.set_read_timeout(Some(IDLE))?;
        Ok(())
    }
}

impl Read for BrokerRead {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(bytes)
    }
}

fn valid_session(session: &str) -> bool {
    !session.is_empty() && session.len() <= 256 && session.bytes().all(|b| b.is_ascii_graphic())
}
fn valid_placeholder(value: &str) -> bool {
    value.strip_prefix("atomics-").is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt initializes the provided, correctly sized ucred buffer on success.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("invalid peer credentials"));
    }
    Ok(unsafe { credentials.assume_init() }.uid)
}
#[cfg(not(target_os = "linux"))]
fn peer_uid(_: &UnixStream) -> io::Result<u32> {
    Err(io::Error::other("the VM broker requires Linux"))
}

fn listen(path: &Path, mode: u32, group: Option<u32>) -> Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let socket = UnixListener::bind(path)?;
    if let Some(group) = group {
        std::os::unix::fs::chown(path, None, Some(group))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(socket)
}

fn group_id(name: &str) -> Result<u32> {
    let name = std::ffi::CString::new(name)?;
    let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut found = std::ptr::null_mut();
    // The dedicated group has only the VM and admin accounts. Bound NSS allocation and
    // fail closed if its entry is missing or exceeds the buffer; never use a shared buffer.
    let mut buffer = vec![0u8; 64 * 1024];
    // SAFETY: the name is NUL-terminated, all writable buffers have the declared sizes,
    // and entry is read only after getgrnam_r reports a successful lookup.
    let result = unsafe {
        libc::getgrnam_r(name.as_ptr(), entry.as_mut_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), &mut found)
    };
    anyhow::ensure!(result == 0 && !found.is_null(), "broker use group is unavailable");
    Ok(unsafe { entry.assume_init() }.gr_gid)
}

pub fn run(options: Options) -> Result<()> {
    // systemd creates the private state directory and broker-owned socket parent. Apply
    // the private mode before opening the catalog, including on an existing state directory.
    std::fs::create_dir_all(&options.state_dir)?;
    std::fs::set_permissions(&options.state_dir, std::fs::Permissions::from_mode(0o700))?;
    let store = crate::credentials::Store::local(Keychain::in_folder(&options.state_dir));
    let broker = Arc::new(Broker {
        store,
        engine: Inner::new(options.state_dir.join("relay.log"), HashMap::new(), true),
        state: options.state_dir,
        host_uid: options.host_uid,
        admin_uid: options.admin_uid,
        edits: Mutex::new(()),
        hosts_dirty: AtomicBool::new(false),
    });
    broker.hosts()?;
    let use_group = options.use_group.as_deref().map(group_id).transpose()?;
    let use_socket = listen(&options.use_socket, 0o660, use_group)?;
    let admin_socket = listen(&options.admin_socket, 0o660, None)?;
    let accepting = broker.clone();
    std::thread::Builder::new()
        .name("secret-broker-admin".into())
        .spawn(move || accept(accepting, admin_socket, true))?;
    accept(broker, use_socket, false);
    Ok(())
}

fn accept(broker: Arc<Broker>, listener: UnixListener, admin: bool) {
    for socket in listener.incoming().flatten() {
        start_request(broker.clone(), socket, admin);
    }
}

fn start_request(broker: Arc<Broker>, socket: UnixStream, admin: bool) {
    if !take_slot(&broker.engine.pending, MAX_PRE_AUTH) {
        return;
    }
    let mut slot = Slot { inner: broker.engine.clone(), authenticated: false };
    let _ = std::thread::Builder::new().name("secret-broker-request".into()).spawn(move || {
        if broker.serve(socket, admin, &mut slot).is_err() {
            eprintln!("secret-broker: malformed or failed request (contents hidden)");
        }
    });
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use atomics_core::secrets::{Auth, Scope};

    fn machine() -> (crate::test_support::TempDir, Arc<Broker>, Client) {
        machine_with(HashMap::new())
    }

    fn machine_with(upstreams: HashMap<String, String>) -> (crate::test_support::TempDir, Arc<Broker>, Client) {
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("use.sock");
        let broker = Arc::new(Broker {
            store: crate::credentials::Store::local(Keychain::in_folder(dir.path())),
            engine: Inner::new(dir.path().join("relay.log"), upstreams, true),
            state: dir.path().into(),
            host_uid: unsafe { libc::getuid() },
            admin_uid: unsafe { libc::getuid() },
            edits: Mutex::new(()),
            hosts_dirty: AtomicBool::new(false),
        });
        let listener = listen(&socket, 0o660, Some(unsafe { libc::getegid() })).unwrap();
        let serving = broker.clone();
        // Test listeners exit when their temporary path is removed; no global environment changes.
        listener.set_nonblocking(true).unwrap();
        let path = socket.clone();
        std::thread::spawn(move || {
            while path.exists() {
                if let Ok((socket, _)) = listener.accept() {
                    start_request(serving.clone(), socket, false);
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        (dir, broker, Client { socket })
    }

    fn set(broker: &Broker, spec: Secret, value: &str) {
        assert!(broker.control(Request::Set { spec, value: value.into() }, true, broker.admin_uid).unwrap().ok);
    }

    #[test]
    fn host_projection_changes_only_with_the_effective_host_list() {
        use std::os::unix::fs::MetadataExt;
        let (dir, broker, _) = machine();
        let path = dir.path().join("rule-hosts.json");
        broker.hosts().unwrap();
        let empty = std::fs::File::open(&path).unwrap();
        broker.hosts().unwrap(); // The same projection at broker restart.
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), empty.metadata().unwrap().ino());
        set(&broker, Secret { name: "EXPOSED".into(), ..Default::default() }, "private-exposed");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), empty.metadata().unwrap().ino());

        let spec = Secret { name: "FIRST".into(), host: Some("api.example.com".into()), ..Default::default() };
        set(&broker, spec.clone(), "private-first");
        let projected = std::fs::File::open(&path).unwrap();
        for _ in 0..8 {
            set(&broker, spec.clone(), "private-first");
        }
        set(&broker, spec, "private-rotated");
        set(
            &broker,
            Secret { name: "SECOND".into(), host: Some("api.example.com".into()), ..Default::default() },
            "private-second",
        );
        for name in ["FIRST", "EXPOSED", "MISSING"] {
            broker.control(Request::Remove { name: name.into() }, true, broker.admin_uid).unwrap();
        }
        broker.hosts().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), projected.metadata().unwrap().ino());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"api.example.com\"]");
        broker.control(Request::Remove { name: "SECOND".into() }, true, broker.admin_uid).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), projected.metadata().unwrap().ino());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }

    #[test]
    fn no_op_sets_skip_projection_and_failed_projection_is_retried() {
        let (dir, broker, _) = machine();
        let path = dir.path().join("rule-hosts.json");
        let spec = Secret { name: "KEY".into(), host: Some("api.example.com".into()), ..Default::default() };
        set(&broker, spec.clone(), "private-canary");
        // An unreadable projection must not be consulted for an acknowledged no-op.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        set(&broker, spec.clone(), "private-canary");
        assert!(!broker.hosts_dirty.load(Ordering::Relaxed));
        let mut changed = spec.clone();
        changed.host = Some("other.example.com".into());
        assert!(
            broker
                .control(Request::Set { spec: changed.clone(), value: "private-canary".into() }, true, broker.admin_uid)
                .is_err()
        );
        assert!(broker.hosts_dirty.load(Ordering::Relaxed));
        std::fs::remove_dir(&path).unwrap();
        // The catalog already contains this rule; the retry still repairs its projection.
        set(&broker, changed, "private-canary");
        assert!(!broker.hosts_dirty.load(Ordering::Relaxed));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"other.example.com\"]");
    }

    #[test]
    fn use_socket_permissions_are_limited_to_its_access_group() {
        use std::os::unix::fs::MetadataExt;
        let (dir, _, _) = machine();
        let meta = std::fs::metadata(dir.path().join("use.sock")).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o660);
        assert_eq!(meta.gid(), unsafe { libc::getegid() });
        assert_eq!(group_id("root").unwrap(), 0);
        assert!(group_id(&format!("absent-{}", uuid::Uuid::new_v4().simple())).is_err());
    }

    #[test]
    fn idle_and_trickling_peers_do_not_occupy_authenticated_broker_slots() {
        let (_dir, broker, client) = machine();
        let mut peers = Vec::new();
        let mut trickler = None;
        let mut peak_pending = 0;
        for index in 0..MAX_PRE_AUTH + 4 {
            let (mut peer, socket) = UnixStream::pair().unwrap();
            if index == 0 {
                // A complete JSON Relay preamble still needs complete authenticated HTTP headers.
                peer.write_all(b"{\"op\":\"relay\"}\nGET /api.example.com/ HTTP/1.1\r\n").unwrap();
            }
            // The real accept loop uses this handler. Paired sockets let us assert the cap
            // after every admission without racing the listener's backlog or polling thread.
            start_request(broker.clone(), socket, false);
            if index == 1 {
                let mut trickle = peer.try_clone().unwrap();
                trickler = Some(std::thread::spawn(move || {
                    while trickle.write_all(b" ").is_ok() {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }));
            }
            let pending = broker.engine.pending.load(Ordering::Acquire);
            assert!(pending <= MAX_PRE_AUTH, "pending peers exceeded their separate pool");
            peak_pending = peak_pending.max(pending);
            peers.push(peer);
        }
        assert!(peak_pending > 0, "broker never admitted a pending peer");
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(8));
        assert_eq!(broker.engine.connections.load(Ordering::Acquire), 0);
        while broker.engine.pending.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "idle/trickling peers outlived the absolute auth deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
        trickler.unwrap().join().unwrap();
        assert!(client.list().unwrap().is_empty(), "host calls work after pending peers expire");
    }

    #[test]
    fn authenticated_relay_body_may_arrive_after_the_pre_auth_deadline() {
        let upstream = fake::start();
        let (_dir, broker, client) = machine_with(HashMap::from([("api.example.com".into(), upstream.url.clone())]));
        set(
            &broker,
            Secret { name: "KEY".into(), host: Some("api.example.com".into()), ..Default::default() },
            "private-canary",
        );
        let placeholder = format!("atomics-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        client.grant("KEY", "s1", &placeholder).unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(8));
        while broker.engine.connections.load(Ordering::Acquire) != 0
            || broker.engine.pending.load(Ordering::Acquire) != 0
        {
            assert!(Instant::now() < deadline, "grant control connection did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut peer = client.connect(&Request::Relay).unwrap();
        peer.write_all(format!("POST /api.example.com/v1/messages HTTP/1.1\r\nAuthorization: Bearer {placeholder}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(8));
        while broker.engine.connections.load(Ordering::Acquire) != 1
            || broker.engine.pending.load(Ordering::Acquire) != 0
        {
            assert!(Instant::now() < deadline, "relay did not authenticate");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(broker.engine.pending.load(Ordering::Acquire), 0);
        std::thread::sleep(AUTH_DEADLINE + Duration::from_millis(100));
        peer.write_all(b"{}").unwrap();
        upstream.release.send(()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let mut response = String::new();
        peer.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    #[test]
    fn broker_admin_has_no_read_operation_and_exposure_is_separate() {
        let (dir, broker, client) = machine();
        let spec = Secret { name: "RELAY_KEY".into(), host: Some("api.example.com".into()), ..Default::default() };
        set(&broker, spec.clone(), "private-relay-canary");
        set(&broker, Secret { name: "EXPOSED_KEY".into(), ..Default::default() }, "exposed-canary");
        let listed = broker.control(Request::List, true, broker.admin_uid).unwrap();
        let text = serde_json::to_string(&listed).unwrap();
        assert!(!text.contains("canary") && !text.contains("exposed\""));
        assert_eq!(client.list().unwrap().len(), 2);
        assert!(client.expose("RELAY_KEY", "s1").is_err());
        assert_eq!(client.expose("EXPOSED_KEY", "s1").unwrap(), "exposed-canary");
        assert!(
            broker
                .control(Request::Expose { name: "EXPOSED_KEY".into(), session: "s1".into() }, true, broker.admin_uid)
                .is_err()
        );
        assert!(broker.control(Request::Remove { name: spec.name.clone() }, false, broker.host_uid).is_err());
        assert!(broker.control(Request::List, true, broker.admin_uid + 10).is_err());
        assert!(
            broker
                .control(
                    Request::Expose { name: "EXPOSED_KEY".into(), session: "s1".into() },
                    false,
                    broker.host_uid + 10
                )
                .is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"op":"get","name":"RELAY_KEY"}"#).is_err());
        broker.control(Request::Remove { name: spec.name }, true, broker.admin_uid).unwrap();
        assert_eq!(client.list().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(dir.path().join("rule-hosts.json")).unwrap(), "[]");
    }

    #[test]
    fn broker_mapping_rejects_guesses_collisions_revoked_and_removed_placeholders() {
        let (_dir, broker, client) = machine();
        set(
            &broker,
            Secret { name: "KEY".into(), host: Some("api.example.com".into()), ..Default::default() },
            "private-canary",
        );
        let placeholder = format!("atomics-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        assert!(client.grant("KEY", "s1", "atomics-guessed").is_err());
        client.grant("KEY", "s1", &placeholder).unwrap();
        assert!(client.grant("KEY", "s2", &placeholder).is_err());
        let request = |token: &str| Head {
            method: "GET".into(),
            target: "/other.example.com/v1/models".into(),
            http11: true,
            headers: vec![("authorization".into(), format!("Bearer {token}"))],
        };
        let mut reply = Vec::new();
        assert_eq!(client.forward(&request("atomics-guessed"), vec![], &mut reply).unwrap().0, 401);
        reply.clear();
        assert_eq!(client.forward(&request(&placeholder), vec![], &mut reply).unwrap().0, 403);
        client.revoke(&placeholder).unwrap();
        reply.clear();
        assert_eq!(client.forward(&request(&placeholder), vec![], &mut reply).unwrap().0, 401);
        client.grant("KEY", "s1", &placeholder).unwrap();
        broker.control(Request::Remove { name: "KEY".into() }, true, broker.admin_uid).unwrap();
        reply.clear();
        assert_eq!(client.forward(&request(&placeholder), vec![], &mut reply).unwrap().0, 401);
        // A fresh broker has no persisted grants, even though its secret catalog persists.
        assert!(Inner::new(broker.state.join("other.log"), HashMap::new(), true).grants.lock().unwrap().is_empty());
    }

    #[test]
    fn admin_sync_is_idempotent_and_changed_values_revoke_old_grants() {
        let (_dir, broker, client) = machine();
        let spec = Secret { name: "KEY".into(), host: Some("api.example.com".into()), ..Default::default() };
        set(&broker, spec.clone(), "first-private-canary");
        let placeholder = format!("atomics-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        client.grant("KEY", "s1", &placeholder).unwrap();
        set(&broker, spec.clone(), "first-private-canary");
        assert!(broker.engine.grants.lock().unwrap().contains_key(&placeholder));
        set(&broker, spec, "second-private-canary");
        assert!(broker.engine.grants.lock().unwrap().is_empty());
        assert_eq!(broker.store.secret_values().unwrap()[0].1.as_str(), "second-private-canary");
        for _ in 0..2 {
            broker.control(Request::Remove { name: "KEY".into() }, true, broker.admin_uid).unwrap();
        }
        assert!(client.list().unwrap().is_empty());
    }

    #[test]
    fn broker_rule_checks_cover_all_auth_styles_host_method_and_path_boundaries() {
        for auth in [Auth::Bearer, Auth::Header("x-key".into()), Auth::Basic("user".into()), Auth::Query("key".into())]
        {
            let (_dir, broker, client) = machine();
            let spec = Secret {
                name: "KEY".into(),
                host: Some("api.example.com".into()),
                methods: vec!["GET".into()],
                path_prefix: Some("/v1/models".into()),
                auth: auth.clone(),
                ..Default::default()
            };
            set(&broker, spec, "canary");
            let placeholder = format!("atomics-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
            client.grant("KEY", "s1", &placeholder).unwrap();
            let credential = broker.engine.grants.lock().unwrap()[&placeholder].credential.clone();
            let mut head = Head {
                method: "GET".into(),
                target: "/api.example.com/v1/models".into(),
                http11: true,
                headers: vec![],
            };
            match auth {
                Auth::Bearer => head.headers.push(("authorization".into(), format!("Bearer {placeholder}"))),
                Auth::Header(name) => head.headers.push((name, placeholder.clone())),
                Auth::Basic(user) => {
                    use base64::Engine;
                    head.headers.push((
                        "authorization".into(),
                        format!(
                            "Basic {}",
                            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{placeholder}"))
                        ),
                    ));
                }
                Auth::Query(name) => head.target.push_str(&format!("?{name}={placeholder}")),
            }
            assert!(head.carries(&placeholder, &credential.authorization));
            for (host, method, path, allowed) in [
                ("api.example.com", "GET", "/v1/models", true),
                ("api.example.com", "GET", "/v1/models/one", true),
                ("api.example.com", "GET", "/v1/models-other", false),
                ("api.example.com", "POST", "/v1/models", false),
                ("other.example.com", "GET", "/v1/models", false),
            ] {
                assert_eq!(credential.rules.iter().any(|rule| rule.allows(host, method, path)), allowed);
            }
        }
    }

    #[test]
    fn broker_streams_through_the_host_relay_without_a_window_or_relay_values_in_the_host() {
        let upstream = fake::start();
        let (dir, broker, client) = machine_with(HashMap::from([("api.example.com".into(), upstream.url.clone())]));
        set(
            &broker,
            Secret {
                name: "KEY".into(),
                host: Some("api.example.com".into()),
                scope: Scope::Workspace("daily".into()),
                ..Default::default()
            },
            "broker-only-canary",
        );
        let host = Service {
            broker: Some(client),
            ..Service::transient(dir.path().join("host"), Keychain::in_folder(&dir.path().join("host-keychain")))
        };
        assert!(host.grant_secrets("other", Some("other"), None, false, None).unwrap().is_none());
        let granted = host.grant_secrets("s1", Some("daily"), None, false, None).unwrap().unwrap();
        assert!(!granted.secrets.values().any(|value| value.contains("broker-only-canary")));
        assert!(!dir.path().join("host-keychain").exists());
        let placeholder = &granted.secrets["KEY"];
        let (head, mut response) = fake::send(
            granted.port,
            &format!(
                "POST /api.example.com/v1/messages HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {placeholder}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(fake::chunk(&mut response).contains("message_start"));
        assert_eq!(upstream.seen.lock().unwrap()[0].header("authorization"), vec!["Bearer broker-only-canary"]);
        upstream.release.send(()).unwrap();
        assert!(fake::chunk(&mut response).contains("message_stop"));
        host.revoke(&granted);
        assert!(broker.engine.grants.lock().unwrap().is_empty());
    }
    #[test]
    fn github_broker_keeps_accounts_private_and_enforces_repository_and_default_policy() {
        let fake = github::tests::Server::start();
        let (dir, broker, client) = machine_with(fake.upstreams());
        let spec = github::account_spec("account-one").unwrap();
        set(&broker, spec.clone(), "ghp_CANARY_BROKER_GITHUB");
        assert!(client.grant(&spec.name, "s1", "atomics-invalid").is_err());
        assert!(client.expose(&spec.name, "s1").is_err());
        let mut service = Service::with_upstreams(
            dir.path().join("host"),
            Keychain::in_folder(&dir.path().join("empty-host-keychain")),
            HashMap::new(),
        );
        service.broker = Some(client.clone());
        let mut scope = github::Scope::from_project(&fake.work, true).unwrap();
        scope.refuse_default_branch = false; // the caller cannot weaken the stored rule
        let grant = service.grant_github("s1", "account-one", scope.clone(), false, None).unwrap().unwrap();
        let run = |grant: &Granted, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&fake.work)
                .args(args)
                .envs(&grant.secrets)
                .output()
                .unwrap()
        };
        assert!(run(&grant, &["push", "origin", "task"]).status.success());
        assert!(run(&grant, &["fetch", "origin"]).status.success());
        assert!(!run(&grant, &["push", "origin", "task:main"]).status.success());
        assert!(!run(&grant, &["push", "https://github.com/other/repo.git", "task"]).status.success());
        let base = format!("http://127.0.0.1:{}/api.github.com", grant.port);
        let header = format!("Bearer {}", grant.secrets["ATOMICS_GITHUB_PLACEHOLDER"]);
        let agent = ureq::Agent::config_builder().proxy(None).http_status_as_error(false).build().new_agent();
        assert_eq!(agent.get(format!("{base}/user")).header("authorization", &header).call().unwrap().status(), 403);
        assert_eq!(
            agent
                .post(format!("{base}/repos/group/repo/pulls"))
                .header("authorization", &header)
                .send(b"{}")
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            agent
                .patch(format!("{base}/repos/group/repo/pulls/1"))
                .header("authorization", &header)
                .send(b"{}")
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            agent
                .post(format!("{base}/repos/other/repo/pulls"))
                .header("authorization", &header)
                .send(b"{}")
                .unwrap()
                .status(),
            403
        );
        let mut readable = format!("{:?}{:?}", grant, grant.secrets);
        readable += &std::fs::read_to_string(dir.path().join("host/relay.log")).unwrap();
        assert!(!readable.contains("CANARY"));
        assert!(crate::credentials::Store::local(service.keychain().clone()).secret_values().unwrap().is_empty());
        // Changing the secret rule invalidates old grants and explicitly permits a default push.
        broker
            .control(Request::SetGithubPolicy { name: spec.name, refuse_default_branch: false }, true, broker.admin_uid)
            .unwrap();
        assert!(!run(&grant, &["fetch", "origin"]).status.success());
        let grant = service.grant_github("s2", "account-one", scope, false, None).unwrap().unwrap();
        assert!(run(&grant, &["push", "origin", "task:main"]).status.success());
        service.revoke(&grant);
    }
    #[test]
    fn github_large_push_streams_after_the_empty_probe_through_the_broker() {
        let server = github::tests::Server::start();
        server.commit_large_blob();
        let (dir, broker, client) = machine_with(server.upstreams());
        set(&broker, github::account_spec("account-one").unwrap(), "ghp_CANARY_BROKER_GITHUB");
        let mut host =
            Service::transient(dir.path().join("host"), Keychain::in_folder(&dir.path().join("host-keychain")));
        host.broker = Some(client);
        let scope = github::Scope::from_project(&server.work, false).unwrap();
        let grant = host.grant_github("connection-check", "account-one", scope, false, None).unwrap().unwrap();
        let (head, mut body) = fake::send(
            grant.port,
            &format!(
                "GET /github.com/group/repo.git/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: relay\r\nAuthorization: Bearer {}\r\n\r\n",
                grant.secrets["ATOMICS_GITHUB_PLACEHOLDER"]
            ),
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.to_ascii_lowercase().contains("\r\nconnection: close\r\n"), "{head}");
        io::copy(&mut body, &mut io::sink()).unwrap();
        host.revoke(&grant);
        server.assert_large_push(&host);
        assert!(broker.engine.grants.lock().unwrap().is_empty());
    }

    #[test]
    fn partial_secret_grant_failure_revokes_new_placeholders_on_host_and_broker() {
        let (dir, broker, client) = machine();
        for name in ["A_KEY", "B_KEY"] {
            set(
                &broker,
                Secret { name: name.into(), host: Some("api.example.com".into()), ..Default::default() },
                "canary",
            );
        }
        set(&broker, github::account_spec("account-one").unwrap(), "ghp_CANARY_BROKER_GITHUB");
        let mut host =
            Service::transient(dir.path().join("host"), Keychain::in_folder(&dir.path().join("host-keychain")));
        host.broker = Some(client);
        let scope = github::Scope { repository: "group/repo".into(), pulls: false, refuse_default_branch: true };
        let existing = host.grant_github("existing", "account-one", scope, false, None).unwrap().unwrap();
        let credential = Arc::new(Credential::claude("unused".into()));
        // Leave room for exactly one new mapping, so the next secret fails at the real broker limit.
        for i in 0..4094 {
            broker.engine.grants.lock().unwrap().insert(
                format!("fixture-{i}"),
                Grant { session: "other".into(), credential: credential.clone(), replies: Arc::default() },
            );
        }
        assert!(host.grant_secrets("failed-session", None, None, false, Some(existing.clone())).is_err());
        assert_eq!(host.live_grants(), 1, "the earlier session's grant remains usable");
        let grants = broker.engine.grants.lock().unwrap();
        assert_eq!(grants.len(), 4095);
        assert_eq!(grants.values().filter(|g| g.session == "existing").count(), 1);
        assert!(grants.values().all(|g| g.session != "failed-session"));
        drop(grants);
        host.revoke(&existing);
    }
}
