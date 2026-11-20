//! Pruebas del camino de herramientas de OpenAI: lo que se manda a
//! `/chat/completions` (las `tools`, los mensajes con rol `tool` y los argumentos
//! como texto) y cómo se lee `message.tool_calls`.

use serde_json::json;

use super::*;
use crate::test_support::{client, local_selection, FakeServer, FixedKey, Reply};

const MODELS: &str = r#"{"data":[{"id":"modelo-a"}]}"#;

fn search_tool() -> ToolDef {
    ToolDef {
        name: "search_mail".into(),
        description: "busca".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }
}

fn user_thread() -> Vec<ChatMessage> {
    vec![ChatMessage::User("hola".into())]
}

async fn server(chat: impl Fn() -> Reply + Send + Sync + 'static) -> FakeServer {
    FakeServer::start(move |pedido| match pedido.path.as_str() {
        "/v1/models" => Reply::json(MODELS),
        "/v1/chat/completions" => chat(),
        _ => Reply::status(404),
    })
    .await
}

fn backend_with(server: &FakeServer, key: Option<&str>, local: bool) -> OpenAiBackend {
    let mut selection = local_selection("openai", &server.url("/v1"), Some("m"));
    selection.local = local;
    OpenAiBackend::new(client(), selection, FixedKey::new(key))
}

fn backend(server: &FakeServer) -> OpenAiBackend {
    backend_with(server, None, true)
}

fn chat_request(server: &FakeServer) -> serde_json::Value {
    server
        .captured()
        .iter()
        .find(|p| p.path == "/v1/chat/completions")
        .expect("hubo un pedido a /chat/completions")
        .json()
}

fn message_with(tool_calls: &str) -> Vec<u8> {
    format!(r#"{{"choices":[{{"message":{{"role":"assistant","content":null,"tool_calls":{tool_calls}}}}}]}}"#)
        .into_bytes()
}

fn only_call(body: &[u8]) -> ToolCall {
    match parse_chat_tools(body).unwrap() {
        ModelTurn::Calls(mut calls) => {
            assert_eq!(calls.len(), 1);
            calls.remove(0)
        }
        other => panic!("se esperaban llamadas, dio {other:?}"),
    }
}

#[test]
fn openai_declara_soporte_de_herramientas() {
    let selection = local_selection("openai", "http://127.0.0.1:8080/v1", None);
    assert!(OpenAiBackend::new(client(), selection, FixedKey::new(None)).supports_tools());
}

#[tokio::test]
async fn chat_tools_manda_modelo_tools_y_el_hilo_en_formato_openai() {
    let server = server(|| Reply::json(r#"{"choices":[{"message":{"content":"ok"}}]}"#)).await;
    let thread = vec![
        ChatMessage::System("sistema".into()),
        ChatMessage::User("buscá facturas".into()),
        ChatMessage::Assistant {
            text: String::new(),
            calls: vec![ToolCall {
                id: "call_1".into(),
                name: "search_mail".into(),
                arguments: json!({"query": "factura"}),
            }],
        },
        ChatMessage::Tool {
            id: "call_1".into(),
            content: "2 resultados".into(),
        },
        ChatMessage::Assistant {
            text: "listo".into(),
            calls: vec![],
        },
    ];
    backend(&server)
        .chat_tools(&thread, &[search_tool()])
        .await
        .unwrap();

    let sent = chat_request(&server);
    assert_eq!(sent["model"], "m");
    assert_eq!(sent["stream"], false);
    assert_eq!(
        sent["tools"],
        json!([{"type": "function", "function": {
            "name": "search_mail",
            "description": "busca",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}},
        }}])
    );
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({"role": "system", "content": "sistema"}));
    assert_eq!(
        messages[1],
        json!({"role": "user", "content": "buscá facturas"})
    );
    let call = &messages[2]["tool_calls"][0];
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(call["id"], "call_1");
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "search_mail");
    // OpenAI exige los argumentos como texto JSON, no como objeto.
    let arguments = call["function"]["arguments"]
        .as_str()
        .expect("es un string");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(arguments).unwrap(),
        json!({"query": "factura"})
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "2 resultados"})
    );
    assert_eq!(
        messages[4],
        json!({"role": "assistant", "content": "listo"})
    );
}

