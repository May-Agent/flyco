//! The generic ACP driver, end to end against a stand-in agent.
//!
//! `fake-acp-agent.py` speaks the ACP subset flycod uses, so this exercises
//! handshake, session open, a turn, permission routing, plan usage, and
//! interrupt without a real agent — and the `[acp]` table is the whole
//! configuration, which is what makes one test file cover every agent the
//! driver could run.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::{HarnessEvent, PermissionMode};
use flyco_daemon::config::{AcpConfig, AcpMethodCall, AcpMethods, AcpMode};
use flyco_daemon::harness::acp::AcpHarness;
use flyco_daemon::harness::{Harness as _, HarnessSession as _, SessionOutput, StartRequest};
use flyco_daemon::mount::{FlycoServer, Mount};
use tokio::sync::mpsc;

/// The MCP servers a test session is given.
///
/// Only flyco's own, and it is never launched: the stand-in agent reports
/// the mount rather than performing it. What the driver does with the
/// report is what these tests are about.
fn mount() -> Mount {
    Mount::new(
        FlycoServer::of_socket(Path::new("/var/lib/flyco/run/mcp.sock"))
            .expect("this test binary has a path"),
        Vec::new(),
        false,
    )
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "flycod-acp-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self(path)
    }

    fn join(&self, tail: &str) -> PathBuf {
        self.0.join(tail)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn script(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    let mut permissions = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make the script executable");
    path
}

/// The `[acp]` table the tests run under, as a provisioner would write it:
/// every extension method named explicitly, so what is exercised is the
/// configuration-driven path rather than a special case.
fn config() -> AcpConfig {
    AcpConfig {
        agent: "fake".to_owned(),
        in_process: None,
        program: script("fake-acp-agent.py"),
        args: Vec::new(),
        auth_method: None,
        mcp_via_files: false,
        env: BTreeMap::new(),
        files: Vec::new(),
        model: None,
        effort: None,
        model_option: "model".to_owned(),
        effort_option: Some("reasoning_effort".to_owned()),
        fused_effort_tails: Vec::new(),
        permission_mode: PermissionMode::Auto,
        modes: BTreeMap::from([(
            PermissionMode::Auto,
            AcpMode {
                set_mode: Some("agent".to_owned()),
                options: Vec::new(),
            },
        )]),
        methods: AcpMethods {
            compact: Some(AcpMethodCall {
                call: "_test/compact".to_owned(),
                params: serde_json::json!({"threadId": "<session>"}),
            }),
            usage: Some(AcpMethodCall {
                call: "_test/rate_limits".to_owned(),
                params: serde_json::Value::Null,
            }),
            mcp_status: Some(AcpMethodCall {
                call: "_test/mcp_status".to_owned(),
                params: serde_json::Value::Null,
            }),
        },
        tui: None,
    }
}

async fn start(
    scratch: &Scratch,
) -> (
    flyco_daemon::harness::acp::AcpSession,
    mpsc::Receiver<SessionOutput>,
) {
    start_with(scratch, None).await
}

async fn start_with(
    scratch: &Scratch,
    resume_session_id: Option<String>,
) -> (
    flyco_daemon::harness::acp::AcpSession,
    mpsc::Receiver<SessionOutput>,
) {
    let harness = AcpHarness::new(config(), mount());
    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id,
            additional_directories: Vec::new(),
        })
        .await
        .expect("the driver must start against the stand-in agent");
    (started.session, started.outputs)
}

#[tokio::test]
async fn an_agent_with_auth_method_mcp_via_files_and_additional_directories_starts() {
    let scratch = Scratch::new("requireauth-mcpviafiles-nohttp-requireextradirs");
    let mut cfg = config();
    cfg.auth_method = Some("gemini-api-key".to_owned());
    cfg.mcp_via_files = true;
    let extra = scratch.join("extra-repo");
    std::fs::create_dir_all(&extra).expect("create extra dir");

    let mount = Mount::new(
        FlycoServer::of_socket(Path::new("/var/lib/flyco/run/mcp.sock"))
            .expect("this test binary has a path"),
        vec![flyco_core::McpServerMount {
            name: "deepwiki".to_owned(),
            config: flyco_core::McpServerConfig::Http {
                url: "https://mcp.deepwiki.com/mcp".to_owned(),
                headers: Vec::new(),
            },
        }],
        false,
    );
    let harness = AcpHarness::new(cfg, mount);
    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
            additional_directories: vec![extra],
        })
        .await
        .expect("authenticate, mcp_via_files, and additionalDirectories must all succeed");
    let mut outputs = started.outputs;
    let _ = next(&mut outputs, "started").await;
    let SessionOutput::Capabilities { capabilities } = next(&mut outputs, "capabilities").await
    else {
        panic!("expected capabilities");
    };
    assert!(
        capabilities
            .iter()
            .any(|cap| cap == "acp:additional_directories"),
        "missing acp:additional_directories in {capabilities:?}"
    );
    started.session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_session_opened_without_flycos_tools_never_becomes_a_session() {
    // The scratch's name is what tells the stand-in agent to report a
    // flyco server with `machine_status` missing. The mount report is
    // asked for before the session is allowed to produce anything, so the
    // failure is `start` refusing, and no session exists to fail.
    let scratch = Scratch::new("unmounted");
    let harness = AcpHarness::new(config(), mount());
    let error = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
            additional_directories: Vec::new(),
        })
        .await
        .expect_err("a session whose agent cannot read its budget must not open");
    let said = error.to_string();
    assert!(
        said.contains("machine_status"),
        "the refusal must name the missing tool: {said}"
    );
}

