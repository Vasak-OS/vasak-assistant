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
use zbus::interface;
use zbus::message::Header;

use vasak_assistant_protocol::{
    Capability, Error, Message, ServiceStatus, DOMAINS, DOMAIN_REMOTE_BACKENDS,
};

use crate::backend::{BackendError, ModelBackend};
use crate::conversations::Conversations;

pub struct AssistantService {
    pub backend: Arc<dyn ModelBackend>,
    pub conversations: Arc<Mutex<Conversations>>,
    /// Si la configuración permite un backend remoto (`allow_remote`).
    pub remote_allowed: bool,
}

#[interface(name = "ar.net.vasak.os.Assistant")]
impl AssistantService {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    async fn status(&self, #[zbus(header)] header: Header<'_>) -> ServiceStatus {
        self.status_for(owner(&header)).await
    }

    /// Las capacidades sobre cada dominio. Las de datos del usuario no están
    /// en esta fase: se dicen no disponibles, nunca rotas.
    fn capabilities(&self) -> Vec<Capability> {
        capabilities(self.remote_allowed)
    }

    /// Los modelos instalados en el backend.
    async fn models(&self) -> Result<Vec<String>, Error> {
        self.backend.models().await.map_err(to_dbus)
    }

    /// Abre una conversación nueva, de quien llama, y devuelve su id.
    fn new_conversation(&self, #[zbus(header)] header: Header<'_>) -> Result<String, Error> {
        self.open(owner(&header))
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
        let context = self
            .conversations
            .lock()
            .begin_turn(owner, conversation_id, content)?;
        let mut turn = Turn {
            conversations: &self.conversations,
            id: conversation_id,
            content,
            reply: None,
        };
        let reply = self.backend.chat(&context).await.map_err(to_dbus)?;
        turn.reply = Some(reply.clone());
        Ok(reply)
    }

    pub fn delete_as(&self, owner: &str, conversation_id: &str) -> Result<(), Error> {
        self.conversations.lock().delete(owner, conversation_id)
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
/// permite un proveedor remoto; las demás, todavía no.
pub fn capabilities(remote_allowed: bool) -> Vec<Capability> {
    DOMAINS
        .iter()
        .map(|&domain| {
            let (available, reason) = match domain {
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
    use crate::test_support::{model_info, p2p_pair, FakeBackend};
    use vasak_assistant_protocol::{
        AssistantProxy, Role, DOMAIN_MAIL, DOMAIN_MEMORY, MAX_CONVERSATIONS_PER_OWNER, SERVICE_PATH,
    };
    use zbus::{connection, Connection, DBusError};

    const OWNER: &str = ":1.1";

    fn service(backend: impl ModelBackend + 'static, remote_allowed: bool) -> AssistantService {
        AssistantService {
            backend: Arc::new(backend),
            conversations: Arc::new(Mutex::new(Conversations::default())),
            remote_allowed,
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
        let all = capabilities(false);
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
        let permitido = capabilities(true);
        let remoto = capability(&permitido, DOMAIN_REMOTE_BACKENDS);
        assert!(remoto.available);
        assert!(remoto.reason.is_empty());

        let prohibido = capabilities(false);
        let remoto = capability(&prohibido, DOMAIN_REMOTE_BACKENDS);
        assert!(!remoto.available);
        assert!(remoto.reason.contains("allow_remote"));
    }

    #[test]
    fn permitir_lo_remoto_no_habilita_las_capacidades_sobre_datos_del_usuario() {
        let all = capabilities(true);
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
        let bus = crate::test_support::PrivateBus::start()?;
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
            eprintln!("sin dbus-daemon: se salta la prueba del bus privado");
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
            eprintln!("sin dbus-daemon: se salta la prueba del bus privado");
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
}
