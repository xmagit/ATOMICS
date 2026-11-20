//! A Mac-only issuer and a VM-local TLS server key. The issuer never goes through SSH.
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use zeroize::Zeroizing;

use super::{BundleInfo, account, built, update};
use crate::relay::Keychain;

const GUEST_CERT: &str = "/var/lib/atomics-localhost-tls/current/cert.pem";
const GUEST_KEY: &str = "/var/lib/atomics-localhost-tls/current/key.pem";
const LEAF_LIFETIME_DAYS: &str = "90";
const RENEW_BEFORE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const MAX_CERT_BYTES: usize = 64 * 1024;
const MAX_CA_KEY_BYTES: usize = 1024;
const CA_SERVICE: &str = "atomics-localhost-ca";
const NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

const EXTENSIONS: &str = r#"
[req]
distinguished_name = dn
[dn]
[ca]
basicConstraints = critical,CA:TRUE,pathlen:0
keyUsage = critical,keyCertSign,cRLSign
extendedKeyUsage = serverAuth
subjectKeyIdentifier = hash
nameConstraints = critical,permitted;DNS:localhost,excluded;DNS:.localhost,permitted;IP:127.0.0.1/255.255.255.255,permitted;IP:0:0:0:0:0:0:0:1/FFFF:FFFF:FFFF:FFFF:FFFF:FFFF:FFFF:FFFF
[leaf]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = serverAuth
authorityKeyIdentifier = keyid
subjectAltName = DNS:localhost,IP:127.0.0.1,IP:::1
"#;

pub(super) fn ca_storage() -> PathBuf {
    atomics_core::paths::atomics_home().join("localhost-tls")
}

/// The certificates and signing scratch files are public. A guest must not be able to replace them.
pub(super) fn check_exports(storage: &Path, shares: &[update::Share]) -> Result<()> {
    if let Some(folder) = update::writable_overlap(storage, &update::writable_folders(shares)) {
        anyhow::bail!("localhost CA storage {} overlaps writable VM share {}", storage.display(), folder.display());
    }
    Ok(())
}

pub(crate) fn agent_env() -> impl Iterator<Item = (String, String)> {
    environment(super::is_guest(), Path::new(GUEST_KEY))
}

fn environment(guest: bool, key: &Path) -> impl Iterator<Item = (String, String)> {
    let available = guest && File::open(key).is_ok();
    [
        ("ATOMICS_LOCALHOST_CERT", GUEST_CERT),
        ("ATOMICS_LOCALHOST_KEY", GUEST_KEY),
        ("ASPNETCORE_Kestrel__Certificates__Default__Path", GUEST_CERT),
        ("ASPNETCORE_Kestrel__Certificates__Default__KeyPath", GUEST_KEY),
    ]
    .into_iter()
    .filter(move |(name, _)| available && std::env::var_os(name).is_none())
    .map(|(name, value)| (name.into(), value.into()))
}

struct Ca {
    cert: PathBuf,
    account: String,
}

fn check_private(path: &Path, metadata: fs::Metadata, dir: bool) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        (if dir { metadata.is_dir() } else { metadata.is_file() })
            && metadata.uid() == uid
            && metadata.mode() & 0o7777 == if dir { 0o700 } else { 0o600 },
        "{} must be private to the current Mac user",
        path.display()
    );
    Ok(())
}

