//! Pi/OMP terminal handoff: what `finish` accepts, what it refuses, and that a refusal writes nothing.
use greencode_core::{
    config::{AcpCfg, Config, Protocol},
    handoff::{self, Outcome},
    session::{Meta, Session},
    Block, Message, Role,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("greencode-terminal-handoff-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("GREENCODE_HOME", &path);
        path
    })
}

fn cfg() -> Config {
    let mut cfg = Config::default();
    cfg.backends.retain(|b| b.name != "pi");
    cfg.backends.push(AcpCfg {
        name: "pi".into(),
        command: vec!["pi".into()],
        protocol: Protocol::Pi,
        env: Default::default(),
    });
    cfg
}

fn meta() -> Meta {
    let cwd = home().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    Meta::new(cwd, "default".into(), "pi".into())
}

fn message(role: &str, text: &str) -> Message {
    if role == "user" {
        Message::user(text)
    } else {
        Message {
            role: Role::Assistant,
            content: vec![Block::Text { text: text.into() }],
        }
    }
}

/// A Pi session file: header, then a linear chain of message entries, optionally followed by OMP's late
/// `session_exit` entry. Returns the message entry ids.
fn write_native(path: &Path, session_id: &str, chain: &[(&str, &str)], exit: bool) -> Vec<String> {
    let mut lines = vec![
        json!({"type":"session","version":3,"id":session_id,"timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}),
    ];
    let (mut ids, mut parent) = (vec![], Value::Null);
    for (i, (role, text)) in chain.iter().enumerate() {
        let id = format!("{session_id}-{i}");
        let body = if *role == "user" {
            json!({"role":"user","content":text})
        } else {
            json!({"role":"assistant","content":[{"type":"text","text":text}]})
        };
        lines.push(
            json!({"type":"message","id":id,"parentId":parent,"timestamp":"t","message":body}),
        );
        parent = json!(id);
        ids.push(id);
    }
    if exit {
        lines.push(json!({"type":"custom","customType":"session_exit","id":format!("{session_id}-exit"),"parentId":parent,"timestamp":"t"}));
    }
    std::fs::write(
        path,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    ids
}

fn note(event: &str, pid: u64, id: &str, file: &Path, branch: &[String]) -> Value {
    json!({"event":event,"reason":null,"pid":pid,"id":id,"file":file,"leaf":branch.last(),"branch":branch})
}

fn write_log(meta: &Meta, raw: &str) {
    let dir = home().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{}.handoff.log", meta.id)), raw).unwrap();
}

fn lines(notes: &[Value]) -> String {
    notes.iter().map(|n| format!("{n}\n")).collect()
}

fn runtime_bytes(meta: &Meta) -> Vec<u8> {
    std::fs::read(home().join("runtime").join(format!("{}.json", meta.id))).unwrap_or_default()
}

fn pointer(meta: &Meta) -> Value {
    Session::runtime(&meta.id).unwrap()["sessionFile"].clone()
}

struct Thread {
    meta: Meta,
    file: PathBuf,
    session: String,
    ids: Vec<String>,
}

/// A thread GreenCode shows `shown` for, whose native session file holds `native`, with the handoff prepared.
fn thread(shown: &[(&str, &str)], native: &[(&str, &str)]) -> Thread {
    let meta = meta();
    let session = Session::create(&meta).unwrap();
    for (role, text) in shown {
        session.append(&message(role, text)).unwrap();
    }
    let file = home().join(format!("{}-a.jsonl", meta.id));
    let session = format!("a{}", meta.id);
    let ids = write_native(&file, &session, native, false);
    Session::save_runtime(&meta.id, &json!({"backend":"pi","sessionFile":file})).unwrap();
    handoff::prepare(&cfg(), &meta).unwrap();
    Thread {
        meta,
        file,
        session,
        ids,
    }
}

const HELLO: &[(&str, &str)] = &[("user", "hello"), ("assistant", "hi")];

#[test]
fn prepare_names_the_exact_session_and_extensions_and_refuses_a_second_handoff() {
    let meta = meta();
    Session::create(&meta).unwrap();
    let file = home().join(format!("{}-p.jsonl", meta.id));
    write_native(&file, "p", HELLO, false);
    Session::save_runtime(&meta.id, &json!({"backend":"pi","sessionFile":file})).unwrap();
    let session = Session::path(&meta.id);
    session_append_hello(&session, &meta);

    let launch = handoff::prepare(&cfg(), &meta).unwrap();
    let at = |flag: &str| launch.argv.iter().position(|a| a == flag).unwrap();
    assert_eq!(launch.argv[0], "pi");
    assert!(
        !launch.argv.iter().any(|a| a == "--mode"),
        "the terminal UI must not run in RPC mode"
    );
    assert_eq!(launch.argv[at("--session") + 1], file.to_str().unwrap());
    assert!(launch.argv[at("--session-dir") + 1].ends_with(&meta.id));
    let extensions: Vec<&String> = launch
        .argv
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--extension")
        .map(|(i, _)| &launch.argv[i + 1])
        .collect();
    assert_eq!(extensions.len(), 2);
    assert!(
        Path::new(extensions[0]).ends_with("greencode-permissions.ts")
            && Path::new(extensions[0]).is_file()
    );
    assert!(std::fs::read_to_string(extensions[1])
        .unwrap()
        .contains("session_shutdown"));
    assert_eq!(launch.cwd, meta.cwd);
    assert!(handoff::is_open(&meta.id));

    let err = handoff::prepare(&cfg(), &meta).unwrap_err();
    assert!(format!("{err:#}").contains("already open"), "{err:#}");
    handoff::abandon(&meta).unwrap();
    assert!(!handoff::is_open(&meta.id));
}

fn session_append_hello(_path: &Path, meta: &Meta) {
    let session = Session::open(&meta.id).unwrap();
    for (role, text) in HELLO {
        session.append(&message(role, text)).unwrap();
    }
}

#[test]
fn writer_lock_excludes_a_second_holder_and_a_prepare() {
    let meta = meta();
    let held = handoff::writer_lock(&meta.id).unwrap();
    assert!(handoff::writer_lock(&meta.id).is_err());
    let err = handoff::prepare(&cfg(), &meta).unwrap_err();
    assert!(
        format!("{err:#}").contains("another GreenCode process"),
        "{err:#}"
    );
    drop(held);
    assert!(handoff::writer_lock(&meta.id).is_ok());
}

#[test]
fn a_fork_that_extends_the_transcript_is_appended_and_becomes_the_runtime_session() {
    let t = thread(HELLO, HELLO);
    // The user forked in the terminal and kept chatting there; OMP appended its late `session_exit` entry.
    let forked = home().join(format!("{}-b.jsonl", t.meta.id));
    let chain = [
        ("user", "hello"),
        ("assistant", "hi"),
        ("user", "more"),
        ("assistant", "done"),
    ];
    let b = format!("b{}", t.meta.id);
    let b_ids = write_native(&forked, &b, &chain, true);
    write_log(
        &t.meta,
        &lines(&[
            note("start", 7, &t.session, &t.file, &t.ids),
            note("branch", 7, &b, &forked, &b_ids),
            note("shutdown", 7, &b, &forked, &b_ids),
        ]),
    );

    assert_eq!(
        handoff::finish(&cfg(), &t.meta).unwrap(),
        Outcome::Appended(2)
    );
    let (_, messages) = Session::load(&t.meta.id).unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(pointer(&t.meta), json!(forked));
    assert!(!handoff::is_open(&t.meta.id));
    let err = handoff::finish(&cfg(), &t.meta).unwrap_err();
    assert!(format!("{err:#}").contains("no terminal handoff is open"));
}

#[test]
fn finishing_twice_after_a_crash_between_steps_is_idempotent() {
    let t = thread(
        HELLO,
        &[("user", "hello"), ("assistant", "hi"), ("user", "more")],
    );
    write_log(
        &t.meta,
        &lines(&[
            note("start", 7, &t.session, &t.file, &t.ids),
            note("shutdown", 7, &t.session, &t.file, &t.ids),
        ]),
    );
    // Simulate a crash after the transcript was appended but before the handoff closed.
    Session::open(&t.meta.id)
        .unwrap()
        .append(&message("user", "more"))
        .unwrap();
    assert_eq!(
        handoff::finish(&cfg(), &t.meta).unwrap(),
        Outcome::Unchanged
    );
    assert_eq!(Session::load(&t.meta.id).unwrap().1.len(), 3);
}

#[test]
fn an_unverifiable_terminal_changes_nothing() {
    let scenarios: Vec<(&str, Box<dyn Fn(&Thread) -> Option<String>>)> = vec![
        ("no tracking log", Box::new(|_| None)),
        (
            "truncated log",
            Box::new(|t| {
                Some(
                    lines(&[note("start", 7, &t.session, &t.file, &t.ids)])
                        .trim_end()
                        .to_string(),
                )
            }),
        ),
        (
            "killed: no shutdown",
            Box::new(|t| Some(lines(&[note("start", 7, &t.session, &t.file, &t.ids)]))),
        ),
        (
            "two processes",
            Box::new(|t| {
                Some(lines(&[
                    note("start", 7, &t.session, &t.file, &t.ids),
                    note("shutdown", 8, &t.session, &t.file, &t.ids),
                ]))
            }),
        ),
        (
            "opened another session",
            Box::new(|t| {
                Some(lines(&[
                    note(
                        "start",
                        7,
                        &t.session,
                        &t.meta.cwd.join("other.jsonl"),
                        &t.ids,
                    ),
                    note("shutdown", 7, &t.session, &t.file, &t.ids),
                ]))
            }),
        ),
        (
            "extension error",
            Box::new(|_| Some(lines(&[json!({"event":"error","pid":7,"message":"boom"})]))),
        ),
        (
            "session id mismatch",
            Box::new(|t| {
                Some(lines(&[
                    note("start", 7, "other", &t.file, &t.ids),
                    note("shutdown", 7, "other", &t.file, &t.ids),
                ]))
            }),
        ),
        (
            "branch the file cannot confirm",
            Box::new(|t| {
                Some(lines(&[
                    note("start", 7, &t.session, &t.file, &t.ids),
                    note("shutdown", 7, &t.session, &t.file, &["x".to_string()]),
                ]))
            }),
        ),
    ];
    for (name, log) in scenarios {
        let t = thread(
            HELLO,
            &[("user", "hello"), ("assistant", "hi"), ("user", "more")],
        );
        if let Some(raw) = log(&t) {
            write_log(&t.meta, &raw);
        }
        let (transcript, runtime) = (
            std::fs::read(Session::path(&t.meta.id)).unwrap(),
            runtime_bytes(&t.meta),
        );
        assert!(handoff::finish(&cfg(), &t.meta).is_err(), "{name}");
        assert_eq!(
            std::fs::read(Session::path(&t.meta.id)).unwrap(),
            transcript,
            "{name}: transcript"
        );
        assert_eq!(runtime_bytes(&t.meta), runtime, "{name}: runtime");
        assert!(
            handoff::is_open(&t.meta.id),
            "{name}: handoff must stay open"
        );
    }
}

#[test]
fn a_transcript_that_does_not_extend_the_displayed_one_needs_an_explicit_adopt() {
    let t = thread(
        HELLO,
        &[
            ("user", "hello"),
            ("assistant", "something else"),
            ("user", "more"),
        ],
    );
    write_log(
        &t.meta,
        &lines(&[
            note("start", 7, &t.session, &t.file, &t.ids),
            note("shutdown", 7, &t.session, &t.file, &t.ids),
        ]),
    );
    let (transcript, runtime) = (
        std::fs::read(Session::path(&t.meta.id)).unwrap(),
        runtime_bytes(&t.meta),
    );

    let Outcome::Diverged(reason) = handoff::finish(&cfg(), &t.meta).unwrap() else {
        panic!("expected Diverged")
    };
    assert!(reason.contains("message 2"), "{reason}");
    assert_eq!(
        std::fs::read(Session::path(&t.meta.id)).unwrap(),
        transcript
    );
    assert_eq!(runtime_bytes(&t.meta), runtime);
    assert!(handoff::is_open(&t.meta.id));

    assert_eq!(handoff::adopt(&cfg(), &t.meta).unwrap(), 3);
    let backup = PathBuf::from(format!(
        "{}.pre-handoff-1",
        Session::path(&t.meta.id).display()
    ));
    assert_eq!(
        std::fs::read(backup).unwrap(),
        transcript,
        "the old transcript is kept"
    );
    let (meta, messages) = Session::load(&t.meta.id).unwrap();
    assert_eq!((meta.id.as_str(), messages.len()), (t.meta.id.as_str(), 3));
    assert!(!handoff::is_open(&t.meta.id));
}

#[test]
fn ending_on_an_earlier_branch_is_diverged_not_synced() {
    let chain = [
        ("user", "hello"),
        ("assistant", "hi"),
        ("user", "more"),
        ("assistant", "done"),
    ];
    let t = thread(&chain, &chain);
    // The user navigated back in the tree without sending anything: the file still resumes on the last entry.
    write_log(
        &t.meta,
        &lines(&[
            note("start", 7, &t.session, &t.file, &t.ids),
            note("shutdown", 7, &t.session, &t.file, &t.ids[..2]),
        ]),
    );
    let Outcome::Diverged(reason) = handoff::finish(&cfg(), &t.meta).unwrap() else {
        panic!("expected Diverged")
    };
    assert!(reason.contains("different branch"), "{reason}");
    assert!(handoff::is_open(&t.meta.id));
}

#[test]
fn a_fresh_thread_is_handed_over_without_inventing_history() {
    let meta = meta();
    let missing = home().join(format!("{}-missing.jsonl", meta.id));
    // Pi/OMP report a session path before anything is written to it.
    Session::save_runtime(&meta.id, &json!({"backend":"pi","sessionFile":missing})).unwrap();
    let launch = handoff::prepare(&cfg(), &meta).unwrap();
    assert!(
        !launch.argv.iter().any(|a| a == "--session"),
        "a path that does not exist is not a conversation to resume"
    );

    // The user quit without a prompt: nothing to sync, the old pointer is left alone.
    let session = format!("fresh{}", meta.id);
    write_log(
        &meta,
        &lines(&[
            note("start", 7, &session, &missing, &[]),
            note("shutdown", 7, &session, &missing, &[]),
        ]),
    );
    let before = runtime_bytes(&meta);
    assert_eq!(handoff::finish(&cfg(), &meta).unwrap(), Outcome::Unchanged);
    assert!(Session::open(&meta.id).is_none());
    assert_eq!(runtime_bytes(&meta), before);

    // Second round: the user chatted in the terminal.
    handoff::prepare(&cfg(), &meta).unwrap();
    let file = home().join(format!("{}-new.jsonl", meta.id));
    let ids = write_native(&file, &session, HELLO, false);
    write_log(
        &meta,
        &lines(&[
            note("start", 7, &session, &file, &[]),
            note("shutdown", 7, &session, &file, &ids),
        ]),
    );
    assert_eq!(
        handoff::finish(&cfg(), &meta).unwrap(),
        Outcome::Appended(2)
    );
    assert_eq!(Session::load(&meta.id).unwrap().1.len(), 2);
    assert_eq!(pointer(&meta), json!(file));
}

#[test]
fn history_without_a_readable_native_session_is_never_handed_over() {
    let meta = meta();
    let session = Session::create(&meta).unwrap();
    session.append(&message("user", "hello")).unwrap();
    for pointer in [None, Some(home().join("gone.jsonl"))] {
        if let Some(file) = &pointer {
            Session::save_runtime(&meta.id, &json!({"backend":"pi","sessionFile":file})).unwrap();
        }
        let err = handoff::prepare(&cfg(), &meta).unwrap_err();
        assert!(format!("{err:#}").contains("silently losing"), "{err:#}");
        assert!(!handoff::is_open(&meta.id));
    }
}

#[test]
fn the_installed_tracker_extension_writes_a_log_finish_accepts() {
    let t = thread(
        HELLO,
        &[("user", "hello"), ("assistant", "hi"), ("user", "more")],
    );
    let tracker = home()
        .join("provider-sessions")
        .join(&t.meta.id)
        .join("greencode-handoff-tracker.ts");
    let script = r#"const h = {};
(await import(process.env.TRACKER)).default({ on: (name, fn) => { h[name] = fn; } });
const entries = JSON.parse(process.env.BRANCH).map((id) => ({ id, type: "message" }));
const ctx = { sessionManager: { getSessionId: () => process.env.SID, getSessionFile: () => process.env.FILE, getLeafId: () => process.env.LEAF, getBranch: () => entries, getEntries: () => entries } };
h.session_start({ reason: "startup" }, ctx);
h.session_shutdown({ reason: "quit" }, ctx);"#;
    let Ok(out) = std::process::Command::new("bun")
        .args(["-e", script])
        .env("TRACKER", &tracker)
        .env("SID", &t.session)
        .env("FILE", &t.file)
        .env("LEAF", t.ids.last().unwrap())
        .env("BRANCH", serde_json::to_string(&t.ids).unwrap())
        .output()
    else {
        return; // bun is not installed
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        handoff::finish(&cfg(), &t.meta).unwrap(),
        Outcome::Appended(1)
    );
}
