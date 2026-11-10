//! Chief requests are data. Only a board decision enters the publishing worker.
use super::*;
use crate::relay::github::Scope;
use atomics_core::identity::{self, Identity};
use atomics_core::plan::Publish;

#[derive(Default)]
pub(super) struct Jobs {
    pending: HashSet<String>,
    running: HashSet<String>,
    jobs: Vec<Receiver<Event>>,
}

enum Event {
    Prepared { plan: String, result: Result<Publish, String> },
    Published { plan: String, request: String, result: Result<plan::PullRequest, String> },
}

fn identity_digest(identity: &Identity) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(identity).expect("identity")))
}

fn prepare(
    repo: &Path,
    identity: Identity,
    branch: String,
    title: String,
    body: String,
    base: String,
) -> anyhow::Result<Publish> {
    identity.validate().map_err(anyhow::Error::msg)?;
    let account =
        identity.github.clone().ok_or_else(|| anyhow::anyhow!("Set identity.github to a connected account"))?;
    crate::relay::github::account_name(&account)?;
    let scope = Scope::from_project(repo, true)?;
    for name in [&branch, &base] {
        anyhow::ensure!(
            !name.starts_with('-') && git::git(repo, &["check-ref-format", "--branch", name]).is_ok(),
            "Invalid branch or base"
        );
    }
    anyhow::ensure!(branch != base, "Publish a task or integration branch");
    anyhow::ensure!(
        !title.trim().is_empty() && title.len() <= 256 && body.len() <= 60000,
        "Supply a title and a description up to 60000 bytes"
    );
    identity::check_pending(&identity, "pull-request", &format!("{title}\n{body}"))?;
    let head_sha =
        git::git(repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}^{{commit}}")])?.trim().to_string();
    let base_sha =
        git::git(repo, &["rev-parse", "--verify", &format!("refs/heads/{base}^{{commit}}")])?.trim().to_string();
    identity::require_clean(&identity::check_publish(repo, &branch, &head_sha, &base_sha, &identity)?)?;
    let commits =
        git::git(repo, &["log", "--no-use-mailmap", "--format=%h %an <%ae> %s", &format!("{base_sha}..{head_sha}")])?;
    anyhow::ensure!(!commits.trim().is_empty(), "No commits to publish");
    Ok(Publish {
        id: uuid::Uuid::new_v4().to_string(),
        repository: scope.repository,
        account,
        branch,
        base,
        head_sha,
        base_sha,
        title,
        body,
        commits,
        identity_digest: identity_digest(&identity),
        url: None,
        error: None,
    })
}

