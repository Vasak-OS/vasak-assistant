//! El backend de Ollama, con su API propia.
//!
//! Usa dos rutas: `/api/tags` (qué modelos hay, y de paso si Ollama está) y
//! `/api/chat` sin streaming (la respuesta entera de una vez). Las defensas
//! comunes —tiempos, tope, sin redirecciones ni proxy— están en [`crate::http`].

use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::Selection;
use crate::http::{self, CHAT_TIMEOUT, LIST_TIMEOUT};

pub struct OllamaBackend {
    client: Client,
    selection: Selection,
    /// Si la configuración permite que la conversación salga de la máquina: lo
    /// decide el modelo por omisión, que puede resultar ser uno `cloud`.
    allow_remote: bool,
}

impl OllamaBackend {
    pub fn new(client: Client, selection: Selection, allow_remote: bool) -> Self {
        Self {
            client,
            selection,
            allow_remote,
        }
    }

    fn base(&self) -> &Url {
        &self.selection.url
    }

    async fn installed_models(&self) -> Result<Vec<String>, BackendError> {
        let response = self
            .client
            .get(http::endpoint(self.base(), "api/tags")?)
            .timeout(LIST_TIMEOUT)
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
        tracing::debug!(
            backend = "ollama",
            path = "api/tags",
            status = response.status().as_u16(),
            "respuesta HTTP"
        );
        if !response.status().is_success() {
            return Err(BackendError::Failed(format!(
                "Ollama respondió {} a la lista de modelos",
                response.status()
            )));
        }
        parse_tags(&http::read_capped(self.base(), response).await?)
    }

    async fn resolve_model(&self) -> Result<String, BackendError> {
        let installed = self.installed_models().await?;
        let model = pick_model(self.selection.model.as_deref(), &installed)?;
        // El modelo por omisión puede ser uno `cloud`: sale de la máquina, así
        // que sin `allow_remote` no se usa. El configurado ya se filtró al
        // elegir el backend (`select_backend`).
        if !self.allow_remote && crate::config::is_ollama_cloud_model(&model) {
            return Err(BackendError::Unavailable(format!(
                "El modelo «{model}» de Ollama corre en la nube: la conversación saldría de la máquina. Para usarlo hay que permitirlo con «allow_remote»."
            )));
        }
        Ok(model)
    }

    /// Una sola resolución del modelo para decidir y mandar. Con
    /// `require_local`, si el modelo resuelto no es local (`cloud`, o una
    /// dirección remota) no se manda nada.
    async fn send_chat(
        &self,
        messages: &[Message],
        require_local: bool,
    ) -> Result<String, BackendError> {
        let model = self.resolve_model().await?;
        if require_local && !(self.info().local && !crate::config::is_ollama_cloud_model(&model)) {
            return Err(crate::backend::not_local());
        }
        let response = self
            .client
            .post(http::endpoint(self.base(), "api/chat")?)
            .timeout(CHAT_TIMEOUT)
            .json(&ChatRequest {
                model: &model,
                messages,
                stream: false,
            })
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
        tracing::debug!(
            backend = "ollama",
            path = "api/chat",
            model = model.as_str(),
            status = response.status().as_u16(),
            "respuesta HTTP"
        );
        match response.status() {
            status if status.is_success() => {}
            // Ollama contesta 404 cuando el modelo no está: se pudo borrar entre
            // la lista y el pedido.
            StatusCode::NOT_FOUND => return Err(model_missing(&model)),
            status => {
                return Err(BackendError::Failed(format!(
                    "Ollama respondió {status} al pedido de chat"
                )))
            }
        }
        parse_chat(&http::read_capped(self.base(), response).await?)
    }
}

impl ModelBackend for OllamaBackend {
    fn info(&self) -> ModelInfo {
        self.selection.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(self.installed_models())
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            let name = self.resolve_model().await?;
            // Un modelo por omisión que resultó `cloud` no es local aunque la
            // dirección sea el loopback: que el estado lo diga.
            let mut info = self.info();
            info.local = info.local && !crate::config::is_ollama_cloud_model(&name);
            info.name = name;
            Ok(info)
        })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(self.send_chat(messages, false))
    }

    fn chat_local_only<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(self.send_chat(messages, true))
    }
}

