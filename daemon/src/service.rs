//! La interfaz D-Bus: lo que el escritorio, Configuración, la terminal y las
//! aplicaciones pueden pedirle al asistente.
//!
//! Los nombres de los métodos en el bus son los de Rust en PascalCase
//! (`new_conversation` → `NewConversation`), y están declarados una sola vez,
//! igual, en el proxy del crate `vasak-assistant-protocol`.
//!
//! El contenido de las conversaciones nunca va al registro.

use std::sync::Arc;

use parking_lot::Mutex;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::{interface, Connection};

use vasak_assistant_protocol::{
    Capability, Error, MemoryStatus, Message, Role, ServiceStatus, DOMAINS, DOMAIN_MEMORY,
    DOMAIN_REMOTE_BACKENDS,
};

use crate::backend::{BackendError, ModelBackend};
use crate::config::{is_local, normalize_url};
use crate::conversations::Conversations;
use crate::keyring::{ApiKey, SecretSource};
use crate::memory::Memory;

pub struct AssistantService {
    pub backend: Arc<dyn ModelBackend>,
    pub conversations: Arc<Mutex<Conversations>>,
    /// Si la configuración permite un backend remoto (`allow_remote`).
    pub remote_allowed: bool,
    /// El llavero, para guardar y consultar la clave del proveedor.
    pub secrets: Arc<dyn SecretSource>,
    /// La memoria del asistente (apagada hasta que la persona la enciende).
    pub memory: Arc<Memory>,
}

#[interface(name = "ar.net.vasak.os.Assistant")]
impl AssistantService {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    async fn status(&self, #[zbus(header)] header: Header<'_>) -> ServiceStatus {
        self.status_for(owner(&header)).await
    }

    /// Las capacidades sobre cada dominio. Las de datos del usuario no están
    /// en esta fase: se dicen no disponibles, nunca rotas. `memory`, según la
    /// memoria esté encendida y abierta.
    async fn capabilities(&self) -> Vec<Capability> {
        capabilities(self.remote_allowed, &self.memory.status().await)
    }

    /// Los modelos instalados en el backend.
    async fn models(&self) -> Result<Vec<String>, Error> {
        self.backend.models().await.map_err(to_dbus)
    }

    /// Abre una conversación nueva, de quien llama, y devuelve su id.
    ///
    /// zbus atiende cada llamada en su propia tarea, y `NameOwnerChanged` se
    /// procesa en otra (`main.rs`): si el cliente pide la conversación y se va
    /// enseguida, la liberación puede correr *antes* que el `create`, y ese
    /// nombre único no vuelve nunca. Por eso, ya creada, se pregunta al bus si
    /// la conexión sigue ahí ([`settle_new_conversation`] decide qué hacer).
    async fn new_conversation(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<String, Error> {
        let id = self.open(owner(&header))?;
        // Sin remitente es una conexión punto a punto: no hay bus a quien
        // preguntar, y el único par es el que está llamando.
        let Some(sender) = header.sender() else {
            return Ok(id);
        };
        let still_connected = match zbus::fdo::DBusProxy::new(connection).await {
            Ok(bus) => bus
                .name_has_owner(BusName::Unique(sender.as_ref()))
                .await
                .map_err(zbus::Error::from),
            Err(error) => Err(error),
        };
        settle_new_conversation(&self.conversations, sender.as_str(), id, still_connected)
    }

    /// Las conversaciones abiertas de quien llama.
    fn conversations(&self, #[zbus(header)] header: Header<'_>) -> Vec<String> {
        self.list(owner(&header))
    }

    /// Los mensajes de una conversación de quien llama.
    fn history(
        &self,
        #[zbus(header)] header: Header<'_>,
        conversation_id: &str,
    ) -> Result<Vec<Message>, Error> {
        self.history_of(owner(&header), conversation_id)
    }

    /// Manda un mensaje y devuelve la respuesta del modelo.
    async fn chat(
        &self,
        #[zbus(header)] header: Header<'_>,
        conversation_id: &str,
        content: &str,
    ) -> Result<String, Error> {
        self.chat_as(owner(&header), conversation_id, content).await
    }

    /// Cierra una conversación de quien llama y olvida sus mensajes.
    fn delete_conversation(
        &self,
        #[zbus(header)] header: Header<'_>,
        conversation_id: &str,
    ) -> Result<(), Error> {
        self.delete_as(owner(&header), conversation_id)
    }

    /// Guarda en el llavero la clave del proveedor de esa dirección.
    async fn set_provider_key(&self, url: &str, key: &str) -> Result<(), Error> {
        self.store_provider_key(url, key).await
    }

    /// Si hay una clave guardada para esa dirección.
    async fn has_provider_key(&self, url: &str) -> Result<bool, Error> {
        self.has_provider_key_for(url).await
    }

    /// Cómo está la memoria. Nunca falla: el problema va en el motivo.
    async fn memory_status(&self) -> MemoryStatus {
        self.memory.status().await
    }

    /// Enciende (crea vacía) o apaga (borra) la memoria.
    async fn set_memory_enabled(&self, enabled: bool) -> Result<(), Error> {
        self.memory.set_enabled(enabled).await
    }

    /// Vacía la memoria si está encendida.
    async fn clear_memory(&self) -> Result<(), Error> {
        self.memory.clear().await
    }
}

/// Lo mismo que la interfaz, con el dueño ya resuelto: se puede llamar sin bus.
impl AssistantService {
    pub async fn status_for(&self, owner: &str) -> ServiceStatus {
        let count = self.conversations.lock().count_for(owner);
        let conversations = u32::try_from(count).unwrap_or(u32::MAX);
        match self.backend.active_model().await {
            Ok(model) => ServiceStatus {
                backend_available: true,
                reason: String::new(),
                model,
                conversations,
            },
            Err(error) => ServiceStatus {
                backend_available: false,
                reason: reason(&error).to_owned(),
                model: self.backend.info(),
                conversations,
            },
        }
    }

