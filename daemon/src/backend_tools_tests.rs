//! Pruebas del contrato de herramientas del rasgo [`ModelBackend`]: lo que
//! hacen por omisión los backends sin soporte y cómo las pasa [`Limited`].

use std::sync::Arc;

use vasak_assistant_protocol::Role;

use super::*;
use crate::test_support::{model_info, FakeBackend, ScriptedBackend};

fn thread() -> Vec<ChatMessage> {
    vec![
        ChatMessage::System("sistema".into()),
        ChatMessage::User("hola".into()),
        ChatMessage::Assistant {
            text: "".into(),
            calls: vec![ToolCall {
                id: "1".into(),
                name: "search_mail".into(),
                arguments: serde_json::json!({}),
            }],
        },
        ChatMessage::Tool {
            id: "1".into(),
            content: "[{\"id\":1}]".into(),
        },
    ]
}

#[test]
fn aplanar_el_hilo_pasa_el_resultado_de_una_herramienta_como_turno_del_usuario() {
    let flat = flatten_messages(&thread());
    let roles: Vec<_> = flat.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::System, Role::User, Role::Assistant, Role::User]
    );
    assert_eq!(flat[3].content, "[{\"id\":1}]");
}

#[tokio::test]
async fn un_backend_sin_soporte_ignora_las_herramientas_y_responde_texto() {
    let backend = FakeBackend::replying("solo texto");
    assert!(!backend.supports_tools());
    let tools = [ToolDef {
        name: "x".into(),
        description: String::new(),
        parameters: serde_json::json!({}),
    }];
    let turn = backend.chat_tools(&thread(), &tools).await.unwrap();
    assert_eq!(turn, ModelTurn::Text("solo texto".into()));
    // Vio el hilo aplanado, sin rol de herramienta.
    let seen = backend.seen.lock();
    assert_eq!(seen[0].len(), 4);
}

#[tokio::test]
async fn chat_tools_local_only_por_omision_rechaza_un_modelo_remoto_sin_llamarlo() {
    let remote = Arc::try_unwrap(ScriptedBackend::new(["no debió salir"]))
        .ok()
        .unwrap();
    let remote = ScriptedBackend {
        local: false,
        ..remote
    };
    let error = remote
        .chat_tools_local_only(&thread(), &[])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::NotLocal), "{error:?}");
    assert!(remote.seen.lock().is_empty());

    let local = ScriptedBackend::new(["ok"]);
    let turn = local.chat_tools_local_only(&thread(), &[]).await.unwrap();
    assert_eq!(turn, ModelTurn::Text("ok".into()));
}

#[tokio::test]
async fn el_backend_rechazado_responde_no_disponible_tambien_con_herramientas() {
    let rejected = RejectedBackend {
        info: model_info("x"),
        reason: "sin permiso".into(),
    };
    assert!(!rejected.supports_tools());
    let error = rejected.chat_tools(&thread(), &[]).await.unwrap_err();
    assert!(matches!(error, BackendError::Unavailable(m) if m == "sin permiso"));
}

#[tokio::test]
async fn limited_reenvia_el_soporte_y_los_turnos_con_herramientas_al_backend_de_adentro() {
    let inner = ScriptedBackend::new(["uno", "dos"]);
    let limited = Limited::new(inner.clone());
    assert!(!limited.supports_tools());
    let turn = limited.chat_tools(&thread(), &[]).await.unwrap();
    assert_eq!(turn, ModelTurn::Text("uno".into()));
    let turn = limited.chat_tools_local_only(&thread(), &[]).await.unwrap();
    assert_eq!(turn, ModelTurn::Text("dos".into()));
    assert_eq!(inner.seen.lock().len(), 2);
}

#[tokio::test]
async fn limited_con_un_modelo_remoto_no_deja_pasar_el_turno_solo_local() {
    let inner = ScriptedBackend::new(["no debió salir"]);
    let remote = Arc::new(ScriptedBackend {
        local: false,
        replies: parking_lot::Mutex::new(std::mem::take(&mut *inner.replies.lock())),
        seen: Default::default(),
        effective_local: None,
        reject_local_only: Default::default(),
    });
    let limited = Limited::new(remote.clone());
    let error = limited
        .chat_tools_local_only(&thread(), &[])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::NotLocal), "{error:?}");
    assert!(remote.seen.lock().is_empty());
}