// ── Lo que va y viene ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagsModel>,
}

#[derive(Deserialize)]
struct TagsModel {
    name: String,
}

/// La respuesta de `/api/chat`: el texto del modelo.
pub fn parse_chat(body: &[u8]) -> Result<String, BackendError> {
    serde_json::from_slice::<ChatResponse>(body)
        .map(|response| response.message.content)
        .map_err(|_| BackendError::Failed("respuesta de Ollama ilegible".into()))
}

/// La respuesta de `/api/tags`: los nombres de los modelos instalados.
pub fn parse_tags(body: &[u8]) -> Result<Vec<String>, BackendError> {
    serde_json::from_slice::<TagsResponse>(body)
        .map(|tags| tags.models.into_iter().map(|model| model.name).collect())
        .map_err(|_| BackendError::Failed("lista de modelos ilegible".into()))
}

/// Con qué modelo responder. Si hay uno configurado tiene que estar instalado;
/// Ollama le agrega `:latest` al nombre sin etiqueta, así que `llama3.2`
/// encuentra a `llama3.2:latest`. Si no hay ninguno configurado, el primero
/// instalado.
pub fn pick_model(configured: Option<&str>, installed: &[String]) -> Result<String, BackendError> {
    match configured {
        Some(wanted) => installed
            .iter()
            .find(|name| *name == wanted || name.strip_suffix(":latest") == Some(wanted))
            .cloned()
            .ok_or_else(|| model_missing(wanted)),
        None => installed.first().cloned().ok_or_else(|| {
            BackendError::Unavailable(
                "Ollama no tiene ningún modelo instalado. Instalá uno con `ollama pull <modelo>`."
                    .into(),
            )
        }),
    }
}

fn model_missing(model: &str) -> BackendError {
    BackendError::Unavailable(format!(
        "El modelo «{model}» no está instalado en Ollama. Instalalo con `ollama pull {model}`."
    ))
}

#[cfg(test)]
mod tests {
    /// El tope del contrato, escrito a mano: si alguien lo sube, estas pruebas lo notan.
    const TOPE: usize = 256 * 1024;

    use super::*;
    use crate::test_support::{client, closed_port_url, local_selection, FakeServer, Reply};
    use vasak_assistant_protocol::Role;

    const TAGS: &str = r#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5:7b"}]}"#;
    const CHAT: &str = r#"{"message":{"role":"assistant","content":"Hola, soy Lince."}}"#;

