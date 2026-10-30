//! A fixed read-only KQL check, with the application ID selected by the user. All Azure token
//! acquisition runs on the host; only a placeholder and a relay URL enter the throwaway VM.

use super::*;

pub fn run(app_id: &str) -> Result<u64> {
    let app_id = uuid::Uuid::parse_str(app_id).context("expected an Application Insights application UUID")?;
    let config = imacs_core::config::Config::load(&paths::config_file())?;
    let mut azure = config.isolation.sandbox.azure;
    if !azure.enabled {
        bail!("Azure CLI relay is off; enable it in Settings → Credentials first")
    }
    azure.query_read_exception = true;
    let home = paths::imacs_home();
    let relay = std::sync::Arc::new(crate::relay::Service::transient(home.clone(), crate::relay::Keychain::current()));
    let name = format!("imacs-azure-check-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let folder = home.join("relay-check").join(&name);
    std::fs::create_dir_all(&folder)?;
    let job = Prepare {
        home: home.clone(),
        sbx: crate::setup::resolve_program("sbx"),
        name,
        agent: "shell",
        cwd: folder.clone(),
        config: SandboxConfig { memory: Some("2g".into()), cpus: Some(2), azure, ..Default::default() },
        warm: Vec::new(),
        exe: crate::setup::executable_path()?,
        socket: paths::socket_path(),
        briefs: None,
        pins: paths::git_pins_dir(),
        relay: Some(relay.clone()),
    };
    let checked = check(&job, &relay, &app_id.to_string());
    let removed = remove(&home, &job.sbx, &job.name).and_then(|()| Ok(std::fs::remove_dir_all(folder)?));
    match (checked, removed) {
        (checked, Ok(())) => checked,
        (Ok(_), Err(error)) => Err(error.context("removing the Azure check sandbox")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("also failed to remove the Azure check sandbox: {cleanup:#}")))
        }
    }
}

fn check(job: &Prepare, relay: &crate::relay::Service, app_id: &str) -> Result<u64> {
    let granted = prepare(job)?.context("host az was not found on the login-shell PATH")?;
    let checked = (|| {
        let placeholder = granted.azure.as_ref().context("host az was not found on the login-shell PATH")?;
        let url = isolation::relay_url(granted.port, "api.applicationinsights.io");
        // The only result allowed out of the VM is the scalar count, never telemetry rows/headers.
        let script = format!(
            r#"import json, os, pathlib, re, urllib.request
assert not pathlib.Path.home().joinpath('.azure').exists()
sources = '\n'.join(os.environ.values())
for name in ['/etc/sandbox-persistent.sh']:
    p = pathlib.Path(name)
    if p.is_file(): sources += p.read_text(errors='ignore')
assert not re.search(r'eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+', sources)
data = json.dumps({{'query': 'requests | where timestamp > ago(1h) | count'}}).encode()
request = urllib.request.Request('{url}/v1/apps/{app_id}/query', data=data,
    headers={{'Authorization': 'Bearer {placeholder}', 'Content-Type': 'application/json'}})
try:
    with urllib.request.urlopen(request, timeout=90) as response:
        result = json.load(response)
    rows = result['tables'][0]['rows']
    assert len(rows) == 1 and len(rows[0]) == 1
    count = rows[0][0]
    assert type(count) is int and count >= 0
    print(count)
except Exception:
    raise SystemExit(1)
"#
        );
        let output = sbx(&job.sbx, &["exec", &job.name, "python3", "-c", &script], false, Duration::from_secs(120))
            .map_err(|_| {
                anyhow::anyhow!(
                    "the sandbox KQL query failed; check host az login, access to the selected app and relay.log"
                )
            })?;
        output.trim().parse::<u64>().map_err(|_| anyhow::anyhow!("the sandbox did not return a scalar count"))
    })();
    relay.revoke(&granted);
    checked
}
