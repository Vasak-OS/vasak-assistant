//! Pruebas del cliente del puente: contra un socket de mentira que contesta
//! `BrokerResponse` prearmados, y contra un socket caído.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

use vasak_assistant_ipc::config::{select_backend, Settings};
use vasak_assistant_protocol::{Message, ModelInfo, Role};

use super::*;
use crate::backend::ToolCall;
use crate::test_support::{model_info, FixedKey, TempDir};

/// Un broker de mentira: un `UnixListener` que contesta con lo que devuelva
/// `handler` y guarda cada sobre que le llegó.
struct FakeBrokerSocket {
    _dir: TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<BrokerEnvelope>>>,
}

impl FakeBrokerSocket {
    async fn start(
        handler: impl Fn(&BrokerRequest) -> BrokerResponse + Send + Sync + 'static,
    ) -> Self {
        let dir = TempDir::new();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).expect("el socket de prueba se abre");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let Ok(envelope) = read_frame::<_, BrokerEnvelope>(&mut stream).await else {
                        return;
                    };
                    let response = handler(&envelope.request);
                    log.lock().push(envelope);
                    let _ = write_frame(&mut stream, &response).await;
                });
            }
        });
        Self {
            _dir: dir,
            path,
            seen,
        }
    }

    /// Un socket que acepta y cierra sin contestar (el broker que se cae a
    /// media conversación).
    async fn hanging_up() -> Self {
        Self::raw(|mut stream| async move {
            let _ = read_frame::<_, BrokerEnvelope>(&mut stream).await;
        })
        .await
    }

    /// Un socket que contesta con bytes crudos que no son un marco entero.
    async fn raw<F, Fut>(on_connection: F) -> Self
    where
        F: Fn(tokio::net::UnixStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let dir = TempDir::new();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).expect("el socket de prueba se abre");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(on_connection(stream));
            }
        });
        Self {
            _dir: dir,
            path,
            seen: Arc::default(),
        }
    }

    fn client(&self, backend: &str, key: Option<&str>) -> BrokerClient {
        client_at(&self.path, backend, key)
    }

    fn requests(&self) -> Vec<BrokerRequest> {
        self.seen.lock().iter().map(|e| e.request.clone()).collect()
    }
}

fn client_at(socket: &Path, backend: &str, key: Option<&str>) -> BrokerClient {
    let selection = select_backend(&Settings {
        backend: Some(backend.into()),
        url: Some("http://127.0.0.1:9/v1".into()),
        ..Settings::default()
    })
    .expect("la selección local se acepta");
    BrokerClient::new(selection, FixedKey::new(key), socket.to_path_buf())
}

fn user(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: text.into(),
    }]
}

fn thread() -> Vec<ChatMessage> {
    vec![ChatMessage::User("hola".into())]
}

fn tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "leer_correo".into(),
        description: "lee".into(),
        parameters: serde_json::json!({"type": "object"}),
    }]
}

fn is_unavailable<T: std::fmt::Debug>(result: Result<T, BackendError>) -> bool {
    matches!(result, Err(BackendError::Unavailable(_)))
}

/// Un broker que contesta bien cada variante, con valores reconocibles.
async fn well_behaved() -> FakeBrokerSocket {
    FakeBrokerSocket::start(|request| match request {
        BrokerRequest::Info => BrokerResponse::Info(model_info("x")),
        BrokerRequest::Models => BrokerResponse::Models(Ok(vec!["a".into(), "b".into()])),
        BrokerRequest::ActiveModel => BrokerResponse::ActiveModel(Ok(model_info("activo"))),
        BrokerRequest::SupportsTools => BrokerResponse::SupportsTools(true),
        BrokerRequest::ActiveSupportsTools => BrokerResponse::Bool(Ok(false)),
        BrokerRequest::Chat { .. } => BrokerResponse::Text(Ok("chat".into())),
        BrokerRequest::ChatLocalOnly { .. } => BrokerResponse::Text(Ok("local".into())),
        BrokerRequest::ChatTools { .. } => {
            BrokerResponse::Turn(Ok(ModelTurn::Text("tools".into())))
        }
        BrokerRequest::ChatToolsLocalOnly { .. } => {
            BrokerResponse::Turn(Ok(ModelTurn::Calls(vec![ToolCall {
                id: "1".into(),
                name: "leer_correo".into(),
                arguments: serde_json::json!({}),
            }])))
        }
    })
    .await
}

// ── Ida y vuelta ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn cada_metodo_del_rasgo_hace_el_viaje_y_devuelve_lo_que_contesta_el_broker() {
    let broker = well_behaved().await;
    let client = broker.client("ollama", None);
    let messages = user("hola");
    let thread = thread();
    let tools = tools();

    assert_eq!(client.models().await, Ok(vec!["a".into(), "b".into()]));
    assert_eq!(client.active_model().await, Ok(model_info("activo")));
    assert_eq!(client.active_supports_tools().await, Ok(false));
    assert_eq!(client.chat(&messages).await, Ok("chat".into()));
    assert_eq!(client.chat_local_only(&messages).await, Ok("local".into()));
    assert_eq!(
        client.chat_tools(&thread, &tools).await,
        Ok(ModelTurn::Text("tools".into()))
    );
    assert!(matches!(
        client.chat_tools_local_only(&thread, &tools).await,
        Ok(ModelTurn::Calls(calls)) if calls[0].name == "leer_correo"
    ));

    // Cada método mandó la variante que le toca, con sus argumentos.
    assert_eq!(
        broker.requests(),
        vec![
            BrokerRequest::Models,
            BrokerRequest::ActiveModel,
            BrokerRequest::ActiveSupportsTools,
            BrokerRequest::Chat {
                messages: messages.clone()
            },
            BrokerRequest::ChatLocalOnly { messages },
            BrokerRequest::ChatTools {
                messages: thread.clone(),
                tools: tools.clone()
            },
            BrokerRequest::ChatToolsLocalOnly {
                messages: thread,
                tools
            },
        ]
    );
}