async fn next(outputs: &mut mpsc::Receiver<SessionOutput>, what: &str) -> SessionOutput {
    tokio::time::timeout(std::time::Duration::from_secs(10), outputs.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the session ended before {what}"))
}

/// Steps past the announcements every session opens with.
///
/// A session is identified, its capabilities, models and commands are
/// announced, and its plan's limits are read, before any turn — so a test
/// about turns walks past all five rather than restating them.
/// `a_session_announces_itself_without_anyone_typing` is where their
/// content is pinned.
async fn announcements(outputs: &mut mpsc::Receiver<SessionOutput>) {
    let _ = next(outputs, "started").await;
    let _ = next(outputs, "capabilities").await;
    let _ = next(outputs, "models").await;
    let _ = next(outputs, "commands").await;
}

#[tokio::test]
async fn a_session_announces_itself_without_anyone_typing() {
    let scratch = Scratch::new("announce");
    let (session, mut outputs) = start(&scratch).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        }
    );

    let SessionOutput::Capabilities { capabilities } = next(&mut outputs, "capabilities").await
    else {
        panic!("a session must announce what its agent negotiated");
    };
    for token in [
        "acp",
        "acp:load_session",
        "acp:resume",
        "acp:close",
        "acp:mcp_http",
    ] {
        assert!(
            capabilities.iter().any(|c| c == token),
            "{token} missing from {capabilities:?}"
        );
    }

    // And what it can run on, before a single turn: the composer's picker
    // has to be right for the first message.
    let SessionOutput::Models { models } = next(&mut outputs, "models").await else {
        panic!("a session must announce the models its harness offers");
    };
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.6-terra", "gpt-5.6-luna"],
    );
    let default = models
        .iter()
        .find(|model| model.is_default)
        .expect("the agent names a current model");
    assert_eq!(default.id, "gpt-5.6-terra");
    assert_eq!(default.default_effort.as_deref(), Some("medium"));
    assert_eq!(default.efforts, ["low", "medium", "high"]);

    let SessionOutput::Commands { commands } = next(&mut outputs, "commands").await else {
        panic!("a session must announce the commands its harness offers");
    };
    assert_eq!(
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["cloudflare"]
    );
    assert_eq!(commands[0].argument_hint, None);
    assert!(
        commands[0]
            .description
            .starts_with("Comprehensive Cloudflare")
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_resuming_session_recovers_its_native_id() {
    // The id the session was opened under is the one a later machine
    // resumes by; the driver names it on `session/resume` and the agent's
    // answer carries the mode and options state the open announced.
    let scratch = Scratch::new("resume");
    let (session, mut outputs) = start_with(&scratch, Some("a-native-session-id".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "a-native-session-id".to_owned(),
        }
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_load_the_agent_rejects_opens_fresh_and_says_so() {
    // `session/load` is what a resumed machine asks for; when the agent
    // refuses it, the session is the restored workspace plus a fresh
    // conversation — not a startup failure — and the room is told the
    // context did not carry over.
    let scratch = Scratch::new("refuseload");
    let (session, mut outputs) = start_with(&scratch, Some("a-lost-session".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        },
        "a fresh conversation carries its own id, not the rejected one"
    );
    let _ = next(&mut outputs, "capabilities").await;
    let _ = next(&mut outputs, "models").await;
    let _ = next(&mut outputs, "commands").await;
    let SessionOutput::Event {
        event: HarnessEvent::LocalCommandOutput { content },
    } = next(&mut outputs, "the restart notice").await
    else {
        panic!("a refused continuation must say so in the room");
    };
    assert!(content.contains("session/load"), "{content}");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_agent_that_cannot_continue_starts_fresh_and_says_so() {
    // No `session/resume`, no `session/load`: the conversation cannot be
    // continued, and an unresumable session is still not a dead one — the
    // work in the checkout is the part that mattered.
    let scratch = Scratch::new("continuless");
    let (session, mut outputs) = start_with(&scratch, Some("a-lost-session".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        }
    );
    let _ = next(&mut outputs, "capabilities").await;
    let _ = next(&mut outputs, "models").await;
    let _ = next(&mut outputs, "commands").await;
    let SessionOutput::Event {
        event: HarnessEvent::LocalCommandOutput { content },
    } = next(&mut outputs, "the restart notice").await
    else {
        panic!("an uncontinuable resume must say so in the room");
    };
    assert!(content.contains("neither resume nor load"), "{content}");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_palette_update_mid_session_re_lists_the_commands() {
    // The scratch's name is what tells the stand-in agent to push an
    // `available_commands_update` when a turn opens: the palette the
    // browser holds has to be re-rendered rather than left stale.
    let scratch = Scratch::new("skillsreload");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");

    // The palette update races the turn's own frames, so this reads
    // forward to it rather than assuming it lands first.
    let mut relisted = None;
    while relisted.is_none() {
        if let SessionOutput::Commands { commands } =
            next(&mut outputs, "the new command list").await
        {
            relisted = Some(commands);
        }
    }
    let commands = relisted.expect("checked above");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["cloudflare", "release"],
        "the agent's new palette is the one announced"
    );
    assert_eq!(commands[1].argument_hint.as_deref(), Some("version"));

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_turn_runs_from_user_message_to_completion() {
    let scratch = Scratch::new("turn");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");

    let SessionOutput::Event {
        event: HarnessEvent::TurnStarted { turn_id },
    } = next(&mut outputs, "turn_started").await
    else {
        panic!("a user message must open a turn");
    };

    assert_eq!(
        next(&mut outputs, "assistant_delta").await,
        SessionOutput::Event {
            event: HarnessEvent::AssistantDelta {
                turn_id: turn_id.clone(),
                text: "hello".to_owned(),
            },
        }
    );

    let SessionOutput::ApprovalRequest { id, tool, .. } =
        next(&mut outputs, "approval_request").await
    else {
        panic!("the tool call must reach flyco's approval UI");
    };
    assert_eq!(tool, "execute");

    session
        .decide_approval(flyco_daemon::harness::ToolApproval::Allow {
            id,
            updated_input: None,
        })
        .await
        .expect("decide the approval");

    let SessionOutput::Event {
        event: HarnessEvent::TurnCompleted {
            turn_id: done,
            usage,
        },
    } = next(&mut outputs, "turn_completed").await
    else {
        panic!("the result must close the turn");
    };
    assert_eq!(done, turn_id);
    assert_eq!(
        usage
            .context
            .expect("the agent reported a window")
            .size_tokens,
        200_000
    );
    assert_eq!(usage.context.expect("checked above").used_tokens, 18);

    // The window the turn left behind, reported unasked: a session whose
    // machine is suspended later still says where its context went,
    // instead of offering to wake one to find out.
    let SessionOutput::Event {
        event: HarnessEvent::ContextUsage { usage: context },
    } = next(&mut outputs, "context usage").await
    else {
        panic!("a finished turn must state what the window holds");
    };
    assert_eq!(
        context
            .window
            .expect("the agent reported a window")
            .size_tokens,
        200_000
    );

    // The turn spent the five-hour window. The reading itself is filed
    // nowhere — the control plane reads the plan from the vendor while it
    // draws it — and what the driver still owes the conversation is the
    // limit: the session is stopped until the window turns over, and the
    // transcript has to say which limit stopped it.
    let SessionOutput::Event {
        event: HarnessEvent::UsageLimited { window },
    } = next(&mut outputs, "usage limited").await
    else {
        panic!("a spent window must reach the conversation");
    };
    assert_eq!(window.label, "5-hour (primary)");
    assert_eq!(window.used_percent, 100);
    assert_eq!(window.resets_at_unix, Some(1_789_002_000));

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_message_sent_mid_turn_waits_and_opens_the_next_turn() {
    // ACP refuses a second `session/prompt` while a turn runs, so the
    // driver queues what arrives during one — a follow-up from the user,
    // an injected notice — and opens it when the running turn closes. A
    // refusal there used to be fatal: it killed the session.
    let scratch = Scratch::new("queued");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("first".to_owned())
        .await
        .expect("send the first message");
    let SessionOutput::Event {
        event: HarnessEvent::TurnStarted { turn_id: first },
    } = next(&mut outputs, "the first turn").await
    else {
        panic!("the first message must open a turn");
    };

    // The approval request is the turn's still-open marker: the prompt has
    // not resolved, so this message lands mid-turn.
    let _ = next(&mut outputs, "assistant_delta").await;
    let SessionOutput::ApprovalRequest { id, .. } = next(&mut outputs, "approval_request").await
    else {
        panic!("the first turn must be waiting on its approval");
    };
    session
        .send_user_message("second".to_owned())
        .await
        .expect("a mid-turn message is accepted, not fatal");

    session
        .decide_approval(flyco_daemon::harness::ToolApproval::Allow {
            id,
            updated_input: None,
        })
        .await
        .expect("decide the first turn's approval");

    // The queued message opens a turn of its own once the first closes.
    // Plan usage and the second TurnStarted are emitted from different
    // tasks, so read past whichever lands first.
    let mut second_turn = None;
    for _ in 0..4 {
        if let SessionOutput::Event {
            event: HarnessEvent::TurnStarted { turn_id },
        } = next(&mut outputs, "the queued message's turn").await
        {
            second_turn = Some(turn_id);
            break;
        }
    }
    let second = second_turn.expect("the queued message must open its own turn");
    assert_ne!(first, second, "each message opens a distinct turn");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_interrupted_turn_completes_as_cancelled() {
    // ACP's interrupt is `session/cancel` plus the prompt resolving with
    // `stopReason: "cancelled"` — a cancelled turn is a completed one in
    // the protocol's vocabulary, and the transcript closes it rather than
    // leaving it open.
    let scratch = Scratch::new("interrupt");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");
    let _ = next(&mut outputs, "turn_started").await;
    let _ = next(&mut outputs, "assistant_delta").await;
    let _ = next(&mut outputs, "approval_request").await;

    session.interrupt().await.expect("interrupt the turn");

    let SessionOutput::Event {
        event: HarnessEvent::TurnCompleted { .. },
    } = next(&mut outputs, "turn_completed").await
    else {
        panic!("an interrupt must close the turn");
    };

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn manual_compaction_uses_the_configured_method_and_reports_completion() {
    let scratch = Scratch::new("compact");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session.compact().await.expect("compact the session");
    assert_eq!(
        next(&mut outputs, "context_compacted").await,
        SessionOutput::Event {
            event: HarnessEvent::ContextCompacted,
        }
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn interrupt_clears_queued_messages_so_none_start_after_cancel() {
    let scratch = Scratch::new("interrupt-clear");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("first".to_owned())
        .await
        .expect("send first message");
    let _ = next(&mut outputs, "turn_started").await;
    let _ = next(&mut outputs, "assistant_delta").await;
    let _ = next(&mut outputs, "approval_request").await;

    // Queue a follow-up while the first turn is blocked on approval, then interrupt.
    session
        .send_user_message("should be discarded".to_owned())
        .await
        .expect("queue follow-up");
    session.interrupt().await.expect("interrupt");

    let SessionOutput::Event {
        event: HarnessEvent::TurnCompleted { .. },
    } = next(&mut outputs, "turn_completed").await
    else {
        panic!("interrupt must close the active turn");
    };
    // Drain the post-turn ContextUsage event.
    let _ = next(&mut outputs, "context_usage").await;

    // Confirm no second TurnStarted arrives after the interrupt.
    let trailing = tokio::time::timeout(std::time::Duration::from_millis(250), async {
        while let Some(output) = outputs.recv().await {
            if matches!(
                output,
                SessionOutput::Event {
                    event: HarnessEvent::TurnStarted { .. }
                }
            ) {
                return Some(output);
            }
        }
        None
    })
    .await;
    assert!(
        trailing.is_err() || trailing.expect("checked").is_none(),
        "queued messages must be cleared when a turn is interrupted"
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn unexpected_agent_exit_emits_fatal() {
    let scratch = Scratch::new("crash");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("trigger crash".to_owned())
        .await
        .expect("send message");

    let mut saw_fatal = false;
    while let Some(output) = tokio::time::timeout(std::time::Duration::from_secs(5), outputs.recv())
        .await
        .expect("timed out waiting for fatal")
    {
        if let SessionOutput::Fatal { error } = output {
            assert!(
                error.contains("closed unexpectedly"),
                "unexpected fatal message: {error}"
            );
            saw_fatal = true;
            break;
        }
    }
    assert!(
        saw_fatal,
        "unexpected agent crash must emit SessionOutput::Fatal"
    );
}

mod in_process {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use claude_acp::ConnectedSession;
    use claude_acp_sdk::{NdjsonReader, NdjsonWriter, SdkError, TranscriptSync};
    use codex_acp::ConnectedAppServer;
    use flyco_core::{HarnessEvent, PermissionMode};
    use flyco_daemon::config::{
        AcpConfig, AcpMethodCall, AcpMethods, AcpMode, AcpOptionWrite, ClaudeAuth, ClaudeConfig,
        InProcessAgent,
    };
    use flyco_daemon::harness::acp::AcpHarness;
    use flyco_daemon::harness::{Harness as _, HarnessSession as _, SessionOutput, StartRequest};
    use futures_lite::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::{Scratch, mount, next};

    struct PipeRead {
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
        buf: VecDeque<u8>,
    }

    struct PipeWrite(Option<mpsc::UnboundedSender<Vec<u8>>>);

    impl AsyncRead for PipeRead {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.buf.is_empty() {
                match self.rx.poll_recv(cx) {
                    Poll::Ready(Some(chunk)) => self.buf.extend(chunk),
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            let count = out.len().min(self.buf.len());
            for (dst, src) in out.iter_mut().zip(self.buf.drain(..count)) {
                *dst = src;
            }
            Poll::Ready(Ok(count))
        }
    }

    impl AsyncWrite for PipeWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let Some(tx) = &self.0 else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pipe closed",
                )));
            };
            tx.send(buf.to_vec())
                .map(|()| buf.len())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed"))
                .into()
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.0 = None;
            Poll::Ready(Ok(()))
        }
    }

    fn pipe_pair() -> (PipeRead, PipeWrite, PipeRead, PipeWrite) {
        let (a_tx, a_rx) = mpsc::unbounded_channel();
        let (b_tx, b_rx) = mpsc::unbounded_channel();
        (
            PipeRead {
                rx: a_rx,
                buf: VecDeque::new(),
            },
            PipeWrite(Some(b_tx)),
            PipeRead {
                rx: b_rx,
                buf: VecDeque::new(),
            },
            PipeWrite(Some(a_tx)),
        )
    }

    #[derive(Clone, Default)]
    struct CountingSync {
        pulls: Arc<AtomicUsize>,
        pushes: Arc<AtomicUsize>,
    }

    impl TranscriptSync for CountingSync {
        fn pull(
            &self,
            _session_id: &str,
            _cwd: &Path,
            _home: &Path,
        ) -> impl std::future::Future<Output = Result<(), SdkError>> + Send {
            self.pulls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        }

        fn push(
            &self,
            _session_id: &str,
            _cwd: &Path,
            _home: &Path,
        ) -> impl std::future::Future<Output = Result<(), SdkError>> + Send {
            self.pushes.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end test drives mock CLI, all 6 modes, extensions, and transcript sync"
    )]
    async fn in_process_claude_supports_extensions_all_modes_and_transcript_sync() {
        let scratch = Scratch::new("claude-in-process");
        let sync = CountingSync::default();
        let seen_modes = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_modes_task = Arc::clone(&seen_modes);

        let (sdk_r, sdk_w, cli_r, cli_w) = pipe_pair();
        let slot = Arc::new(Mutex::new(Some((sdk_r, sdk_w))));

        // Mock native Claude CLI speaking the bidirectional stream-json protocol.
        tokio::spawn(async move {
            let mut reader = NdjsonReader::new(cli_r);
            let mut writer = NdjsonWriter::new(cli_w);
            while let Ok(Some(line)) = reader.next_line().await {
                let msg: Value = serde_json::from_str(&line).expect("valid JSON");
                match msg.get("type").and_then(Value::as_str) {
                    Some("control_request") => {
                        let req_id = msg["request_id"].clone();
                        let subtype = msg["request"]["subtype"].as_str().unwrap_or("");
                        let body = match subtype {
                            "initialize" => json!({
                                "commands": [
                                    {"name": "review", "description": "Review changes", "argumentHint": ""}
                                ],
                                "agents": [],
                                "models": [
                                    {"value": "sonnet", "displayName": "Sonnet"},
                                    {"value": "opus", "displayName": "Opus"}
                                ]
                            }),
                            "set_permission_mode" => {
                                if let Some(mode) = msg["request"]["mode"].as_str() {
                                    seen_modes_task.lock().expect("modes").push(mode.to_owned());
                                }
                                json!({})
                            }
                            "mcp_status" => json!({
                                "mcpServers": [{
                                    "name": "flyco",
                                    "status": "connected",
                                    "tools": [
                                        {"name": "machine_status"},
                                        {"name": "budget_status"},
                                        {"name": "machine_resize"}
                                    ]
                                }]
                            }),
                            "get_context_usage" => json!({
                                "model": "sonnet",
                                "totalTokens": 1200,
                                "maxTokens": 200_000,
                                "rawMaxTokens": 200_000,
                                "percentage": 0.6,
                                "isAutoCompactEnabled": true,
                                "autoCompactThreshold": 160_000,
                                "categories": [{"name": "Messages", "tokens": 1200}],
                                "mcpTools": [{"name": "machine_status", "tokens": 100, "isLoaded": true}],
                                "memoryFiles": [],
                                "agents": [],
                                "skills": {"skillFrontmatter": []}
                            }),
                            _ => json!({}),
                        };
                        let _ = writer
                            .send_json(&json!({
                                "type": "control_response",
                                "response": {
                                    "subtype": "success",
                                    "request_id": req_id,
                                    "response": body
                                }
                            }))
                            .await;
                    }
                    Some("user") => {
                        let _ = writer
                            .send_json(&json!({
                                "type": "rate_limit_event",
                                "session_id": "claude-s1",
                                "uuid": "rl-1",
                                "rate_limit_info": {
                                    "status": "rejected",
                                    "resetsAt": 1_789_002_000,
                                    "rateLimitType": "five_hour",
                                    "utilization": 1.0
                                }
                            }))
                            .await;
                        let _ = writer
                            .send_json(&json!({
                                "type": "assistant",
                                "session_id": "claude-s1",
                                "message": {
                                    "id": "msg_1",
                                    "role": "assistant",
                                    "model": "sonnet",
                                    "content": [{"type": "text", "text": "hello from claude"}]
                                }
                            }))
                            .await;
                        let _ = writer
                            .send_json(&json!({
                                "type": "result",
                                "subtype": "success",
                                "is_error": false,
                                "duration_ms": 5,
                                "duration_api_ms": 4,
                                "num_turns": 1,
                                "result": "hello from claude",
                                "session_id": "claude-s1",
                                "total_cost_usd": 0.01,
                                "usage": {
                                    "input_tokens": 50,
                                    "output_tokens": 10
                                }
                            }))
                            .await;
                    }
                    _ => {}
                }
            }
        });

        let claude_cfg = ClaudeConfig {
            claude_path: None,
            model: None,
            effort: None,
            permission_mode: PermissionMode::Auto,
            auth: ClaudeAuth::Inherit,
            managed_dir: None,
        };
        let harness = AcpHarness::claude(
            claude_cfg,
            mount(),
            scratch.join("transcripts"),
            sync.clone(),
        )
        .with_claude_connector(move |_spawn, handler| {
            let pair = slot.lock().expect("slot").take();
            async move {
                let (r, w) = pair.expect("connector called once");
                Ok(ConnectedSession::attach(r, w, handler))
            }
        });

        let started = harness
            .start(StartRequest {
                workdir: scratch.join("work"),
                resume_session_id: None,
                additional_directories: Vec::new(),
            })
            .await
            .expect("in-process Claude starts");
        let session = started.session;
        let mut outputs = started.outputs;

        // Exercise every flyco PermissionMode mapping on Claude.
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
            PermissionMode::DontAsk,
            PermissionMode::BypassPermissions,
        ] {
            session
                .set_permission_mode(mode)
                .await
                .unwrap_or_else(|e| panic!("mode {mode:?} must succeed: {e}"));
        }
        {
            let recorded = seen_modes.lock().expect("modes").clone();
            for expected in [
                "default",
                "acceptEdits",
                "plan",
                "auto",
                "dontAsk",
                "bypassPermissions",
            ] {
                assert!(
                    recorded.iter().any(|m| m == expected),
                    "expected Claude CLI mode `{expected}` in {recorded:?}"
                );
            }
        }

        // Run a turn and verify assistant text, rich _claude/context_usage,
        // and _claude/rate_limit notification handling.
        session
            .send_user_message("hello".to_owned())
            .await
            .expect("send prompt");

        let mut saw_text = false;
        let mut saw_context = false;
        let mut saw_rate_limit = false;
        let mut seen = Vec::new();
        while let Ok(Some(output)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), outputs.recv()).await
        {
            match &output {
                SessionOutput::Event {
                    event: HarnessEvent::AssistantDelta { text, .. },
                } if text.contains("hello from claude") => saw_text = true,
                SessionOutput::Event {
                    event: HarnessEvent::ContextUsage { usage },
                } if !usage.categories.is_empty() && !usage.mcp_tools.is_empty() => {
                    saw_context = true;
                }
                SessionOutput::Event {
                    event: HarnessEvent::UsageLimited { window },
                } if window.used_percent == 100 => {
                    saw_rate_limit = true;
                }
                _ => {}
            }
            seen.push(output);
            if saw_text && saw_context && saw_rate_limit {
                break;
            }
        }
        assert!(
            saw_text,
            "expected assistant delta from in-process Claude; seen={seen:#?}"
        );
        assert!(
            saw_context,
            "expected rich _claude/context_usage payload; seen={seen:#?}"
        );
        assert!(
            saw_rate_limit,
            "expected _claude/rate_limit notification; seen={seen:#?}"
        );

        session.flush().await.expect("flush pushes transcripts");
        session.shutdown().await.expect("shutdown");
        assert!(
            sync.pushes.load(Ordering::SeqCst) >= 2,
            "transcript sync should push after turn, flush, and shutdown"
        );
    }

    async fn write_json_line(w: &mut PipeWrite, val: &Value) {
        let mut bytes = serde_json::to_vec(val).expect("ser");
        bytes.push(b'\n');
        let _ = w.write_all(&bytes).await;
        let _ = w.flush().await;
    }

    enum MockCodexEvent {
        Read(std::io::Result<usize>),
        Inject(Option<Value>),
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end test drives mock app-server, background approvals, extensions, and plan mode"
    )]
    async fn in_process_codex_drives_background_extensions_and_plan_mode() {
        let scratch = Scratch::new("codex-in-process");
        let (bridge_r, bridge_w, mock_r, mut mock_w) = pipe_pair();
        let slot = Arc::new(Mutex::new(Some((bridge_r, bridge_w))));
        let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Value>();
        let seen_calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let seen_calls_task = Arc::clone(&seen_calls);
        let answered_server_req = Arc::new(Mutex::new(Vec::<Value>::new()));
        let answered_task = Arc::clone(&answered_server_req);

        // Mock `codex app-server` JSON-RPC loop.
        tokio::spawn(async move {
            let mut reader = futures_lite::io::BufReader::new(mock_r);
            let mut line = String::new();
            loop {
                line.clear();
                let ev = tokio::select! {
                    r = reader.read_line(&mut line) => MockCodexEvent::Read(r),
                    i = inject_rx.recv() => MockCodexEvent::Inject(i),
                };
                match ev {
                    MockCodexEvent::Inject(Some(frame)) => {
                        write_json_line(&mut mock_w, &frame).await;
                    }
                    MockCodexEvent::Inject(None) | MockCodexEvent::Read(Ok(0) | Err(_)) => break,
                    MockCodexEvent::Read(Ok(_)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg: Value = serde_json::from_str(trimmed).expect("json");
                        if msg.get("method").is_none() {
                            answered_task.lock().expect("answered").push(msg);
                            continue;
                        }
                        let id = msg.get("id").cloned();
                        let method = msg["method"].as_str().unwrap_or("").to_owned();
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        seen_calls_task
                            .lock()
                            .expect("calls")
                            .push((method.clone(), params.clone()));
                        let Some(id) = id else {
                            continue;
                        };
                        let result = match method.as_str() {
                            "initialize" => json!({"userAgent": "codex-mock/0.1"}),
                            "model/list" => json!({
                                "data": [{
                                    "id": "gpt-5.4",
                                    "model": "gpt-5.4",
                                    "displayName": "GPT-5.4",
                                    "isDefault": true,
                                    "supportedReasoningEfforts": [
                                        {"reasoningEffort": "medium", "description": "Medium"},
                                        {"reasoningEffort": "high", "description": "High"}
                                    ],
                                    "defaultReasoningEffort": "medium"
                                }],
                                "nextCursor": null
                            }),
                            "thread/start" => json!({
                                "thread": {"id": "thr-1"},
                                "model": "gpt-5.4",
                                "reasoningEffort": "medium"
                            }),
                            "mcpServerStatus/list" => json!({
                                "data": [{
                                    "name": "flyco",
                                    "runtimeStatus": "connected",
                                    "tools": {
                                        "machine_status": {},
                                        "budget_status": {},
                                        "machine_resize": {}
                                    }
                                }],
                                "nextCursor": null
                            }),
                            "account/rateLimits/read" => json!({
                                "rateLimits": {
                                    "primary": {
                                        "usedPercent": 25,
                                        "windowDurationMins": 300,
                                        "resetsAt": 1_789_002_000
                                    }
                                }
                            }),
                            "turn/start" => {
                                write_json_line(
                                    &mut mock_w,
                                    &json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {"turn": {"id": "turn-codex-1"}}
                                    }),
                                )
                                .await;
                                write_json_line(
                                    &mut mock_w,
                                    &json!({
                                        "jsonrpc": "2.0",
                                        "method": "item/agentMessage/delta",
                                        "params": {
                                            "threadId": "thr-1",
                                            "turnId": "turn-codex-1",
                                            "itemId": "msg-1",
                                            "delta": "codex plan reply"
                                        }
                                    }),
                                )
                                .await;
                                write_json_line(
                                    &mut mock_w,
                                    &json!({
                                        "jsonrpc": "2.0",
                                        "method": "turn/completed",
                                        "params": {
                                            "threadId": "thr-1",
                                            "turn": {"id": "turn-codex-1", "status": "completed"}
                                        }
                                    }),
                                )
                                .await;
                                continue;
                            }
                            _ => json!({}),
                        };
                        write_json_line(
                            &mut mock_w,
                            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        )
                        .await;
                    }
                }
            }
        });

        let codex_cfg = AcpConfig {
            agent: "codex".to_owned(),
            in_process: Some(InProcessAgent::Codex),
            program: std::path::PathBuf::from("/usr/local/bin/codex"),
            args: Vec::new(),
            auth_method: None,
            mcp_via_files: false,
            env: BTreeMap::new(),
            files: Vec::new(),
            model: None,
            effort: None,
            model_option: "model".to_owned(),
            effort_option: Some("reasoning_effort".to_owned()),
            fused_effort_tails: Vec::new(),
            permission_mode: PermissionMode::Plan,
            modes: BTreeMap::from([(
                PermissionMode::Plan,
                AcpMode {
                    set_mode: Some("read-only".to_owned()),
                    options: vec![AcpOptionWrite {
                        option: "collaboration_mode".to_owned(),
                        value: "plan".to_owned(),
                    }],
                },
            )]),
            methods: AcpMethods {
                compact: Some(AcpMethodCall {
                    call: "_codex/compact".to_owned(),
                    params: json!({"threadId": "<session>"}),
                }),
                usage: Some(AcpMethodCall {
                    call: "_codex/rate_limits".to_owned(),
                    params: Value::Null,
                }),
                mcp_status: Some(AcpMethodCall {
                    call: "_codex/mcp_status".to_owned(),
                    params: Value::Null,
                }),
            },
            tui: None,
        };

        let harness =
            AcpHarness::new(codex_cfg, mount()).with_codex_connector(move |_spawn, handler| {
                let pair = slot.lock().expect("slot").take();
                async move {
                    let (r, w) = pair.expect("codex connector called once");
                    Ok(ConnectedAppServer::attach(r, w, handler))
                }
            });

        let started = harness
            .start(StartRequest {
                workdir: scratch.join("work"),
                resume_session_id: None,
                additional_directories: Vec::new(),
            })
            .await
            .expect("in-process Codex starts");
        let session = started.session;
        let mut outputs = started.outputs;

        // Verify out-of-prompt server-initiated approval is serviced while idle
        // (proving `CodexAgent::drive_background()` is running concurrently!).
        inject_tx
            .send(json!({
                "jsonrpc": "2.0",
                "id": 777,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": "thr-1",
                    "turnId": "idle-bg",
                    "itemId": "cmd-bg-1",
                    "command": "git status"
                }
            }))
            .expect("inject idle server request");

        let approval_id = loop {
            if let SessionOutput::ApprovalRequest { id, .. } =
                next(&mut outputs, "background approval request").await
            {
                break id;
            }
        };
        session
            .decide_approval(flyco_daemon::harness::ToolApproval::Allow {
                id: approval_id,
                updated_input: None,
            })
            .await
            .expect("decide idle approval");

        // Wait for the response to reach the mock app-server while still idle.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if answered_server_req
                .lock()
                .expect("answered")
                .iter()
                .any(|v| v.get("id") == Some(&json!(777)))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "drive_background did not deliver response for idle server request"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Verify `_codex/compact` works.
        session.compact().await.expect("_codex/compact succeeds");
        loop {
            if matches!(
                next(&mut outputs, "context_compacted").await,
                SessionOutput::Event {
                    event: HarnessEvent::ContextCompacted
                }
            ) {
                break;
            }
        }

        // Verify `collaboration_mode = "plan"` reached `turn/start`.
        session
            .send_user_message("draft a plan".to_owned())
            .await
            .expect("send plan prompt");
        loop {
            if matches!(
                next(&mut outputs, "turn_completed").await,
                SessionOutput::Event {
                    event: HarnessEvent::TurnCompleted { .. }
                }
            ) {
                break;
            }
        }

        let turn_starts: Vec<Value> = seen_calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(m, _)| m == "turn/start")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(turn_starts.len(), 1);
        assert_eq!(
            turn_starts[0]["collaborationMode"]["mode"],
            json!("plan"),
            "Plan permission mode must set collaborationMode.mode = plan: {}",
            turn_starts[0]
        );

        session.shutdown().await.expect("shutdown codex");
    }
}

