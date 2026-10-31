//! The sign-in experience against the fake authorization and MCP servers of `mcp_client`'s tests.
use std::time::{Duration, Instant};

use super::*;
use crate::mcp_client::{FakeOAuthServer as Fake, McpClient, McpError};
use crate::tickets::{TicketJob, TicketOp, spawn_with};
use atomics_core::tickets::TicketLink;

fn machine() -> (crate::test_support::TempDir, OAuth) {
    crate::mcp_client::oauth_test_machine()
}

/// The redirect the fake authorization server answers an authorization URL with: what a browser ends on.
fn browser_end(url: &str) -> String {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let response = agent.get(url).call().unwrap();
    response.headers().get("location").and_then(|v| v.to_str().ok()).unwrap().to_string()
}

fn until<T>(mut step: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
    loop {
        if let Some(value) = step() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start(oauth: &OAuth, server: &McpServer) -> SignInJob {
    SignInJob::start(oauth.clone(), server.clone(), false, &crate::host::Wake::default())
}

fn announced(job: &mut SignInJob) -> String {
    until(|| {
        assert!(job.poll().is_none(), "finished before the address was used");
        job.progress.url.clone()
    })
}

/// Waits for what the sign-in reported (a wrong paste, a browser that failed) while it keeps waiting.
fn reported(job: &mut SignInJob, what: impl Fn(&Progress) -> Option<String>) -> String {
    until(|| {
        assert!(job.poll().is_none(), "the sign-in must keep waiting");
        what(&job.progress)
    })
}

fn finished(job: &mut SignInJob) -> Result<(), String> {
    until(|| job.poll())
}

fn config(server: &McpServer) -> TicketConfig {
    let mut config: TicketConfig =
        toml::from_str("system = \"echo\"\n[operations.comment]\ntool = \"note\"\narguments = { body = \"{body}\" }")
            .unwrap();
    config.server = server.clone();
    config
}

#[test]
fn headless_means_ssh_or_a_linux_machine_without_a_display() {
    let set = |names: &'static [&'static str]| move |name: &str| names.contains(&name);
    assert!(!headless_in("linux", set(&["DISPLAY"])));
    assert!(!headless_in("linux", set(&["WAYLAND_DISPLAY"])));
    assert!(headless_in("linux", set(&[])));
    assert!(headless_in("linux", set(&["DISPLAY", "SSH_CONNECTION"])));
    assert!(headless_in("macos", set(&["SSH_TTY"])));
    assert!(!headless_in("macos", set(&[])));
    assert!(!headless_in("windows", set(&[])));
}

#[test]
fn a_sign_in_through_the_browser_end_signs_in_and_the_state_follows() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedOut);
    let mut job = start(&oauth, &server);
    let url = announced(&mut job);
    assert!(url.contains("code_challenge_method=S256") && url.contains("resource="));
    crate::mcp_client::oauth_test_browse(&url);
    finished(&mut job).unwrap();
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedIn);
    // A server without oauth has no sign-in state to show.
    assert_eq!(SignState::of(&oauth, &McpServer::default()), SignState::SignedIn);
}

#[test]
fn the_paste_flow_rejects_a_wrong_state_and_keeps_waiting() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    let mut job = start(&oauth, &server);
    let url = announced(&mut job);
    let end = browser_end(&url);
    let state = url::Url::parse(&end).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    job.paste(&end.replace(&state, "not-the-state"));
    until(|| {
        assert!(job.poll().is_none(), "a wrong state must not end the sign-in");
        job.progress.rejected.clone()
    });
    assert!(job.progress.rejected.as_deref().unwrap().contains("state mismatch"));
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedOut);
    // Another address, and text that is no address, are refused too; the right one is used.
    job.paste("hello");
    job.paste(&end.replace("/callback", "/elsewhere"));
    job.paste(&format!("  {end}  "));
    finished(&mut job).unwrap();
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedIn);
}