fn openssl(dir: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Output> {
    Command::new("/usr/bin/openssl")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running macOS openssl")
}

/// LibreSSL accepts /dev/stdin for a PEM key, including for `x509 -CAkey`.
fn openssl_with_key(dir: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>, key: &str) -> Result<Output> {
    let mut child = Command::new("/usr/bin/openssl")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running macOS openssl")?;
    let written = child.stdin.take().unwrap().write_all(key.as_bytes());
    let output = child.wait_with_output()?;
    written.context("sending the PEM key to openssl")?;
    Ok(output)
}

fn generate_key(dir: &Path) -> Result<Zeroizing<String>> {
    // LibreSSL otherwise generates explicit EC parameters, which macOS TLS rejects.
    let output = openssl(
        dir,
        [
            "genpkey",
            "-algorithm",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-pkeyopt",
            "ec_param_enc:named_curve",
        ],
    )?;
    let key = Zeroizing::new(output.stdout);
    ensure!(output.status.success(), "openssl could not generate a localhost TLS key");
    Ok(Zeroizing::new(std::str::from_utf8(&key)?.to_owned()))
}

impl Ca {
    fn at(storage: &Path) -> Result<Self> {
        let storage = storage.canonicalize()?;
        // A stable name lets a retry replace the item if no certificate was published.
        let account = built::sha256(storage.as_os_str().as_encoded_bytes());
        Ok(Self { cert: storage.join("ca/cert.pem"), account })
    }

    fn load(storage: &Path) -> Result<Option<Self>> {
        let path = std::path::absolute(storage)?.join("ca");
        if !path.try_exists()? {
            return Ok(None);
        }
        check_private(storage, storage.symlink_metadata()?, true)?;
        check_private(&path, path.symlink_metadata()?, true)?;
        let ca = Self::at(storage)?;
        check_private(&ca.cert, ca.cert.symlink_metadata()?, false)?;
        Ok(Some(ca))
    }

    fn create(storage: &Path, keychain: &Keychain) -> Result<Self> {
        atomics_core::store::private_dir(storage)?;
        check_private(storage, storage.symlink_metadata()?, true)?;
        let ca = Self::at(storage)?;
        let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(storage)?;
        fs::write(stage.path().join("extensions.cnf"), EXTENSIONS)?;
        let key = generate_key(stage.path())?;
        let serial = format!("0x{}", uuid::Uuid::new_v4().simple());
        let output = openssl_with_key(
            stage.path(),
            [
                "req",
                "-new",
                "-x509",
                "-key",
                "/dev/stdin",
                "-sha256",
                "-days",
                "3650",
                "-config",
                "extensions.cnf",
                "-extensions",
                "ca",
                "-subj",
                "/CN=ATOMICS Localhost CA",
                "-set_serial",
                &serial,
                "-out",
                "cert.pem",
            ],
            &key,
        )?;
        ensure!(output.status.success(), "openssl could not create the localhost CA certificate");
        fs::remove_file(stage.path().join("extensions.cnf"))?;
        let cert = stage.path().join("cert.pem");
        fs::set_permissions(&cert, Permissions::from_mode(0o600))?;
        File::open(&cert)?.sync_all()?;
        ca.write_keychain(keychain, &key)?;
        fs::rename(stage.path(), storage.join("ca"))?;
        Self::load(storage)?.context("loading the newly created localhost CA")
    }

    fn read_keychain(&self, keychain: &Keychain) -> Result<Option<Zeroizing<String>>> {
        let Some(encoded) = keychain.read_item(CA_SERVICE, &self.account)? else { return Ok(None) };
        let encoded = Zeroizing::new(encoded);
        let bytes =
            Zeroizing::new(URL_SAFE_NO_PAD.decode(encoded.as_bytes()).context("decoding the localhost CA key")?);
        ensure!(bytes.len() <= MAX_CA_KEY_BYTES, "invalid localhost CA key length");
        Ok(Some(Zeroizing::new(std::str::from_utf8(&bytes).context("decoding the localhost CA PEM")?.to_owned())))
    }

    fn write_keychain(&self, keychain: &Keychain, key: &str) -> Result<()> {
        let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(key));
        keychain.write_item(CA_SERVICE, &self.account, "ATOMICS Localhost CA", &encoded)
    }

    fn key(&self, keychain: &Keychain, migrate: bool) -> Result<Zeroizing<String>> {
        let legacy = self.cert.with_file_name("key.pem");
        let (key, legacy_exists) =
            match File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&legacy) {
                Ok(file) => {
                    check_private(&legacy, file.metadata()?, false)?;
                    let mut key = Zeroizing::new(String::new());
                    file.take(MAX_CA_KEY_BYTES as u64 + 1).read_to_string(&mut key)?;
                    ensure!(key.len() <= MAX_CA_KEY_BYTES, "invalid legacy localhost CA key length");
                    (key, true)
                }
                Err(error) if error.kind() == ErrorKind::NotFound => (
                    self.read_keychain(keychain)?
                        .context("localhost CA signing key is missing from the login keychain")?,
                    false,
                ),
                Err(error) => return Err(error).with_context(|| format!("opening {}", legacy.display())),
            };
        let dir = self.cert.parent().unwrap();
        let cert = openssl(dir, ["x509", "-in", "cert.pem", "-pubkey", "-noout"])?;
        let public_key = openssl_with_key(dir, ["pkey", "-pubout"], &key)?;
        ensure!(
            cert.status.success() && public_key.status.success() && cert.stdout == public_key.stdout,
            "localhost CA signing key does not match its certificate"
        );
        if legacy_exists && migrate {
            // The legacy key matches the existing certificate. It can repair an interrupted import.
            self.write_keychain(keychain, &key)?;
            fs::remove_file(legacy)?;
        }
        Ok(key)
    }

    fn issue(&self, dir: &Path, ca_key: &str, extensions: &str, days: &str) -> Result<Zeroizing<String>> {
        fs::write(dir.join("extensions.cnf"), extensions)?;
        let key = generate_key(dir)?;
        let request = openssl_with_key(
            dir,
            [
                "req",
                "-new",
                "-key",
                "/dev/stdin",
                "-config",
                "extensions.cnf",
                "-subj",
                "/CN=localhost",
                "-out",
                "leaf.csr",
            ],
            &key,
        )?;
        ensure!(request.status.success(), "openssl could not create the localhost certificate request");
        let serial = format!("0x{}", uuid::Uuid::new_v4().simple());
        let issuer = [
            OsStr::new("x509"),
            OsStr::new("-req"),
            OsStr::new("-CA"),
            self.cert.as_os_str(),
            OsStr::new("-CAkey"),
            OsStr::new("/dev/stdin"),
        ];
        let options = [
            "-in",
            "leaf.csr",
            "-set_serial",
            &serial,
            "-days",
            days,
            "-sha256",
            "-extfile",
            "extensions.cnf",
            "-extensions",
            "leaf",
            "-out",
            "cert.pem",
        ];
        let output = openssl_with_key(dir, issuer.into_iter().chain(options.map(OsStr::new)), ca_key)?;
        ensure!(output.status.success(), "openssl could not sign the localhost certificate");
        Ok(key)
    }

    fn current_at(&self, leaf: &[u8], now: UnixTime) -> Result<bool> {
        let Ok(leaf) = CertificateDer::from_pem_slice(leaf) else { return Ok(false) };
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_file(&self.cert)?)?;
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()?;
        Ok(NAMES.iter().all(|name| {
            verifier
                .verify_server_cert(&leaf, &[], &ServerName::try_from(*name).expect("loopback name"), &[], now)
                .is_ok()
        }))
    }
}

