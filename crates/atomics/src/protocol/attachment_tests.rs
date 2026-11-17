use super::tests::TestClient;
use super::{Commands, OpRequest, Server, ServerConfig, Terminals};
use atomics_core::{
    attachments::{Config, FileInfo, Reply, Request, disk::Inbox, encode_chunk, hash},
    projection::Projections,
    wire::{CAPABILITIES, Envelope, Message, Role, VERSION},
};
use std::sync::{Arc, Mutex};

struct Files(Mutex<Inbox>);
impl Commands for Files {
    fn submit(&self, _: OpRequest) -> bool {
        panic!("attachments never enter the mutation/tool path")
    }
    fn attachment(&self, owner: u64, request: Request, _: super::LiveGrant) -> Result<Reply, String> {
        self.0.lock().unwrap().request(owner, request)
    }
    fn attachment_disconnect(&self, owner: u64) {
        self.0.lock().unwrap().disconnect(owner);
    }
}
struct NoDiffs;
impl super::DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, atomics_core::diff::FileDiff)> {
        None
    }
}
fn connect(server: &Server, capabilities: Vec<String>) -> TestClient {
    let mut client = TestClient::connect(server.addr(), server.token());
    client.send(&Envelope::new(Message::Hello {
        client_id: "attachment-test".into(),
        versions: vec![VERSION],
        capabilities,
        prior_epoch: None,
        prior_cursor: None,
    }));
    loop {
        if matches!(client.step().unwrap().message, Message::Welcome { .. }) {
            return client;
        }
    }
}
fn request(client: &mut TestClient, request: Request) -> Reply {
    client.send(&Envelope::new(Message::Attachment(request)));
    loop {
        if let Message::AttachmentReply(reply) = client.step().unwrap().message {
            return reply;
        }
    }
}
#[test]
fn attachment_controller_without_terminal_lease_and_viewer_gates() {
    for role in [Role::Controller, Role::Viewer] {
        let home = tempfile::tempdir().unwrap();
        let files = Arc::new(Files(Mutex::new(Inbox::new(home.path(), Config::default()))));
        let server = Server::start(
            ServerConfig { role, ..Default::default() },
            Arc::new(Projections::default()),
            Arc::new(NoDiffs),
            Arc::new(Terminals::default()),
            files,
        )
        .unwrap();
        let mut client = connect(&server, CAPABILITIES.iter().map(|s| s.to_string()).collect());
        let begin = request(
            &mut client,
            Request::Begin {
                plan: None,
                files: vec![FileInfo { name: "test.txt".into(), size: 3, sha256: hash(b"abc") }],
                exchange: false,
            },
        );
        assert_eq!(begin.error.is_none(), role == Role::Controller);
        let chunk = request(
            &mut client,
            Request::Chunk { id: begin.id.clone(), file: 0, offset: 0, data: encode_chunk(b"abc") },
        );
        assert_eq!(chunk.error.is_none(), role == Role::Controller);
        let done = request(&mut client, Request::Commit { id: begin.id });
        assert_eq!(done.error.is_none(), role == Role::Controller);
        if role == Role::Controller {
            assert_eq!(std::fs::read(&done.files[0].path).unwrap(), b"abc");
        }
        let mut old =
            connect(&server, CAPABILITIES.iter().filter(|s| **s != "attachments").map(|s| s.to_string()).collect());
        assert!(request(&mut old, Request::Limits).error.is_some());
    }
}

#[test]
fn attachment_trace_is_readable_without_the_attachment_capability() {
    use atomics_core::{
        attachments::Receipt,
        trace::{TraceLog, TraceOp},
        trace_feed::TraceData,
        wire::Seq,
    };
    let home = tempfile::tempdir().unwrap();
    let projections = Arc::new(Projections::default());
    let mut log = TraceLog::default();
    let entry = log.attachment(
        "chief",
        None,
        &Receipt {
            id: Default::default(),
            at: 0,
            file: FileInfo { name: "input.txt".into(), size: 3, sha256: hash(b"abc") },
            path: "/host/input.txt".into(),
        },
        1,
    );
    projections.trace.publish(TraceData { entries: vec![entry], ..Default::default() });
    let server = Server::start(
        ServerConfig::default(),
        projections,
        Arc::new(NoDiffs),
        Arc::new(Terminals::default()),
        Arc::new(Files(Mutex::new(Inbox::new(home.path(), Config::default())))),
    )
    .unwrap();
    let mut client =
        connect(&server, CAPABILITIES.iter().filter(|s| **s != "attachments").map(|s| s.to_string()).collect());
    client.send(&Envelope::new(Message::TraceGet { revision: Seq(1), offset: 0, limit: 10 }));
    loop {
        if let Message::TracePage(page) = client.step().unwrap().message {
            assert_eq!(page.data.entries[0].op, TraceOp::Other);
            assert!(page.data.entries[0].summary.contains("input.txt"));
            break;
        }
    }
}
