//! Pruebas en proceso del servidor IPC: despacho, rechazo, una conexión por
//! `UnixStream::pair`, el plazo de lectura con el reloj pausado y el bucle de
//! aceptación sobre un socket temporal.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

use vasak_assistant_ipc::{
    read_frame, write_frame, BackendError, BrokerEnvelope, BrokerRequest, BrokerResponse,
    ChatMessage, ModelTurn, ToolDef,
};
use vasak_assistant_protocol::{Message, ModelInfo, Role};

use super::*;
use crate::test_support::{client, local_selection, FakeServer, Reply};

const MODELS: &str = r#"{"data":[{"id":"modelo-a"},{"id":"modelo-b"}]}"#;
const COMPLETION: &str =
    r#"{"choices":[{"message":{"role":"assistant","content":"Hola, soy Lince."}}]}"#;

/// Un servidor compatible con OpenAI y un estado listo contra él.
async fn ready() -> (FakeServer, State) {
    let server = FakeServer::start(|pedido| match pedido.path.as_str() {
        "/v1/models" => Reply::json(MODELS),
        "/v1/chat/completions" => Reply::json(COMPLETION),
        _ => Reply::status(404),
    })
    .await;
    let selection = local_selection("openai", &server.url("/v1"), Some("modelo-a"));
    let state = State::from_resolved(
        client(),
        Resolved::Ready {
            selection,
            allow_remote: false,
        },
    );
    (server, state)
}

fn rejected() -> (State, Rejection) {
    let rejection = Rejection {
        info: ModelInfo {
            provider: "raro".into(),
            name: String::new(),
            local: true,
            host: String::new(),
        },
        reason: "proveedor desconocido".into(),
    };
    (
        State::from_resolved(client(), Resolved::Rejected(rejection.clone())),
        rejection,
    )
}

fn hello() -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: "hola".into(),
    }]
}

fn thread() -> Vec<ChatMessage> {
    vec![ChatMessage::User("hola".into())]
}

fn tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "t".into(),
        description: "d".into(),
        parameters: serde_json::json!({"type": "object"}),
    }]
}

// ── handle con backend listo ──────────────────────────────────────────────────

#[tokio::test]
async fn con_un_backend_listo_cada_pedido_da_su_variante_de_respuesta() {
    let (_server, state) = ready().await;

    let r = |request| state.handle(request, Some("sk-x".into()), None);
    assert!(
        matches!(r(BrokerRequest::Info).await, BrokerResponse::Info(i) if i.provider == "openai")
    );
    assert_eq!(
        r(BrokerRequest::Models).await,
        BrokerResponse::Models(Ok(vec!["modelo-a".into(), "modelo-b".into()]))
    );
    assert!(matches!(
        r(BrokerRequest::ActiveModel).await,
        BrokerResponse::ActiveModel(Ok(i)) if i.name == "modelo-a"
    ));
    assert_eq!(
        r(BrokerRequest::SupportsTools).await,
        BrokerResponse::SupportsTools(true)
    );
    assert_eq!(
        r(BrokerRequest::ActiveSupportsTools).await,
        BrokerResponse::Bool(Ok(true))
    );
    for request in [
        BrokerRequest::Chat { messages: hello() },
        BrokerRequest::ChatLocalOnly { messages: hello() },
    ] {
        assert_eq!(
            r(request).await,
            BrokerResponse::Text(Ok("Hola, soy Lince.".into()))
        );
    }
    for request in [
        BrokerRequest::ChatTools {
            messages: thread(),
            tools: tools(),
        },
        BrokerRequest::ChatToolsLocalOnly {
            messages: thread(),
            tools: tools(),
        },
    ] {
        assert_eq!(
            r(request).await,
            BrokerResponse::Turn(Ok(ModelTurn::Text("Hola, soy Lince.".into())))
        );
    }
}

