mod common;

use std::time::Duration;

use common::test_component;
use youth_runtime::{
    AppId, EditorLocalEdit, RuntimeEvent, StateLocation, TurnOrigin, WorkspaceGrant,
    YouthAppConfig, YouthAppHandle,
};
use youth_sdk::named_node_id;
use youth_tree::{NodeData, NodeId};

fn node(name: &str) -> NodeId {
    NodeId::new(named_node_id(name)).unwrap()
}

#[tokio::test]
async fn document_edit_save_completion_and_restart_use_exact_file_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let document = root.join("note.md");
    std::fs::write(&document, b"\xef\xbb\xbfHello\r\n").unwrap();
    let state = temporary.path().join("state.sqlite3");
    let config = YouthAppConfig {
        component_path: test_component("youth-sdk-text-document"),
        app_id: AppId::parse("dev.youth.text-document").unwrap(),
        state: StateLocation::File(state),
        limits: youth_runtime::RuntimeLimits::default(),
        workspace: Some(WorkspaceGrant::text_document(&root, "note.md")),
    };

    let app = YouthAppHandle::spawn(config.clone()).unwrap();
    let mut events = app.subscribe();
    let mounted = app.mount().await.unwrap();
    assert_eq!(
        mounted
            .nodes
            .iter()
            .find(|candidate| candidate.id == node("document"))
            .map(|candidate| &candidate.data),
        mounted
            .nodes
            .iter()
            .find(|candidate| matches!(candidate.data, NodeData::TextDocumentEditor { .. }))
            .map(|candidate| &candidate.data)
    );
    let edited = app
        .edit_editor_locally(
            node("document"),
            EditorLocalEdit::InsertText("Changed ".into()),
        )
        .await
        .unwrap();
    assert!(edited.content_changed);
    assert!(edited.locally_dirty);
    assert_eq!(
        app.editor_snapshot(node("document")).await.unwrap().text,
        "Hello\r\nChanged "
    );

    app.activate(node("save")).await.unwrap();
    let completion = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event,
                RuntimeEvent::TurnCommitted(ref turn)
                    if matches!(turn.origin, TurnOrigin::TextDocumentSaveCompleted { .. })
            ) {
                break event;
            }
        }
    })
    .await
    .expect("save completion is delivered");
    assert!(matches!(completion, RuntimeEvent::TurnCommitted(_)));
    assert_eq!(
        std::fs::read(&document).unwrap(),
        b"\xef\xbb\xbfHello\r\nChanged "
    );
    assert!(
        !app.editor_snapshot(node("document"))
            .await
            .unwrap()
            .text
            .is_empty()
    );
    app.stop().await.unwrap();

    let restarted = YouthAppHandle::spawn(config).unwrap();
    restarted.mount().await.unwrap();
    assert_eq!(
        restarted
            .editor_snapshot(node("document"))
            .await
            .unwrap()
            .text,
        "Hello\r\nChanged "
    );
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn external_content_change_returns_conflict_without_overwriting() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let document = root.join("note.txt");
    std::fs::write(&document, b"original").unwrap();
    let app = YouthAppHandle::spawn(YouthAppConfig {
        component_path: test_component("youth-sdk-text-document"),
        app_id: AppId::parse("dev.youth.text-document-conflict").unwrap(),
        state: StateLocation::Memory,
        limits: youth_runtime::RuntimeLimits::default(),
        workspace: Some(WorkspaceGrant::text_document(&root, "note.txt")),
    })
    .unwrap();
    let mut events = app.subscribe();
    app.mount().await.unwrap();
    app.edit_editor_locally(
        node("document"),
        EditorLocalEdit::InsertText("local ".into()),
    )
    .await
    .unwrap();
    std::fs::write(&document, b"external").unwrap();
    app.activate(node("save")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                RuntimeEvent::TurnCommitted(ref turn)
                    if matches!(turn.origin, TurnOrigin::TextDocumentSaveCompleted { .. })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read(&document).unwrap(), b"external");
    assert!(
        app.editor_snapshot(node("document"))
            .await
            .unwrap()
            .text
            .ends_with("local ")
    );
    app.stop().await.unwrap();
}

#[tokio::test]
async fn one_mebibyte_document_opens_edits_saves_and_restarts() {
    let scenario_started = std::time::Instant::now();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let document = root.join("large.txt");
    let initial = vec![b'a'; 1024 * 1024 - 1];
    std::fs::write(&document, &initial).unwrap();
    let config = YouthAppConfig {
        component_path: test_component("youth-sdk-text-document"),
        app_id: AppId::parse("dev.youth.text-document-large").unwrap(),
        state: StateLocation::Memory,
        // The production default's 100ms `handle` deadline is Wasmtime's
        // real epoch-interruption budget (crates/youth-runtime/src/
        // engine.rs); it exists to trap a genuinely runaway guest, and this
        // test isn't testing that containment property. A 1 MiB document's
        // coalesced dirty-notification turn re-lays out the full Parley
        // buffer, which in an unoptimized debug build has been observed to
        // exceed 100ms of real wall-clock time (release-build evidence in
        // docs/metrics/scratchpad-gate-b-local.md stays well under it),
        // tripping the trap as a false positive and faulting the app before
        // the test's own Save assertion runs. Widened generously here, in
        // this test only -- production still enforces the tight default via
        // RuntimeLimits::default() elsewhere, unchanged.
        limits: youth_runtime::RuntimeLimits {
            handle: youth_runtime::CallBudget {
                deadline: Duration::from_secs(5),
                ..youth_runtime::RuntimeLimits::default().handle
            },
            ..youth_runtime::RuntimeLimits::default()
        },
        workspace: Some(WorkspaceGrant::text_document(&root, "large.txt")),
    };
    let open_started = std::time::Instant::now();
    let app = YouthAppHandle::spawn(config.clone()).unwrap();
    let mut events = app.subscribe();
    app.mount().await.unwrap();
    let open_elapsed = open_started.elapsed();
    assert_eq!(
        app.editor_snapshot(node("document"))
            .await
            .unwrap()
            .text
            .len(),
        initial.len()
    );
    let edit_started = std::time::Instant::now();
    app.edit_editor_locally(node("document"), EditorLocalEdit::InsertText("!".into()))
        .await
        .unwrap();
    let edit_elapsed = edit_started.elapsed();
    let save_started = std::time::Instant::now();
    app.activate(node("save")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                RuntimeEvent::TurnCommitted(ref turn)
                    if matches!(turn.origin, TurnOrigin::TextDocumentSaveCompleted { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("one-mebibyte save completes");
    let save_elapsed = save_started.elapsed();
    app.stop().await.unwrap();

    let saved = std::fs::read(&document).unwrap();
    assert_eq!(saved.len(), 1024 * 1024);
    assert_eq!(saved.last(), Some(&b'!'));
    let restart_started = std::time::Instant::now();
    let restarted = YouthAppHandle::spawn(config).unwrap();
    restarted.mount().await.unwrap();
    let restart_elapsed = restart_started.elapsed();
    assert_eq!(
        restarted
            .editor_snapshot(node("document"))
            .await
            .unwrap()
            .text
            .len(),
        1024 * 1024
    );
    restarted.stop().await.unwrap();
    eprintln!(
        "1 MiB text-document evidence: open={open_elapsed:?} edit={edit_elapsed:?} save={save_elapsed:?} restart={restart_elapsed:?} total={:?}",
        scenario_started.elapsed()
    );
}