#[tokio::test]
async fn acp_harness_watches_antigravity_swarm_dir_read_only() {
    let scratch = Scratch::new("swarm-watch");
    let app_data_root = scratch.join("app-data");
    let swarm_dir = app_data_root
        .join("brain")
        .join("fake-session")
        .join(".system_generated")
        .join("swarm");

    let harness = AcpHarness::new(config(), mount())
        .with_swarm_root(app_data_root)
        .with_swarm_timings(flyco_daemon::swarm::WatchTimings {
            idle_poll: std::time::Duration::from_millis(20),
            active_poll: std::time::Duration::from_millis(20),
            coalesce: std::time::Duration::from_millis(30),
        });
    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
            additional_directories: Vec::new(),
        })
        .await
        .expect("harness starts");
    let mut outputs = started.outputs;
    announcements(&mut outputs).await;

    // The watcher must not create the swarm directory on its own.
    assert!(!swarm_dir.exists());

    std::fs::create_dir_all(&swarm_dir).expect("create swarm dir");
    std::fs::write(
        swarm_dir.join("swarm.json"),
        r#"{"version":1,"backend":"local","swarm_id":"sw-acp"}"#,
    )
    .expect("write swarm.json");
    std::fs::write(
        swarm_dir.join("members.json"),
        r#"{"title":"ACP Swarm","members":{"coordinator":{"role":"coordinator","phase":"working"}}}"#,
    )
    .expect("write members.json");
    std::fs::write(
        swarm_dir.join("board.jsonl"),
        "{\"seq\":1,\"agent\":\"coordinator\",\"text\":\"@all ready\"}\n",
    )
    .expect("write board.jsonl");

    let (snapshot, board_messages) = loop {
        if let SessionOutput::SwarmUpdated {
            snapshot,
            board_messages,
        } = next(&mut outputs, "swarm_updated").await
        {
            break (snapshot, board_messages);
        }
    };
    let snap = snapshot.expect("snapshot emitted");
    assert_eq!(snap.swarm_id.as_deref(), Some("sw-acp"));
    assert_eq!(snap.title.as_deref(), Some("ACP Swarm"));
    assert_eq!(board_messages.len(), 1);
    assert_eq!(board_messages[0].seq, 1);
    assert_eq!(board_messages[0].text, "@all ready");

    started.session.shutdown().await.expect("shutdown");
}
