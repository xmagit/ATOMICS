//! What a board or a chat produced: the pull requests and links that came out of it, each with the task it came from.
//! They are claims of the agents', recorded as data; opening one is the user's.

use serde::{Deserialize, Serialize};

/// How many of them one board or chat keeps.
pub const MAX: usize = 100;
/// How many one report names.
pub const MAX_REPORTED: usize = 20;
const MAX_URL: usize = 2048;
const MAX_TITLE: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The task it came from; none for what the chief made itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// When it was recorded, in seconds.
    #[serde(default)]
    pub at: u64,
}

/// Something a session's own tool made, as its hook saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Made {
    pub url: String,
    /// The branch it was made from, where the command names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl Output {
    /// An address the user can open, with no credentials in it, and a title that fits a line.
    pub fn checked(url: &str, title: &str) -> Result<Output, String> {
        let url = url.trim();
        let rest = url.strip_prefix("https://").ok_or("An output is an https address")?;
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if url.len() > MAX_URL
            || host.is_empty()
            || host.contains('@')
            || url.chars().any(|c| c.is_control() || c == ' ')
        {
            return Err("An output is an https address without credentials".into());
        }
        let title: String = title.trim().chars().filter(|c| !c.is_control()).take(MAX_TITLE).collect();
        Ok(Output { url: url.into(), title, ..Output::default() })
    }

    /// The repository and number of a pull request.
    pub fn pull_request(&self) -> Option<(&str, u64)> {
        pull_request(&self.url)
    }

    /// What it is called in a list.
    pub fn name(&self) -> String {
        match self.pull_request() {
            Some((repo, number)) => format!("{} #{number}", repo.rsplit('/').next().unwrap_or(repo)),
            None if !self.title.is_empty() => self.title.clone(),
            None => self.url.trim_start_matches("https://").trim_end_matches('/').into(),
        }
    }
}

/// The repository and number of the GitHub pull request at `url`, exactly.
pub fn pull_request(url: &str) -> Option<(&str, u64)> {
    let (repo, number) = url.strip_prefix("https://github.com/")?.split_once("/pull/")?;
    let part = |part: &str| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let (owner, name) = repo.split_once('/')?;
    let number = number.parse::<u64>().ok().filter(|n| *n > 0 && n.to_string() == number)?;
    (url.len() <= MAX_URL && part(owner) && part(name)).then_some((repo, number))
}

/// Adds `output` where its address is not there yet; whether it did.
pub fn add(list: &mut Vec<Output>, output: Output) -> bool {
    if list.len() >= MAX || list.iter().any(|known| known.url == output.url) {
        return false;
    }
    list.push(output);
    true
}

/// The pull request a tool call made: from the answer of `gh pr create`, or of a tool that creates one. A pull request
/// that is only mentioned is none.
pub fn made(tool: &str, input: &serde_json::Value, response: &serde_json::Value) -> Option<Made> {
    let command = input.get("command").or_else(|| input.get("cmd")).map(|command| match command {
        serde_json::Value::Array(words) => words.iter().filter_map(|w| w.as_str()).collect::<Vec<_>>().join(" "),
        other => other.as_str().unwrap_or_default().to_owned(),
    });
    let (text, head) = match command {
        Some(command) => {
            // A shell's quotes do not matter for the words looked for.
            let plain = command.replace(['"', '\'', '`', ';', '(', ')'], " ");
            let words: Vec<&str> = plain.split_whitespace().collect();
            let call = words.split(|word| matches!(*word, "&&" | "||" | "|" | "&")).find_map(|words| {
                let gh = words.iter().position(|word| *word == "gh" || word.ends_with("/gh"))?;
                words[gh..].windows(2).any(|w| w == ["pr", "create"]).then_some(&words[gh..])
            })?;
            (output_text(response), flag(call, &["--head", "-H"]))
        }
        None if tool.ends_with("create_pull_request") => {
            (output_text(response), input.get("head").and_then(|h| h.as_str()).map(str::to_owned))
        }
        None => return None,
    };
    let url = text
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ',' | '<' | '>' | '(' | ')' | '`'))
        .rfind(|word| pull_request(word).is_some())?;
    let head = head.filter(|head| head.len() <= 255 && !head.chars().any(char::is_control));
    Some(Made { url: url.into(), head })
}

fn flag(words: &[&str], names: &[&str]) -> Option<String> {
    words.iter().enumerate().find_map(|(i, word)| {
        if names.contains(word) {
            return words.get(i + 1).map(|value| value.to_string());
        }
        let (name, value) = word.split_once('=')?;
        names.contains(&name).then(|| value.to_owned())
    })
}

