//! Pruebas del camino de herramientas de Ollama: lo que se manda a `/api/chat`
//! (las `tools` y los mensajes con rol `tool`) y cómo se lee `message.tool_calls`.

use serde_json::json;

use super::*;
use crate::test_support::{client, local_selection, ollama_backend, FakeServer, Reply};

const TAGS: &str = r#"{"models":[{"name":"llama3.2:latest"}]}"#;

fn thread() -> Vec<ChatMessage> {
    vec![
        ChatMessage::System("sistema".into()),
        ChatMessage::User("hola".into()),
    ]
}

fn search_tool() -> ToolDef {
    ToolDef {
        name: "search_mail".into(),
        description: "busca".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }
}

async fn server(chat_body: &'static str) -> FakeServer {
    FakeServer::start(move |pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(TAGS),
        "/api/chat" => Reply::json(chat_body),
        _ => Reply::status(404),
    })
    .await
}

fn backend(server: &FakeServer) -> OllamaBackend {
    OllamaBackend::new(
        client(),
        local_selection("ollama", &server.url(""), None),
        false,
    )
}

fn chat_request(server: &FakeServer) -> serde_json::Value {
    server
        .captured()
        .iter()
        .find(|p| p.path == "/api/chat")
        .expect("hubo un pedido a /api/chat")
        .json()
}

#[test]
fn ollama_declara_soporte_de_herramientas() {
    let selection = local_selection("ollama", "http://127.0.0.1:11434", None);
    assert!(OllamaBackend::new(client(), selection, false).supports_tools());
}

#[test]
fn parse_chat_lee_las_llamadas_a_herramientas_con_argumentos_como_objeto() {
    let body = br#"{"message":{"role":"assistant","content":"","tool_calls":[
        {"function":{"name":"search_mail","arguments":{"query":"factura"}}},
        {"function":{"name":"read_mail"}}]}}"#;
    let calls = parse_chat(body).unwrap().message.tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].function.name, "search_mail");
    assert_eq!(calls[0].function.arguments, json!({"query": "factura"}));
    // Sin `arguments`, null: la habilidad lo rechaza con InvalidArgs.
    assert!(calls[1].function.arguments.is_null());
}

#[test]
fn parse_chat_sin_tool_calls_deja_la_lista_vacia() {
    let reply = parse_chat(br#"{"message":{"content":"hola"}}"#).unwrap();
    assert!(reply.message.tool_calls.is_empty());
}

#[tokio::test]
async fn una_respuesta_con_tool_calls_se_interpreta_como_llamadas_con_ids_propios() {
    let server = server(
        r#"{"message":{"content":"","tool_calls":[
            {"function":{"name":"search_mail","arguments":{"query":"x"}}},
            {"function":{"name":"read_mail","arguments":{"message_id":3}}}]}}"#,
    )
    .await;
    let turn = backend(&server)
        .chat_tools(&thread(), &[search_tool()])
        .await
        .unwrap();
    let ModelTurn::Calls(calls) = turn else {
        panic!("tenían que ser llamadas: {turn:?}");
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "search_mail");
    assert_eq!(calls[0].arguments, json!({"query": "x"}));
    assert_eq!(calls[1].arguments, json!({"message_id": 3}));
    // Ollama no manda id: se genera uno por llamada, y no se repiten.
    assert!(!calls[0].id.is_empty() && calls[0].id != calls[1].id);
}

#[tokio::test]
async fn una_respuesta_sin_tool_calls_es_texto() {
    let server = server(r#"{"message":{"content":"Hola, soy Lince."}}"#).await;
    let turn = backend(&server)
        .chat_tools(&thread(), &[search_tool()])
        .await
        .unwrap();
    assert_eq!(turn, ModelTurn::Text("Hola, soy Lince.".into()));
}

#[tokio::test]
async fn el_pedido_lleva_las_tools_en_formato_function_y_los_mensajes_con_rol_tool() {
    let server = server(r#"{"message":{"content":"ok"}}"#).await;
    let call = ToolCall {
        id: "x".into(),
        name: "search_mail".into(),
        arguments: json!({"query": "q"}),
    };
    let messages = vec![
        ChatMessage::System("sistema".into()),
        ChatMessage::User("hola".into()),
        ChatMessage::Assistant {
            text: String::new(),
            calls: vec![call],
        },
        ChatMessage::Tool {
            id: "x".into(),
            content: "[]".into(),
        },
        ChatMessage::Assistant {
            text: "listo".into(),
            calls: vec![],
        },
    ];
    backend(&server)
        .chat_tools(&messages, &[search_tool()])
        .await
        .unwrap();
    let sent = chat_request(&server);
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["model"], "llama3.2:latest");
    assert_eq!(
        sent["tools"],
        json!([{"type": "function", "function": {
            "name": "search_mail",
            "description": "busca",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}},
        }}])
    );
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "system", "content": "sistema"},
            {"role": "user", "content": "hola"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": "search_mail", "arguments": {"query": "q"}}}]},
            {"role": "tool", "content": "[]"},
            {"role": "assistant", "content": "listo"},
        ])
    );
}

