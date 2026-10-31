#[test]
fn ci_job_has_a_fifteen_minute_timeout() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    let job = workflow.split_once("\n  check:\n").unwrap().1.split_once("\n    steps:\n").unwrap().0;
    assert!(job.lines().any(|line| line == "    timeout-minutes: 15"));
}

#[test]
fn readme_release_sentence_refers_to_plain_messages() {
    let readme = include_str!("../../../README.md");
    let paragraph = readme
        .split_once("While a permission request is pending, `atomics_send` holds plain messages")
        .unwrap()
        .1
        .split("\n\n")
        .next()
        .unwrap();
    assert!(paragraph.find("It releases these messages").unwrap() < paragraph.find("Raw input").unwrap());
}
