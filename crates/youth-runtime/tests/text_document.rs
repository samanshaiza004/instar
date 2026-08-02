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