#[tokio::test]
async fn chat_tools_local_only_con_un_modelo_local_responde() {
    let server = server(r#"{"message":{"content":"ok"}}"#).await;
    let turn = backend(&server)
        .chat_tools_local_only(&thread(), &[search_tool()])
        .await
        .unwrap();
    assert_eq!(turn, ModelTurn::Text("ok".into()));
}

#[tokio::test]
async fn chat_tools_local_only_con_el_modelo_en_la_nube_no_manda_el_chat() {
    let server = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(r#"{"models":[{"name":"gpt-oss:120b-cloud"}]}"#),
        "/api/chat" => Reply::json(r#"{"message":{"content":"no debió salir"}}"#),
        _ => Reply::status(404),
    })
    .await;
    let error = ollama_backend(&server.url(""), None, true)
        .chat_tools_local_only(&thread(), &[search_tool()])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::NotLocal), "{error:?}");
    assert!(server.captured().iter().all(|p| p.path != "/api/chat"));
}

#[tokio::test]
async fn si_la_respuesta_vino_de_un_modelo_remoto_se_descarta_con_el_error_que_corresponde() {
    let remote = r#"{"remote_host":"https://ollama.com:443","message":{"content":"","tool_calls":[
        {"function":{"name":"search_mail","arguments":{}}}]}}"#;
    for (allow_remote, local_only) in [(false, false), (false, true), (true, true)] {
        let server = FakeServer::start(move |pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(TAGS),
            "/api/chat" => Reply::json(remote),
            _ => Reply::status(404),
        })
        .await;
        let backend = ollama_backend(&server.url(""), None, allow_remote);
        let result = if local_only {
            backend
                .chat_tools_local_only(&thread(), &[search_tool()])
                .await
        } else {
            backend.chat_tools(&thread(), &[search_tool()]).await
        };
        match (allow_remote, result) {
            (false, Err(BackendError::NotAllowed(_))) => {}
            (true, Err(BackendError::RemoteAfterSend)) => {}
            (allow, other) => panic!("allow_remote={allow} local_only={local_only}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn con_allow_remote_el_chat_tools_comun_acepta_la_respuesta_remota() {
    let server = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(TAGS),
        "/api/chat" => {
            Reply::json(r#"{"remote_host":"https://ollama.com:443","message":{"content":"ok"}}"#)
        }
        _ => Reply::status(404),
    })
    .await;
    let turn = ollama_backend(&server.url(""), None, true)
        .chat_tools(&thread(), &[])
        .await
        .unwrap();
    assert_eq!(turn, ModelTurn::Text("ok".into()));
}

#[tokio::test]
async fn los_errores_http_del_chat_con_herramientas_se_mapean_como_en_el_chat_comun() {
    let not_found = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(TAGS),
        _ => Reply::status(404),
    })
    .await;
    let error = backend(&not_found)
        .chat_tools(&thread(), &[])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");

    let broken = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(TAGS),
        _ => Reply::status(500),
    })
    .await;
    let error = backend(&broken)
        .chat_tools(&thread(), &[])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::Failed(m) if m.contains("500")));

    let garbage = server("no es json").await;
    let error = backend(&garbage)
        .chat_tools(&thread(), &[])
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::Failed(_)), "{error:?}");
}

// ── Capacidad `tools` del modelo efectivo (/api/show) ───────────────────────────

#[test]
fn parse_capabilities_dice_si_el_modelo_tiene_la_capacidad_tools() {
    assert!(parse_capabilities(br#"{"capabilities":["completion","tools"]}"#).unwrap());
    assert!(!parse_capabilities(br#"{"capabilities":["completion"]}"#).unwrap());
    // Una versión vieja de Ollama no manda `capabilities`: no se asume.
    assert!(!parse_capabilities(b"{}").unwrap());
    assert!(matches!(
        parse_capabilities(b"no es json"),
        Err(BackendError::Failed(_))
    ));
}

#[tokio::test]
async fn active_supports_tools_pregunta_a_api_show_por_el_modelo_efectivo() {
    for (capabilities, expected) in [
        (r#"["completion","tools"]"#, true),
        (r#"["completion"]"#, false),
    ] {
        let server = FakeServer::start(move |pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(TAGS),
            "/api/show" => Reply::json(&format!(r#"{{"capabilities":{capabilities}}}"#)),
            _ => Reply::status(404),
        })
        .await;
        let answer = backend(&server).active_supports_tools().await.unwrap();
        assert_eq!(answer, expected, "{capabilities}");
        let show = server
            .captured()
            .into_iter()
            .find(|p| p.path == "/api/show")
            .expect("hubo un pedido a /api/show");
        assert_eq!(show.json()["model"], "llama3.2:latest");
    }
}

#[tokio::test]
async fn si_api_show_falla_active_supports_tools_da_error_y_no_afirma_nada() {
    let server = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/api/tags" => Reply::json(TAGS),
        _ => Reply::status(500),
    })
    .await;
    let error = backend(&server).active_supports_tools().await.unwrap_err();
    assert!(matches!(error, BackendError::Failed(m) if m.contains("500")));
}