fn publish(
    relay: &crate::relay::Service,
    repo: &Path,
    workspace: Option<&str>,
    request: &Publish,
    identity: &Identity,
) -> anyhow::Result<plan::PullRequest> {
    // Re-run the identity guard and bind the write to the immutable commit the card showed.
    let fresh = prepare(
        repo,
        identity.clone(),
        request.branch.clone(),
        request.title.clone(),
        request.body.clone(),
        request.base.clone(),
    )?;
    anyhow::ensure!(
        fresh.repository == request.repository
            && fresh.head_sha == request.head_sha
            && fresh.base_sha == request.base_sha
            && fresh.commits == request.commits,
        "Branch, base or repository changed; request publication again"
    );
    let granted = relay
        .grant_project_github(&format!("publish-{}", request.id), &request.account, repo, workspace, false, None)?
        .context("No GitHub account")?;
    let result = (|| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "push",
                "--",
                &format!("https://github.com/{}.git", request.repository),
                &format!("{}:refs/heads/{}", request.head_sha, request.branch),
            ])
            .envs(&granted.secrets)
            .stdin(std::process::Stdio::null())
            .output()?;
        anyhow::ensure!(output.status.success(), "GitHub push refused or failed; check the relay audit status");
        let placeholder = &granted.secrets["ATOMICS_GITHUB_PLACEHOLDER"];
        let endpoint = format!("http://127.0.0.1:{}/api.github.com/repos/{}/pulls", granted.port, request.repository);
        let agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().new_agent();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("state", "open")
            .append_pair("head", &format!("{}:{}", request.repository.split('/').next().unwrap(), request.branch))
            .finish();
        let mut response = agent
            .get(format!("{endpoint}?{query}"))
            .header("authorization", format!("Bearer {placeholder}"))
            .call()
            .map_err(|_| anyhow::anyhow!("GitHub pull request lookup failed"))?;
        anyhow::ensure!(response.status().is_success(), "GitHub pull request lookup refused");
        let pulls: serde_json::Value = response
            .body_mut()
            .read_to_vec()
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or(())
            .map_err(|_| anyhow::anyhow!("Invalid pull request response"))?;
        let payload = json!({"title":request.title,"body":request.body,"base":request.base,"head":request.branch});
        let mut response =
            if let Some(number) = pulls.as_array().and_then(|a| a.first()).and_then(|p| p["number"].as_u64()) {
                let payload = json!({"title":request.title,"body":request.body,"base":request.base});
                agent
                    .patch(format!("{endpoint}/{number}"))
                    .header("authorization", format!("Bearer {placeholder}"))
                    .header("content-type", "application/json")
                    .send(serde_json::to_vec(&payload)?)
            } else {
                agent
                    .post(&endpoint)
                    .header("authorization", format!("Bearer {placeholder}"))
                    .header("content-type", "application/json")
                    .send(serde_json::to_vec(&payload)?)
            }
            .map_err(|_| anyhow::anyhow!("GitHub pull request write failed"))?;
        anyhow::ensure!(response.status().is_success(), "GitHub pull request write refused");
        let value: serde_json::Value = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_vec()
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or(())
            .map_err(|_| anyhow::anyhow!("Invalid pull request response"))?;
        let url =
            value["html_url"].as_str().ok_or_else(|| anyhow::anyhow!("GitHub did not return a pull request URL"))?;
        anyhow::ensure!(
            url.starts_with(&format!("https://github.com/{}/pull/", request.repository)),
            "Invalid pull request URL"
        );
        let pr = plan::PullRequest {
            url: url.to_string(),
            number: value["number"].as_u64().context("GitHub did not return a PR number")?,
            head: request.branch.clone(),
            base: request.base.clone(),
        };
        pr.validate().map_err(anyhow::Error::msg)?;
        Ok(pr)
    })();
    relay.revoke(&granted);
    result
}

use anyhow::Context;
impl State {
    pub(super) fn request_publish(
        &mut self,
        plan: &str,
        branch: Option<String>,
        title: String,
        body: String,
        base: String,
    ) -> Result<(), String> {
        let index = self.plans.iter().position(|p| p.id == plan).ok_or("No such plan")?;
        let p = &self.plans[index];
        if !p.is_open() || !p.approved {
            return Err("Approve an open plan first".into());
        }
        if self.publishing.pending.contains(plan)
            || p.publish.as_ref().is_some_and(|p| self.publishing.running.contains(&p.id))
        {
            return Err("Publication is already pending".into());
        }
        let repo = p.project.clone().ok_or("Plan has no project")?;
        let identity = self.project_identity(&repo);
        let branch = branch.unwrap_or_else(|| p.integration_branch());
        let plan = plan.to_string();
        self.publishing.pending.insert(plan.clone());
        let (tx, rx) = self.wake.channel();
        self.publishing.jobs.push(rx);
        std::thread::spawn(move || {
            let result = prepare(&repo, identity, branch, title, body, base).map_err(|e| e.to_string());
            let _ = tx.send(Event::Prepared { plan, result });
        });
        Ok(())
    }