fn renewal_check_time(now: UnixTime) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(now.as_secs()) + RENEW_BEFORE)
}

fn security_program() -> PathBuf {
    // Isolated CLI tests use the same stand-in for trust and private key access.
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ATOMICS_TEST_SECURITY").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return path;
    }
    PathBuf::from("/usr/bin/security")
}

fn security(args: &[&OsStr]) -> Result<Output> {
    Command::new(security_program()).args(args).output().context("running macOS security")
}

fn trusted(probe: &Path, security: &impl Fn(&[&OsStr]) -> Result<Output>) -> Result<bool> {
    Ok(security(&[
        OsStr::new("verify-cert"),
        OsStr::new("-q"),
        OsStr::new("-L"),
        OsStr::new("-p"),
        OsStr::new("ssl"),
        OsStr::new("-n"),
        OsStr::new("localhost"),
        OsStr::new("-c"),
        probe.as_os_str(),
    ])?
    .status
    .success())
}

fn ensure_trusted(ca: &Path, probe: &Path, security: impl Fn(&[&OsStr]) -> Result<Output>) -> Result<()> {
    if trusted(probe, &security)? {
        return Ok(());
    }
    let login = security(&["login-keychain", "-d", "user"].map(OsStr::new))?;
    let keychain = String::from_utf8(login.stdout)?;
    let keychain = keychain.trim().trim_matches('"');
    ensure!(login.status.success() && Path::new(keychain).is_absolute(), "could not locate the macOS login keychain");
    println!("macOS may ask you to trust the ATOMICS Localhost CA for SSL.");
    let output = security(&[
        OsStr::new("add-trusted-cert"),
        OsStr::new("-r"),
        OsStr::new("trustRoot"),
        OsStr::new("-p"),
        OsStr::new("ssl"),
        OsStr::new("-k"),
        OsStr::new(keychain),
        ca.as_os_str(),
    ])?;
    ensure!(output.status.success(), "macOS did not add SSL trust: {}", String::from_utf8_lossy(&output.stderr).trim());
    ensure!(trusted(probe, &security)?, "macOS does not trust the localhost CA for SSL");
    Ok(())
}