#[tokio::test]
async fn un_backend_ollama_listo_tambien_se_arma() {
    // Sin servidor: sólo importa que el despacho elija Ollama y que su error de
    // red salga como respuesta, no como pánico.
    let selection = local_selection("ollama", &crate::test_support::closed_port_url(), None);
    let state = State::from_resolved(
        client(),
        Resolved::Ready {
            selection,
            allow_remote: false,
        },
    );
    assert!(matches!(
        state.handle(BrokerRequest::Models, None, None).await,
        BrokerResponse::Models(Err(_))
    ));
}

// ── handle con la config rechazada ────────────────────────────────────────────

#[tokio::test]
async fn con_la_config_rechazada_todo_da_no_disponible_con_el_motivo() {
    let (state, rejection) = rejected();
    fn unavailable<T>(reason: &str) -> Result<T, BackendError> {
        Err(BackendError::Unavailable(reason.to_owned()))
    }
    let reason = rejection.reason.as_str();

    let r = |request| state.handle(request, None, None);
    assert_eq!(
        r(BrokerRequest::Info).await,
        BrokerResponse::Info(rejection.info.clone())
    );
    assert_eq!(
        r(BrokerRequest::Models).await,
        BrokerResponse::Models(unavailable(reason))
    );
    assert_eq!(
        r(BrokerRequest::ActiveModel).await,
        BrokerResponse::ActiveModel(unavailable(reason))
    );
    assert_eq!(
        r(BrokerRequest::SupportsTools).await,
        BrokerResponse::SupportsTools(false)
    );
    assert_eq!(
        r(BrokerRequest::ActiveSupportsTools).await,
        BrokerResponse::Bool(unavailable(reason))
    );
    for request in [
        BrokerRequest::Chat { messages: hello() },
        BrokerRequest::ChatLocalOnly { messages: hello() },
    ] {
        assert_eq!(r(request).await, BrokerResponse::Text(unavailable(reason)));
    }
    for request in [
        BrokerRequest::ChatTools {
            messages: thread(),
            tools: tools(),
        },
        BrokerRequest::ChatToolsLocalOnly {
            messages: thread(),
            tools: tools(),
        },
    ] {
        assert_eq!(r(request).await, BrokerResponse::Turn(unavailable(reason)));
    }
}

#[test]
fn desde_la_config_un_proveedor_desconocido_queda_rechazado_y_uno_valido_listo() {
    let bad = Settings {
        backend: Some("raro".into()),
        ..Settings::default()
    };
    assert!(matches!(
        State::from_settings(client(), &bad).resolved,
        Resolved::Rejected(_)
    ));
    assert!(matches!(
        State::from_settings(client(), &Settings::default()).resolved,
        Resolved::Ready { .. }
    ));
}

// ── serve ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn serve_contesta_un_sobre_enmarcado_por_el_par_de_sockets() {
    let (_server, state) = ready().await;
    let (mut client_end, server_end) = UnixStream::pair().unwrap();

    let envelope = BrokerEnvelope {
        api_key: None,
        key_base_url: None,
        request: BrokerRequest::Chat { messages: hello() },
    };
    write_frame(&mut client_end, &envelope).await.unwrap();
    serve(server_end, &state).await.unwrap();

    let response: BrokerResponse = read_frame(&mut client_end).await.unwrap();
    assert_eq!(
        response,
        BrokerResponse::Text(Ok("Hola, soy Lince.".into()))
    );
}