    pub(super) fn decide_publish(&mut self, index: usize, id: &str, approve: bool) -> Result<(), String> {
        let plan = &self.plans[index];
        let request =
            plan.publish.as_ref().filter(|p| p.id == id).ok_or("Publish card changed; review it again")?.clone();
        if self.publishing.running.contains(id) {
            return Err("Publication is running".into());
        }
        if !approve {
            self.plans[index].publish = None;
            self.save_plan(index);
            return Ok(());
        }
        if !plan.is_open() || !plan.approved || request.url.is_some() {
            return Err("Plan cannot be published".into());
        }
        let repo = plan.project.clone().ok_or("Plan has no project")?;
        let identity = self.project_identity(&repo);
        if identity_digest(&identity) != request.identity_digest {
            return Err("Project identity changed; request publication again".into());
        }
        self.publishing.running.insert(id.to_string());
        let plan = plan.id.clone();
        let relay = self.relay.clone();
        let workspace = self.workspace.clone();
        let (tx, rx) = self.wake.channel();
        self.publishing.jobs.push(rx);
        std::thread::spawn(move || {
            let result = publish(&relay, &repo, workspace.as_deref(), &request, &identity).map_err(|e| e.to_string());
            let _ = tx.send(Event::Published { plan, request: request.id, result });
        });
        Ok(())
    }

