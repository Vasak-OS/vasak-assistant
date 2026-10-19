//! El backend genérico compatible con la API de OpenAI.
//!
//! `GET {base}/models` y `POST {base}/chat/completions` sin streaming. Lo
//! hablan el propio OpenAI y la mayoría de los proveedores, y también
//! llama.cpp server y LM Studio en la máquina. Las defensas comunes —tiempos,
//! tope, sin redirecciones ni proxy— están en [`crate::http`].
//!
//! La clave sale del llavero en cada pedido ([`crate::keyring`]), así una clave
//! nueva sirve sin reiniciar. Si no hay, el pedido va sin `Authorization`: un
//! servidor local no la suele pedir, y uno que sí contesta 401, que se informa
//! como «no disponible» con el motivo.

use std::sync::Arc;

use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::Selection;
use crate::http::{self, CHAT_TIMEOUT, LIST_TIMEOUT};
use crate::keyring::{SecretSource, ITEM_SERVICE};

pub struct OpenAiBackend {
    client: Client,
    selection: Selection,
    secrets: Arc<dyn SecretSource>,
}

impl OpenAiBackend {
    pub fn new(client: Client, selection: Selection, secrets: Arc<dyn SecretSource>) -> Self {
        Self {
            client,
            selection,
            secrets,
        }
    }

    fn base(&self) -> &Url {
        &self.selection.url
    }

    /// Le pone la clave al pedido, si hay. `bearer_auth` marca la cabecera
    /// como sensible: no aparece en ningún `Debug`.
    async fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match self.secrets.api_key(self.base().as_str()).await {
            Some(key) => request.bearer_auth(key.expose()),
            None => request,
        }
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, BackendError> {
        let response = self
            .authorize(request)
            .await
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
        match response.status() {
            status if status.is_success() => Ok(response),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(self.key_missing()),
            status => Err(BackendError::Failed(format!(
                "{} respondió {status}",
                http::origin(self.base())
            ))),
        }
    }