    pub fn open(&self, owner: &str) -> Result<String, Error> {
        self.conversations.lock().create(owner)
    }

    pub fn list(&self, owner: &str) -> Vec<String> {
        self.conversations.lock().ids(owner)
    }

    pub fn history_of(&self, owner: &str, conversation_id: &str) -> Result<Vec<Message>, Error> {
        self.conversations.lock().history(owner, conversation_id)
    }

    pub async fn chat_as(
        &self,
        owner: &str,
        conversation_id: &str,
        content: &str,
    ) -> Result<String, Error> {
        let mut context = self
            .conversations
            .lock()
            .begin_turn(owner, conversation_id, content)?;
        // El guardián va antes de cualquier espera: si el pedido se abandona
        // mientras se lee la memoria, la conversación igual se libera.
        let mut turn = Turn {
            conversations: &self.conversations,
            id: conversation_id,
            content,
            reply: None,
        };
        let first_turn = context.iter().filter(|m| m.role == Role::User).count() == 1;
        // Las notas de la memoria van sólo en el contexto de este turno: no al
        // historial ni a `History`.
        let notes = self.memory.recall(content, first_turn).await;
        let with_notes = notes.map(|notes| {
            let mut with_notes = Vec::with_capacity(context.len() + 1);
            with_notes.push(Message {
                role: Role::System,
                content: notes,
            });
            with_notes.extend(context.iter().cloned());
            with_notes
        });
        let reply = self
            .backend
            .chat(with_notes.as_deref().unwrap_or(&context))
            .await
            .map_err(to_dbus)?;
        turn.reply = Some(reply.clone());
        context.push(Message {
            role: Role::Assistant,
            content: reply.clone(),
        });
        self.memory
            .learn_after(self.backend.clone(), conversation_id, context);
        Ok(reply)
    }

    pub fn delete_as(&self, owner: &str, conversation_id: &str) -> Result<(), Error> {
        self.conversations.lock().delete(owner, conversation_id)
    }

    /// La dirección se normaliza igual que al elegir el backend, así el ítem
    /// queda con el mismo atributo `url` que después se busca. A un remoto sólo
    /// se le guarda clave por `https`: por `http` viajaría en claro. Ni la
    /// clave ni la dirección cruda van a un mensaje de error.
    pub async fn store_provider_key(&self, url: &str, key: &str) -> Result<(), Error> {
        let url = normalize_url(url).map_err(Error::InvalidArgs)?;
        if !is_local(&url) && url.scheme() != "https" {
            return Err(Error::InvalidArgs(format!(
                "«{}» es un proveedor remoto: su clave sólo se guarda para una dirección https.",
                url.host_str().unwrap_or_default()
            )));
        }
        let key = ApiKey::parse(key.as_bytes()).ok_or_else(|| {
            Error::InvalidArgs(
                "La clave no es válida: no puede estar vacía, pasar de 4096 bytes ni llevar caracteres de control."
                    .into(),
            )
        })?;
        self.secrets.store_key(url.as_str(), &key).await
    }

    pub async fn has_provider_key_for(&self, url: &str) -> Result<bool, Error> {
        let url = normalize_url(url).map_err(Error::InvalidArgs)?;
        self.secrets.has_key(url.as_str()).await
    }
}

/// Qué hacer con una conversación recién abierta según si su dueño sigue
/// conectado. Sin bus de por medio, para poder probarla sola:
///
/// - sigue conectado: se devuelve el id;
/// - ya se fue: se liberan todas sus conversaciones (la liberación por
///   `NameOwnerChanged` pudo haber llegado antes que esta) y se contesta
///   `NotFound`, que nadie va a leer;
/// - no se pudo preguntar: se conserva la conversación y queda en el registro.
///   Si de verdad se fue, el peor caso es una conversación huérfana, no romper
///   el caso normal.
pub fn settle_new_conversation(
    conversations: &Mutex<Conversations>,
    owner: &str,
    id: String,
    still_connected: zbus::Result<bool>,
) -> Result<String, Error> {
    match still_connected {
        Ok(true) => Ok(id),
        Ok(false) => {
            conversations.lock().release_owner(owner);
            Err(Error::NotFound(
                "la conexión que pidió la conversación ya no está".into(),
            ))
        }
        Err(error) => {
            tracing::warn!("no se pudo comprobar si {owner} sigue conectado: {error}");
            Ok(id)
        }
    }
}

/// El dueño de un pedido: el nombre único de la conexión que lo manda. En el
/// bus siempre viene (lo pone el demonio del bus, no el cliente); sólo una
/// conexión punto a punto, sin bus y con un único par, llega sin remitente, y
/// ahí ese par es el único dueño posible.
fn owner<'a>(header: &'a Header<'_>) -> &'a str {
    header.sender().map(|sender| sender.as_str()).unwrap_or("")
}

/// Cierra el turno pase lo que pase: con la respuesta si la hubo, y si no —un
/// error del modelo, o el futuro abandonado a mitad de camino— liberando la
/// conversación, para que no quede ocupada para siempre.
struct Turn<'a> {
    conversations: &'a Mutex<Conversations>,
    id: &'a str,
    content: &'a str,
    reply: Option<String>,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.conversations
            .lock()
            .finish_turn(self.id, self.content, self.reply.as_deref());
    }
}