    pub(super) fn poll_publishing(&mut self) -> bool {
        let mut changed = false;
        for rx in std::mem::take(&mut self.publishing.jobs) {
            let event = match rx.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.publishing.jobs.push(rx);
                    continue;
                }
                Err(_) => continue,
            };
            changed = true;
            match event {
                Event::Prepared { plan, result } => {
                    self.publishing.pending.remove(&plan);
                    match result {
                        Ok(request) => {
                            if let Some(index) = self.plans.iter().position(|p| p.id == plan && p.is_open()) {
                                self.plans[index].publish = Some(request);
                                self.save_plan(index);
                            }
                        }
                        Err(error) => self.toast(error),
                    }
                }
                Event::Published { plan, request, result } => {
                    self.publishing.running.remove(&request);
                    if let Some(index) = self.plans.iter().position(|p| p.id == plan) {
                        if let Some(card) = self.plans[index].publish.as_mut().filter(|p| p.id == request) {
                            match &result {
                                Ok(pr) => {
                                    card.url = Some(pr.url.clone());
                                    card.error = None;
                                }
                                Err(error) => card.error = Some(error.clone()),
                            }
                        }
                        self.save_plan(index);
                        if let Ok(pr) = result {
                            self.plans[index].pull_request = Some(pr.clone());
                            self.save_plan(index);
                            if let Err(error) = self.record_publication_stage(index, None, pr.url, self.chief_author())
                            {
                                self.toast(error);
                            }
                        }
                    }
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::with_app;
    use crate::relay::github::tests::{Server, git as fixture_git};

    fn poll(app: &mut Desktop<'_>, predicate: impl Fn(&Desktop<'_>) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !predicate(app) {
            app.runtime.poll_publishing();
            assert!(Instant::now() < deadline, "publish worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn github_publish_card_waits_for_user_click_records_url_and_checks_identity() {
        with_app(
            "app::publish::tests::github_publish_card_waits_for_user_click_records_url_and_checks_identity",
            |app, _ctx| {
                let fake = Server::start();
                app.runtime.relay = Arc::new(fake.relay());
                app.runtime.config.identity.default = Identity {
                    github: Some("account-two".into()),
                    email: Some("contributor@example.invalid".into()),
                    ..Default::default()
                };
                app.runtime.plans = vec![Plan {
                    id: "publish-plan".into(),
                    approved: true,
                    goal: "Publish tested work".into(),
                    project: Some(fake.work.clone()),
                    integration: Some("task".into()),
                    ..Default::default()
                }];
                app.runtime
                    .request_publish(
                        "publish-plan",
                        None,
                        "Task changes".into(),
                        "Full description\n\nChecks performed locally.".into(),
                        "main".into(),
                    )
                    .unwrap();
                poll(app, |a| a.runtime.plans[0].publish.is_some());
                let card = app.runtime.plans[0].publish.clone().unwrap();
                assert_eq!(card.account, "account-two");
                assert!(card.commits.contains("contributor@example.invalid"));
                assert_eq!(card.body, "Full description\n\nChecks performed locally.");
                assert!(fake.seen.lock().unwrap().is_empty(), "requesting a card does no network I/O");
                assert!(fixture_git(&fake.bare, &["branch", "--list", "task"]).is_empty());
                assert!(app.runtime.decide_publish(0, "stale-request", true).is_err());
                app.runtime.config.identity.default.github = Some("account-one".into());
                assert!(app.runtime.decide_publish(0, &card.id, true).is_err());
                app.runtime.config.identity.default.github = Some("account-two".into());
                app.runtime.decide_publish(0, &card.id, true).unwrap();
                assert!(app.runtime.decide_publish(0, &card.id, true).is_err(), "double clicks do not publish twice");
                poll(app, |a| {
                    a.runtime.plans[0].publish.as_ref().unwrap().url.is_some()
                        || a.runtime.plans[0].publish.as_ref().unwrap().error.is_some()
                });
                let card = app.runtime.plans[0].publish.as_ref().unwrap();
                assert_eq!(card.error, None);
                assert_eq!(card.url.as_deref(), Some("https://github.com/group/repo/pull/1"));
                assert!(app.runtime.plans[0].stages.iter().any(|s| s.stage == "pr_opened" && s.url == card.url));
                assert_eq!(
                    fixture_git(&fake.bare, &["rev-parse", "task"]),
                    fixture_git(&fake.work, &["rev-parse", "task"])
                );
                assert_eq!(app.runtime.relay.live_grants(), 0);
            },
        );
    }

    #[test]
    fn github_publish_snapshot_and_identity_guard_refuse_changed_or_wrong_author_work() {
        let fake = Server::start();
        let identity = Identity {
            github: Some("account-one".into()),
            email: Some("other@example.invalid".into()),
            ..Default::default()
        };
        assert!(
            prepare(&fake.work, identity.clone(), "task".into(), "Title".into(), "Description".into(), "main".into())
                .is_err()
        );
        let identity = Identity { email: Some("contributor@example.invalid".into()), ..identity };
        let request =
            prepare(&fake.work, identity.clone(), "task".into(), "Title".into(), "Description".into(), "main".into())
                .unwrap();
        fixture_git(
            &fake.work,
            &[
                "-c",
                "user.name=Contributor",
                "-c",
                "user.email=contributor@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "changed",
            ],
        );
        assert!(publish(&fake.relay(), &fake.work, None, &request, &identity).is_err());
        assert!(fake.seen.lock().unwrap().is_empty());
    }
}

impl State {
    pub(super) fn validate_reported_pull_request(
        &self,
        pane: Option<&str>,
        pr: &plan::PullRequest,
    ) -> Result<(), String> {
        pr.validate()?;
        let task = self
            .plans
            .iter()
            .flat_map(|p| &p.tasks)
            .find(|t| pane.is_some() && t.session.as_deref() == pane)
            .ok_or("A pull request report must belong to a task session")?;
        if task.branch.as_ref().is_some_and(|branch| *branch != pr.head) {
            return Err("Reported pull request head must match your task branch".into());
        }
        Ok(())
    }

    pub(super) fn report_pull_request(&mut self, pane: Option<&str>, pr: plan::PullRequest) {
        let Some((index, task)) = self.plans.iter().enumerate().find_map(|(i, p)| {
            p.tasks.iter().find(|t| pane.is_some() && t.session.as_deref() == pane).map(|t| (i, t.id.clone()))
        }) else {
            return;
        };
        let entry = self.plans[index].task_mut(&task).unwrap();
        entry.pull_request = Some(pr.clone());
        let author =
            atomics_core::tickets::Author::Agent(atomics_core::tickets::model_label(&entry.agent, &entry.args));
        self.save_plan(index);
        if let Err(error) = self.record_publication_stage(index, Some(&task), pr.url, author) {
            self.toast(error);
        }
    }
}