    fn key_missing(&self) -> BackendError {
        BackendError::Unavailable(format!(
            "{} pide una clave y en el llavero no hay ninguna válida para «{}» (service={ITEM_SERVICE}, url={}).",
            http::origin(self.base()),
            self.base(),
            self.base(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<String>, BackendError> {
        let request = self
            .client
            .get(http::endpoint(self.base(), "models")?)
            .timeout(LIST_TIMEOUT);
        let response = self.send(request).await?;
        parse_models(&http::read_capped(self.base(), response).await?)
    }

    /// El modelo configurado tal cual (un proveedor no siempre lista todos los
    /// que acepta), o el primero de la lista si no hay. Un remoto sin modelo ya
    /// lo rechazó `config::select_backend`.
    async fn resolve_model(&self) -> Result<String, BackendError> {
        if let Some(model) = &self.selection.model {
            return Ok(model.clone());
        }
        self.list_models().await?.into_iter().next().ok_or_else(|| {
            BackendError::Unavailable(format!(
                "{} no ofrece ningún modelo.",
                http::origin(self.base())
            ))
        })
    }

    async fn send_chat(&self, messages: &[Message]) -> Result<String, BackendError> {
        let model = self.resolve_model().await?;
        let request = self
            .client
            .post(http::endpoint(self.base(), "chat/completions")?)
            .timeout(CHAT_TIMEOUT)
            .json(&ChatRequest {
                model: &model,
                messages,
                stream: false,
            });
        let response = self.send(request).await?;
        parse_chat(&http::read_capped(self.base(), response).await?)
    }
}

impl ModelBackend for OpenAiBackend {
    fn info(&self) -> ModelInfo {
        self.selection.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(self.list_models())
    }

    /// Pregunta la lista aunque el modelo esté configurado: es lo que dice si
    /// el servidor responde y si acepta la clave.
    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            let listed = self.list_models().await?;
            let name = match &self.selection.model {
                Some(model) => model.clone(),
                None => listed.into_iter().next().ok_or_else(|| {
                    BackendError::Unavailable(format!(
                        "{} no ofrece ningún modelo.",
                        http::origin(self.base())
                    ))
                })?,
            };
            Ok(ModelInfo {
                name,
                ..self.info()
            })
        })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(self.send_chat(messages))
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
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    /// Puede venir `null` (una llamada a herramienta, un rechazo).
    content: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// La respuesta de `/chat/completions`: el texto de la primera opción.
pub fn parse_chat(body: &[u8]) -> Result<String, BackendError> {
    let response = serde_json::from_slice::<ChatResponse>(body)
        .map_err(|error| BackendError::Failed(format!("respuesta del modelo ilegible: {error}")))?;
    response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| BackendError::Failed("el modelo respondió sin texto".into()))
}

/// La respuesta de `/models`: los ids de los modelos.
pub fn parse_models(body: &[u8]) -> Result<Vec<String>, BackendError> {
    serde_json::from_slice::<ModelsResponse>(body)
        .map(|models| models.data.into_iter().map(|model| model.id).collect())
        .map_err(|error| BackendError::Failed(format!("lista de modelos ilegible: {error}")))
}

#[cfg(test)]
mod tests {
    /// El tope del contrato, escrito a mano: si alguien lo sube, estas pruebas lo notan.
    const TOPE: usize = 256 * 1024;

    use super::*;
    use crate::test_support::{
        client, closed_port_url, local_selection, FakeServer, FixedKey, Reply,
    };
    use vasak_assistant_protocol::Role;

    const MODELS: &str = r#"{"data":[{"id":"modelo-a"},{"id":"modelo-b"}]}"#;
    const COMPLETION: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"Hola, soy Lince."}}]}"#;

    fn backend(
        server: &FakeServer,
        model: Option<&str>,
        key: Option<&str>,
    ) -> (OpenAiBackend, Arc<FixedKey>) {
        let secrets = FixedKey::new(key);
        let backend = OpenAiBackend::new(
            client(),
            local_selection("openai", &server.url("/v1"), model),
            secrets.clone(),
        );
        (backend, secrets)
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.into(),
        }
    }

    /// Un servidor compatible con OpenAI que contesta `chat` a `/chat/completions`.
    async fn openai(chat: impl Fn() -> Reply + Send + Sync + 'static) -> FakeServer {
        FakeServer::start(move |pedido| match pedido.path.as_str() {
            "/v1/models" => Reply::json(MODELS),
            "/v1/chat/completions" => chat(),
            _ => Reply::status(404),
        })
        .await
    }

    #[test]
    fn parse_chat_toma_el_texto_de_la_primera_opcion() {
        let body = r#"{"choices":[{"message":{"content":"uno"}},{"message":{"content":"dos"}}]}"#;
        assert_eq!(parse_chat(body.as_bytes()).unwrap(), "uno");
    }

    #[test]
    fn parse_chat_sin_opciones_o_sin_texto_o_ilegible_da_fallo() {
        for body in [
            &br#"{"choices":[]}"#[..],
            br#"{"choices":[{"message":{"content":null}}]}"#,
            br#"{"choices":[{"message":{}}]}"#,
            b"no es json",
            b"{}",
        ] {
            assert!(
                matches!(parse_chat(body), Err(BackendError::Failed(_))),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn parse_models_lee_los_ids_y_rechaza_lo_ilegible() {
        assert_eq!(
            parse_models(MODELS.as_bytes()).unwrap(),
            ["modelo-a", "modelo-b"]
        );
        assert!(matches!(parse_models(b"[]"), Err(BackendError::Failed(_))));
    }

    #[tokio::test]
    async fn el_chat_manda_el_modelo_la_conversacion_y_stream_apagado_a_la_ruta_correcta() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (backend, _) = backend(&server, Some("gpt-local"), None);

        let reply = backend.chat(&[user("hola")]).await.unwrap();

        assert_eq!(reply, "Hola, soy Lince.");
        let pedidos = server.captured();
        assert_eq!(
            pedidos.len(),
            1,
            "con modelo configurado no se pide la lista"
        );
        assert_eq!(pedidos[0].method, "POST");
        assert_eq!(pedidos[0].path, "/v1/chat/completions");
        assert_eq!(
            pedidos[0].json(),
            serde_json::json!({
                "model": "gpt-local",
                "messages": [{"role": "user", "content": "hola"}],
                "stream": false
            })
        );
    }

    #[tokio::test]
    async fn sin_modelo_configurado_el_chat_usa_el_primero_de_la_lista() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (backend, _) = backend(&server, None, None);
        backend.chat(&[user("hola")]).await.unwrap();
        let chat = server
            .captured()
            .into_iter()
            .find(|p| p.method == "POST")
            .unwrap();
        assert_eq!(chat.json()["model"], "modelo-a");
    }

    #[tokio::test]
    async fn models_lista_los_ids_del_servidor() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (backend, _) = backend(&server, None, None);
        assert_eq!(backend.models().await.unwrap(), ["modelo-a", "modelo-b"]);
        assert_eq!(server.captured()[0].path, "/v1/models");
        assert_eq!(server.captured()[0].method, "GET");
    }

    #[tokio::test]
    async fn active_model_pregunta_la_lista_aunque_haya_modelo_configurado() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (backend, _) = backend(&server, Some("otro"), None);
        let active = backend.active_model().await.unwrap();
        assert_eq!(active.name, "otro");
        assert_eq!(active.provider, "openai");
        assert_eq!(server.captured().len(), 1);
    }

    #[tokio::test]
    async fn active_model_sin_configurar_trae_el_primero_y_sin_ninguno_da_no_disponible() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (backend, _) = backend(&server, None, None);
        assert_eq!(backend.active_model().await.unwrap().name, "modelo-a");

        let vacio = FakeServer::start(|_| Reply::json(r#"{"data":[]}"#)).await;
        let (backend, _) = self::backend(&vacio, None, None);
        for result in [
            backend.active_model().await.map(drop),
            backend.chat(&[user("hola")]).await.map(drop),
        ] {
            match result {
                Err(BackendError::Unavailable(text)) => assert!(text.contains("ningún modelo")),
                other => panic!("se esperaba Unavailable, dio {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn con_clave_en_el_llavero_va_como_bearer_y_sin_clave_no_hay_autorizacion() {
        let server = openai(|| Reply::json(COMPLETION)).await;
        let (con_clave, secrets) = backend(&server, Some("m"), Some("sk-prueba"));
        con_clave.chat(&[user("hola")]).await.unwrap();
        con_clave.models().await.unwrap();
        for pedido in server.captured() {
            assert_eq!(pedido.header("authorization"), Some("Bearer sk-prueba"));
        }
        // La clave se pide por la dirección base normalizada.
        assert!(secrets
            .asked
            .lock()
            .iter()
            .all(|url| url == &server.url("/v1/")));

        let otro = openai(|| Reply::json(COMPLETION)).await;
        let (sin_clave, _) = backend(&otro, Some("m"), None);
        sin_clave.chat(&[user("hola")]).await.unwrap();
        assert_eq!(otro.captured()[0].header("authorization"), None);
    }

    #[tokio::test]
    async fn un_401_o_un_403_dan_no_disponible_con_el_motivo_y_sin_la_clave() {
        for status in [401, 403] {
            let server = FakeServer::start(move |_| Reply::status(status)).await;
            let (backend, _) = backend(&server, Some("m"), Some("sk-prueba"));
            for result in [
                backend.chat(&[user("hola")]).await.map(drop),
                backend.models().await.map(drop),
                backend.active_model().await.map(drop),
            ] {
                match result {
                    Err(BackendError::Unavailable(text)) => {
                        assert!(text.contains("pide una clave"), "{status}: {text}");
                        assert!(text.contains(ITEM_SERVICE));
                        assert!(!text.contains("sk-prueba"));
                    }
                    other => panic!("{status}: se esperaba Unavailable, dio {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn otro_estado_de_error_da_fallo_con_el_estado() {
        let server = openai(|| Reply::status(500)).await;
        let (backend, _) = backend(&server, Some("m"), None);
        match backend.chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("500")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn una_respuesta_con_json_invalido_da_fallo() {
        let server = openai(|| Reply::json("<html>no es json</html>")).await;
        let (backend, _) = backend(&server, Some("m"), None);
        match backend.chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("ilegible")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn un_cuerpo_de_mas_de_256_kib_se_corta() {
        let enorme = format!(
            r#"{{"choices":[{{"message":{{"content":"{}"}}}}]}}"#,
            "a".repeat(TOPE)
        );
        let server = openai(move || Reply::json(&enorme)).await;
        let (backend, _) = backend(&server, Some("m"), None);
        match backend.chat(&[user("hola")]).await {
            Err(BackendError::Failed(text)) => assert!(text.contains("tope")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn una_redireccion_no_se_sigue_y_la_clave_no_llega_al_otro_servidor() {
        let destino = FakeServer::start(|_| Reply::json(MODELS)).await;
        let url = destino.url("/robado");
        let origen = FakeServer::start(move |_| Reply::redirect(&url)).await;
        let (backend, _) = backend(&origen, Some("m"), Some("sk-prueba"));

        let result = backend.chat(&[user("hola")]).await;

        assert!(matches!(result, Err(BackendError::Failed(_))), "{result:?}");
        assert_eq!(origen.captured().len(), 1);
        assert!(destino.captured().is_empty());
    }

    #[tokio::test]
    async fn sin_servidor_da_no_disponible_con_la_direccion() {
        let url = closed_port_url();
        let backend = OpenAiBackend::new(
            client(),
            local_selection("openai", &format!("{url}v1"), Some("m")),
            FixedKey::new(None),
        );
        match backend.chat(&[user("hola")]).await {
            Err(BackendError::Unavailable(text)) => assert!(text.contains("no responde")),
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
    }
}