/// A tool's answer as text, bounded: providers put it in a string, a field or a list of parts.
fn output_text(response: &serde_json::Value) -> String {
    const ENOUGH: usize = 64 * 1024;
    fn read(value: &serde_json::Value, into: &mut String) {
        match value {
            serde_json::Value::String(text) if into.len() < ENOUGH => {
                into.push(' ');
                // The end holds what a command printed last.
                let mut from = text.len().saturating_sub(ENOUGH);
                while !text.is_char_boundary(from) {
                    from += 1;
                }
                into.push_str(&text[from..]);
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| read(item, into)),
            serde_json::Value::Object(fields) => {
                fields.iter().filter(|(key, _)| *key != "stderr").for_each(|(_, value)| read(value, into))
            }
            _ => {}
        }
    }
    let mut text = String::new();
    read(response, &mut text);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const URL: &str = "https://github.com/o/r/pull/12";

    #[test]
    fn a_pull_request_is_what_a_command_made_and_not_what_it_mentions() {
        let bash = |command: &str, response: serde_json::Value| made("Bash", &json!({"command":command}), &response);
        let out = json!({"stdout": format!("Warning: 1 uncommitted change\n{URL}\n"), "stderr": ""});
        let one = Some(Made { url: URL.into(), head: None });
        assert_eq!(bash("gh pr create --fill", out.clone()), one);
        assert_eq!(bash("cd /w/repo && gh  pr create --title 'A' --body B", json!(URL)), one);
        assert_eq!(bash("GH_TOKEN=x /usr/bin/gh -R o/r pr create --fill | tail -1", json!(URL)), one);
        assert_eq!(
            bash("gh pr create --head \"atomics/t1\" --base main", out.clone()),
            Some(Made { url: URL.into(), head: Some("atomics/t1".into()) })
        );
        assert_eq!(bash("gh pr create -H t2", out.clone()).unwrap().head.as_deref(), Some("t2"));
        assert_eq!(bash("gh pr create --head=t3", out.clone()).unwrap().head.as_deref(), Some("t3"));
        // Codex names its command as words, and answers with parts.
        let words = json!({"command": ["bash", "-lc", "gh pr create --fill"]});
        assert_eq!(made("shell", &words, &json!([{"type":"text", "text": URL}])), one);
        assert_eq!(bash("bash -lc \"gh pr create --fill\"", out.clone()), one);
        let tool = json!({"content":[{"type":"text", "text": format!("{{\"html_url\":\"{URL}\"}}")}]});
        assert_eq!(made("mcp__github__create_pull_request", &json!({"title":"A"}), &tool), one);
        for (command, response) in [
            ("gh pr view 12", out.clone()),
            ("gh pr list && echo pr create", out.clone()),
            ("cat notes.md", out.clone()),
            ("gh pr create --fill", json!({"stdout":"", "stderr": format!("a pull request already exists: {URL}")})),
            ("gh pr create --fill", json!({"stdout":"https://github.com/o/r/pull/12/files"})),
            ("gh pr create --fill", json!(null)),
        ] {
            assert_eq!(bash(command, response.clone()), None, "{command} {response}");
        }
        assert_eq!(made("mcp__github__get_pull_request", &json!({}), &tool), None);
    }

    #[test]
    fn an_output_is_an_address_without_credentials_and_is_listed_once() {
        for bad in ["http://a.example/x", "javascript:alert(1)", "https://u:p@a.example/", "https://", "https://a b/"] {
            assert!(Output::checked(bad, "").is_err(), "{bad}");
        }
        let link = Output::checked(" https://a.example/report ", " The\u{7} report ").unwrap();
        assert_eq!((link.url.as_str(), link.title.as_str()), ("https://a.example/report", "The report"));
        assert_eq!((link.name().as_str(), link.pull_request()), ("The report", None));
        let pull = Output::checked(URL, "").unwrap();
        assert_eq!((pull.name().as_str(), pull.pull_request()), ("r #12", Some(("o/r", 12))));
        assert_eq!(Output::checked("https://a.example/x/", "").unwrap().name(), "a.example/x");
        let mut list = vec![];
        assert!(add(&mut list, pull.clone()) && !add(&mut list, pull) && add(&mut list, link));
        for n in 0..MAX {
            add(&mut list, Output { url: format!("https://a.example/{n}"), ..Output::default() });
        }
        assert_eq!(list.len(), MAX);
        for other in ["https://github.com/o/r/pull/0", "https://github.com/o/r/pull/012", "https://github.com/o/pull/1"]
        {
            assert_eq!(pull_request(other), None, "{other}");
        }
    }
}