#[tokio::test]
async fn serve_con_un_sobre_ilegible_da_error_y_no_panica() {
    let (state, _) = rejected();
    let (mut client_end, server_end) = UnixStream::pair().unwrap();
    client_end
        .write_all(&[0, 0, 0, 4, b'n', b'o', b'p', b'e'])
        .await
        .unwrap();
    assert!(serve(server_end, &state).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn serve_cierra_sin_colgar_al_cliente_que_no_manda_el_marco() {
    let (state, _) = rejected();
    // `_idle` queda abierto: no hay EOF, sólo silencio.
    let (_idle, server_end) = UnixStream::pair().unwrap();

    let started = tokio::time::Instant::now();
    // Con el reloj pausado, el runtime lo adelanta solo cuando no hay nada más
    // que hacer: el plazo de 10 s se cumple sin esperarlo de verdad.
    let result = serve(server_end, &state).await;

    assert!(result.is_ok());
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(11));
}

// ── run ───────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_atiende_una_conexion_por_el_socket_y_sigue_aceptando() {
    let (_server, state) = ready().await;
    let dir = std::env::temp_dir().join(format!("vasak-broker-run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("b.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let task = tokio::spawn(run(listener, Arc::new(state)));

    // Dos viajes seguidos: el bucle no se cierra tras el primero.
    for _ in 0..2 {
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let envelope = BrokerEnvelope {
            api_key: None,
            key_base_url: None,
            request: BrokerRequest::Models,
        };
        write_frame(&mut stream, &envelope).await.unwrap();
        let response: BrokerResponse = read_frame(&mut stream).await.unwrap();
        assert!(matches!(response, BrokerResponse::Models(Ok(_))));
    }
    assert!(!task.is_finished());

    task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── La clave sólo viaja si es para la dirección de la config ─────────────────

/// La dirección base normalizada de la config del broker en `ready()`.
fn base_of(server: &FakeServer) -> String {
    crate::config::normalize_url(&server.url("/v1"))
        .unwrap()
        .to_string()
}

fn authorization(server: &FakeServer) -> Vec<Option<String>> {
    server
        .captured()
        .iter()
        .map(|c| c.header("authorization").map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn la_clave_se_usa_si_key_base_url_es_la_direccion_de_la_config() {
    let (server, state) = ready().await;
    let base = base_of(&server);

    state
        .handle(BrokerRequest::Models, Some("sk-buena".into()), Some(base))
        .await;

    assert_eq!(authorization(&server), [Some("Bearer sk-buena".to_owned())]);
}

#[tokio::test]
async fn la_clave_se_descarta_si_key_base_url_es_de_otra_direccion_o_falta() {
    let (server, state) = ready().await;

    for other in [Some("https://otro.example/v1/".to_owned()), None] {
        state
            .handle(BrokerRequest::Models, Some("sk-ajena".into()), other)
            .await;
    }

    // El pedido salió igual, pero sin clave: nunca con la de otro proveedor.
    assert_eq!(authorization(&server), [None, None]);
}

#[tokio::test]
async fn el_destino_sale_de_la_config_y_nunca_de_key_base_url() {
    let (server, state) = ready().await;
    let decoy = FakeServer::start(|_| Reply::json(MODELS)).await;

    // `key_base_url` apunta al señuelo: no se conecta ahí, ni con ni sin clave.
    state
        .handle(
            BrokerRequest::Models,
            Some("sk-x".into()),
            Some(base_of(&decoy)),
        )
        .await;

    assert!(decoy.captured().is_empty(), "el broker conectó al señuelo");
    assert_eq!(server.captured().len(), 1);
    assert_eq!(authorization(&server), [None]);
}

// ── WRITE_TIMEOUT ─────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn serve_no_se_cuelga_con_un_cliente_que_manda_el_pedido_y_no_lee() {
    // Una respuesta más grande que el búfer del socket: la escritura se traba
    // mientras el cliente no lea.
    let reason = "x".repeat(400 * 1024);
    let state = State::from_resolved(
        client(),
        Resolved::Rejected(Rejection {
            info: ModelInfo {
                provider: "raro".into(),
                name: String::new(),
                local: true,
                host: String::new(),
            },
            reason,
        }),
    );
    let (mut client_end, server_end) = UnixStream::pair().unwrap();
    let envelope = BrokerEnvelope {
        api_key: None,
        key_base_url: None,
        request: BrokerRequest::Models,
    };
    write_frame(&mut client_end, &envelope).await.unwrap();

    let started = tokio::time::Instant::now();
    // El plazo externo hace fallar la prueba (en vez de colgarla) si `serve`
    // no tiene su propio plazo de escritura.
    let result = tokio::time::timeout(Duration::from_secs(120), serve(server_end, &state)).await;

    assert!(matches!(result, Ok(Ok(()))), "serve no terminó: {result:?}");
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(11));
    drop(client_end);
}