#[tokio::test]
async fn un_error_del_broker_llega_tal_cual_al_que_llama() {
    let broker =
        FakeBrokerSocket::start(|_| BrokerResponse::Text(Err(BackendError::NotLocal))).await;
    let client = broker.client("ollama", None);
    assert_eq!(
        client.chat_local_only(&user("x")).await,
        Err(BackendError::NotLocal)
    );
}

#[tokio::test]
async fn info_y_supports_tools_salen_de_la_config_propia_sin_ir_al_broker() {
    let broker = well_behaved().await;
    let client = broker.client("ollama", None);
    let info: ModelInfo = client.info();
    assert_eq!(info.provider, "ollama");
    assert!(info.local);
    assert!(client.supports_tools());
    assert!(broker.requests().is_empty(), "no debió conectarse");
}

#[tokio::test]
async fn la_clave_viaja_en_el_sobre_solo_con_openai() {
    let broker = well_behaved().await;
    broker
        .client("openai", Some("sk-secreta"))
        .models()
        .await
        .unwrap();
    broker
        .client("ollama", Some("sk-secreta"))
        .models()
        .await
        .unwrap();
    let keys: Vec<Option<String>> = broker
        .seen
        .lock()
        .iter()
        .map(|e| e.api_key.clone())
        .collect();
    assert_eq!(keys, vec![Some("sk-secreta".to_owned()), None]);
}

// ── Puente caído: Unavailable, nunca pánico ──────────────────────────────────

/// Todos los métodos asíncronos del rasgo contra `client`: cada uno tiene que
/// dar `Unavailable`.
async fn assert_every_method_unavailable(client: &BrokerClient) {
    let messages = user("hola");
    let thread = thread();
    let tools = tools();
    assert!(is_unavailable(client.models().await), "models");
    assert!(is_unavailable(client.active_model().await), "active_model");
    assert!(
        is_unavailable(client.active_supports_tools().await),
        "active_supports_tools"
    );
    assert!(is_unavailable(client.chat(&messages).await), "chat");
    assert!(
        is_unavailable(client.chat_local_only(&messages).await),
        "chat_local_only"
    );
    assert!(
        is_unavailable(client.chat_tools(&thread, &tools).await),
        "chat_tools"
    );
    assert!(
        is_unavailable(client.chat_tools_local_only(&thread, &tools).await),
        "chat_tools_local_only"
    );
}

#[tokio::test]
async fn con_el_socket_inexistente_todo_da_no_disponible() {
    let dir = TempDir::new();
    let client = client_at(&dir.path().join("no-hay.sock"), "ollama", None);
    assert_every_method_unavailable(&client).await;
}

#[tokio::test]
async fn con_el_socket_cerrado_a_media_conversacion_todo_da_no_disponible() {
    let broker = FakeBrokerSocket::hanging_up().await;
    assert_every_method_unavailable(&broker.client("ollama", None)).await;
}

#[tokio::test]
async fn una_respuesta_cortada_a_la_mitad_da_no_disponible() {
    // Declara 100 bytes y manda 3.
    let broker = FakeBrokerSocket::raw(|mut stream| async move {
        let _ = read_frame::<_, BrokerEnvelope>(&mut stream).await;
        let _ = stream.write_all(&[0, 0, 0, 100, b'{', b'"', b'a']).await;
    })
    .await;
    assert_every_method_unavailable(&broker.client("ollama", None)).await;
}

#[tokio::test]
async fn una_respuesta_que_no_es_json_da_no_disponible() {
    let broker = FakeBrokerSocket::raw(|mut stream| async move {
        let _ = read_frame::<_, BrokerEnvelope>(&mut stream).await;
        let _ = stream
            .write_all(&[0, 0, 0, 4, b'n', b'o', b'p', b'e'])
            .await;
    })
    .await;
    assert_every_method_unavailable(&broker.client("ollama", None)).await;
}

#[tokio::test]
async fn una_respuesta_de_otra_variante_se_trata_como_puente_caido() {
    // Pide `models` y el broker contesta con la variante de `chat`.
    let broker = FakeBrokerSocket::start(|_| BrokerResponse::Text(Ok("x".into()))).await;
    let client = broker.client("ollama", None);
    assert!(is_unavailable(client.models().await));
    assert!(is_unavailable(client.active_model().await));
    assert!(is_unavailable(client.active_supports_tools().await));
    assert!(is_unavailable(client.chat_tools(&thread(), &tools()).await));
}