    fn backend(server: &FakeServer, model: Option<&str>) -> OllamaBackend {
        OllamaBackend::new(
            client(),
            local_selection("ollama", &server.url(""), model),
            false,
        )
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.into(),
        }
    }

    /// Un Ollama que tiene los modelos de `TAGS` y contesta `chat` al chat.
    async fn ollama(chat: impl Fn() -> Reply + Send + Sync + 'static) -> FakeServer {
        FakeServer::start(move |pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(TAGS),
            "/api/chat" => chat(),
            _ => Reply::status(404),
        })
        .await
    }

    /// Un Ollama cuyo único modelo —el que se toma por omisión— es `cloud`.
    const CLOUD_TAGS: &str = r#"{"models":[{"name":"gpt-oss:120b-cloud"}]}"#;

    async fn ollama_cloud() -> FakeServer {
        FakeServer::start(|pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(CLOUD_TAGS),
            "/api/chat" => Reply::json(CHAT),
            _ => Reply::status(404),
        })
        .await
    }

    fn cloud_backend(server: &FakeServer, allow_remote: bool) -> OllamaBackend {
        OllamaBackend::new(
            client(),
            local_selection("ollama", &server.url(""), None),
            allow_remote,
        )
    }

    /// #14: el modelo por omisión que resulta ser uno `cloud` no se usa sin
    /// `allow_remote` —ni para el estado ni para el chat—: saldría de la máquina.
    #[tokio::test]
    async fn un_modelo_por_omision_cloud_sin_allow_remote_no_se_usa() {
        let server = ollama_cloud().await;
        let backend = cloud_backend(&server, false);
        match backend.active_model().await {
            Err(BackendError::Unavailable(text)) => {
                assert!(
                    text.contains("nube") && text.contains("allow_remote"),
                    "{text}"
                );
            }
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
        match backend.chat(&[user("hola")]).await {
            Err(BackendError::Unavailable(_)) => {}
            other => panic!("el chat tenía que rechazarse, dio {other:?}"),
        }
    }

    /// Con `allow_remote`, el mismo modelo por omisión `cloud` se usa, y el
    /// estado lo muestra como remoto aunque la dirección sea el loopback.
    #[tokio::test]
    async fn un_modelo_por_omision_cloud_con_allow_remote_se_usa_y_se_muestra_remoto() {
        let server = ollama_cloud().await;
        let active = cloud_backend(&server, true).active_model().await.unwrap();
        assert_eq!(active.name, "gpt-oss:120b-cloud");
        assert!(!active.local, "un modelo cloud no es local");
    }

    #[test]
    fn pick_model_sin_configurar_toma_el_primero_instalado() {
        let instalados = vec!["a:1".to_owned(), "b:2".to_owned()];
        assert_eq!(pick_model(None, &instalados).unwrap(), "a:1");
    }

    #[test]
    fn pick_model_sin_nada_instalado_da_no_disponible_y_dice_como_instalar() {
        match pick_model(None, &[]) {
            Err(BackendError::Unavailable(text)) => assert!(text.contains("ollama pull")),
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
    }

    #[test]
    fn pick_model_encuentra_el_nombre_exacto_y_el_que_ollama_etiqueta_latest() {
        let instalados = vec!["llama3.2:latest".to_owned(), "qwen2.5:7b".to_owned()];
        assert_eq!(
            pick_model(Some("llama3.2"), &instalados).unwrap(),
            "llama3.2:latest"
        );
        assert_eq!(
            pick_model(Some("qwen2.5:7b"), &instalados).unwrap(),
            "qwen2.5:7b"
        );
        assert_eq!(
            pick_model(Some("llama3.2:latest"), &instalados).unwrap(),
            "llama3.2:latest"
        );
    }

    #[test]
    fn pick_model_no_confunde_un_prefijo_ni_otra_etiqueta_con_el_modelo() {
        let instalados = vec!["qwen2.5:7b".to_owned(), "llama3.2:1b".to_owned()];
        for pedido in ["qwen2.5", "qwen", "llama3.2"] {
            match pick_model(Some(pedido), &instalados) {
                Err(BackendError::Unavailable(text)) => assert!(text.contains(pedido)),
                other => panic!("{pedido}: se esperaba Unavailable, dio {other:?}"),
            }
        }
    }

    #[test]
    fn parse_tags_lee_los_nombres_y_rechaza_lo_ilegible() {
        assert_eq!(
            parse_tags(TAGS.as_bytes()).unwrap(),
            ["llama3.2:latest", "qwen2.5:7b"]
        );
        assert!(matches!(
            parse_tags(b"no es json"),
            Err(BackendError::Failed(_))
        ));
        assert!(matches!(parse_tags(b"{}"), Err(BackendError::Failed(_))));
    }

    #[test]
    fn parse_chat_lee_el_texto_y_rechaza_lo_ilegible() {
        assert_eq!(parse_chat(CHAT.as_bytes()).unwrap(), "Hola, soy Lince.");
        assert!(matches!(parse_chat(b"{}"), Err(BackendError::Failed(_))));
        assert!(matches!(
            parse_chat(br#"{"message":{"content":null}}"#),
            Err(BackendError::Failed(_))
        ));
    }

    #[tokio::test]
    async fn el_chat_manda_el_modelo_resuelto_la_conversacion_y_stream_apagado() {
        let server = ollama(|| Reply::json(CHAT)).await;
        let reply = backend(&server, Some("llama3.2"))
            .chat(&[user("hola")])
            .await
            .unwrap();
        assert_eq!(reply, "Hola, soy Lince.");

        let pedidos = server.captured();
        let chat = pedidos.iter().find(|p| p.path == "/api/chat").unwrap();
        assert_eq!(chat.method, "POST");
        assert_eq!(
            chat.json(),
            serde_json::json!({
                "model": "llama3.2:latest",
                "messages": [{"role": "user", "content": "hola"}],
                "stream": false
            })
        );
    }

    #[tokio::test]
    async fn sin_modelo_configurado_el_chat_usa_el_primero_instalado() {
        let server = ollama(|| Reply::json(CHAT)).await;
        backend(&server, None).chat(&[user("hola")]).await.unwrap();
        let chat = server
            .captured()
            .into_iter()
            .find(|p| p.path == "/api/chat")
            .unwrap();
        assert_eq!(chat.json()["model"], "llama3.2:latest");
    }

    #[tokio::test]
    async fn models_lista_lo_instalado() {
        let server = ollama(|| Reply::json(CHAT)).await;
        assert_eq!(
            backend(&server, None).models().await.unwrap(),
            ["llama3.2:latest", "qwen2.5:7b"]
        );
    }

    #[tokio::test]
    async fn active_model_trae_el_modelo_resuelto_y_el_resto_de_la_info() {
        let server = ollama(|| Reply::json(CHAT)).await;
        let backend = backend(&server, Some("qwen2.5:7b"));
        let active = backend.active_model().await.unwrap();
        assert_eq!(active.name, "qwen2.5:7b");
        assert_eq!(active.provider, "ollama");
        assert!(active.local);
        assert_eq!(backend.info().host, "127.0.0.1");
    }

    #[tokio::test]
    async fn un_modelo_que_no_esta_instalado_da_no_disponible_y_no_se_manda_el_chat() {
        let server = ollama(|| Reply::json(CHAT)).await;
        let result = backend(&server, Some("mistral"))
            .chat(&[user("hola")])
            .await;
        match result {
            Err(BackendError::Unavailable(text)) => assert!(text.contains("ollama pull mistral")),
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
        assert!(server.captured().iter().all(|p| p.path != "/api/chat"));
    }

    #[tokio::test]
    async fn el_404_del_chat_es_un_modelo_que_se_borro_y_da_no_disponible() {
        let server = ollama(|| Reply::status(404)).await;
        let result = backend(&server, Some("llama3.2"))
            .chat(&[user("hola")])
            .await;
        match result {
            Err(BackendError::Unavailable(text)) => {
                assert!(text.contains("llama3.2") && text.contains("no está instalado"));
            }
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn otro_estado_de_error_en_el_chat_da_fallo_con_el_estado() {
        let server = ollama(|| Reply::status(500)).await;
        match backend(&server, None).chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("500")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn un_error_en_la_lista_de_modelos_da_fallo() {
        let server = FakeServer::start(|_| Reply::status(503)).await;
        match backend(&server, None).models().await {
            Err(BackendError::Failed(text)) => assert!(text.contains("503")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn una_respuesta_con_json_invalido_da_fallo() {
        let server = ollama(|| Reply::json("{esto no es json")).await;
        match backend(&server, None).chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("ilegible")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn un_cuerpo_de_mas_de_256_kib_se_corta() {
        let enorme = format!(r#"{{"message":{{"content":"{}"}}}}"#, "a".repeat(TOPE));
        let server = ollama(move || Reply::json(&enorme)).await;
        match backend(&server, None).chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("tope")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn una_redireccion_no_se_sigue() {
        let destino = FakeServer::start(|_| Reply::json(TAGS)).await;
        let url = destino.url("/api/tags");
        let origen = FakeServer::start(move |_| Reply::redirect(&url)).await;
        let result = backend(&origen, None).models().await;
        assert!(matches!(result, Err(BackendError::Failed(_))), "{result:?}");
        assert!(destino.captured().is_empty());
    }

    #[tokio::test]
    async fn sin_ollama_corriendo_da_no_disponible_con_la_direccion() {
        let url = closed_port_url();
        let backend = OllamaBackend::new(client(), local_selection("ollama", &url, None), false);
        match backend.active_model().await {
            Err(BackendError::Unavailable(text)) => assert!(text.contains("no responde")),
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
    }

    #[test]
    fn los_errores_de_json_no_repiten_lo_que_mando_el_servidor() {
        let chat = parse_chat(br#"{"message":"SECRETO-DEL-SERVIDOR"}"#).unwrap_err();
        let lista = parse_tags(br#"{"models":"SECRETO-DEL-SERVIDOR"}"#).unwrap_err();
        assert_eq!(
            chat,
            BackendError::Failed("respuesta de Ollama ilegible".into())
        );
        assert_eq!(
            lista,
            BackendError::Failed("lista de modelos ilegible".into())
        );
    }

    // ── chat_local_only: una sola resolución decide y manda ───────────────────

    use crate::backend::is_not_local;

    fn chats(server: &FakeServer) -> usize {
        server
            .captured()
            .iter()
            .filter(|r| r.path == "/api/chat")
            .count()
    }

    #[tokio::test]
    async fn chat_local_only_con_un_modelo_local_manda_el_chat_con_una_sola_lista_de_modelos() {
        let server = ollama(|| Reply::json(CHAT)).await;

        let reply = backend(&server, None)
            .chat_local_only(&[user("hola")])
            .await
            .unwrap();

        assert_eq!(reply, "Hola, soy Lince.");
        let paths: Vec<_> = server.captured().iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, ["/api/tags", "/api/chat"]);
    }

    #[tokio::test]
    async fn chat_local_only_con_el_primer_modelo_en_la_nube_no_manda_nada_ni_con_allow_remote() {
        let server = ollama_cloud().await;

        let error = cloud_backend(&server, true)
            .chat_local_only(&[user("hola")])
            .await
            .unwrap_err();

        assert!(is_not_local(&error), "{error:?}");
        assert_eq!(chats(&server), 0);
        // El mismo modelo por `chat` común sí sale (allow_remote lo permite).
        cloud_backend(&server, true)
            .chat(&[user("hola")])
            .await
            .unwrap();
        assert_eq!(chats(&server), 1);
    }

    #[tokio::test]
    async fn chat_local_only_ve_el_cambio_de_modelo_aunque_el_estado_hubiera_dicho_local() {
        let tags_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = tags_calls.clone();
        let server = FakeServer::start(move |pedido| match pedido.path.as_str() {
            // La primera lista tiene un local; de ahí en adelante, sólo uno `cloud`.
            "/api/tags" if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 => {
                Reply::json(TAGS)
            }
            "/api/tags" => Reply::json(CLOUD_TAGS),
            "/api/chat" => Reply::json(CHAT),
            _ => Reply::status(404),
        })
        .await;
        let ollama = cloud_backend(&server, true);

        let info = ollama.active_model().await.unwrap();
        assert!(info.local, "{info:?}");
        let error = ollama.chat_local_only(&[user("hola")]).await.unwrap_err();

        assert!(is_not_local(&error), "{error:?}");
        assert_eq!(chats(&server), 0);
        assert_eq!(tags_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_local_only_con_una_direccion_remota_no_manda() {
        let server = ollama(|| Reply::json(CHAT)).await;
        let mut selection = local_selection("ollama", &server.url(""), None);
        selection.local = false;

        let error = OllamaBackend::new(client(), selection, true)
            .chat_local_only(&[user("hola")])
            .await
            .unwrap_err();

        assert!(is_not_local(&error), "{error:?}");
        assert_eq!(chats(&server), 0);
    }
}