#[tokio::test]
async fn chat_tools_lee_las_llamadas_conservando_el_id_y_parseando_los_argumentos() {
    let body = String::from_utf8(message_with(
        r#"[{"id":"call_abc","type":"function","function":{"name":"search_mail","arguments":"{\"query\":\"factura\"}"}}]"#,
    ))
    .unwrap();
    let server = server(move || Reply::json(&body)).await;
    let turn = backend(&server)
        .chat_tools(&user_thread(), &[search_tool()])
        .await
        .unwrap();
    assert_eq!(
        turn,
        ModelTurn::Calls(vec![ToolCall {
            id: "call_abc".into(),
            name: "search_mail".into(),
            arguments: json!({"query": "factura"}),
        }])
    );
}

#[test]
fn una_llamada_sin_id_recibe_uno_generado() {
    let call = only_call(&message_with(
        r#"[{"function":{"name":"search_mail","arguments":"{}"}}]"#,
    ));
    assert!(!call.id.is_empty());
    let vacio = only_call(&message_with(
        r#"[{"id":"","function":{"name":"search_mail","arguments":"{}"}}]"#,
    ));
    assert!(!vacio.id.is_empty());
    let otra = only_call(&message_with(
        r#"[{"id":"","function":{"name":"search_mail","arguments":"{}"}}]"#,
    ));
    assert_ne!(vacio.id, otra.id);
}

#[test]
fn argumentos_vacios_o_ilegibles_dan_objeto_vacio_y_los_validos_se_parsean() {
    for raw in ["", "   ", "no-json"] {
        let body = message_with(&format!(
            r#"[{{"id":"x","function":{{"name":"t","arguments":{}}}}}]"#,
            serde_json::to_string(raw).unwrap()
        ));
        assert_eq!(only_call(&body).arguments, json!({}), "{raw:?}");
    }
    let sin_campo = message_with(r#"[{"id":"x","function":{"name":"t"}}]"#);
    assert_eq!(only_call(&sin_campo).arguments, json!({}));
    let valido = message_with(r#"[{"id":"x","function":{"name":"t","arguments":"{\"a\":1}"}}]"#);
    assert_eq!(only_call(&valido).arguments, json!({"a": 1}));
}

#[test]
fn una_respuesta_con_solo_texto_da_text() {
    let body = br#"{"choices":[{"message":{"role":"assistant","content":"Hola"}}]}"#;
    assert_eq!(
        parse_chat_tools(body).unwrap(),
        ModelTurn::Text("Hola".into())
    );
}

#[test]
fn sin_texto_ni_llamadas_o_sin_opciones_o_ilegible_da_fallo_sin_citar_al_servidor() {
    let sin_texto = br#"{"choices":[{"message":{"role":"assistant","content":null}}]}"#;
    let sin_opciones = br#"{"choices":[]}"#;
    let ilegible = br#"{"choices":"CANARIO-DEL-SERVIDOR"}"#;
    for body in [&sin_texto[..], &sin_opciones[..], &ilegible[..]] {
        match parse_chat_tools(body) {
            Err(BackendError::Failed(text)) => assert!(!text.contains("CANARIO"), "{text}"),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }
    assert_eq!(
        parse_chat_tools(b"no es json").unwrap_err(),
        BackendError::Failed("respuesta del modelo ilegible".into())
    );
}

#[tokio::test]
async fn chat_tools_local_only_no_manda_nada_si_el_modelo_no_es_local() {
    let server = server(|| Reply::json(r#"{"choices":[{"message":{"content":"ok"}}]}"#)).await;
    let remote = backend_with(&server, None, false);
    let result = remote
        .chat_tools_local_only(&user_thread(), &[search_tool()])
        .await;
    assert_eq!(result, Err(BackendError::NotLocal));
    assert!(server.captured().is_empty());

    let local = backend_with(&server, None, true);
    let turn = local
        .chat_tools_local_only(&user_thread(), &[search_tool()])
        .await
        .unwrap();
    assert_eq!(turn, ModelTurn::Text("ok".into()));
    assert!(!server.captured().is_empty());
}

#[tokio::test]
async fn un_401_o_un_403_en_chat_tools_dan_no_disponible_sin_la_clave() {
    for status in [401, 403] {
        let server = FakeServer::start(move |_| Reply::status(status)).await;
        let backend = backend_with(&server, Some("sk-prueba"), true);
        match backend.chat_tools(&user_thread(), &[search_tool()]).await {
            Err(BackendError::Unavailable(text)) => {
                assert!(text.contains("pide una clave"), "{status}: {text}");
                assert!(!text.contains("sk-prueba"));
            }
            other => panic!("{status}: se esperaba Unavailable, dio {other:?}"),
        }
    }
}