fn vm_connection(info: &BundleInfo) -> Result<account::Connection> {
    let path = built::BootAccounts::path(info);
    let (_, connection) = built::BootAccounts::load(&path)?.select(&path, info, |candidate| {
        let connection = account::Connection::open(candidate, account::Operation::LocalhostTls)?;
        Ok((connection.system.clone(), connection))
    })?;
    Ok(connection)
}

pub(super) fn provision(installed: &Path) -> Result<()> {
    let info = update::running_info(installed, &BundleInfo::read(installed)?)
        .context("finding the running VM for localhost TLS")?;
    let storage = ca_storage();
    check_exports(
        &storage,
        &super::devices::Inventory::read(&info.instance).context("checking VM shares for localhost TLS")?.exports(),
    )?;
    let guest_cert = vm_connection(&info)
        .and_then(|vm| vm.exchange(&[], MAX_CERT_BYTES))
        .context("reading the VM localhost certificate")?;
    let keychain = Keychain::at_login(&security_program())?;
    let ca = match Ca::load(&storage).context("loading the Mac localhost CA")? {
        Some(ca) => ca,
        None => Ca::create(&storage, &keychain).context("creating the Mac localhost CA")?,
    };
    let ca_key = ca.key(&keychain, true).context("loading the Mac localhost CA signing key")?;
    let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(&storage)?;
    let cert = stage.path().join("cert.pem");
    if ca.current_at(&guest_cert, renewal_check_time(UnixTime::now()))? {
        fs::write(&cert, guest_cert)?;
    } else {
        let key = ca.issue(stage.path(), &ca_key, EXTENSIONS, LEAF_LIFETIME_DAYS)?;
        let cert_pem = fs::read_to_string(&cert)?;
        #[derive(serde::Serialize)]
        struct Leaf<'a> {
            cert: &'a str,
            key: &'a str,
        }
        let material = Zeroizing::new(serde_json::to_vec(&Leaf { cert: &cert_pem, key: &key })?);
        vm_connection(&info).and_then(|vm| vm.send(&material)).context("installing the VM localhost certificate")?;
    }
    if let Err(error) = ensure_trusted(&ca.cert, &cert, security) {
        eprintln!("warning: Mac localhost CA trust is incomplete: {error:#}. Run atomics vm setup or update to retry.");
    }
    Ok(())
}

pub(super) fn report(info: &BundleInfo) -> Result<()> {
    let storage = ca_storage();
    let ca = Ca::load(&storage)?;
    let guest = vm_connection(info).and_then(|vm| vm.exchange(&[], MAX_CERT_BYTES));
    let mac = match &ca {
        None => "missing",
        Some(ca) => {
            check_exports(&storage, &super::devices::Inventory::read(&info.instance)?.exports())?;
            let key = ca.key(&Keychain::at_login(&security_program())?, false)?;
            let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(&storage)?;
            ca.issue(stage.path(), &key, EXTENSIONS, LEAF_LIFETIME_DAYS)?;
            if trusted(&stage.path().join("cert.pem"), &security)? { "trusted for SSL" } else { "not trusted for SSL" }
        }
    };
    println!("Mac localhost CA: {mac}");
    let now = UnixTime::now();
    match guest {
        Ok(cert) => {
            let state = match ca {
                _ if cert.is_empty() => "missing or unusable",
                None => "unverified (Mac CA missing)",
                Some(ca) if !ca.current_at(&cert, now)? => "expired or invalid",
                Some(ca) if !ca.current_at(&cert, renewal_check_time(now))? => "current (renewal due)",
                Some(_) => "current",
            };
            println!("VM localhost certificate: {state}");
        }
        Err(error) => println!("VM localhost certificate: unavailable ({error:#})"),
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