/// Las capacidades de [`DOMAINS`]. `remote_backends` está si la configuración
/// permite un proveedor remoto; `memory`, si la memoria está encendida y
/// abierta, con el motivo de `memory` si no; las demás, todavía no.
pub fn capabilities(remote_allowed: bool, memory: &MemoryStatus) -> Vec<Capability> {
    DOMAINS
        .iter()
        .map(|&domain| {
            let (available, reason) = match domain {
                DOMAIN_MEMORY => (memory.enabled && memory.available, memory.reason.as_str()),
                DOMAIN_REMOTE_BACKENDS if remote_allowed => (true, ""),
                DOMAIN_REMOTE_BACKENDS => (
                    false,
                    "Desactivado: la configuración no permite proveedores remotos («allow_remote»).",
                ),
                _ => (
                    false,
                    "Todavía no está disponible en esta versión del asistente.",
                ),
            };
            Capability {
                domain: domain.to_owned(),
                available,
                reason: reason.to_owned(),
            }
        })
        .collect()
}

fn reason(error: &BackendError) -> &str {
    match error {
        BackendError::Unavailable(text) | BackendError::Failed(text) => text,
    }
}

pub fn to_dbus(error: BackendError) -> Error {
    match error {
        BackendError::Unavailable(text) => Error::Unavailable(text),
        BackendError::Failed(text) => Error::Backend(text),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::backend::RejectedBackend;
    use crate::test_support::{
        memory_off, memory_status_off, model_info, p2p_pair, FakeBackend, FixedKey,
    };
    use vasak_assistant_protocol::{
        AssistantProxy, Role, DOMAIN_MAIL, DOMAIN_MEMORY, MAX_CONVERSATIONS,
        MAX_CONVERSATIONS_PER_OWNER, SERVICE_PATH,
    };
    use zbus::{connection, Connection, DBusError};

    const OWNER: &str = ":1.1";

    fn service(backend: impl ModelBackend + 'static, remote_allowed: bool) -> AssistantService {
        AssistantService {
            backend: Arc::new(backend),
            conversations: Arc::new(Mutex::new(Conversations::default())),
            remote_allowed,
            secrets: FixedKey::new(None),
            memory: memory_off(),
        }
    }

    /// El servicio servido por una conexión punto a punto y su proxy de cliente.
    async fn connected(service: AssistantService) -> (AssistantProxy<'static>, zbus::Connection) {
        let (server, client) = p2p_pair(|builder| builder.serve_at(SERVICE_PATH, service))
            .await
            .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        (proxy, server)
    }

    fn capability<'a>(all: &'a [Capability], domain: &str) -> &'a Capability {
        all.iter().find(|c| c.domain == domain).unwrap()
    }

    #[test]
    fn las_capacidades_cubren_todos_los_dominios_en_orden_y_ninguna_de_datos_esta_disponible() {
        let all = capabilities(false, &memory_status_off());
        assert_eq!(
            all.iter().map(|c| c.domain.as_str()).collect::<Vec<_>>(),
            DOMAINS
        );
        for capability in &all {
            assert!(!capability.available, "{}", capability.domain);
            assert!(!capability.reason.is_empty(), "{}", capability.domain);
        }
    }

    #[test]
    fn remote_backends_esta_disponible_solo_si_la_configuracion_lo_permite() {
        let permitido = capabilities(true, &memory_status_off());
        let remoto = capability(&permitido, DOMAIN_REMOTE_BACKENDS);
        assert!(remoto.available);
        assert!(remoto.reason.is_empty());

        let prohibido = capabilities(false, &memory_status_off());
        let remoto = capability(&prohibido, DOMAIN_REMOTE_BACKENDS);
        assert!(!remoto.available);
        assert!(remoto.reason.contains("allow_remote"));
    }

    #[test]
    fn permitir_lo_remoto_no_habilita_las_capacidades_sobre_datos_del_usuario() {
        let all = capabilities(true, &memory_status_off());
        assert!(!capability(&all, DOMAIN_MAIL).available);
        assert!(!capability(&all, DOMAIN_MEMORY).available);
    }

    #[test]
    fn cada_error_del_backend_se_traduce_al_error_del_bus_que_toca() {
        assert!(matches!(
            to_dbus(BackendError::Unavailable("a".into())),
            Error::Unavailable(text) if text == "a"
        ));
        assert!(matches!(
            to_dbus(BackendError::Failed("b".into())),
            Error::Backend(text) if text == "b"
        ));
    }

    #[tokio::test]
    async fn el_estado_con_modelo_listo_lo_informa_y_cuenta_las_conversaciones() {
        let service = service(FakeBackend::replying("hola"), false);
        service.open(OWNER).unwrap();
        service.open(OWNER).unwrap();

        let status = service.status_for(OWNER).await;

        assert!(status.backend_available);
        assert!(status.reason.is_empty());
        assert_eq!(status.model, model_info("modelo-de-prueba"));
        assert_eq!(status.conversations, 2);
    }

    #[tokio::test]
    async fn el_estado_sin_modelo_dice_por_que_y_muestra_lo_que_se_sabe_del_backend() {
        for error in [
            BackendError::Unavailable("no hay modelo".into()),
            BackendError::Failed("respuesta rota".into()),
        ] {
            let texto = match &error {
                BackendError::Unavailable(t) | BackendError::Failed(t) => t.clone(),
            };
            let mut backend = FakeBackend::replying("");
            backend.active_result = Err(error);
            let status = service(backend, false).status_for(OWNER).await;

            assert!(!status.backend_available);
            assert_eq!(status.reason, texto);
            assert_eq!(status.model, model_info(""));
        }
    }

    #[tokio::test]
    async fn con_la_configuracion_rechazada_el_estado_explica_el_motivo() {
        let rejected = RejectedBackend {
            info: model_info("gpt"),
            reason: "Es remoto y no está permitido.".into(),
        };
        let status = service(rejected, false).status_for(OWNER).await;
        assert!(!status.backend_available);
        assert_eq!(status.reason, "Es remoto y no está permitido.");
        assert_eq!(status.model.name, "gpt");
    }

    #[tokio::test]
    async fn models_devuelve_la_lista_del_backend_o_su_error() {
        let ok = service(FakeBackend::replying(""), false);
        assert_eq!(ok.models().await.unwrap(), ["modelo-de-prueba"]);

        let mut roto = FakeBackend::replying("");
        roto.models_result = Err(BackendError::Unavailable("apagado".into()));
        assert!(matches!(
            service(roto, false).models().await,
            Err(Error::Unavailable(text)) if text == "apagado"
        ));
    }

    #[tokio::test]
    async fn el_chat_guarda_el_intercambio_y_el_modelo_ve_el_historial_completo() {
        let backend = Arc::new(FakeBackend::replying("respuesta"));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: memory_off(),
        };
        let id = service.open(OWNER).unwrap();

        assert_eq!(
            service.chat_as(OWNER, &id, "uno").await.unwrap(),
            "respuesta"
        );
        assert_eq!(
            service.chat_as(OWNER, &id, "dos").await.unwrap(),
            "respuesta"
        );

        let history = service.history_of(OWNER, &id).unwrap();
        let textos: Vec<_> = history
            .iter()
            .map(|m| (m.role, m.content.as_str()))
            .collect();
        assert_eq!(
            textos,
            [
                (Role::User, "uno"),
                (Role::Assistant, "respuesta"),
                (Role::User, "dos"),
                (Role::Assistant, "respuesta"),
            ]
        );
        let visto = backend.seen.lock();
        assert_eq!(visto[0].len(), 1);
        assert_eq!(visto[1].len(), 3);
        assert_eq!(visto[1][2].content, "dos");
    }

    #[tokio::test]
    async fn un_chat_fallido_deja_la_conversacion_como_estaba_y_libre() {
        let backend = Arc::new(FakeBackend::failing(BackendError::Failed("se cayó".into())));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: memory_off(),
        };
        let id = service.open(OWNER).unwrap();
        service.chat_as(OWNER, &id, "primero").await.unwrap_err();
        assert!(service.history_of(OWNER, &id).unwrap().is_empty());

        *backend.chat_result.lock() = Ok("ahora sí".into());
        assert_eq!(
            service.chat_as(OWNER, &id, "segundo").await.unwrap(),
            "ahora sí"
        );
        assert_eq!(service.history_of(OWNER, &id).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn un_chat_con_la_conversacion_ocupada_da_busy_y_el_primero_termina_bien() {
        let (backend, gate) = FakeBackend::gated("respuesta");
        let entered = backend.entered.clone();
        let service = Arc::new(service(backend, false));
        let id = service.open(OWNER).unwrap();

        let primero = tokio::spawn({
            let (service, id) = (service.clone(), id.clone());
            async move { service.chat_as(OWNER, &id, "uno").await }
        });
        entered.notified().await;

        // Con un plazo: si la marca de ocupada fallara, el segundo chat
        // esperaría al modelo para siempre en vez de fallar la prueba.
        let segundo =
            tokio::time::timeout(Duration::from_secs(5), service.chat_as(OWNER, &id, "dos"))
                .await
                .expect("el segundo chat no debía quedar esperando al modelo");
        assert!(matches!(segundo, Err(Error::Busy(_))));
        gate.notify_one();
        assert_eq!(primero.await.unwrap().unwrap(), "respuesta");
        assert_eq!(service.history_of(OWNER, &id).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn un_chat_abandonado_a_mitad_libera_la_conversacion_y_no_guarda_nada() {
        let (backend, gate) = FakeBackend::gated("respuesta");
        let entered = backend.entered.clone();
        let service = service(backend, false);
        let id = service.open(OWNER).unwrap();

        // El pedido queda esperando al modelo y se abandona (el cliente se fue).
        tokio::select! {
            _ = service.chat_as(OWNER, &id, "uno") => panic!("no debía terminar"),
            _ = entered.notified() => {}
        }

        assert!(service.history_of(OWNER, &id).unwrap().is_empty());
        gate.notify_one();
        assert_eq!(
            service.chat_as(OWNER, &id, "dos").await.unwrap(),
            "respuesta"
        );
        assert_eq!(service.history_of(OWNER, &id).unwrap()[0].content, "dos");
    }

    #[tokio::test]
    async fn un_chat_con_mensaje_invalido_no_llega_al_modelo() {
        let backend = Arc::new(FakeBackend::replying("x"));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: memory_off(),
        };
        let id = service.open(OWNER).unwrap();
        assert!(matches!(
            service.chat_as(OWNER, &id, "  ").await,
            Err(Error::InvalidArgs(_))
        ));
        assert!(matches!(
            service.chat_as(OWNER, "basura", "hola").await,
            Err(Error::InvalidArgs(_))
        ));
        assert!(backend.seen.lock().is_empty());
    }

    #[tokio::test]
    async fn abrir_listar_y_cerrar_conversaciones_respeta_el_tope() {
        let service = service(FakeBackend::replying("x"), false);
        let ids: Vec<String> = (0..MAX_CONVERSATIONS_PER_OWNER)
            .map(|_| service.open(OWNER).unwrap())
            .collect();
        assert_eq!(service.list(OWNER).len(), MAX_CONVERSATIONS_PER_OWNER);
        assert!(matches!(service.open(OWNER), Err(Error::LimitExceeded(_))));
        service.delete_as(OWNER, &ids[0]).unwrap();
        assert_eq!(service.list(OWNER).len(), MAX_CONVERSATIONS_PER_OWNER - 1);
        assert!(matches!(
            service.delete_as(OWNER, &ids[0]),
            Err(Error::NotFound(_))
        ));
    }

    // ── De punta a punta, por D-Bus ───────────────────────────────────────────

    #[tokio::test]
    async fn por_el_bus_status_capabilities_models_y_conversaciones_responden() {
        let (proxy, _server) = connected(service(FakeBackend::replying("hola"), true)).await;

        let status = proxy.status().await.unwrap();
        assert!(status.backend_available);
        assert_eq!(status.model.name, "modelo-de-prueba");
        assert_eq!(status.conversations, 0);

        let all = proxy.capabilities().await.unwrap();
        assert_eq!(all.len(), DOMAINS.len());
        assert!(capability(&all, DOMAIN_REMOTE_BACKENDS).available);

        assert_eq!(proxy.models().await.unwrap(), ["modelo-de-prueba"]);

        let id = proxy.new_conversation().await.unwrap();
        assert_eq!(
            proxy.conversations().await.unwrap(),
            std::slice::from_ref(&id)
        );
        proxy.delete_conversation(&id).await.unwrap();
        assert!(proxy.conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn por_el_bus_remote_backends_sigue_a_allow_remote() {
        let (proxy, _server) = connected(service(FakeBackend::replying("x"), false)).await;
        let all = proxy.capabilities().await.unwrap();
        assert!(!capability(&all, DOMAIN_REMOTE_BACKENDS).available);
    }

    #[tokio::test]
    async fn por_el_bus_el_chat_y_el_historial_van_de_punta_a_punta() {
        let (proxy, _server) =
            connected(service(FakeBackend::replying("Hola, soy Lince."), false)).await;
        let id = proxy.new_conversation().await.unwrap();

        assert_eq!(proxy.chat(&id, "hola").await.unwrap(), "Hola, soy Lince.");

        let history = proxy.history(&id).await.unwrap();
        assert_eq!(
            history,
            [
                Message {
                    role: Role::User,
                    content: "hola".into()
                },
                Message {
                    role: Role::Assistant,
                    content: "Hola, soy Lince.".into()
                },
            ]
        );
        assert_eq!(proxy.status().await.unwrap().conversations, 1);
    }

    #[tokio::test]
    async fn por_el_bus_cada_error_llega_con_su_nombre() {
        let mut backend = FakeBackend::failing(BackendError::Unavailable("sin modelo".into()));
        backend.models_result = Err(BackendError::Failed("roto".into()));
        let (proxy, _server) = connected(service(backend, false)).await;
        let id = proxy.new_conversation().await.unwrap();

        assert!(matches!(
            proxy.chat(&id, "hola").await,
            Err(Error::Unavailable(text)) if text == "sin modelo"
        ));
        assert!(matches!(proxy.models().await, Err(Error::Backend(text)) if text == "roto"));
        assert!(matches!(
            proxy.chat(&id, " ").await,
            Err(Error::InvalidArgs(_))
        ));
        let otro = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            proxy.history(&otro).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            proxy.history("basura").await,
            Err(Error::InvalidArgs(_))
        ));
    }

    #[tokio::test]
    async fn por_el_bus_el_limite_y_la_conversacion_ocupada_llegan_como_errores_propios() {
        let (backend, gate) = FakeBackend::gated("listo");
        let entered = backend.entered.clone();
        let (proxy, _server) = connected(service(backend, false)).await;
        let id = proxy.new_conversation().await.unwrap();

        let ocupada = tokio::spawn({
            let (proxy, id) = (proxy.clone(), id.clone());
            async move { proxy.chat(&id, "uno").await }
        });
        entered.notified().await;
        let error = tokio::time::timeout(Duration::from_secs(5), proxy.chat(&id, "dos"))
            .await
            .expect("el segundo chat no debía quedar esperando al modelo")
            .unwrap_err();
        assert!(matches!(error, Error::Busy(_)), "{error:?}");
        assert_eq!(
            error.name().as_str(),
            "ar.net.vasak.os.Assistant.Error.Busy"
        );
        gate.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), ocupada)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "listo"
        );

        // Por el bus, el que se agota primero es el tope de la conexión.
        for _ in 1..MAX_CONVERSATIONS_PER_OWNER {
            proxy.new_conversation().await.unwrap();
        }
        assert!(matches!(
            proxy.new_conversation().await,
            Err(Error::LimitExceeded(_))
        ));
    }

    // ── Dos remitentes: las conversaciones son de quien las abrió ─────────────

    fn error_text(error: &Error) -> String {
        match error {
            Error::NotFound(text) | Error::InvalidArgs(text) => text.clone(),
            other => panic!("se esperaba NotFound, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn otro_dueno_no_ve_lee_escribe_ni_borra_lo_ajeno() {
        let backend = Arc::new(FakeBackend::replying("respuesta"));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: memory_off(),
        };
        let id = service.open(":1.1").unwrap();
        service.chat_as(":1.1", &id, "secreto").await.unwrap();

        assert!(service.list(":1.2").is_empty());
        assert_eq!(service.status_for(":1.2").await.conversations, 0);
        assert_eq!(service.status_for(":1.1").await.conversations, 1);
        let leer = service.history_of(":1.2", &id).unwrap_err();
        let escribir = service.chat_as(":1.2", &id, "intruso").await.unwrap_err();
        let borrar = service.delete_as(":1.2", &id).unwrap_err();
        assert!(matches!(leer, Error::NotFound(_)));
        assert!(matches!(escribir, Error::NotFound(_)));
        assert!(matches!(borrar, Error::NotFound(_)));
        // Es el mismo NotFound que da un id que no existe: no confirma el id.
        let inexistente = service
            .history_of(":1.2", &uuid::Uuid::new_v4().to_string())
            .unwrap_err();
        assert_eq!(error_text(&leer), error_text(&inexistente));

        // Nada de eso tocó la conversación ni llegó al modelo.
        let history = service.history_of(":1.1", &id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "secreto");
        assert_eq!(backend.seen.lock().len(), 1);
    }

    #[tokio::test]
    async fn pasarse_del_tope_por_dueno_no_bloquea_a_otro_dueno() {
        let service = service(FakeBackend::replying("x"), false);
        for _ in 0..MAX_CONVERSATIONS_PER_OWNER {
            service.open(":1.1").unwrap();
        }
        assert!(matches!(service.open(":1.1"), Err(Error::LimitExceeded(_))));
        assert!(service.open(":1.2").is_ok());
        assert_eq!(service.list(":1.1").len(), MAX_CONVERSATIONS_PER_OWNER);
    }

    /// Un bus privado con el servicio y dos clientes con remitentes distintos.
    async fn two_clients(
        service: AssistantService,
    ) -> Option<(
        crate::test_support::PrivateBus,
        Connection,
        AssistantProxy<'static>,
        AssistantProxy<'static>,
    )> {
        let bus = crate::test_support::PrivateBus::start_or_skip()?;
        let server = connection::Builder::address(bus.address.as_str())
            .unwrap()
            .serve_at(SERVICE_PATH, service)
            .unwrap()
            .name(vasak_assistant_protocol::SERVICE_NAME)
            .unwrap()
            .build()
            .await
            .unwrap();
        let (a, b) = (bus.connect().await, bus.connect().await);
        assert_ne!(
            a.unique_name().unwrap().as_str(),
            b.unique_name().unwrap().as_str()
        );
        let proxy =
            |c: Connection| async move { AssistantProxy::builder(&c).build().await.unwrap() };
        let (a, b) = (proxy(a).await, proxy(b).await);
        Some((bus, server, a, b))
    }

    #[tokio::test]
    async fn por_un_bus_real_la_segunda_conexion_no_ve_lee_escribe_ni_borra_lo_de_la_primera() {
        let Some((_bus, _server, a, b)) =
            two_clients(service(FakeBackend::replying("respuesta"), false)).await
        else {
            return;
        };
        let id = a.new_conversation().await.unwrap();
        a.chat(&id, "secreto").await.unwrap();

        assert_eq!(a.conversations().await.unwrap(), std::slice::from_ref(&id));
        assert!(b.conversations().await.unwrap().is_empty());
        assert_eq!(a.status().await.unwrap().conversations, 1);
        assert_eq!(b.status().await.unwrap().conversations, 0);
        assert!(matches!(b.history(&id).await, Err(Error::NotFound(_))));
        assert!(matches!(
            b.chat(&id, "intruso").await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            b.delete_conversation(&id).await,
            Err(Error::NotFound(_))
        ));

        let history = a.history(&id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "secreto");
        // El dueño sí puede cerrarla.
        a.delete_conversation(&id).await.unwrap();
        assert!(a.conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn por_un_bus_real_el_tope_de_una_conexion_no_bloquea_a_la_otra() {
        let Some((_bus, _server, a, b)) =
            two_clients(service(FakeBackend::replying("x"), false)).await
        else {
            return;
        };
        for _ in 0..MAX_CONVERSATIONS_PER_OWNER {
            a.new_conversation().await.unwrap();
        }
        assert!(matches!(
            a.new_conversation().await,
            Err(Error::LimitExceeded(_))
        ));
        assert!(b.new_conversation().await.is_ok());
    }

    // ── Conversación huérfana: el dueño se fue antes del create ──────────────

    fn settle(
        conversations: &Mutex<Conversations>,
        still_connected: zbus::Result<bool>,
    ) -> (String, Result<String, Error>) {
        let id = conversations.lock().create(":1.7").unwrap();
        let result = settle_new_conversation(conversations, ":1.7", id.clone(), still_connected);
        (id, result)
    }

    #[test]
    fn si_el_dueno_sigue_conectado_la_conversacion_se_conserva_y_se_devuelve_el_id() {
        let conversations = Mutex::new(Conversations::default());
        let (id, result) = settle(&conversations, Ok(true));
        assert_eq!(result.unwrap(), id);
        assert_eq!(conversations.lock().count_for(":1.7"), 1);
    }

    #[test]
    fn si_el_dueno_ya_se_fue_se_liberan_sus_conversaciones_y_se_contesta_not_found() {
        let conversations = Mutex::new(Conversations::default());
        let ajena = conversations.lock().create(":1.8").unwrap();
        conversations.lock().create(":1.7").unwrap();

        let (_, result) = settle(&conversations, Ok(false));

        assert!(matches!(result, Err(Error::NotFound(_))));
        let conversations = conversations.lock();
        assert_eq!(conversations.count_for(":1.7"), 0);
        // La de otro dueño no se toca.
        assert!(conversations.history(":1.8", &ajena).is_ok());
    }

    #[test]
    fn repetido_muchas_veces_el_dueno_que_se_va_no_agota_las_plazas() {
        let conversations = Mutex::new(Conversations::default());
        for i in 0..MAX_CONVERSATIONS * 3 {
            let owner = format!(":1.{}", 100 + i);
            let id = conversations.lock().create(&owner).unwrap();
            let result = settle_new_conversation(&conversations, &owner, id, Ok(false));
            assert!(matches!(result, Err(Error::NotFound(_))));
        }
        assert_eq!(conversations.lock().len(), 0);
        assert!(conversations.lock().create(":1.1").is_ok());
    }

    #[test]
    fn si_no_se_pudo_preguntar_al_bus_se_conserva_la_conversacion_y_se_devuelve_el_id() {
        let conversations = Mutex::new(Conversations::default());
        let (id, result) = settle(&conversations, Err(zbus::Error::Failure("sin bus".into())));
        assert_eq!(result.unwrap(), id);
        assert_eq!(conversations.lock().count_for(":1.7"), 1);
    }

    const CLAVE: &str = "sk-clave-que-no-debe-filtrarse";

    fn service_with_keys() -> (AssistantService, Arc<FixedKey>) {
        let keys = FixedKey::new(None);
        let service = AssistantService {
            secrets: keys.clone(),
            ..service(FakeBackend::replying(""), false)
        };
        (service, keys)
    }

    fn stored(keys: &FixedKey) -> Vec<(String, String)> {
        keys.stored
            .lock()
            .iter()
            .map(|(url, key)| (url.clone(), key.expose().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn guardar_la_clave_de_un_local_por_http_la_deja_en_el_llavero_con_la_direccion_normalizada(
    ) {
        let (service, keys) = service_with_keys();
        service
            .store_provider_key("http://127.0.0.1:8080/v1?x=1", &format!("  {CLAVE}\n"))
            .await
            .unwrap();
        assert_eq!(
            stored(&keys),
            [("http://127.0.0.1:8080/v1/".to_owned(), CLAVE.to_owned())]
        );
    }

    #[tokio::test]
    async fn guardar_la_clave_de_un_remoto_por_https_se_acepta() {
        let (service, keys) = service_with_keys();
        service
            .store_provider_key("https://api.openai.com/v1", CLAVE)
            .await
            .unwrap();
        assert_eq!(stored(&keys)[0].0, "https://api.openai.com/v1/");
    }

    #[tokio::test]
    async fn la_clave_se_guarda_con_la_misma_direccion_que_despues_busca_el_backend() {
        use crate::config::{select_backend, Settings};
        for raw in [
            "https://localhost:8443/v1",
            "http://127.0.0.1:8080/v1/?a=b#c",
        ] {
            let (service, keys) = service_with_keys();
            service.store_provider_key(raw, CLAVE).await.unwrap();
            let selection = select_backend(&Settings {
                backend: Some("openai".into()),
                url: Some(raw.into()),
                ..Settings::default()
            })
            .unwrap();
            assert_eq!(stored(&keys)[0].0, selection.url.as_str(), "{raw}");
        }
    }

    #[tokio::test]
    async fn la_clave_de_un_remoto_por_http_se_rechaza_sin_guardar_ni_nombrar_la_clave() {
        let (service, keys) = service_with_keys();
        let error = service
            .store_provider_key("http://api.ejemplo.com/v1?token=SECRETO", CLAVE)
            .await
            .unwrap_err();
        let Error::InvalidArgs(text) = error else {
            panic!("tenía que ser InvalidArgs");
        };
        assert!(text.contains("api.ejemplo.com"), "{text}");
        assert!(text.contains("https"), "{text}");
        assert!(!text.contains(CLAVE) && !text.contains("SECRETO"), "{text}");
        assert!(stored(&keys).is_empty());
    }

    #[tokio::test]
    async fn una_direccion_que_no_vale_se_rechaza_con_invalid_args_sin_la_clave() {
        for raw in ["", "no es url", "ftp://127.0.0.1/", "http://u:p@127.0.0.1/"] {
            let (service, keys) = service_with_keys();
            let error = service.store_provider_key(raw, CLAVE).await.unwrap_err();
            let Error::InvalidArgs(text) = error else {
                panic!("{raw:?}: tenía que ser InvalidArgs");
            };
            assert!(!text.contains(CLAVE), "{raw:?}: {text}");
            assert!(stored(&keys).is_empty(), "{raw:?}");
        }
    }

    #[tokio::test]
    async fn una_clave_vacia_larga_o_con_caracteres_de_control_se_rechaza_sin_repetirla() {
        let larga = "k".repeat(4097);
        let casos = [
            "",
            "   \n\t ",
            larga.as_str(),
            "sk-\r\nX-Otra: 1",
            "sk-a\0b",
        ];
        for key in casos {
            let (service, keys) = service_with_keys();
            let error = service
                .store_provider_key("https://api.openai.com/v1", key)
                .await
                .unwrap_err();
            let Error::InvalidArgs(text) = error else {
                panic!("{key:?}: tenía que ser InvalidArgs");
            };
            assert!(text.contains("clave"), "{text}");
            if !key.trim().is_empty() {
                assert!(!text.contains(key.trim()), "el mensaje repite la clave");
            }
            assert!(stored(&keys).is_empty(), "{key:?}");
        }
    }

    #[tokio::test]
    async fn una_clave_de_4096_bytes_justos_se_acepta() {
        let (service, keys) = service_with_keys();
        service
            .store_provider_key("https://api.openai.com/v1", &"k".repeat(4096))
            .await
            .unwrap();
        assert_eq!(stored(&keys).len(), 1);
    }

    #[tokio::test]
    async fn has_provider_key_for_consulta_con_la_direccion_normalizada_y_rechaza_la_ilegible() {
        let (service, _keys) = service_with_keys();
        assert!(!service
            .has_provider_key_for("https://api.openai.com/v1")
            .await
            .unwrap());
        service
            .store_provider_key("https://api.openai.com/v1", CLAVE)
            .await
            .unwrap();
        // Sin la barra final y con consulta: es la misma clave.
        assert!(service
            .has_provider_key_for("https://api.openai.com/v1?x=1")
            .await
            .unwrap());
        assert!(!service
            .has_provider_key_for("https://otro.ejemplo.com/v1")
            .await
            .unwrap());
        assert!(matches!(
            service.has_provider_key_for("no es url").await,
            Err(Error::InvalidArgs(_))
        ));
    }

    #[tokio::test]
    async fn por_dbus_set_provider_key_guarda_y_has_provider_key_lo_ve() {
        let (service, keys) = service_with_keys();
        let (proxy, _server) = connected(service).await;

        assert!(!proxy
            .has_provider_key("https://api.openai.com/v1")
            .await
            .unwrap());
        proxy
            .set_provider_key("https://api.openai.com/v1", CLAVE)
            .await
            .unwrap();

        assert_eq!(
            stored(&keys),
            [("https://api.openai.com/v1/".to_owned(), CLAVE.to_owned())]
        );
        assert!(proxy
            .has_provider_key("https://api.openai.com/v1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn por_dbus_una_clave_o_direccion_invalida_llega_como_invalid_args_sin_la_clave() {
        let (service, keys) = service_with_keys();
        let (proxy, _server) = connected(service).await;

        for (url, key) in [
            ("https://api.openai.com/v1", "   "),
            ("http://api.ejemplo.com/v1", CLAVE),
            ("no es url", CLAVE),
        ] {
            match proxy.set_provider_key(url, key).await {
                Err(Error::InvalidArgs(text)) => assert!(!text.contains(CLAVE), "{text}"),
                otro => panic!("{url:?}: se esperaba InvalidArgs, vino {otro:?}"),
            }
        }
        assert!(stored(&keys).is_empty());
        assert!(matches!(
            proxy.has_provider_key("no es url").await,
            Err(Error::InvalidArgs(_))
        ));
    }
}