#[test]
fn only_the_query_of_the_address_is_enough_and_a_wrong_issuer_is_refused() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    let mut job = start(&oauth, &server);
    let end = browser_end(&announced(&mut job));
    job.paste(&end.replace(&end[..end.find('?').unwrap()], "").replace("iss=", "iss=https%3A%2F%2Fother.example&x="));
    reported(&mut job, |p| p.rejected.clone());
    assert!(job.progress.rejected.as_deref().unwrap().contains("issuer mismatch"));
    job.paste(end.split_once('?').unwrap().1);
    finished(&mut job).unwrap();
}

#[test]
fn cancel_ends_a_sign_in_that_is_waiting() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let mut job = start(&oauth, &fake.server());
    announced(&mut job);
    job.cancel();
    assert_eq!(finished(&mut job).unwrap_err(), "sign-in cancelled");
    assert_eq!(SignState::of(&oauth, &fake.server()), SignState::SignedOut);
}

#[test]
fn a_browser_that_cannot_open_leaves_the_address_to_paste() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    // `machine` has a browser program that does not exist.
    let mut job = SignInJob::start(oauth.clone(), server.clone(), true, &crate::host::Wake::default());
    let url = announced(&mut job);
    reported(&mut job, |p| p.browser_failed.clone());
    job.paste(&browser_end(&url));
    finished(&mut job).unwrap();
}

#[test]
fn expiry_refreshes_then_invalid_grant_signs_out_and_the_worker_does_not_back_off() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    fake.state.lock().unwrap().expires_in = 0;
    let mut job = start(&oauth, &server);
    crate::mcp_client::oauth_test_browse(&announced(&mut job));
    finished(&mut job).unwrap();

    // The access token is already due: connecting refreshes it, and the refresh token rotates.
    let mut client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
    assert!(client.tool_names().unwrap().contains(&"search".to_string()));
    let refreshes = |fake: &Fake| {
        fake.state.lock().unwrap().token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").count()
    };
    assert!(refreshes(&fake) >= 1);
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedIn);

    // The server forgets the sign-in: invalid_grant is "signed out", not a transport failure.
    {
        let mut st = fake.state.lock().unwrap();
        st.access.clear();
        st.refresh.clear();
    }
    let error = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedOut);

    // Through the worker: the job says sign-in required at once, twice, and after the person signs in
    // the very next job on the same worker goes through (no backoff to wait out).
    fake.state.lock().unwrap().expires_in = 3600;
    let (done, results) = crate::host::Wake::default().channel();
    let jobs = spawn_with(done, oauth.clone());
    let config = Arc::new(config(&server));
    let comment = |tag: u32| {
        let link = TicketLink { id: "1".into(), ..TicketLink::default() };
        jobs.send(TicketJob { config: config.clone(), op: TicketOp::Comment(link, "Hi".into()), tag }).unwrap();
        results.recv_timeout(Duration::from_secs(10)).unwrap().1
    };
    for tag in 0..2 {
        let error = comment(tag).err().unwrap();
        assert!(crate::mcp_client::is_sign_in_required(&error), "{error}");
    }
    let mut job = start(&oauth, &server);
    crate::mcp_client::oauth_test_browse(&announced(&mut job));
    finished(&mut job).unwrap();
    assert!(comment(9).is_ok());
}

