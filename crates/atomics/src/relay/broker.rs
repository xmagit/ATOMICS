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
}

// No Debug: Set carries a secret. No generic get/read/copy operation exists.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List,
    Set { spec: Secret, value: String },
    Remove { name: String },
    Grant { name: String, session: String, placeholder: String },
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
        let stream = self.connect(&request)?;
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
}

impl Broker {
    fn hosts(&self) -> Result<()> {
        let mut hosts: Vec<_> = self.store.secrets()?.into_iter().filter_map(|spec| spec.host).collect();
        hosts.sort();
        hosts.dedup();
        // A root-owned path unit consumes only this validated, value-free projection.
        write_private(&self.state, &self.state.join("rule-hosts.json"), &serde_json::to_string(&hosts)?)?;
        Ok(())
    }

    fn control(&self, request: Request, admin: bool, uid: u32) -> Result<Reply> {
        let _edit = self.edits.lock().unwrap();
        let mut reply = Reply { ok: true, ..Reply::default() };
        if admin {
            anyhow::ensure!(uid == 0 || uid == self.admin_uid, "admin peer refused");
            match request {
                Request::List => reply.secrets = self.store.secrets()?,
                Request::Set { spec, mut value } => {
                    use zeroize::Zeroize;
                    let result = self.store.add_secret_rule(spec, &value);
                    value.zeroize();
                    result?;
                    self.hosts()?;
                    eprintln!("secret-broker: secret set");
                }
                Request::Remove { name } => {
                    self.store.remove_secret(&name)?;
                    self.engine
                        .grants
                        .lock()
                        .unwrap()
                        .retain(|_, grant| grant.credential.label != format!("secret:{name}"));
                    self.hosts()?;
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
                        .find(|(spec, _)| spec.name == name && spec.host.is_some())
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

    fn serve(&self, socket: UnixStream, admin: bool) -> Result<()> {
        socket.set_read_timeout(Some(IDLE))?;
        socket.set_write_timeout(Some(IDLE))?;
        let uid = peer_uid(&socket)?;
        let mut reader = BufReader::new(socket.try_clone()?);
        let mut writer = socket;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        reader.by_ref().take(MAX_CONTROL).read_until(b'\n', &mut bytes)?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid broker request"))?;
        if matches!(request, Request::Relay) && !admin {
            // Any peer may present a placeholder; the engine accepts only a host-issued mapping.
            self.engine.exchange(&mut reader, &mut writer)?;
        } else {
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

fn listen(path: &Path, mode: u32) -> Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let socket = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(socket)
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
    });
    broker.hosts()?;
    let use_socket = listen(&options.use_socket, 0o666)?;
    let admin_socket = listen(&options.admin_socket, 0o660)?;
    let accepting = broker.clone();
    std::thread::Builder::new()
        .name("secret-broker-admin".into())
        .spawn(move || accept(accepting, admin_socket, true))?;
    accept(broker, use_socket, false);
    Ok(())
}

fn accept(broker: Arc<Broker>, listener: UnixListener, admin: bool) {
    for socket in listener.incoming().flatten() {
        if broker.engine.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            broker.engine.connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let slot = Slot(broker.engine.clone());
        let broker = broker.clone();
        let _ = std::thread::Builder::new().name("secret-broker-request".into()).spawn(move || {
            if broker.serve(socket, admin).is_err() {
                eprintln!("secret-broker: malformed or failed request (contents hidden)");
            }
            drop(slot);
        });
    }
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
        });
        let listener = listen(&socket, 0o666).unwrap();
        let serving = broker.clone();
        // Test listeners exit when their temporary path is removed; no global environment changes.
        listener.set_nonblocking(true).unwrap();
        let path = socket.clone();
        std::thread::spawn(move || {
            while path.exists() {
                if let Ok((socket, _)) = listener.accept() {
                    let serving = serving.clone();
                    std::thread::spawn(move || {
                        let _ = serving.serve(socket, false);
                    });
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
}
