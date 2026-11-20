//! A Mac-only issuer and a VM-local TLS server key. The issuer never goes through SSH.
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use zeroize::Zeroizing;

use super::{BundleInfo, account, built, update};

const GUEST_CERT: &str = "/var/lib/atomics-localhost-tls/current/cert.pem";
const GUEST_KEY: &str = "/var/lib/atomics-localhost-tls/current/key.pem";
const LEAF_LIFETIME_DAYS: &str = "90";
const RENEW_BEFORE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const MAX_CERT_BYTES: usize = 64 * 1024;
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

/// Even read-only devices would give the guest access to the Mac's signing key.
pub(super) fn check_exports(storage: &Path, shares: &[update::Share]) -> Result<()> {
    for share in shares {
        let mut locations = vec![share.location.clone()];
        locations.extend(share.location.canonicalize().ok());
        if let Some(folder) = update::writable_overlap(storage, &locations) {
            anyhow::bail!("localhost CA storage {} overlaps VM share {}", storage.display(), folder.display());
        }
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
    key: PathBuf,
}

fn check_private(path: &Path, dir: bool) -> Result<()> {
    let metadata = path.symlink_metadata().with_context(|| format!("reading {}", path.display()))?;
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

fn openssl_ok(dir: &Path, args: &[&str]) -> Result<()> {
    let output = openssl(dir, args)?;
    ensure!(output.status.success(), "openssl {} failed: {}", args[0], String::from_utf8_lossy(&output.stderr).trim());
    Ok(())
}

fn generate_key(dir: &Path, mut args: Vec<&str>) -> Result<()> {
    // LibreSSL otherwise generates explicit EC parameters, which macOS TLS rejects.
    args.extend(["-pkeyopt", "ec_paramgen_curve:prime256v1", "-pkeyopt", "ec_param_enc:named_curve"]);
    openssl_ok(dir, &args)
}

impl Ca {
    fn load(storage: &Path) -> Result<Option<Self>> {
        let path = std::path::absolute(storage)?.join("ca");
        if !path.try_exists()? {
            return Ok(None);
        }
        check_private(storage, true)?;
        check_private(&path, true)?;
        let key = path.join("key.pem");
        check_private(&key, false)?;
        let cert = path.join("cert.pem");
        check_private(&cert, false)?;
        Ok(Some(Self { cert, key }))
    }

    fn create(storage: &Path) -> Result<Self> {
        atomics_core::store::private_dir(storage)?;
        check_private(storage, true)?;
        let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(storage)?;
        fs::write(stage.path().join("extensions.cnf"), EXTENSIONS)?;
        let serial = format!("0x{}", uuid::Uuid::new_v4().simple());
        generate_key(
            stage.path(),
            vec![
                "req",
                "-new",
                "-x509",
                "-nodes",
                "-newkey",
                "ec",
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
                "-keyout",
                "key.pem",
                "-out",
                "cert.pem",
            ],
        )?;
        fs::remove_file(stage.path().join("extensions.cnf"))?;
        for file in ["key.pem", "cert.pem"] {
            fs::set_permissions(stage.path().join(file), Permissions::from_mode(0o600))?;
            File::open(stage.path().join(file))?.sync_all()?;
        }
        fs::rename(stage.path(), storage.join("ca"))?;
        Self::load(storage)?.context("loading the newly created localhost CA")
    }

    fn issue(&self, dir: &Path, extensions: &str, days: &str) -> Result<()> {
        fs::write(dir.join("extensions.cnf"), extensions)?;
        generate_key(
            dir,
            vec![
                "req",
                "-new",
                "-nodes",
                "-newkey",
                "ec",
                "-config",
                "extensions.cnf",
                "-subj",
                "/CN=localhost",
                "-keyout",
                "key.pem",
                "-out",
                "leaf.csr",
            ],
        )?;
        fs::set_permissions(dir.join("key.pem"), Permissions::from_mode(0o600))?;
        let serial = format!("0x{}", uuid::Uuid::new_v4().simple());
        let issuer = [
            OsStr::new("x509"),
            OsStr::new("-req"),
            OsStr::new("-CA"),
            self.cert.as_os_str(),
            OsStr::new("-CAkey"),
            self.key.as_os_str(),
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
        let output = openssl(dir, issuer.into_iter().chain(options.map(OsStr::new)))?;
        ensure!(
            output.status.success(),
            "signing localhost certificate: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
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

fn security(args: &[&OsStr]) -> Result<Output> {
    Command::new("security").args(args).output().context("running macOS security")
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
    let ca = match Ca::load(&storage).context("loading the Mac localhost CA")? {
        Some(ca) => ca,
        None => Ca::create(&storage).context("creating the Mac localhost CA")?,
    };
    let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(&storage)?;
    let cert = stage.path().join("cert.pem");
    if ca.current_at(&guest_cert, renewal_check_time(UnixTime::now()))? {
        fs::write(&cert, guest_cert)?;
    } else {
        ca.issue(stage.path(), EXTENSIONS, LEAF_LIFETIME_DAYS)?;
        let key = Zeroizing::new(fs::read_to_string(stage.path().join("key.pem"))?);
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
            let stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o700)).tempdir_in(&storage)?;
            ca.issue(stage.path(), EXTENSIONS, LEAF_LIFETIME_DAYS)?;
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