#[test]
fn signing_out_revokes_both_tokens_and_forgets_them_even_when_revocation_fails() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    let wake = crate::host::Wake::default();
    let mut job = start(&oauth, &server);
    crate::mcp_client::oauth_test_browse(&announced(&mut job));
    finished(&mut job).unwrap();
    let (access, refresh) = {
        let st = fake.state.lock().unwrap();
        (st.access.iter().next().unwrap().clone(), st.refresh.iter().next().unwrap().clone())
    };

    let result = sign_out(oauth.clone(), server.clone(), &wake).recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(result, Ok(Revocation::Revoked));
    assert_eq!(sign_out_message("host", &result), "Signed out of host; the server revoked the sign-in");
    {
        let st = fake.state.lock().unwrap();
        let tokens: Vec<_> = st.revoked.iter().map(|r| (r["token_type_hint"].as_str(), r["token"].as_str())).collect();
        assert_eq!(tokens, [("refresh_token", refresh.as_str()), ("access_token", access.as_str())]);
        assert!(st.access.is_empty() && st.refresh.is_empty());
    }
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedOut);

    // A server that errors on revocation: reported, and the local sign-out still completed.
    fake.state.lock().unwrap().revoke_status = Some(500);
    let mut job = start(&oauth, &server);
    crate::mcp_client::oauth_test_browse(&announced(&mut job));
    finished(&mut job).unwrap();
    let result = sign_out(oauth.clone(), server.clone(), &wake).recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(result, Ok(Revocation::Failed(_))), "{result:?}");
    assert!(sign_out_message("host", &result).contains("revocation failed"));
    assert_eq!(SignState::of(&oauth, &server), SignState::SignedOut);
    // Signing out when nothing is signed in is fine.
    let result = sign_out(oauth, server, &wake).recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(result, Ok(Revocation::NotOffered));
}

#[test]
fn no_token_or_client_secret_reaches_doctor_output_plan_files_errors_or_messages() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server();
    let mut config = config(&server);
    let mut outputs = vec![doctor_line(&oauth, &config).unwrap()];
    let mut job = start(&oauth, &server);
    let url = announced(&mut job);
    let end = browser_end(&url);
    // Every message a failed paste can produce.
    job.paste(&end.replace("code=", "code=SECRET-CODE&state=x&c="));
    job.paste(&end.replace("/callback", "/elsewhere"));
    job.paste("code=ABC");
    reported(&mut job, |p| p.rejected.clone().filter(|why| why.contains("state")));
    outputs.push(job.progress.rejected.clone().unwrap());
    job.paste(&end);
    finished(&mut job).unwrap();

    let line = doctor_line(&oauth, &config).unwrap();
    assert!(line.starts_with("✓ credential MCP sign-in (127.0.0.1): present"), "{line}");
    assert!(line.ends_with("(values hidden)"));
    outputs.push(line);
    outputs.push(format!("{:?}", SignState::of(&oauth, &server)));

    // Secrets as the server and the keychain hold them.
    let mut secrets = oauth.secrets(&server);
    secrets.extend(fake.state.lock().unwrap().access.iter().cloned());
    secrets.extend(fake.state.lock().unwrap().refresh.iter().cloned());
    secrets.extend(fake.state.lock().unwrap().clients.keys().cloned());
    assert!(secrets.len() >= 3, "{}", secrets.len());

    // A plan following a ticket, as it is saved.
    let plan = atomics_core::plan::Plan {
        id: "p1".into(),
        goal: "Sign in".into(),
        ticket: Some(TicketLink { id: "1".into(), key: Some("T-1".into()), system: "echo".into(), ..TicketLink::default() }),
        ..Default::default()
    };
    let path = dir.path().join("plan.json");
    plan.save(&path).unwrap();
    outputs.push(std::fs::read_to_string(path).unwrap());

    // Errors: a revoked sign-in through the client, and a job through the worker.
    {
        let mut st = fake.state.lock().unwrap();
        st.access.clear();
        st.refresh.clear();
    }
    outputs.push(McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap().to_string());
    let (done, results) = crate::host::Wake::default().channel();
    let jobs = spawn_with(done, oauth.clone());
    config.server = server.clone();
    let link = TicketLink { id: "1".into(), ..TicketLink::default() };
    jobs.send(TicketJob { config: Arc::new(config), op: TicketOp::Comment(link, "Hi".into()), tag: 0 }).unwrap();
    outputs.push(results.recv_timeout(Duration::from_secs(10)).unwrap().1.err().unwrap());
    outputs.push(sign_out_message("h", &Err("boom".into())));

    let joined = outputs.join("\n");
    for secret in &secrets {
        assert!(!joined.contains(secret.as_str()), "a secret appears in an output");
    }
    assert!(!joined.contains("SECRET-CODE") && !joined.contains("code_verifier"));
}
