//! La interfaz D-Bus: lo que el escritorio, Configuración, la terminal y las
//! aplicaciones pueden pedirle al asistente.
//!
//! Los nombres de los métodos en el bus son los de Rust en PascalCase
//! (`new_conversation` → `NewConversation`), y están declarados una sola vez,
//! igual, en el proxy del crate `vasak-assistant-protocol`.
//!
//! El contenido de las conversaciones nunca va al registro.
//!
//! # Aplicaciones confinadas
//!
//! La memoria y las claves son de la persona, no de cada aplicación. Un
//! llamador confinado por AppArmor (su `LinuxSecurityLabel`, que da el bus con
//! `GetConnectionCredentials`, existe y no es `unconfined`) no puede
//! encender, apagar ni borrar la memoria ni guardar o consultar claves
//! (`AccessDenied`), y su `Chat` va sin memoria: no se le pasan notas ni se
//! aprende de lo que dice. `MemoryStatus` y `Capabilities` siguen abiertos.
//! Sin etiqueta (no hay AppArmor) se deja pasar; una vacía, no. No se mira
//! `/proc/<pid>/exe`: la unidad corre con un namespace de usuario y da EACCES.

use std::sync::Arc;

use parking_lot::Mutex;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::{interface, Connection};

use vasak_assistant_protocol::{
    Capability, Error, MemoryStatus, Message, Role, ServiceStatus, DOMAINS, DOMAIN_MEMORY,
    DOMAIN_REMOTE_BACKENDS,
};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::{is_local, normalize_url};
use crate::conversations::Conversations;
use crate::keyring::{ApiKey, SecretSource};
use crate::logging::Call;
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
    /// De dónde sale la etiqueta de seguridad de quien llama.
    pub labels: Arc<dyn CallerLabels>,
}

/// La etiqueta de seguridad de quien llama. Un rasgo para que las pruebas
/// pongan una de mentira (`test_support::FixedLabel`).
pub trait CallerLabels: Send + Sync {
    /// `Ok(None)` sin etiqueta (no hay AppArmor). `Err` sin remitente o si el
    /// bus no contestó: se trata como
    /// confinado.
    fn label<'a>(
        &'a self,
        connection: &'a Connection,
        sender: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ()>>;
}

/// La del bus: `org.freedesktop.DBus.GetConnectionCredentials(remitente)`,
/// campo `LinuxSecurityLabel`.
pub struct BusLabels;

impl CallerLabels for BusLabels {
    fn label<'a>(
        &'a self,
        connection: &'a Connection,
        sender: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ()>> {
        Box::pin(async move {
            // Sin remitente, o sin bus que lo respalde (punto a punto, donde
            // el par pone el remitente que quiere), no hay a quién
            // preguntarle: falla cerrado.
            let Some(sender) = sender else {
                return Err(());
            };
            if !connection.is_bus() {
                return Err(());
            }
            let name = zbus::names::UniqueName::try_from(sender).map_err(drop)?;
            let bus = zbus::fdo::DBusProxy::new(connection).await.map_err(drop)?;
            let credentials = bus
                .get_connection_credentials(BusName::Unique(name))
                .await
                .map_err(|error| {
                    tracing::warn!("no se pudieron leer las credenciales de {sender}: {error}");
                })?;
            Ok(credentials.into_linux_security_label())
        })
    }
}

/// Si la etiqueta de AppArmor dice que el llamador está confinado: existe y
/// no es exactamente `unconfined`. Llega como bytes terminados en NUL, por
/// ejemplo `vasak-appimage (enforce)`. Sin etiqueta (`None`), no está
/// confinado; una etiqueta vacía o que no se entiende, sí: falla cerrado.
pub fn is_confined(label: Option<&[u8]>) -> bool {
    let Some(label) = label else {
        return false;
    };
    let label = label.strip_suffix(&[0]).unwrap_or(label);
    String::from_utf8_lossy(label).trim() != "unconfined"
}

fn access_denied() -> Error {
    Error::AccessDenied(
        "Una aplicación confinada no puede cambiar la memoria ni las claves del asistente.".into(),
    )
}

#[interface(name = "ar.net.vasak.os.Assistant")]
impl AssistantService {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    async fn status(&self, #[zbus(header)] header: Header<'_>) -> Result<ServiceStatus, Error> {
        let call = Call::new("Status", &header);
        let result = async { Ok(self.status_for(owner(&header)?).await) }.await;
        call.finish(result)
    }

    /// Las capacidades sobre cada dominio. Las de datos del usuario no están
    /// en esta fase: se dicen no disponibles, nunca rotas. `memory`, según la
    /// memoria esté encendida y abierta; a un llamador confinado, no
    /// disponible con un motivo fijo, sin abrir la base ni preguntar al
    /// llavero: si no, sabría si está encendida y cómo está el llavero.
    async fn capabilities(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Vec<Capability> {
        let call = Call::new("Capabilities", &header);
        let memory = if self.confined(connection, &header).await {
            MemoryStatus {
                enabled: false,
                available: false,
                reason: crate::memory::REASON_CONFINED.to_owned(),
                bytes: 0,
            }
        } else {
            self.memory_status_now().await
        };
        let all = capabilities(self.remote_allowed, &memory);
        call.done();
        all
    }

    /// Los modelos instalados en el backend.
    #[zbus(name = "Models")]
    async fn models_for(&self, #[zbus(header)] header: Header<'_>) -> Result<Vec<String>, Error> {
        let call = Call::new("Models", &header);
        call.finish(self.models().await)
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
        let call = Call::new("NewConversation", &header);
        let result = self.new_conversation_for(&header, connection).await;
        let call = match &result {
            Ok(id) => call.conversation(id),
            Err(_) => call,
        };
        call.finish(result)
    }

    /// Las conversaciones abiertas de quien llama.
    fn conversations(&self, #[zbus(header)] header: Header<'_>) -> Result<Vec<String>, Error> {
        let call = Call::new("Conversations", &header);
        call.finish(owner(&header).map(|owner| self.list(owner)))
    }

    /// Los mensajes de una conversación de quien llama.
    fn history(
        &self,
        #[zbus(header)] header: Header<'_>,
        conversation_id: &str,
    ) -> Result<Vec<Message>, Error> {
        let call = Call::new("History", &header).conversation(conversation_id);
        call.finish(owner(&header).and_then(|owner| self.history_of(owner, conversation_id)))
    }

    /// Manda un mensaje y devuelve la respuesta del modelo. Un llamador
    /// confinado chatea sin memoria.
    async fn chat(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        conversation_id: &str,
        content: &str,
    ) -> Result<String, Error> {
        let call = Call::new("Chat", &header).conversation(conversation_id);
        let result = async {
            let owner = owner(&header)?;
            let use_memory = !self.confined(connection, &header).await;
            if !use_memory {
                tracing::info!(
                    method = "Chat",
                    sender = owner,
                    conversation = %crate::logging::short_id(conversation_id),
                    "llamador confinado: el chat va sin memoria"
                );
            }
            self.chat_with(owner, conversation_id, content, use_memory)
                .await
        }
        .await;
        call.finish(result)
    }

    /// Cierra una conversación de quien llama y olvida sus mensajes.
    async fn delete_conversation(
        &self,
        #[zbus(header)] header: Header<'_>,
        conversation_id: &str,
    ) -> Result<(), Error> {
        let call = Call::new("DeleteConversation", &header).conversation(conversation_id);
        let result = async {
            self.delete_conversation_as(owner(&header)?, conversation_id)
                .await
        }
        .await;
        call.finish(result)
    }

    /// Guarda en el llavero la clave del proveedor de esa dirección.
    /// `AccessDenied` para un llamador confinado.
    async fn set_provider_key(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        url: &str,
        key: &str,
    ) -> Result<(), Error> {
        let call = Call::new("SetProviderKey", &header);
        let result = async {
            self.deny_confined(connection, &header).await?;
            self.store_provider_key(url, key).await
        }
        .await;
        call.finish(result)
    }

    /// Si hay una clave guardada para esa dirección. `AccessDenied` para un
    /// llamador confinado.
    async fn has_provider_key(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        url: &str,
    ) -> Result<bool, Error> {
        let call = Call::new("HasProviderKey", &header);
        let result = async {
            self.deny_confined(connection, &header).await?;
            self.has_provider_key_for(url).await
        }
        .await;
        call.finish(result)
    }

    /// Cómo está la memoria. Fuera de la puerta no falla: el problema va en el
    /// motivo. `AccessDenied` para un llamador confinado: lo que ocupa crece
    /// con el uso y deja inferir cuánto se usa el asistente (#22).
    async fn memory_status(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<MemoryStatus, Error> {
        let call = Call::new("MemoryStatus", &header);
        let result = async {
            self.deny_confined(connection, &header).await?;
            Ok(self.memory_status_now().await)
        }
        .await;
        call.finish(result)
    }

    /// Enciende o apaga la memoria. Encender reabre la base que haya (o la
    /// crea vacía con una llave nueva); apagar deja de usarla y de escribirla,
    /// sin borrar nada: borrar es sólo `ClearMemory`. `AccessDenied` para un
    /// llamador confinado.
    async fn set_memory_enabled(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        enabled: bool,
    ) -> Result<(), Error> {
        let call = Call::new("SetMemoryEnabled", &header);
        let result = async {
            self.deny_confined(connection, &header).await?;
            self.memory.set_enabled(enabled).await
        }
        .await;
        call.finish(result)
    }

    /// Borra la memoria (la llave y los archivos) y, si estaba encendida, la
    /// recrea vacía. `AccessDenied` para un llamador confinado.
    async fn clear_memory(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<(), Error> {
        let call = Call::new("ClearMemory", &header);
        let result = async {
            self.deny_confined(connection, &header).await?;
            self.memory.clear().await
        }
        .await;
        call.finish(result)
    }
}

impl AssistantService {
    /// El cuerpo de `NewConversation`, para que la interfaz registre cómo
    /// terminó.
    async fn new_conversation_for(
        &self,
        header: &Header<'_>,
        connection: &Connection,
    ) -> Result<String, Error> {
        let Some(sender) = header.sender() else {
            return Err(no_sender());
        };
        let id = self.open(sender.as_str())?;
        // Punto a punto no hay demonio del bus a quien preguntar (el par no
        // contestaría y la llamada quedaría colgada), y el único par es el
        // que está llamando.
        if !connection.is_bus() {
            return Ok(id);
        }
        let still_connected = match zbus::fdo::DBusProxy::new(connection).await {
            Ok(bus) => bus
                .name_has_owner(BusName::Unique(sender.as_ref()))
                .await
                .map_err(zbus::Error::from),
            Err(error) => Err(error),
        };
        settle_new_conversation(&self.conversations, sender.as_str(), id, still_connected)
    }
}

impl AssistantService {
    /// El estado de la memoria con la misma puerta que los pedidos: si está
    /// disponible, se mira el modelo efectivo. Remoto sin
    /// `share_memory_with_remote`, no permitido por la configuración (aunque se
    /// comparta: el chat igual lo rechaza) o sin respuesta, no lo está.
    pub async fn memory_status_now(&self) -> MemoryStatus {
        let mut status = self.memory.status().await;
        if status.available {
            let reason = match self.backend.active_model().await {
                Ok(model) if model.local || self.memory.shares_with_remote() => None,
                Ok(_) => Some(crate::memory::REASON_REMOTE),
                Err(BackendError::NotAllowed(_)) => Some(crate::memory::REASON_MODEL_NOT_ALLOWED),
                Err(_) => Some(crate::memory::REASON_MODEL_UNREACHABLE),
            };
            if let Some(reason) = reason {
                status.available = false;
                status.reason = reason.to_owned();
            }
        }
        status
    }
}

/// El pegamento con el bus para la puerta de AppArmor.
impl AssistantService {
    /// Si quien manda el pedido está confinado. Si el bus no contesta, sí.
    async fn confined(&self, connection: &Connection, header: &Header<'_>) -> bool {
        let sender = header.sender().map(|sender| sender.as_str());
        let label = self.labels.label(connection, sender).await;
        let confined = match &label {
            Ok(label) => is_confined(label.as_deref()),
            Err(()) => true,
        };
        // La etiqueta es el nombre de un perfil de AppArmor: no es un secreto,
        // y es lo que hace falta para entender por qué se rechazó.
        tracing::debug!(
            sender = sender.unwrap_or("(sin remitente)"),
            label = %match &label {
                Ok(Some(label)) => String::from_utf8_lossy(label).trim_end_matches('\0').to_owned(),
                Ok(None) => "(sin etiqueta)".to_owned(),
                Err(()) => "(no se pudo leer)".to_owned(),
            },
            confined,
            "puerta de AppArmor"
        );
        confined
    }

    async fn deny_confined(
        &self,
        connection: &Connection,
        header: &Header<'_>,
    ) -> Result<(), Error> {
        if self.confined(connection, header).await {
            return Err(access_denied());
        }
        Ok(())
    }
}

/// Lo mismo que la interfaz, con el dueño ya resuelto: se puede llamar sin bus.
impl AssistantService {
    /// Los modelos instalados en el backend (`Models`, sin el registro).
    pub async fn models(&self) -> Result<Vec<String>, Error> {
        self.backend.models().await.map_err(to_dbus)
    }

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
                reason: error.text().to_owned(),
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

    /// El chat con memoria. Lo usan las pruebas; el bus pasa por
    /// [`Self::chat_with`].
    #[cfg(test)]
    pub async fn chat_as(
        &self,
        owner: &str,
        conversation_id: &str,
        content: &str,
    ) -> Result<String, Error> {
        self.chat_with(owner, conversation_id, content, true).await
    }

    /// El chat, con o sin memoria (un llamador confinado va sin: ni notas ni
    /// aprendizaje).
    pub async fn chat_with(
        &self,
        owner: &str,
        conversation_id: &str,
        content: &str,
        use_memory: bool,
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
        // La época del comienzo del turno, antes de leer la memoria: si un
        // `ClearMemory` (o encender, apagar) llega mientras el modelo responde,
        // de este turno no se aprende nada en la memoria nueva (#20).
        let epoch = self.memory.epoch();
        // La puerta del remoto con el modelo que va a responder este pedido:
        // remoto y sin `share_memory_with_remote`, ni notas ni aprendizaje.
        // Filtro previo: no se lee la memoria si el modelo de ahora no la
        // puede ver. La decisión que vale es la de `chat_local_only`, que usa
        // la misma resolución que manda el pedido.
        let mut use_memory = use_memory && self.memory.allowed_for(&*self.backend).await;
        // Las notas de la memoria van sólo en el contexto de este turno: no al
        // historial ni a `History`.
        let notes = if use_memory {
            self.memory.recall(content, first_turn).await
        } else {
            None
        };
        let with_notes = notes.map(|notes| {
            let mut with_notes = Vec::with_capacity(context.len() + 1);
            with_notes.push(Message {
                role: Role::System,
                content: notes,
            });
            with_notes.extend(context.iter().cloned());
            with_notes
        });
        let reply = match &with_notes {
            Some(with_notes) if !self.memory.shares_with_remote() => {
                match self.backend.chat_local_only(with_notes).await {
                    // El modelo pasó a ser remoto entre la puerta y el envío:
                    // se reintenta sin notas (la persona igual tiene respuesta,
                    // y la conversación ya podía ir a ese modelo) y ese turno
                    // no se aprende.
                    Err(BackendError::NotLocal) => {
                        use_memory = false;
                        self.backend.chat(&context).await
                    }
                    other => other,
                }
            }
            Some(with_notes) => self.backend.chat(with_notes).await,
            None => self.backend.chat(&context).await,
        }
        .map_err(to_dbus)?;
        turn.reply = Some(reply.clone());
        context.push(Message {
            role: Role::Assistant,
            content: reply.clone(),
        });
        if use_memory {
            // Con el candado de las conversaciones: o el aprendizaje queda
            // anotado en vuelo antes de que un borrado lo busque, o la
            // conversación ya se borró y no se aprende.
            let conversations = self.conversations.lock();
            if conversations.contains(owner, conversation_id) {
                self.memory
                    .learn_after(self.backend.clone(), conversation_id, context, epoch);
            }
        }
        Ok(reply)
    }

    pub fn delete_as(&self, owner: &str, conversation_id: &str) -> Result<(), Error> {
        self.conversations.lock().delete(owner, conversation_id)
    }

    /// `DeleteConversation`: la cierra y, en la memoria, cancela lo que se
    /// esté extrayendo de ella y borra sus resúmenes. Lo de la memoria nunca
    /// hace fallar el borrado ([`Memory::forget_conversation`]).
    pub async fn delete_conversation_as(
        &self,
        owner: &str,
        conversation_id: &str,
    ) -> Result<(), Error> {
        self.delete_as(owner, conversation_id)?;
        self.memory.forget_conversation(conversation_id).await;
        Ok(())
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
        self.secrets.store_key(url.as_str(), &key).await?;
        tracing::info!(
            origin = %crate::logging::redact_url(url.as_str()),
            "se guardó la clave del proveedor"
        );
        Ok(())
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
/// bus siempre viene (lo pone el demonio del bus, no el cliente). Sin
/// remitente (una conexión punto a punto, o algo que no lo ponga) la llamada se
/// rechaza: con un dueño vacío, todos los pares compartirían las mismas
/// conversaciones (#4). Es `AccessDenied` y no `InvalidArgs`: los argumentos
/// están bien, lo que falta es saber quién llama.
fn owner<'a>(header: &'a Header<'_>) -> Result<&'a str, Error> {
    header
        .sender()
        .map(|sender| sender.as_str())
        .ok_or_else(no_sender)
}

fn no_sender() -> Error {
    Error::AccessDenied(
        "La llamada no trae remitente: no se puede saber de quién son las conversaciones.".into(),
    )
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

pub fn to_dbus(error: BackendError) -> Error {
    match error {
        BackendError::Busy => Error::Busy(error.text().to_owned()),
        BackendError::NotLocal => Error::Unavailable(error.text().to_owned()),
        BackendError::Unavailable(text) | BackendError::NotAllowed(text) => {
            Error::Unavailable(text)
        }
        BackendError::Failed(text) => Error::Backend(text),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::backend::RejectedBackend;
    use crate::test_support::{memory_off, memory_status_off, model_info, FakeBackend, FixedKey};
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
            // Las pruebas por el bus van punto a punto: no hay demonio del bus
            // a quien pedirle la etiqueta, y BusLabels fallaría cerrado.
            labels: crate::test_support::FixedLabel::none(),
        }
    }

    /// El servicio servido por una conexión punto a punto y su proxy de cliente.
    async fn connected(service: AssistantService) -> (AssistantProxy<'static>, zbus::Connection) {
        let (server, client) = crate::test_support::p2p_pair_as(OWNER, |builder| {
            builder.serve_at(SERVICE_PATH, service)
        })
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
            let texto = error.text().to_owned();
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
            labels: std::sync::Arc::new(crate::service::BusLabels),
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
            labels: std::sync::Arc::new(crate::service::BusLabels),
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
            labels: std::sync::Arc::new(crate::service::BusLabels),
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
            labels: std::sync::Arc::new(crate::service::BusLabels),
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
            labels: crate::test_support::FixedLabel::none(),
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

/// La memoria de punta a punta, por el servicio: aprender de un turno y
/// recuperarlo en otro, lo apagado, lo remoto y lo que nunca se guarda.
#[cfg(test)]
mod memory_tests {
    use std::time::Duration;

    use super::*;
    use crate::memory::{REASON_OFF, REASON_REMOTE};
    use crate::test_support::{FixedKey, ScriptedBackend, TempDir};
    use vasak_assistant_protocol::{AssistantProxy, SERVICE_PATH};

    const OWNER: &str = ":1.1";
    const PREFERENCE: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere respuestas cortas"}],"summary":null}"#;

    struct World {
        tmp: TempDir,
        keys: Arc<FixedKey>,
        backend: Arc<ScriptedBackend>,
        service: AssistantService,
    }

    impl World {
        /// Un servicio con la memoria encendida y un backend guionado.
        async fn new(replies: impl IntoIterator<Item = &'static str>) -> Self {
            let world = Self::off(replies, false);
            world.service.memory.set_enabled(true).await.unwrap();
            world
        }

        fn off(replies: impl IntoIterator<Item = &'static str>, remote_blocked: bool) -> Self {
            let tmp = TempDir::new();
            let keys = FixedKey::new(None);
            let backend = ScriptedBackend::new(replies);
            let memory = Arc::new(
                Memory::new(Some(tmp.memory_dir()), keys.clone())
                    .with_remote_blocked(remote_blocked),
            );
            let service = AssistantService {
                backend: backend.clone(),
                conversations: Arc::default(),
                remote_allowed: false,
                secrets: keys.clone(),
                memory,
                labels: crate::test_support::FixedLabel::none(),
            };
            Self {
                tmp,
                keys,
                backend,
                service,
            }
        }

        async fn say(&self, content: &str) -> (String, String) {
            let id = self.service.open(OWNER).unwrap();
            let reply = self.service.chat_as(OWNER, &id, content).await.unwrap();
            (id, reply)
        }

        /// Espera a que lo extraído en segundo plano esté guardado.
        async fn until_remembered(&self, needle: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let notes = self.service.memory.recall("zzz", true).await;
                if notes.is_some_and(|notes| notes.contains(needle)) {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "«{needle}» no se guardó"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Espera a que termine cualquier extracción lanzada por el último chat
        /// (el permiso se toma antes de que `chat_as` vuelva).
        async fn settle(&self) {
            self.service.memory.wait_until_idle().await;
        }

        fn first_of_call(&self, n: usize) -> Message {
            self.backend.seen.lock()[n][0].clone()
        }
    }

    #[tokio::test]
    async fn lo_que_se_aprende_en_un_turno_llega_al_modelo_en_otra_conversacion_y_no_al_historial()
    {
        let world = World::new(["Listo.", PREFERENCE, "Hola."]).await;

        world.say("prefiero respuestas cortas").await;
        world.backend.wait_for_calls(2).await;
        world.until_remembered("Prefiere respuestas cortas").await;

        let (id, _) = world.say("hola, ¿qué tal?").await;

        let first = world.first_of_call(2);
        assert_eq!(first.role, Role::System);
        assert!(first.content.contains("Prefiere respuestas cortas"));
        assert!(first.content.len() <= crate::memory::recall::MEMORY_CONTEXT_BYTES);
        // El bloque va una sola vez y el mensaje de la persona sigue último.
        let sent = world.backend.seen.lock()[2].clone();
        assert_eq!(sent.iter().filter(|m| m.role == Role::System).count(), 1);
        assert_eq!(sent.last().unwrap().content, "hola, ¿qué tal?");
        // La historia no lo muestra.
        let history = world.service.history_of(OWNER, &id).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|m| m.role != Role::System));
        assert!(history.iter().all(|m| !m.content.contains("Prefiere")));
    }

    #[tokio::test]
    async fn el_pedido_de_extraccion_no_lleva_las_notas_de_la_memoria() {
        let world = World::new(["Listo.", PREFERENCE, "Ok.", PREFERENCE]).await;
        world.say("prefiero respuestas cortas").await;
        world.backend.wait_for_calls(2).await;
        world.until_remembered("Prefiere respuestas cortas").await;

        world.say("además prefiero el mate").await;
        world.backend.wait_for_calls(4).await;

        let extraction = world.backend.seen.lock()[3].clone();
        assert!(extraction
            .iter()
            .all(|m| !m.content.contains("Prefiere respuestas cortas")));
    }

    #[tokio::test]
    async fn sin_recuerdos_el_modelo_recibe_solo_la_conversacion() {
        let world = World::new(["Hola."]).await;

        world.say("hola, ¿qué tal?").await;

        let sent = world.backend.seen.lock()[0].clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].role, Role::User);
    }

    #[tokio::test]
    async fn apagada_no_se_lee_ni_se_escribe_y_al_encender_el_recuerdo_esta() {
        let world = World::new(["Listo.", PREFERENCE, "Ok.", "Hola."]).await;
        world.say("prefiero respuestas cortas").await;
        world.backend.wait_for_calls(2).await;
        world.until_remembered("Prefiere respuestas cortas").await;
        let key = world.keys.memory_key.lock().clone();
        assert!(key.is_some());

        world.service.memory.set_enabled(false).await.unwrap();
        world.say("prefiero el mate amargo").await;
        world.settle().await;

        // Sólo la llamada del chat: ni notas ni una extracción.
        assert_eq!(world.backend.seen.lock().len(), 3);
        assert_eq!(world.first_of_call(2).role, Role::User);
        // Los archivos y la llave quedan.
        assert!(world.tmp.memory_dir().exists());
        assert_eq!(*world.keys.memory_key.lock(), key);

        world.service.memory.set_enabled(true).await.unwrap();
        world.say("hola otra vez").await;
        let first = world.first_of_call(3);
        assert_eq!(first.role, Role::System);
        assert!(first.content.contains("Prefiere respuestas cortas"));
    }

    #[tokio::test]
    async fn con_un_backend_remoto_sin_permiso_no_se_inyecta_ni_se_extrae_y_se_ve_el_motivo() {
        // Primero se aprende con un backend local...
        let world = World::new(["Listo.", PREFERENCE]).await;
        world.say("prefiero respuestas cortas").await;
        world.backend.wait_for_calls(2).await;
        world.until_remembered("Prefiere respuestas cortas").await;
        world.service.memory.close().await;

        // ...y la misma carpeta con un backend remoto sin el permiso.
        let backend = Arc::new(ScriptedBackend {
            replies: parking_lot::Mutex::default(),
            seen: parking_lot::Mutex::default(),
            local: false,
            effective_local: None,
            reject_local_only: parking_lot::Mutex::default(),
        });
        let remote =
            Memory::new(Some(world.tmp.memory_dir()), world.keys.clone()).with_remote_blocked(true);
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: true,
            secrets: world.keys.clone(),
            memory: Arc::new(remote),
            labels: std::sync::Arc::new(crate::service::BusLabels),
        };
        let id = service.open(OWNER).unwrap();

        service
            .chat_as(OWNER, &id, "prefiero el mate amargo")
            .await
            .unwrap();
        world.settle().await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 1, "ni una extracción");
        assert!(seen[0].iter().all(|m| m.role != Role::System));
        let status = service.memory.status().await;
        assert_eq!(status.reason, REASON_REMOTE);
        assert_eq!(
            REASON_REMOTE,
            "La memoria no se comparte con un proveedor remoto"
        );
        let all = capabilities(true, &status);
        let memory = all.iter().find(|c| c.domain == DOMAIN_MEMORY).unwrap();
        assert!(!memory.available);
        assert_eq!(memory.reason, REASON_REMOTE);
    }

    #[tokio::test]
    async fn un_recuerdo_con_un_secreto_o_un_dato_de_la_persona_no_se_guarda() {
        const EXTRACTION: &str = r#"{"items":[
            {"kind":"preference","text":"Usa la clave sk-abcdefghijklmnop"},
            {"kind":"habit","text":"Su correo es ana@ejemplo.com"},
            {"kind":"habit","text":"Guarda todo en /home/ana/docs"},
            {"kind":"habit","text":"Nació el 12/03/1990"},
            {"kind":"habit","text":"Su tarjeta es 4111 1111 1111 1111"},
            {"kind":"preference","text":"Madruga siempre"}
          ],"summary":null}"#;
        let world = World::new(["Listo.", EXTRACTION]).await;

        world.say("suelo madrugar siempre").await;
        world.backend.wait_for_calls(2).await;
        world.until_remembered("Madruga siempre").await;

        let notes = world.service.memory.recall("zzz", true).await.unwrap();
        for forbidden in ["sk-", "clave", "ana@", "/home", "1990", "4111"] {
            assert!(!notes.contains(forbidden), "{forbidden}: {notes}");
        }
        world.service.memory.close().await;
        let db = world.service.memory.database().await.unwrap();
        let count = tokio::task::spawn_blocking(move || {
            crate::memory::schema::MemoryKind::ALL
                .into_iter()
                .map(|kind| crate::memory::schema::entries(&db, kind).len())
                .sum::<usize>()
        })
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn un_ultimo_mensaje_con_un_dato_de_la_persona_no_manda_nada_al_modelo_extractor() {
        let world = World::new(["Listo."]).await;

        world
            .say("escribime a ana@ejemplo.com, prefiero el mate")
            .await;
        world.settle().await;

        // La ventana quedó vacía: no hay nada que pedirle al modelo.
        assert_eq!(world.backend.seen.lock().len(), 1, "sólo el chat");
    }

    #[tokio::test]
    async fn un_turno_con_un_secreto_no_dispara_la_extraccion() {
        let world = World::new(["Listo."]).await;

        world.say("mi contraseña es hunter2").await;
        world.say("prefiero usar sk-abcdefgh12345678 siempre").await;
        world.settle().await;

        assert_eq!(world.backend.seen.lock().len(), 2, "sólo los dos chats");
    }

    #[tokio::test]
    async fn una_extraccion_con_json_roto_no_rompe_el_chat_ni_guarda_nada() {
        let world = World::new(["Listo.", "{\"items\": [{\"kind\": ", "Sigue."]).await;

        world.say("prefiero respuestas cortas").await;
        world.backend.wait_for_calls(2).await;
        world.settle().await;
        let (_, reply) = world.say("hola, ¿qué tal?").await;

        assert_eq!(reply, "Sigue.");
        assert_eq!(world.first_of_call(2).role, Role::User);
    }

    #[tokio::test]
    async fn el_resumen_de_la_conversacion_se_pide_cada_seis_turnos_y_vuelve_en_la_siguiente() {
        const SUMMARY: &str = r#"{"items":[],"summary":"Hablaron de plantas y de mate"}"#;
        let world = World::new(["r", "r", "r", "r", "r", "r", SUMMARY, "Hola."]).await;
        let id = world.service.open(OWNER).unwrap();
        for n in 0..6 {
            world
                .service
                .chat_as(OWNER, &id, &format!("pregunta {n} sobre el clima"))
                .await
                .unwrap();
        }
        world.backend.wait_for_calls(7).await;
        world
            .until_remembered("Hablaron de plantas y de mate")
            .await;

        world.say("hola").await;

        let first = world.first_of_call(7);
        assert_eq!(first.role, Role::System);
        assert!(first
            .content
            .contains(r#"Conversación anterior: "Hablaron de plantas"#));
    }

    #[tokio::test]
    async fn por_dbus_el_estado_de_la_memoria_cambia_al_encender_apagar_y_borrar() {
        let world = World::off([], false);
        let (server, client) =
            crate::test_support::p2p_pair_as(OWNER, |b| b.serve_at(SERVICE_PATH, world.service))
                .await
                .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();

        let off = proxy.memory_status().await.unwrap();
        assert!(!off.enabled && !off.available);
        assert_eq!(off.reason, REASON_OFF);
        assert_eq!(off.bytes, 0);

        proxy.set_memory_enabled(true).await.unwrap();
        let on = proxy.memory_status().await.unwrap();
        assert!(on.enabled && on.available && on.reason.is_empty() && on.bytes > 0);
        let capabilities = proxy.capabilities().await.unwrap();
        assert!(
            capabilities
                .iter()
                .find(|c| c.domain == DOMAIN_MEMORY)
                .unwrap()
                .available
        );

        proxy.set_memory_enabled(false).await.unwrap();
        let apagada = proxy.memory_status().await.unwrap();
        assert!(!apagada.enabled);
        assert!(apagada.bytes > 0, "apagar no borra");

        proxy.clear_memory().await.unwrap();
        assert_eq!(proxy.memory_status().await.unwrap().bytes, 0);
        drop(server);
    }
}

/// La puerta de AppArmor: la decisión pura y de punta a punta por el bus, con
/// una etiqueta de mentira.
#[cfg(test)]
mod gate_tests {

    use super::*;
    use crate::memory::learn::{Extraction, Item};
    use crate::memory::schema::MemoryKind;
    use crate::test_support::{FixedKey, FixedLabel, ScriptedBackend, TempDir};
    use vasak_assistant_protocol::{AssistantProxy, DOMAIN_MEMORY, SERVICE_PATH};

    const CONFINED: &[u8] = b"vasak-appimage (enforce)\0";

    #[test]
    fn solo_una_etiqueta_que_existe_y_no_es_unconfined_confina() {
        for (label, confined) in [
            (None, false),
            // Una etiqueta vacía falla cerrado.
            (Some(&b""[..]), true),
            (Some(&b"\0"[..]), true),
            (Some(&b"   \0"[..]), true),
            (Some(&b"unconfined"[..]), false),
            (Some(&b"unconfined\0"[..]), false),
            (Some(&b" unconfined \0"[..]), false),
            (Some(CONFINED), true),
            // Configuración no tiene perfil y no se exime: si algún día lo
            // tiene, la excepción va por nombre exacto y con security.
            (Some(&b"vasak-settings (enforce)\0"[..]), true),
            (Some(&b"vasak-settings (complain)\0"[..]), true),
            (Some(&b"/usr/bin/vasak-settings (enforce)\0"[..]), true),
            (Some(&b"vasak-appimage (enforce)"[..]), true),
            (Some(&b"docker-default (enforce)\0"[..]), true),
            (Some(&b"/usr/bin/firefox (complain)\0"[..]), true),
            (Some(&b"unconfined (enforce)\0"[..]), true),
            (Some(&b"Unconfined\0"[..]), true),
            // No es UTF-8: no se adivina, se la trata como una etiqueta cualquiera.
            (Some(&[0xff, 0xfe, 0x00][..]), true),
        ] {
            assert_eq!(is_confined(label), confined, "{label:?}");
        }
    }

    #[test]
    fn el_mensaje_de_acceso_denegado_es_un_access_denied_y_no_nombra_secretos() {
        let Error::AccessDenied(text) = access_denied() else {
            panic!("tenía que ser AccessDenied");
        };
        assert!(text.contains("confinada"));
    }

    struct Gate {
        tmp: TempDir,
        keys: Arc<FixedKey>,
        backend: Arc<ScriptedBackend>,
        label: Arc<FixedLabel>,
        memory: Arc<Memory>,
        proxy: AssistantProxy<'static>,
        _server: zbus::Connection,
    }

    async fn gate(label: Arc<FixedLabel>, replies: impl IntoIterator<Item = &'static str>) -> Gate {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let backend = ScriptedBackend::new(replies);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys.clone(),
            memory: memory.clone(),
            labels: label.clone(),
        };
        let (server, client) = crate::test_support::p2p_pair_as(":1.1", |builder| {
            builder.serve_at(SERVICE_PATH, service)
        })
        .await
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        Gate {
            tmp,
            keys,
            backend,
            label,
            memory,
            proxy,
            _server: server,
        }
    }

    fn denied<T: std::fmt::Debug>(result: Result<T, Error>) {
        assert!(
            matches!(result, Err(Error::AccessDenied(_))),
            "se esperaba AccessDenied: {result:?}"
        );
    }

    fn labels() -> Vec<(&'static str, Arc<FixedLabel>)> {
        vec![
            ("confinado", FixedLabel::of(CONFINED)),
            ("el bus no contesta", FixedLabel::failing()),
        ]
    }

    #[tokio::test]
    async fn un_llamador_confinado_no_enciende_la_memoria_y_no_se_crea_nada() {
        for (name, label) in labels() {
            let gate = gate(label, []).await;

            denied(gate.proxy.set_memory_enabled(true).await);

            assert!(!gate.tmp.memory_dir().exists(), "{name}");
            assert!(gate.keys.memory_key.lock().is_none(), "{name}");
            assert!(gate.keys.calls.lock().is_empty(), "{name}");
            assert!(!gate.memory.status().await.enabled, "{name}");
        }
    }

    #[tokio::test]
    async fn un_llamador_confinado_no_apaga_ni_borra_una_memoria_encendida() {
        for (name, label) in labels() {
            let gate = gate(label, []).await;
            gate.memory.set_enabled(true).await.unwrap();
            let key = gate.keys.memory_key.lock().clone();
            let calls = gate.keys.calls.lock().len();

            denied(gate.proxy.set_memory_enabled(false).await);
            denied(gate.proxy.clear_memory().await);

            let status = gate.memory.status().await;
            assert!(status.enabled && status.available, "{name}");
            assert_eq!(*gate.keys.memory_key.lock(), key, "{name}");
            assert!(
                !gate.keys.calls.lock()[calls..]
                    .iter()
                    .any(|call| call.starts_with("delete_memory_key")),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn un_llamador_confinado_no_puede_preguntar_el_estado_de_la_memoria() {
        for (name, label) in labels() {
            let gate = gate(label, []).await;
            gate.memory.set_enabled(true).await.unwrap();

            denied(gate.proxy.memory_status().await);

            assert!(
                *gate.label.asked.lock() >= 1,
                "{name}: se preguntó la etiqueta"
            );
        }
    }

    #[tokio::test]
    async fn un_llamador_confinado_no_guarda_ni_consulta_claves_del_proveedor() {
        for (name, label) in labels() {
            let gate = gate(label, []).await;

            denied(
                gate.proxy
                    .set_provider_key("https://api.openai.com/v1", "sk-del-confinado")
                    .await,
            );
            denied(
                gate.proxy
                    .has_provider_key("https://api.openai.com/v1")
                    .await,
            );

            assert!(gate.keys.stored.lock().is_empty(), "{name}");
            assert!(gate.keys.asked.lock().is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn sin_etiqueta_o_unconfined_los_cinco_metodos_funcionan() {
        for (name, label) in [
            ("sin AppArmor", FixedLabel::none()),
            ("unconfined", FixedLabel::of(b"unconfined\0")),
        ] {
            let gate = gate(label, []).await;

            gate.proxy.set_memory_enabled(true).await.unwrap();
            assert!(gate.memory.status().await.available, "{name}");
            gate.proxy
                .set_provider_key("https://api.openai.com/v1", "sk-propia")
                .await
                .unwrap();
            assert!(gate
                .proxy
                .has_provider_key("https://api.openai.com/v1")
                .await
                .unwrap());
            let status = gate.proxy.memory_status().await.unwrap();
            assert!(status.enabled && status.available, "{name}");
            gate.proxy.clear_memory().await.unwrap();
            gate.proxy.set_memory_enabled(false).await.unwrap();
            assert!(!gate.memory.status().await.enabled, "{name}");
            assert!(*gate.label.asked.lock() >= 6, "se pregunta en cada pedido");
        }
    }

    #[tokio::test]
    async fn el_estado_las_capacidades_y_las_conversaciones_siguen_abiertos_a_un_confinado() {
        let gate = gate(FixedLabel::of(CONFINED), []).await;
        gate.memory.set_enabled(true).await.unwrap();

        // `MemoryStatus` ya no: da `AccessDenied` a un confinado (#22).
        // `Capabilities` sigue abierto, pero no le cuenta cómo está la memoria.
        let capabilities = gate.proxy.capabilities().await.unwrap();
        let memory = capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_MEMORY)
            .unwrap();
        assert!(!memory.available);
        assert_eq!(memory.reason, crate::memory::REASON_CONFINED);
        let id = gate.proxy.new_conversation().await.unwrap();
        gate.proxy.conversations().await.unwrap();
        gate.proxy.status().await.unwrap();
        gate.proxy.history(&id).await.unwrap();
        gate.proxy.delete_conversation(&id).await.unwrap();
    }

    fn memory_capability(capabilities: &[Capability]) -> &Capability {
        capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_MEMORY)
            .unwrap()
    }

    #[tokio::test]
    async fn sin_confinar_y_con_la_memoria_encendida_las_capacidades_dicen_que_esta_disponible() {
        for (name, label) in [
            ("sin AppArmor", FixedLabel::none()),
            ("unconfined", FixedLabel::of(b"unconfined\0")),
        ] {
            let gate = gate(label, []).await;
            gate.memory.set_enabled(true).await.unwrap();

            let capabilities = gate.proxy.capabilities().await.unwrap();

            let memory = memory_capability(&capabilities);
            assert!(memory.available, "{name}: {memory:?}");
            assert_ne!(memory.reason, crate::memory::REASON_CONFINED, "{name}");
        }
    }

    #[tokio::test]
    async fn a_un_confinado_las_capacidades_no_le_consultan_el_llavero_ni_dicen_si_esta_encendida()
    {
        for (name, label) in labels() {
            for enabled in [true, false] {
                for unreachable in [false, true] {
                    let gate = gate(label.clone(), []).await;
                    if enabled {
                        gate.memory.set_enabled(true).await.unwrap();
                    }
                    *gate.keys.unreachable.lock() = unreachable;
                    let calls = gate.keys.calls.lock().len();

                    let capabilities = gate.proxy.capabilities().await.unwrap();

                    let memory = memory_capability(&capabilities);
                    let caso = format!("{name}, encendida={enabled}, llavero caído={unreachable}");
                    assert!(!memory.available, "{caso}");
                    assert_eq!(memory.reason, crate::memory::REASON_CONFINED, "{caso}");
                    assert_eq!(
                        gate.keys.calls.lock().len(),
                        calls,
                        "{caso}: tocó el llavero"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn compartiendo_con_remotos_un_modelo_cloud_sin_allow_remote_sigue_no_disponible_por_el_bus(
    ) {
        use crate::ollama::OllamaBackend;
        use crate::test_support::{client, local_selection, FakeServer, Reply};

        let server = FakeServer::start(|pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(r#"{"models":[{"name":"gpt-oss:120b-cloud"}]}"#),
            _ => Reply::status(404),
        })
        .await;
        let ollama = OllamaBackend::new(
            client(),
            local_selection("ollama", &server.url(""), None),
            false,
        );
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(
            Memory::new(Some(tmp.memory_dir()), keys.clone()).with_share_with_remote(true),
        );
        memory.set_enabled(true).await.unwrap();
        let service = AssistantService {
            backend: Arc::new(crate::backend::Limited::new(Arc::new(ollama))),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory,
            labels: FixedLabel::none(),
        };
        let (_server, client) = crate::test_support::p2p_pair_as(":1.1", |builder| {
            builder.serve_at(SERVICE_PATH, service)
        })
        .await
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();

        let status = proxy.memory_status().await.unwrap();
        assert!(!status.available && status.enabled, "{status:?}");
        assert_eq!(status.reason, crate::memory::REASON_MODEL_NOT_ALLOWED);
        let capabilities = proxy.capabilities().await.unwrap();
        let memory = memory_capability(&capabilities);
        assert!(!memory.available, "{memory:?}");
        assert_eq!(memory.reason, crate::memory::REASON_MODEL_NOT_ALLOWED);
    }

    fn remembered() -> Extraction {
        Extraction {
            items: vec![Item {
                kind: MemoryKind::Preference,
                text: "Prefiere respuestas cortas".into(),
                phrase: None,
                topics: Vec::new(),
            }],
            summary: None,
        }
    }

    #[tokio::test]
    async fn el_chat_de_un_confinado_funciona_pero_sin_notas_y_sin_aprender() {
        for (name, label) in labels() {
            let gate = gate(label, ["Hola.", "No debería pedirse"]).await;
            gate.memory.set_enabled(true).await.unwrap();
            gate.memory.store(remembered(), "c0").await.unwrap();
            let id = gate.proxy.new_conversation().await.unwrap();

            let reply = gate
                .proxy
                .chat(&id, "prefiero respuestas cortas, ¿qué tal?")
                .await
                .unwrap();
            gate.memory.wait_until_idle().await;

            assert_eq!(reply, "Hola.", "{name}");
            let seen = gate.backend.seen.lock().clone();
            assert_eq!(seen.len(), 1, "{name}: ni una extracción");
            assert!(seen[0].iter().all(|m| m.role != Role::System), "{name}");
            assert!(
                seen[0]
                    .iter()
                    .all(|m| !m.content.contains("Prefiere respuestas cortas")),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn el_chat_de_un_llamador_sin_confinar_si_recibe_las_notas() {
        for (name, label) in [
            ("sin AppArmor", FixedLabel::none()),
            ("unconfined", FixedLabel::of(b"unconfined\0")),
        ] {
            let gate = gate(label, ["Hola."]).await;
            gate.memory.set_enabled(true).await.unwrap();
            gate.memory.store(remembered(), "c0").await.unwrap();
            let id = gate.proxy.new_conversation().await.unwrap();

            gate.proxy.chat(&id, "hola, ¿qué tal?").await.unwrap();

            let seen = gate.backend.seen.lock().clone();
            assert_eq!(seen[0][0].role, Role::System, "{name}");
            assert!(
                seen[0][0].content.contains("Prefiere respuestas cortas"),
                "{name}"
            );
        }
    }
}

/// La etiqueta que da el bus de verdad, contra un `dbus-daemon` privado.
#[cfg(test)]
mod bus_label_tests {
    use super::*;
    use crate::test_support::PrivateBus;

    #[tokio::test]
    async fn sin_remitente_no_hay_etiqueta_y_con_un_nombre_invalido_el_bus_falla() {
        let (_server, client) = crate::test_support::p2p_pair(Ok).await.unwrap();

        // Sin remitente falla cerrado (#4).
        assert_eq!(BusLabels.label(&client, None).await, Err(()));
        assert_eq!(
            BusLabels.label(&client, Some("no es un nombre")).await,
            Err(())
        );
    }

    #[tokio::test]
    async fn por_un_bus_real_se_pregunta_por_el_remitente_y_uno_inexistente_es_un_error() {
        let Some(bus) = PrivateBus::start_or_skip() else {
            return;
        };
        let (me, other) = (bus.connect().await, bus.connect().await);
        let sender = other.unique_name().unwrap().to_string();

        // Con o sin AppArmor en la máquina que corre la prueba, el bus contesta.
        let label = BusLabels.label(&me, Some(&sender)).await;
        assert!(label.is_ok(), "{label:?}");
        // Sin remitente, en una conexión de bus de verdad (pasa `is_bus()`): lo que
        // falla es la guarda del remitente ausente, no el chequeo de punto a punto.
        assert!(me.is_bus());
        assert_eq!(BusLabels.label(&me, None).await, Err(()));
        assert_eq!(BusLabels.label(&me, Some(":1.99999")).await, Err(()));
    }
}

/// La puerta del remoto se decide con el modelo efectivo de cada pedido.
#[cfg(test)]
mod effective_model_tests {

    use super::*;
    use crate::memory::schema::MemoryKind;
    use crate::memory::{learn::Extraction, learn::Item, REASON_REMOTE};
    use crate::test_support::{FakeBackend, FixedKey, ScriptedBackend, TempDir};

    const OWNER: &str = ":1.1";
    const EXTRACTION: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#;

    struct Rig {
        _tmp: TempDir,
        service: AssistantService,
    }

    /// Un servicio con la memoria encendida y con "Prefiere respuestas cortas"
    /// ya guardado, sobre el backend dado.
    async fn rig(backend: Arc<dyn ModelBackend>, share_with_remote: bool) -> Rig {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        // Como en `main`: lo configurado es local, así que `remote_blocked` es falso;
        // lo que decide es el modelo efectivo de cada pedido.
        let memory = Arc::new(
            Memory::new(Some(tmp.memory_dir()), keys.clone())
                .with_remote_blocked(false)
                .with_share_with_remote(share_with_remote),
        );
        memory.set_enabled(true).await.unwrap();
        memory
            .store(
                Extraction {
                    items: vec![Item {
                        kind: MemoryKind::Preference,
                        text: "Prefiere respuestas cortas".into(),
                        phrase: None,
                        topics: Vec::new(),
                    }],
                    summary: None,
                },
                "c0",
            )
            .await
            .unwrap();
        let service = AssistantService {
            backend,
            conversations: Arc::default(),
            remote_allowed: true,
            secrets: keys,
            memory,
            labels: Arc::new(BusLabels),
        };
        Rig { _tmp: tmp, service }
    }

    async fn chat(rig: &Rig, text: &str) {
        let id = rig.service.open(OWNER).unwrap();
        rig.service.chat_as(OWNER, &id, text).await.unwrap();
        // Una extracción que no tendría que ocurrir ya estaría tomando el permiso.
        rig.service.memory.wait_until_idle().await;
    }

    async fn remembers(rig: &Rig, needle: &str) -> bool {
        rig.service
            .memory
            .recall("zzz", true)
            .await
            .is_some_and(|notes| notes.contains(needle))
    }

    #[tokio::test]
    async fn con_el_primer_modelo_instalado_en_la_nube_y_sin_permiso_la_memoria_no_entra_ni_se_aprende(
    ) {
        let backend = ScriptedBackend::cloud_by_default(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 1, "ni una extracción");
        assert!(seen[0].iter().all(|m| m.role != Role::System));
        assert!(seen[0]
            .iter()
            .all(|m| !m.content.contains("Prefiere respuestas cortas")));
        assert!(!remembers(&rig, "mate amargo").await, "nada se guardó");
        // Lo que ya había sigue ahí, para quien sí puede verlo.
        assert!(remembers(&rig, "Prefiere respuestas cortas").await);
    }

    #[tokio::test]
    async fn el_estado_y_las_capacidades_dicen_que_la_memoria_no_se_comparte_con_ese_modelo() {
        let rig = rig(ScriptedBackend::cloud_by_default([]), false).await;

        let status = rig.service.memory_status_now().await;
        assert!(status.enabled && !status.available);
        assert_eq!(status.reason, REASON_REMOTE);
        let all = capabilities(true, &status);
        let memory = all
            .iter()
            .find(|c| c.domain == vasak_assistant_protocol::DOMAIN_MEMORY)
            .unwrap();
        assert!(!memory.available);
        assert_eq!(memory.reason, REASON_REMOTE);
    }

    #[tokio::test]
    async fn con_share_memory_with_remote_el_mismo_modelo_en_la_nube_si_ve_la_memoria() {
        let backend = ScriptedBackend::cloud_by_default(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), true).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 2, "el chat y la extracción");
        assert_eq!(seen[0][0].role, Role::System);
        assert!(seen[0][0].content.contains("Prefiere respuestas cortas"));
        assert!(remembers(&rig, "mate amargo").await);
        let status = rig.service.memory_status_now().await;
        assert!(status.available, "{status:?}");
    }

    #[tokio::test]
    async fn con_un_modelo_efectivo_local_la_memoria_entra_aunque_no_se_comparta() {
        let backend = ScriptedBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0][0].role, Role::System);
        assert!(remembers(&rig, "mate amargo").await);
        assert!(rig.service.memory_status_now().await.available);
    }

    #[tokio::test]
    async fn si_el_backend_no_dice_que_modelo_usa_la_puerta_falla_cerrada() {
        let mut fake = FakeBackend::replying("Hola.");
        fake.active_result = Err(crate::backend::BackendError::Unavailable(
            "sin modelo".into(),
        ));
        let backend = Arc::new(fake);
        let rig = rig(backend.clone(), false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].iter().all(|m| m.role != Role::System));
        assert!(!remembers(&rig, "mate amargo").await);
        // Con permiso para compartir no hace falta saber qué modelo es.
        let allowed = Memory::new(None, FixedKey::new(None)).with_share_with_remote(true);
        assert!(allowed.allowed_for(&*backend).await);
    }

    #[tokio::test]
    async fn un_llamador_sin_confinar_y_un_modelo_en_la_nube_tampoco_se_salta_la_puerta() {
        // La puerta del modelo vale para cualquier llamador, no sólo para los confinados.
        let backend = ScriptedBackend::cloud_by_default(["Hola."]);
        let rig = rig(backend.clone(), false).await;
        let id = rig.service.open(OWNER).unwrap();

        rig.service
            .chat_with(OWNER, &id, "hola", true)
            .await
            .unwrap();

        assert!(backend.seen.lock()[0]
            .iter()
            .all(|m| m.role != Role::System));
    }

    // ── La carrera entre la puerta y el envío ─────────────────────────────────

    use crate::backend::Limited;
    use crate::memory::{REASON_MODEL_NOT_ALLOWED, REASON_MODEL_UNREACHABLE};
    use crate::test_support::FlippingBackend;

    /// El modelo se resuelve local en la puerta y `:cloud` al mandar el chat.
    #[tokio::test]
    async fn si_el_modelo_pasa_a_la_nube_entre_la_puerta_y_el_envio_el_chat_sale_sin_notas_y_no_se_aprende(
    ) {
        let backend = FlippingBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), false).await;
        let id = rig.service.open(OWNER).unwrap();

        let reply = rig
            .service
            .chat_as(OWNER, &id, "prefiero el mate amargo, ¿qué tal?")
            .await
            .unwrap();
        rig.service.memory.wait_until_idle().await;

        // La persona recibe su respuesta.
        assert_eq!(reply, "Hola.");
        // Un solo pedido salió al modelo, sin notas.
        let seen = backend.inner.seen.lock().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].iter().all(|m| m.role != Role::System));
        assert!(seen[0]
            .iter()
            .all(|m| !m.content.contains("Prefiere respuestas cortas")));
        // El turno no se aprende: sin extracción y sin nada nuevo guardado.
        assert!(!remembers(&rig, "mate amargo").await);
        // La conversación quedó con el intercambio.
        assert_eq!(rig.service.history_of(OWNER, &id).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn la_misma_carrera_por_un_backend_con_cache_tambien_se_frena() {
        // `Limited` guarda el modelo activo 5 s: la puerta puede ver un local viejo.
        let backend = FlippingBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(Arc::new(Limited::new(backend.clone())), false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.inner.seen.lock().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].iter().all(|m| m.role != Role::System));
        assert!(!remembers(&rig, "mate amargo").await);
    }

    #[tokio::test]
    async fn si_el_modelo_pasa_a_la_nube_entre_la_puerta_y_la_extraccion_esta_no_sale() {
        let backend = FlippingBackend::new([EXTRACTION]);
        let rig = rig(backend.clone(), false).await;
        let conversation = vec![
            Message {
                role: Role::User,
                content: "prefiero el mate amargo".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Anotado.".into(),
            },
        ];

        // La puerta (`allowed_for`, primera resolución) ve un modelo local y la
        // extracción (`chat_local_only`, segunda) lo ve en la nube.
        rig.service.memory.learn_after(
            backend.clone(),
            "c1",
            conversation,
            rig.service.memory.epoch(),
        );
        rig.service.memory.wait_until_idle().await;

        assert!(backend.inner.seen.lock().is_empty(), "nada salió a la nube");
        assert_eq!(
            backend
                .resolutions
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "la puerta y el envío"
        );
        assert!(!remembers(&rig, "mate amargo").await);
    }

    #[tokio::test]
    async fn con_permiso_para_compartir_la_misma_carrera_no_frena_nada() {
        let backend = FlippingBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), true).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.inner.seen.lock().clone();
        assert_eq!(seen.len(), 2, "el chat con notas y la extracción");
        assert_eq!(seen[0][0].role, Role::System);
        assert!(remembers(&rig, "mate amargo").await);
    }

    #[tokio::test]
    async fn un_modelo_local_estable_manda_las_notas_con_chat_local_only_y_se_aprende() {
        let backend = ScriptedBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone(), false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        let seen = backend.seen.lock().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0][0].role, Role::System);
        assert!(remembers(&rig, "mate amargo").await);
    }

    #[tokio::test]
    async fn un_modelo_no_permitido_da_su_propio_motivo_y_no_el_de_que_no_responde() {
        let mut fake = FakeBackend::replying("");
        fake.active_result = Err(BackendError::NotAllowed("no permitido".into()));
        let rig = rig(Arc::new(fake), false).await;

        let status = rig.service.memory_status_now().await;

        assert!(!status.available && status.enabled, "{status:?}");
        assert_eq!(status.reason, REASON_MODEL_NOT_ALLOWED);
        assert_ne!(REASON_MODEL_NOT_ALLOWED, REASON_MODEL_UNREACHABLE);
    }

    #[tokio::test]
    async fn compartiendo_con_remotos_un_modelo_cloud_sin_allow_remote_no_esta_disponible() {
        use crate::ollama::OllamaBackend;
        use crate::test_support::{client, local_selection, FakeServer, Reply};

        let server = FakeServer::start(|pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(r#"{"models":[{"name":"gpt-oss:120b-cloud"}]}"#),
            _ => Reply::status(404),
        })
        .await;
        let ollama = OllamaBackend::new(
            client(),
            local_selection("ollama", &server.url(""), None),
            false,
        );
        let rig = rig(Arc::new(Limited::new(Arc::new(ollama))), true).await;

        let status = rig.service.memory_status_now().await;

        assert!(!status.available && status.enabled, "{status:?}");
        assert_eq!(status.reason, REASON_MODEL_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn compartiendo_con_remotos_un_modelo_remoto_que_responde_sigue_disponible() {
        let rig = rig(ScriptedBackend::cloud_by_default([]), true).await;

        let status = rig.service.memory_status_now().await;

        assert!(status.available && status.reason.is_empty(), "{status:?}");
    }

    #[tokio::test]
    async fn compartiendo_con_remotos_un_backend_caido_da_el_motivo_de_que_no_responde() {
        let mut fake = FakeBackend::replying("");
        fake.active_result = Err(BackendError::Failed("caído".into()));
        let rig = rig(Arc::new(fake), true).await;

        let status = rig.service.memory_status_now().await;

        assert!(!status.available && status.enabled, "{status:?}");
        assert_eq!(status.reason, REASON_MODEL_UNREACHABLE);
    }

    #[tokio::test]
    async fn un_ollama_con_modelo_por_omision_cloud_sin_allow_remote_explica_el_motivo_en_el_estado(
    ) {
        use crate::ollama::OllamaBackend;
        use crate::test_support::{client, local_selection, FakeServer, Reply};

        let server = FakeServer::start(|pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(r#"{"models":[{"name":"gpt-oss:120b-cloud"}]}"#),
            _ => Reply::status(404),
        })
        .await;
        let ollama = OllamaBackend::new(
            client(),
            local_selection("ollama", &server.url(""), None),
            false,
        );
        let rig = rig(Arc::new(Limited::new(Arc::new(ollama))), false).await;

        let status = rig.service.memory_status_now().await;

        assert!(!status.available && status.enabled, "{status:?}");
        assert_eq!(status.reason, REASON_MODEL_NOT_ALLOWED);
    }

    /// Un backend que rechaza `chat_local_only` con el mismo texto que
    /// `NotLocal` pero como `Unavailable`: no es el rechazo, no se reintenta.
    #[tokio::test]
    async fn un_unavailable_con_el_texto_de_no_local_no_reintenta_sin_notas() {
        let inner = ScriptedBackend::new(["Hola."]);
        *inner.reject_local_only.lock() =
            Some(BackendError::Unavailable(crate::backend::NOT_LOCAL.into()));
        let rig = rig(inner.clone(), false).await;
        let id = rig.service.open(OWNER).unwrap();

        let result = rig
            .service
            .chat_as(OWNER, &id, "prefiero el mate amargo, ¿qué tal?")
            .await;

        assert!(matches!(result, Err(Error::Unavailable(_))), "{result:?}");
        assert!(inner.seen.lock().is_empty(), "no se mandó ningún pedido");
    }

    #[tokio::test]
    async fn el_estado_de_la_memoria_dice_por_que_no_esta_disponible_segun_el_modelo() {
        // Local: disponible.
        let local = rig(ScriptedBackend::new([]), false).await;
        let status = local.service.memory_status_now().await;
        assert!(status.available && status.reason.is_empty(), "{status:?}");

        // En la nube: el motivo del remoto.
        let cloud = rig(ScriptedBackend::cloud_by_default([]), false).await;
        let status = cloud.service.memory_status_now().await;
        assert!(!status.available && status.enabled);
        assert_eq!(status.reason, REASON_REMOTE);

        // Que el modelo no conteste es otro motivo, no el del remoto.
        let mut fake = FakeBackend::replying("");
        fake.active_result = Err(crate::backend::BackendError::Unavailable("apagado".into()));
        let silent = rig(Arc::new(fake), false).await;
        let status = silent.service.memory_status_now().await;
        assert!(!status.available && status.enabled);
        assert_eq!(status.reason, REASON_MODEL_UNREACHABLE);
        assert_ne!(REASON_MODEL_UNREACHABLE, REASON_REMOTE);

        // Con permiso para compartir, la nube no la apaga; el silencio sí.
        let shared = rig(ScriptedBackend::cloud_by_default([]), true).await;
        assert!(shared.service.memory_status_now().await.available);
        let mut fake = FakeBackend::replying("");
        fake.active_result = Err(crate::backend::BackendError::Unavailable("apagado".into()));
        let shared = rig(Arc::new(fake), true).await;
        let status = shared.service.memory_status_now().await;
        assert!(!status.available && status.enabled);
        assert_eq!(status.reason, REASON_MODEL_UNREACHABLE);

        // Apagada, el motivo sigue siendo el de apagada.
        let off = rig(ScriptedBackend::cloud_by_default([]), false).await;
        off.service.memory.set_enabled(false).await.unwrap();
        let status = off.service.memory_status_now().await;
        assert_eq!(status.reason, crate::memory::REASON_OFF);
    }

    /// Un modelo que está en la nube sólo en la segunda resolución (la del
    /// envío del chat) y local antes y después: si el turno se aprendiera
    /// igual, la extracción saldría bien y se guardaría.
    struct Blink {
        inner: Arc<ScriptedBackend>,
        resolutions: std::sync::atomic::AtomicUsize,
    }

    impl Blink {
        fn local_now(&self) -> bool {
            self.resolutions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                != 1
        }
    }

    impl ModelBackend for Blink {
        fn info(&self) -> vasak_assistant_protocol::ModelInfo {
            self.inner.info()
        }

        fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
            self.inner.models()
        }

        fn active_model(
            &self,
        ) -> BoxFuture<'_, Result<vasak_assistant_protocol::ModelInfo, BackendError>> {
            let mut info = self.inner.info();
            info.local = self.local_now();
            Box::pin(async move { Ok(info) })
        }

        fn chat<'a>(
            &'a self,
            messages: &'a [Message],
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            self.inner.chat(messages)
        }

        fn chat_local_only<'a>(
            &'a self,
            messages: &'a [Message],
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            if !self.local_now() {
                return Box::pin(async { Err(BackendError::NotLocal) });
            }
            self.inner.chat(messages)
        }
    }

    #[tokio::test]
    async fn el_turno_que_tuvo_que_reintentarse_sin_notas_no_se_aprende_aunque_el_modelo_vuelva_a_ser_local(
    ) {
        let inner = ScriptedBackend::new(["Hola.", EXTRACTION]);
        let backend = Arc::new(Blink {
            inner: inner.clone(),
            resolutions: Default::default(),
        });
        let rig = rig(backend, false).await;

        chat(&rig, "prefiero el mate amargo, ¿qué tal?").await;

        assert_eq!(inner.seen.lock().len(), 1, "sólo el chat, sin extracción");
        assert!(!remembers(&rig, "mate amargo").await);
    }
}

/// Las llamadas sin remitente (#4): conexión punto a punto, sin demonio del bus
/// que ponga el nombre del que llama.
#[cfg(test)]
mod no_sender_tests {
    use std::time::Duration;

    use super::*;
    use crate::conversations::Conversations;
    use crate::test_support::{
        memory_off, p2p_pair, p2p_pair_as, FakeBackend, FixedKey, FixedLabel, TempDir,
    };
    use vasak_assistant_protocol::{AssistantProxy, SERVICE_PATH};

    const ID: &str = "00000000-0000-4000-8000-000000000000";

    struct Rig {
        proxy: AssistantProxy<'static>,
        conversations: Arc<parking_lot::Mutex<Conversations>>,
        backend: Arc<FakeBackend>,
        keys: Arc<FixedKey>,
        _server: Connection,
    }

    /// El servicio por una conexión punto a punto SIN remitente.
    async fn rig(labels: Arc<dyn CallerLabels>, memory: Arc<Memory>) -> Rig {
        let backend = Arc::new(FakeBackend::replying("hola"));
        let keys = FixedKey::new(None);
        let conversations = Arc::new(parking_lot::Mutex::new(Conversations::default()));
        let service = AssistantService {
            backend: backend.clone(),
            conversations: conversations.clone(),
            remote_allowed: false,
            secrets: keys.clone(),
            memory,
            labels,
        };
        let (server, client) = p2p_pair(|b| b.serve_at(SERVICE_PATH, service))
            .await
            .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        Rig {
            proxy,
            conversations,
            backend,
            keys,
            _server: server,
        }
    }

    async fn plain() -> Rig {
        rig(FixedLabel::none(), memory_off()).await
    }

    /// `AccessDenied`; con `needle`, además el motivo tiene que decirlo.
    fn denied_with<T: std::fmt::Debug>(name: &str, needle: &str, result: Result<T, Error>) {
        match result {
            Err(Error::AccessDenied(text)) => assert!(text.contains(needle), "{name}: {text}"),
            other => panic!("{name}: se esperaba AccessDenied, vino {other:?}"),
        }
    }

    fn denied<T: std::fmt::Debug>(name: &str, result: Result<T, Error>) {
        denied_with(name, "remitente", result);
    }

    /// Una llamada que no contesta en 5 s es un cuelgue.
    async fn quickly<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("la llamada se colgó")
    }

    /// Un `#[test]` por método, así cada falla dice cuál fue. La llamada va sin
    /// remitente y con timeout; `$after` revisa que no se tocó nada.
    macro_rules! denied_without_sender {
        ($name:ident, |$rig:ident| $call:expr, $after:expr) => {
            #[tokio::test]
            async fn $name() {
                let $rig = plain().await;
                denied(stringify!($name), quickly($call).await);
                let check: fn(&Rig) = $after;
                check(&$rig);
            }
        };
    }

    fn nothing_created(rig: &Rig) {
        assert_eq!(rig.conversations.lock().len(), 0);
        assert_eq!(rig.conversations.lock().count_for(""), 0);
        assert!(rig.backend.seen.lock().is_empty());
    }

    denied_without_sender!(
        status_sin_remitente_da_access_denied,
        |rig| rig.proxy.status(),
        nothing_created
    );
    denied_without_sender!(
        new_conversation_sin_remitente_da_access_denied_y_no_crea_nada,
        |rig| rig.proxy.new_conversation(),
        nothing_created
    );
    denied_without_sender!(
        conversations_sin_remitente_da_access_denied,
        |rig| rig.proxy.conversations(),
        nothing_created
    );
    denied_without_sender!(
        history_sin_remitente_da_access_denied,
        |rig| rig.proxy.history(ID),
        nothing_created
    );
    denied_without_sender!(
        chat_sin_remitente_da_access_denied_y_no_llega_al_modelo,
        |rig| rig.proxy.chat(ID, "hola"),
        nothing_created
    );
    denied_without_sender!(
        delete_conversation_sin_remitente_da_access_denied,
        |rig| rig.proxy.delete_conversation(ID),
        nothing_created
    );

    #[tokio::test]
    async fn sin_remitente_dos_pares_no_comparten_un_duenio_vacio() {
        // Antes del arreglo todos los pares sin remitente eran el dueño «»: lo
        // que uno abría lo veía y lo borraba otro.
        let rig = plain().await;
        denied("NewConversation", rig.proxy.new_conversation().await);
        denied("Conversations", rig.proxy.conversations().await);
        assert_eq!(rig.conversations.lock().count_for(""), 0);
    }

    #[tokio::test]
    async fn con_la_puerta_de_verdad_los_metodos_de_la_memoria_y_las_claves_tambien_fallan_cerrado()
    {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        // `BusLabels` de verdad, sin remitente: no se puede saber quién llama.
        let rig = rig(Arc::new(BusLabels), memory.clone()).await;

        denied_with(
            "SetMemoryEnabled",
            "",
            rig.proxy.set_memory_enabled(true).await,
        );
        denied_with("ClearMemory", "", rig.proxy.clear_memory().await);
        denied_with(
            "SetProviderKey",
            "",
            rig.proxy
                .set_provider_key("https://api.openai.com/v1", "sk-x")
                .await,
        );
        denied_with(
            "HasProviderKey",
            "",
            rig.proxy
                .has_provider_key("https://api.openai.com/v1")
                .await,
        );

        assert!(!tmp.memory_dir().exists());
        assert!(keys.memory_key.lock().is_none());
        assert!(rig.keys.stored.lock().is_empty());
        assert!(!memory.status().await.enabled);
    }

    #[tokio::test]
    async fn bus_labels_sin_remitente_falla_cerrado() {
        let (_server, client) = p2p_pair(Ok).await.unwrap();
        assert_eq!(BusLabels.label(&client, None).await, Err(()));
    }

    #[tokio::test]
    async fn bus_labels_en_una_conexion_que_no_es_de_bus_falla_cerrado_aunque_haya_remitente() {
        let (server, client) = p2p_pair(Ok).await.unwrap();
        for connection in [&server, &client] {
            assert!(!connection.is_bus());
            // El par pone el remitente que quiere: no hay a quién preguntarle.
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                BusLabels.label(connection, Some(":1.1")),
            )
            .await
            .expect("no tiene que esperar a un bus que no existe");
            assert_eq!(result, Err(()));
        }
    }

    #[tokio::test]
    async fn con_remitente_new_conversation_por_punto_a_punto_contesta_rapido_y_no_se_cuelga() {
        let keys = FixedKey::new(None);
        let service = AssistantService {
            backend: Arc::new(FakeBackend::replying("hola")),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory: memory_off(),
            labels: FixedLabel::none(),
        };
        let (_server, client) = p2p_pair_as(":1.7", |b| b.serve_at(SERVICE_PATH, service))
            .await
            .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();

        let id = quickly(proxy.new_conversation()).await.unwrap();

        assert_eq!(quickly(proxy.conversations()).await.unwrap(), [id]);
    }
}

/// `DeleteConversation` también borra el resumen de la conversación (#12).
#[cfg(test)]
mod delete_conversation_tests {
    use super::*;
    use crate::memory::learn::{Extraction, Item};
    use crate::memory::schema::{entries, MemoryKind};
    use crate::test_support::{p2p_pair_as, FakeBackend, FixedKey, FixedLabel, TempDir};
    use vasak_assistant_protocol::{AssistantProxy, SERVICE_PATH};

    const OWNER: &str = ":1.1";
    const OTHER: &str = ":1.2";

    struct Rig {
        tmp: TempDir,
        service: AssistantService,
    }

    async fn rig() -> Rig {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        memory.set_enabled(true).await.unwrap();
        let service = AssistantService {
            backend: Arc::new(FakeBackend::replying("hola")),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory,
            labels: FixedLabel::none(),
        };
        Rig { tmp, service }
    }

    fn summary(text: &str) -> Extraction {
        Extraction {
            items: Vec::<Item>::new(),
            summary: Some(text.into()),
        }
    }

    /// Los textos de los resúmenes guardados, en orden.
    async fn summaries(memory: &Memory) -> Vec<String> {
        let db = memory.database().await.unwrap();
        let mut all: Vec<String> = tokio::task::spawn_blocking(move || {
            entries(&db, MemoryKind::ConversationSummary)
                .into_iter()
                .map(|e| e.text)
                .collect()
        })
        .await
        .unwrap();
        all.sort();
        all
    }

    /// Dos conversaciones del mismo dueño, cada una con su resumen.
    async fn two_with_summaries(rig: &Rig) -> (String, String) {
        let (a, b) = (
            rig.service.open(OWNER).unwrap(),
            rig.service.open(OWNER).unwrap(),
        );
        rig.service
            .memory
            .store(summary("Resumen de A"), &a)
            .await
            .unwrap();
        rig.service
            .memory
            .store(summary("Resumen de B"), &b)
            .await
            .unwrap();
        (a, b)
    }

    #[tokio::test]
    async fn borrar_una_conversacion_borra_su_resumen_y_queda_solo_el_de_la_otra() {
        let rig = rig().await;
        let (a, b) = two_with_summaries(&rig).await;

        rig.service.delete_conversation_as(OWNER, &a).await.unwrap();

        assert_eq!(summaries(&rig.service.memory).await, ["Resumen de B"]);
        assert_eq!(rig.service.list(OWNER), [b]);
        // En la próxima conversación, la memoria ya no menciona la borrada.
        let block = rig.service.memory.recall("hola", true).await.unwrap();
        assert!(block.contains("Resumen de B") && !block.contains("Resumen de A"));
    }

    #[tokio::test]
    async fn por_dbus_delete_conversation_borra_el_resumen_de_esa_conversacion() {
        let rig = rig().await;
        let (a, b) = two_with_summaries(&rig).await;
        let memory = rig.service.memory.clone();
        let (_server, client) =
            p2p_pair_as(OWNER, |builder| builder.serve_at(SERVICE_PATH, rig.service))
                .await
                .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();

        proxy.delete_conversation(&a).await.unwrap();

        assert_eq!(summaries(&memory).await, ["Resumen de B"]);
        assert_eq!(
            proxy.conversations().await.unwrap(),
            std::slice::from_ref(&b)
        );
        // Borrar la otra deja la memoria sin resúmenes.
        proxy.delete_conversation(&b).await.unwrap();
        assert!(summaries(&memory).await.is_empty());
    }

    #[tokio::test]
    async fn con_la_memoria_apagada_borrar_la_conversacion_no_toca_la_base_y_el_resumen_queda() {
        let rig = rig().await;
        let (a, _b) = two_with_summaries(&rig).await;
        rig.service.memory.set_enabled(false).await.unwrap();

        rig.service.delete_conversation_as(OWNER, &a).await.unwrap();

        // La conversación se cerró; la memoria apagada no se leyó ni se escribió.
        assert!(rig.service.history_of(OWNER, &a).is_err());
        rig.service.memory.set_enabled(true).await.unwrap();
        assert_eq!(
            summaries(&rig.service.memory).await,
            ["Resumen de A", "Resumen de B"]
        );
        assert!(rig.tmp.memory_dir().exists());
    }

    #[tokio::test]
    async fn si_la_conexion_del_duenio_se_cierra_los_resumenes_quedan() {
        let rig = rig().await;
        let (_a, _b) = two_with_summaries(&rig).await;

        let released = rig.service.conversations.lock().release_owner(OWNER);

        assert_eq!(released, 2);
        assert!(rig.service.list(OWNER).is_empty());
        assert_eq!(
            summaries(&rig.service.memory).await,
            ["Resumen de A", "Resumen de B"]
        );
    }

    #[tokio::test]
    async fn borrar_una_que_no_existe_o_de_otro_duenio_no_borra_resumenes_ajenos() {
        let rig = rig().await;
        let (a, _b) = two_with_summaries(&rig).await;
        let unknown = "00000000-0000-4000-8000-000000000000";
        // Un resumen de una conversación que nadie tiene abierta.
        rig.service
            .memory
            .store(summary("Resumen de una vieja"), unknown)
            .await
            .unwrap();

        // Inexistente.
        assert!(matches!(
            rig.service.delete_conversation_as(OWNER, unknown).await,
            Err(Error::NotFound(_))
        ));
        // De otro dueño: el mismo NotFound, y la conversación sigue abierta.
        assert!(matches!(
            rig.service.delete_conversation_as(OTHER, &a).await,
            Err(Error::NotFound(_))
        ));

        assert_eq!(
            summaries(&rig.service.memory).await,
            ["Resumen de A", "Resumen de B", "Resumen de una vieja"]
        );
        assert_eq!(rig.service.list(OWNER).len(), 2);
        // Y la extracción de esa conversación sigue pudiendo guardar: no se anotó como borrada.
        rig.service
            .memory
            .store(summary("Otro de A"), &a)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn borrar_dos_veces_la_misma_da_not_found_la_segunda_y_no_rompe_nada() {
        let rig = rig().await;
        let (a, _b) = two_with_summaries(&rig).await;

        rig.service.delete_conversation_as(OWNER, &a).await.unwrap();
        let second = rig.service.delete_conversation_as(OWNER, &a).await;

        assert!(matches!(second, Err(Error::NotFound(_))));
        assert_eq!(summaries(&rig.service.memory).await, ["Resumen de B"]);
    }

    #[tokio::test]
    async fn dos_resumenes_iguales_de_conversaciones_distintas_borrar_una_deja_el_de_la_otra() {
        let rig = rig().await;
        let (a, b) = (
            rig.service.open(OWNER).unwrap(),
            rig.service.open(OWNER).unwrap(),
        );
        for id in [&a, &b] {
            rig.service
                .memory
                .store(summary("Hablaron de gatos"), id)
                .await
                .unwrap();
        }
        assert_eq!(summaries(&rig.service.memory).await.len(), 2);

        rig.service.delete_conversation_as(OWNER, &a).await.unwrap();

        assert_eq!(summaries(&rig.service.memory).await, ["Hablaron de gatos"]);
        rig.service.delete_conversation_as(OWNER, &b).await.unwrap();
        assert!(summaries(&rig.service.memory).await.is_empty());
    }

    const EXTRACTION: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#;

    /// Un servicio cuyo backend (el chat y la extracción) espera la compuerta.
    async fn gated_rig() -> (Rig, Arc<FakeBackend>, Arc<tokio::sync::Notify>) {
        let (fake, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(fake);
        let mut rig = rig().await;
        rig.service.backend = backend.clone();
        (rig, backend, gate)
    }

    #[tokio::test]
    async fn borrar_la_conversacion_mientras_su_extraccion_esta_en_vuelo_no_guarda_nada() {
        let (rig, backend, gate) = gated_rig().await;
        let service = Arc::new(rig.service);
        let id = service.open(OWNER).unwrap();

        let chat = {
            let (service, id) = (service.clone(), id.clone());
            tokio::spawn(async move {
                service
                    .chat_as(OWNER, &id, "prefiero el mate amargo, ¿qué tal?")
                    .await
            })
        };
        // El chat sale al modelo; se lo libera y arranca la extracción.
        backend.entered.notified().await;
        gate.notify_one();
        chat.await.unwrap().unwrap();
        backend.entered.notified().await;
        assert_eq!(backend.seen.lock().len(), 2, "el chat y la extracción");

        service.delete_conversation_as(OWNER, &id).await.unwrap();
        gate.notify_one();
        service.memory.wait_until_idle().await;

        assert_eq!(service.memory.recall("mate", true).await, None);
    }

    #[tokio::test]
    async fn borrar_la_conversacion_mientras_el_chat_espera_la_respuesta_no_aprende_nada() {
        let (rig, backend, gate) = gated_rig().await;
        let service = Arc::new(rig.service);
        let id = service.open(OWNER).unwrap();

        let chat = {
            let (service, id) = (service.clone(), id.clone());
            tokio::spawn(async move {
                service
                    .chat_as(OWNER, &id, "prefiero el mate amargo, ¿qué tal?")
                    .await
            })
        };
        backend.entered.notified().await;
        service.delete_conversation_as(OWNER, &id).await.unwrap();
        // Con otros cientos de borrados, «id» ya no está en la lista de borradas:
        // lo que frena el aprendizaje es que la conversación ya no existe.
        for n in 0..300 {
            service
                .memory
                .forget_conversation(&format!("otra-{n}"))
                .await;
        }
        gate.notify_one();
        // La persona igual recibe la respuesta.
        chat.await.unwrap().unwrap();
        service.memory.wait_until_idle().await;

        assert_eq!(backend.seen.lock().len(), 1, "sin extracción");
        assert_eq!(service.memory.recall("mate", true).await, None);
        assert!(service.list(OWNER).is_empty());
    }

    #[tokio::test]
    async fn sin_borrar_nada_la_misma_secuencia_si_aprende() {
        let (rig, backend, gate) = gated_rig().await;
        let service = Arc::new(rig.service);
        let id = service.open(OWNER).unwrap();

        let chat = {
            let (service, id) = (service.clone(), id.clone());
            tokio::spawn(async move {
                service
                    .chat_as(OWNER, &id, "prefiero el mate amargo, ¿qué tal?")
                    .await
            })
        };
        backend.entered.notified().await;
        gate.notify_one();
        chat.await.unwrap().unwrap();
        backend.entered.notified().await;
        gate.notify_one();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while service.memory.recall("mate", true).await.is_none() {
            assert!(tokio::time::Instant::now() < deadline, "no se aprendió");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

/// El registro de las llamadas D-Bus: una línea por error, con el método, el
/// remitente y la conversación en corto, y nunca el contenido.
#[cfg(test)]
mod log_tests {
    use super::*;
    use crate::memory::{learn::Extraction, learn::Item, schema::MemoryKind};
    use crate::test_support::{
        capture_logs, p2p_pair, p2p_pair_as, FakeBackend, FixedKey, FixedLabel, TempDir,
    };
    use vasak_assistant_protocol::{AssistantProxy, MAX_CONVERSATIONS_PER_OWNER, SERVICE_PATH};

    const SENDER: &str = ":1.42";
    const CONFINED: &[u8] = b"vasak-appimage (enforce)\0";

    struct Rig {
        _tmp: TempDir,
        proxy: AssistantProxy<'static>,
        memory: Arc<Memory>,
        backend: Arc<FakeBackend>,
        _server: Connection,
    }

    async fn rig_with(
        labels: Arc<dyn CallerLabels>,
        backend: FakeBackend,
        sender: Option<&str>,
    ) -> Rig {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        let backend = Arc::new(backend);
        let service = AssistantService {
            backend: backend.clone(),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory: memory.clone(),
            labels,
        };
        let (server, client) = match sender {
            Some(sender) => p2p_pair_as(sender, |b| b.serve_at(SERVICE_PATH, service)).await,
            None => p2p_pair(|b| b.serve_at(SERVICE_PATH, service)).await,
        }
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        Rig {
            _tmp: tmp,
            proxy,
            memory,
            backend,
            _server: server,
        }
    }

    async fn rig() -> Rig {
        rig_with(
            FixedLabel::none(),
            FakeBackend::replying("hola"),
            Some(SENDER),
        )
        .await
    }

    /// La línea del registro que tiene todos esos pedazos, o falla con todo el texto.
    fn line(log: &crate::test_support::LogCapture, pieces: &[&str]) -> String {
        let text = log.text();
        text.lines()
            .find(|l| pieces.iter().all(|piece| l.contains(piece)))
            .unwrap_or_else(|| panic!("falta una línea con {pieces:?}:\n{text}"))
            .to_owned()
    }

    #[tokio::test]
    async fn un_limit_exceeded_deja_su_linea_con_el_metodo_el_remitente_y_el_error() {
        let rig = rig().await;
        for _ in 0..MAX_CONVERSATIONS_PER_OWNER {
            rig.proxy.new_conversation().await.unwrap();
        }
        let (log, _guard) = capture_logs();

        let result = rig.proxy.new_conversation().await;

        assert!(matches!(result, Err(Error::LimitExceeded(_))));
        let line = line(&log, &["la llamada se rechazó", "LimitExceeded"]);
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("method=\"NewConversation\""), "{line}");
        assert!(line.contains(&format!("sender=\"{SENDER}\"")), "{line}");
        assert!(line.contains("error=\"LimitExceeded\""), "{line}");
    }

    #[tokio::test]
    async fn un_busy_deja_su_linea_con_la_conversacion_en_corto() {
        let (backend, gate) = FakeBackend::gated("hola");
        let rig = rig_with(FixedLabel::none(), backend, Some(SENDER)).await;
        let id = rig.proxy.new_conversation().await.unwrap();
        let (log, _guard) = capture_logs();

        let first = {
            let proxy = rig.proxy.clone();
            let id = id.clone();
            tokio::spawn(async move { proxy.chat(&id, "primero").await })
        };
        rig.backend.entered.notified().await;
        let busy = rig.proxy.chat(&id, "segundo").await;
        gate.notify_one();
        first.await.unwrap().unwrap();

        assert!(matches!(busy, Err(Error::Busy(_))), "{busy:?}");
        let line = line(&log, &["la llamada se rechazó", "Busy"]);
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("method=\"Chat\""), "{line}");
        assert!(
            line.contains(&format!("conversation=\"{}\"", &id[..8])),
            "{line}"
        );
        assert!(line.contains(&format!("sender=\"{SENDER}\"")), "{line}");
        // El id entero no se registra, y tampoco el texto del mensaje.
        assert!(!log.text().contains(&id), "{}", log.text());
        assert!(!log.text().contains("segundo"));
    }

    #[tokio::test]
    async fn un_llamador_confinado_deja_la_linea_de_acceso_denegado_y_la_de_la_puerta() {
        let rig = rig_with(
            FixedLabel::of(CONFINED),
            FakeBackend::replying("hola"),
            Some(SENDER),
        )
        .await;
        let (log, _guard) = capture_logs();

        let result = rig.proxy.set_memory_enabled(true).await;

        assert!(matches!(result, Err(Error::AccessDenied(_))));
        let denied = line(&log, &["la llamada se rechazó", "SetMemoryEnabled"]);
        assert!(denied.contains("WARN"), "{denied}");
        assert!(denied.contains("error=\"AccessDenied\""), "{denied}");
        assert!(denied.contains(&format!("sender=\"{SENDER}\"")), "{denied}");
        let gate = line(&log, &["puerta de AppArmor"]);
        assert!(gate.contains("DEBUG"), "{gate}");
        assert!(gate.contains("vasak-appimage (enforce)"), "{gate}");
        assert!(gate.contains("confined=true"), "{gate}");
    }

    #[tokio::test]
    async fn los_cuatro_metodos_protegidos_dejan_su_linea_de_acceso_denegado() {
        let rig = rig_with(
            FixedLabel::of(CONFINED),
            FakeBackend::replying("hola"),
            Some(SENDER),
        )
        .await;
        let (log, _guard) = capture_logs();

        let _ = rig.proxy.set_memory_enabled(true).await;
        let _ = rig.proxy.clear_memory().await;
        let _ = rig
            .proxy
            .set_provider_key("https://api.openai.com/v1", "sk-canario")
            .await;
        let _ = rig
            .proxy
            .has_provider_key("https://api.openai.com/v1")
            .await;

        for method in [
            "SetMemoryEnabled",
            "ClearMemory",
            "SetProviderKey",
            "HasProviderKey",
        ] {
            let l = line(&log, &["la llamada se rechazó", method]);
            assert!(l.contains("error=\"AccessDenied\""), "{l}");
        }
        assert!(!log.text().contains("sk-canario"));
    }

    #[tokio::test]
    async fn el_chat_de_un_confinado_deja_una_linea_info_de_que_va_sin_memoria() {
        let rig = rig_with(
            FixedLabel::of(CONFINED),
            FakeBackend::replying("hola"),
            Some(SENDER),
        )
        .await;
        let id = rig.proxy.new_conversation().await.unwrap();
        let (log, _guard) = capture_logs();

        rig.proxy
            .chat(&id, "mensaje-canario-confinado")
            .await
            .unwrap();

        let l = line(&log, &["llamador confinado: el chat va sin memoria"]);
        assert!(l.contains("INFO"), "{l}");
        assert!(l.contains("method=\"Chat\""), "{l}");
        assert!(l.contains(&format!("sender=\"{SENDER}\"")), "{l}");
        // Esta línea escribe el id corto con `%` (sin comillas).
        assert!(l.contains(&format!("conversation={}", &id[..8])), "{l}");
        assert!(!l.contains(&id), "{l}");
        assert!(!log.text().contains("mensaje-canario-confinado"));
    }

    #[tokio::test]
    async fn una_llamada_sin_remitente_deja_su_linea_con_el_remitente_ausente() {
        let rig = rig_with(FixedLabel::none(), FakeBackend::replying("hola"), None).await;
        let (log, _guard) = capture_logs();

        let result = rig.proxy.status().await;

        assert!(matches!(result, Err(Error::AccessDenied(_))));
        let l = line(&log, &["la llamada se rechazó", "Status"]);
        assert!(l.contains("WARN"), "{l}");
        assert!(l.contains("sender=\"(sin remitente)\""), "{l}");
        assert!(l.contains("error=\"AccessDenied\""), "{l}");
        assert!(l.contains("remitente"), "{l}");
    }

    #[tokio::test]
    async fn lo_que_sale_bien_va_en_debug_con_el_metodo_y_nada_en_info() {
        let rig = rig().await;
        let (log, _guard) = capture_logs();

        let id = rig.proxy.new_conversation().await.unwrap();
        rig.proxy.chat(&id, "hola").await.unwrap();

        let l = line(&log, &["llamada atendida", "NewConversation"]);
        assert!(
            l.contains("DEBUG") && l.contains(&format!("sender=\"{SENDER}\"")),
            "{l}"
        );
        assert!(l.contains(&format!("conversation=\"{}\"", &id[..8])), "{l}");
        line(&log, &["llamada atendida", "Chat"]);
        assert!(
            !log.text()
                .lines()
                .any(|l| l.contains(" WARN ") || l.contains(" ERROR ")),
            "{}",
            log.text()
        );
    }

    #[tokio::test]
    async fn encender_apagar_y_borrar_la_memoria_dejan_su_linea_info() {
        let rig = rig().await;
        let (log, _guard) = capture_logs();

        rig.proxy.set_memory_enabled(true).await.unwrap();
        let on = line(&log, &["memoria encendida"]);
        assert!(on.contains("INFO") && on.contains("created=true"), "{on}");
        // Encender otra vez no cambia nada y no escribe otra línea.
        rig.proxy.set_memory_enabled(true).await.unwrap();
        assert_eq!(log.text().matches("memoria encendida").count(), 1);

        rig.proxy.clear_memory().await.unwrap();
        let cleared = line(&log, &["memoria borrada"]);
        assert!(
            cleared.contains("INFO") && cleared.contains("recreated=true"),
            "{cleared}"
        );

        rig.proxy.set_memory_enabled(false).await.unwrap();
        let off = line(&log, &["memoria apagada"]);
        assert!(off.contains("INFO"), "{off}");

        rig.proxy.clear_memory().await.unwrap();
        assert!(log.text().contains("recreated=false"), "{}", log.text());
    }

    #[tokio::test]
    async fn guardar_la_clave_registra_solo_el_origen_sin_usuario_ruta_ni_consulta() {
        let rig = rig().await;
        let (log, _guard) = capture_logs();

        rig.proxy
            .set_provider_key("https://api.openai.com/v1", "sk-canarioclaveqzx")
            .await
            .unwrap();

        let l = line(&log, &["se guardó la clave del proveedor"]);
        assert!(l.contains("INFO"), "{l}");
        assert!(l.contains("origin=https://api.openai.com"), "{l}");
        assert!(!l.contains("/v1"), "{l}");
        assert!(!log.text().contains("canarioclaveqzx"));
    }

    #[tokio::test]
    async fn una_clave_con_una_direccion_con_usuario_y_consulta_no_las_repite_en_ningun_nivel() {
        let rig = rig().await;
        let (log, _guard) = capture_logs();

        for url in [
            "https://usuariocanario:clavecanario@api.ejemplo.com/v1?token=consultacanaria",
            "http://usuariocanario:clavecanario@api.ejemplo.com/v1?token=consultacanaria",
            "usuariocanario:clavecanario@api.ejemplo.com/v1?token=consultacanaria",
        ] {
            let _ = rig.proxy.set_provider_key(url, "sk-canarioclaveqzx").await;
            let _ = rig.proxy.has_provider_key(url).await;
        }

        let text = log.text();
        for secret in [
            "usuariocanario",
            "clavecanario",
            "consultacanaria",
            "canarioclaveqzx",
        ] {
            assert!(!text.contains(secret), "{secret}:\n{text}");
        }
    }

    #[tokio::test]
    async fn un_extra_de_memoria_con_recuerdos_no_se_vuelca_al_registro_al_leerlos() {
        // Recordar y borrar la memoria no deja el texto de los recuerdos.
        let rig = rig().await;
        rig.memory.set_enabled(true).await.unwrap();
        let (log, _guard) = capture_logs();
        rig.memory
            .store(
                Extraction {
                    items: vec![Item {
                        kind: MemoryKind::Preference,
                        text: "Prefiere notacanariaqzx".into(),
                        phrase: None,
                        topics: Vec::new(),
                    }],
                    summary: Some("resumencanarioqzx".into()),
                },
                "c1",
            )
            .await
            .unwrap();

        rig.memory.recall("notacanariaqzx", true).await.unwrap();
        rig.memory.forget_conversation("c1").await;
        rig.proxy.clear_memory().await.unwrap();

        let text = log.text();
        assert!(
            !text.contains("notacanariaqzx") && !text.contains("resumencanarioqzx"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn capabilities_y_memory_status_dejan_su_linea_debug() {
        let rig = rig().await;
        let (log, _guard) = capture_logs();

        rig.proxy.capabilities().await.unwrap();
        rig.proxy.memory_status().await.unwrap();
        rig.proxy.models().await.unwrap();

        for method in ["Capabilities", "MemoryStatus", "Models"] {
            let l = line(&log, &["llamada atendida", &format!("method=\"{method}\"")]);
            assert!(l.contains("DEBUG"), "{l}");
            // Quien llama, también en los métodos que no miran el dueño.
            assert!(l.contains(&format!("sender=\"{SENDER}\"")), "{method}: {l}");
        }
        assert!(!log.text().contains(" WARN ") && !log.text().contains(" INFO "));
    }
}

/// Privacidad del registro: un chat completo con memoria, aprendizaje,
/// extracción y errores no deja en ningún nivel (hasta `trace`) el texto de los
/// mensajes, las respuestas, las notas, los recuerdos, los resúmenes, las
/// claves ni las direcciones con usuario o consulta.
#[cfg(test)]
mod privacy_tests {
    use super::*;
    use crate::backend::Limited;
    use crate::memory::{learn::Extraction, learn::Item, schema::MemoryKind};
    use crate::ollama::OllamaBackend;
    use crate::openai::OpenAiBackend;
    use crate::test_support::{
        capture_logs, client, local_selection, FakeServer, FixedKey, FixedLabel, Reply, TempDir,
    };

    const OWNER: &str = ":1.7";
    const TAGS: &str = r#"{"models":[{"name":"llama3.2:latest"}]}"#;

    /// Cada texto de la persona o de la memoria lleva su propio canario.
    const CANARIES: [&str; 8] = [
        "mensajecanarioqzx",
        "respuestacanariaqzx",
        "notacanariaqzx",
        "resumencanarioqzx",
        "recuerdocanarioqzx",
        "segundocanarioqzx",
        "cuerpocanarioqzx",
        "clavecanariaqzx",
    ];

    fn extraction_reply() -> &'static str {
        r#"{"message":{"role":"assistant","content":"{\"items\":[{\"kind\":\"preference\",\"text\":\"Prefiere recuerdocanarioqzx\"}],\"summary\":null}"}}"#
    }

    async fn seeded(memory: &Memory) {
        memory.set_enabled(true).await.unwrap();
        memory
            .store(
                Extraction {
                    items: vec![Item {
                        kind: MemoryKind::Preference,
                        text: "Prefiere notacanariaqzx".into(),
                        phrase: None,
                        topics: Vec::new(),
                    }],
                    summary: Some("resumencanarioqzx".into()),
                },
                "vieja",
            )
            .await
            .unwrap();
    }

    fn assert_clean(text: &str) {
        for canary in CANARIES {
            assert!(!text.contains(canary), "{canary} en el registro:\n{text}");
        }
        for secret in ["Bearer", "usuariocanario", "consultacanaria"] {
            assert!(!text.contains(secret), "{secret} en el registro:\n{text}");
        }
    }

    #[tokio::test]
    async fn un_chat_completo_con_ollama_memoria_extraccion_y_errores_no_deja_ningun_contenido() {
        let status = Arc::new(parking_lot::Mutex::new(200u16));
        let current = status.clone();
        let server = FakeServer::start(move |pedido| match pedido.path.as_str() {
            "/api/tags" => Reply::json(TAGS),
            "/api/chat" if *current.lock() != 200 => {
                Reply::new(*current.lock(), "cuerpocanarioqzx clavecanariaqzx")
            }
            "/api/chat" => {
                if String::from_utf8_lossy(&pedido.body).contains("Extraés recuerdos") {
                    Reply::json(extraction_reply())
                } else {
                    Reply::json(
                        r#"{"message":{"role":"assistant","content":"respuestacanariaqzx"}}"#,
                    )
                }
            }
            _ => Reply::status(404),
        })
        .await;
        let tmp = TempDir::new();
        let keys = FixedKey::new(Some("clavecanariaqzx"));
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        seeded(&memory).await;
        let service = AssistantService {
            backend: Arc::new(Limited::new(Arc::new(OllamaBackend::new(
                client(),
                local_selection("ollama", &server.url(""), None),
                false,
            )))),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory: memory.clone(),
            labels: FixedLabel::none(),
        };
        let (log, _guard) = capture_logs();

        let id = service.open(OWNER).unwrap();
        service
            .chat_as(OWNER, &id, "prefiero mensajecanarioqzx")
            .await
            .unwrap();
        memory.wait_until_idle().await;
        service
            .chat_as(OWNER, &id, "además segundocanarioqzx")
            .await
            .unwrap();
        memory.wait_until_idle().await;
        // Errores: el servidor falla, una conversación que no existe, el borrado.
        *status.lock() = 500;
        service
            .chat_as(OWNER, &id, "prefiero mensajecanarioqzx otra vez")
            .await
            .unwrap_err();
        service
            .chat_as(
                OWNER,
                "00000000-0000-4000-8000-000000000000",
                "mensajecanarioqzx",
            )
            .await
            .unwrap_err();
        service.delete_conversation_as(OWNER, &id).await.unwrap();
        service.memory_status_now().await;

        let text = log.text();
        // Hay registro de verdad (de debug para arriba), y está limpio.
        assert!(text.contains("DEBUG") && text.contains("WARN"), "{text}");
        assert!(
            text.contains("respuesta HTTP") && text.contains("se guardaron recuerdos"),
            "{text}"
        );
        assert_clean(&text);
    }

    #[tokio::test]
    async fn un_chat_con_openai_y_clave_no_deja_la_clave_ni_la_cabecera_en_ningun_nivel() {
        let server = FakeServer::start(|pedido| match pedido.path.as_str() {
            "/v1/models" => Reply::json(r#"{"data":[{"id":"modelo-a"}]}"#),
            "/v1/chat/completions" => Reply::json(
                r#"{"choices":[{"message":{"role":"assistant","content":"respuestacanariaqzx"}}]}"#,
            ),
            _ => Reply::status(404),
        })
        .await;
        let tmp = TempDir::new();
        let keys = FixedKey::new(Some("sk-clavecanariaqzx"));
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        seeded(&memory).await;
        let service = AssistantService {
            backend: Arc::new(Limited::new(Arc::new(OpenAiBackend::new(
                client(),
                local_selection("openai", &server.url("/v1"), Some("modelo-a")),
                keys.clone(),
            )))),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory: memory.clone(),
            labels: FixedLabel::none(),
        };
        let (log, _guard) = capture_logs();

        let id = service.open(OWNER).unwrap();
        service
            .chat_as(OWNER, &id, "prefiero mensajecanarioqzx")
            .await
            .unwrap();
        memory.wait_until_idle().await;
        service.status_for(OWNER).await;

        let text = log.text();
        assert!(text.contains("respuesta HTTP"), "{text}");
        assert_clean(&text);
    }

    #[tokio::test]
    async fn los_errores_del_servicio_con_texto_de_la_persona_no_lo_repiten() {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        let service = AssistantService {
            backend: Arc::new(crate::test_support::FakeBackend::replying(
                "respuestacanariaqzx",
            )),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory,
            labels: FixedLabel::none(),
        };
        let (log, _guard) = capture_logs();

        // Un mensaje demasiado largo, vacío, un id que no es un uuid y una dirección inválida.
        let id = service.open(OWNER).unwrap();
        let _ = service
            .chat_as(
                OWNER,
                &id,
                &format!("mensajecanarioqzx{}", "x".repeat(40_000)),
            )
            .await;
        let _ = service.chat_as(OWNER, &id, "").await;
        let _ = service.chat_as(OWNER, "mensajecanarioqzx", "hola").await;
        let _ = service
            .store_provider_key(
                "https://usuariocanario:clavecanariaqzx@x.com/?q=consultacanaria",
                "clavecanariaqzx",
            )
            .await;

        assert_clean(&log.text());
    }
}

/// El tope de chats en vuelo por dueño, de punta a punta por el servicio (#3).
#[cfg(test)]
mod owner_chat_limit_tests {
    use super::*;
    use crate::backend::{BackendError, Limited, MAX_IN_FLIGHT};
    use crate::test_support::{capture_logs, p2p_pair_as, FakeBackend, FixedKey, FixedLabel};
    use std::time::Duration;
    use vasak_assistant_protocol::{AssistantProxy, MAX_CHATS_IN_FLIGHT_PER_OWNER, SERVICE_PATH};

    const A: &str = ":1.1";
    const B: &str = ":1.2";

    fn service(backend: Arc<dyn ModelBackend>) -> Arc<AssistantService> {
        Arc::new(AssistantService {
            backend,
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: crate::test_support::memory_off(),
            labels: FixedLabel::none(),
        })
    }

    async fn quickly<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("la llamada se colgó")
    }

    /// Un chat que queda esperando al modelo.
    fn start_chat(
        service: &Arc<AssistantService>,
        owner: &'static str,
        id: &str,
    ) -> tokio::task::JoinHandle<Result<String, Error>> {
        let (service, id) = (service.clone(), id.to_owned());
        tokio::spawn(async move { service.chat_as(owner, &id, "hola").await })
    }

    #[tokio::test]
    async fn el_tercer_chat_de_un_duenio_da_busy_sin_llegar_al_modelo_ni_gastar_cupo_global() {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let ids: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let first = start_chat(&service, A, &ids[0]);
        quickly(backend.entered.notified()).await;
        let second = start_chat(&service, A, &ids[1]);
        quickly(backend.entered.notified()).await;

        let third = quickly(service.chat_as(A, &ids[2], "tres")).await;

        assert_eq!(MAX_CHATS_IN_FLIGHT_PER_OWNER, 2);
        assert!(matches!(third, Err(Error::Busy(_))), "{third:?}");
        assert_eq!(
            backend.seen.lock().len(),
            2,
            "el tercero no salió al modelo"
        );
        // Dos lugares globales siguen libres: otros dos chats de otro dueño entran.
        let others: Vec<_> = (0..2).map(|_| service.open(B).unwrap()).collect();
        let b1 = start_chat(&service, B, &others[0]);
        quickly(backend.entered.notified()).await;
        let b2 = start_chat(&service, B, &others[1]);
        quickly(backend.entered.notified()).await;
        assert_eq!(backend.seen.lock().len(), 4);
        assert_eq!(MAX_IN_FLIGHT, 4);

        for _ in 0..4 {
            gate.notify_one();
        }
        for task in [first, second, b1, b2] {
            assert_eq!(quickly(task).await.unwrap().unwrap(), "hola");
        }
    }

    #[tokio::test]
    async fn con_un_duenio_en_su_tope_otro_chatea_con_normalidad() {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let a: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let a1 = start_chat(&service, A, &a[0]);
        quickly(backend.entered.notified()).await;
        let a2 = start_chat(&service, A, &a[1]);
        quickly(backend.entered.notified()).await;
        assert!(matches!(
            quickly(service.chat_as(A, &a[2], "tres")).await,
            Err(Error::Busy(_))
        ));
        let b = service.open(B).unwrap();

        let other = start_chat(&service, B, &b);
        quickly(backend.entered.notified()).await;
        // Los tres están en el modelo a la vez: entró el de otro dueño.
        assert_eq!(backend.seen.lock().len(), 3);
        for _ in 0..3 {
            gate.notify_one();
        }

        assert_eq!(quickly(other).await.unwrap().unwrap(), "hola");
        for task in [a1, a2] {
            quickly(task).await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn cuando_termina_un_chat_el_duenio_vuelve_a_tener_lugar() {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let ids: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let first = start_chat(&service, A, &ids[0]);
        quickly(backend.entered.notified()).await;
        let second = start_chat(&service, A, &ids[1]);
        quickly(backend.entered.notified()).await;
        assert!(quickly(service.chat_as(A, &ids[2], "tres")).await.is_err());

        gate.notify_one();
        quickly(first).await.unwrap().unwrap();
        let third = start_chat(&service, A, &ids[2]);
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        gate.notify_one();

        quickly(second).await.unwrap().unwrap();
        quickly(third).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn un_chat_que_falla_o_se_pasa_de_tiempo_tambien_devuelve_el_lugar() {
        for error in [
            BackendError::Failed("el modelo falló".into()),
            BackendError::Unavailable("tiempo agotado: el modelo no contestó".into()),
        ] {
            let fake = FakeBackend::failing(error);
            let service = service(Arc::new(Limited::new(Arc::new(fake))));
            let ids: Vec<_> = (0..4).map(|_| service.open(A).unwrap()).collect();

            // Más chats seguidos que el tope: cada error libera el lugar.
            for id in &ids {
                let result = quickly(service.chat_as(A, id, "hola")).await;
                assert!(
                    !matches!(result, Err(Error::Busy(_))),
                    "se debería haber liberado: {result:?}"
                );
                assert!(result.is_err());
            }
            assert_eq!(
                service.conversations.lock().len(),
                4,
                "las conversaciones siguen abiertas"
            );
        }
    }

    #[tokio::test]
    async fn un_chat_abandonado_a_mitad_tambien_devuelve_el_lugar() {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let ids: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let first = start_chat(&service, A, &ids[0]);
        quickly(backend.entered.notified()).await;
        let second = start_chat(&service, A, &ids[1]);
        quickly(backend.entered.notified()).await;
        assert!(quickly(service.chat_as(A, &ids[2], "tres")).await.is_err());

        // El cliente se va a mitad del pedido (el pedido se abandona).
        first.abort();
        let _ = first.await;

        let third = start_chat(&service, A, &ids[2]);
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        gate.notify_one();
        gate.notify_one();
        quickly(third).await.unwrap().unwrap();
        quickly(second).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn la_extraccion_en_segundo_plano_no_cuenta_para_el_tope_del_duenio() {
        let (fake, gate) = FakeBackend::gated(
            r#"{"items":[{"kind":"preference","text":"Prefiere el mate"}],"summary":null}"#,
        );
        let backend = Arc::new(fake);
        let tmp = crate::test_support::TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        memory.set_enabled(true).await.unwrap();
        let service = Arc::new(AssistantService {
            backend: Arc::new(Limited::new(backend.clone())),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory: memory.clone(),
            labels: FixedLabel::none(),
        });
        let ids: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let first = {
            let (service, id) = (service.clone(), ids[0].clone());
            tokio::spawn(async move { service.chat_as(A, &id, "prefiero el mate amargo").await })
        };
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        quickly(first).await.unwrap().unwrap();
        // La extracción de ese turno queda esperando al modelo.
        quickly(backend.entered.notified()).await;

        // El dueño igual tiene sus dos lugares.
        let second = start_chat(&service, A, &ids[1]);
        quickly(backend.entered.notified()).await;
        let third = start_chat(&service, A, &ids[2]);
        quickly(backend.entered.notified()).await;
        let busy = quickly(service.chat_as(A, &service.open(A).unwrap(), "otro")).await;
        assert!(matches!(busy, Err(Error::Busy(_))), "{busy:?}");

        for _ in 0..3 {
            gate.notify_one();
        }
        quickly(second).await.unwrap().unwrap();
        quickly(third).await.unwrap().unwrap();
        memory.wait_until_idle().await;
    }

    #[tokio::test]
    async fn el_rechazo_por_cupo_deja_su_linea_warn_con_el_remitente_y_la_conversacion() {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = AssistantService {
            backend: Arc::new(Limited::new(backend.clone())),
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: FixedKey::new(None),
            memory: crate::test_support::memory_off(),
            labels: FixedLabel::none(),
        };
        let (_server, client) = p2p_pair_as(A, |b| b.serve_at(SERVICE_PATH, service))
            .await
            .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let ids = [
            proxy.new_conversation().await.unwrap(),
            proxy.new_conversation().await.unwrap(),
            proxy.new_conversation().await.unwrap(),
        ];
        let (log, _guard) = capture_logs();
        let tasks: Vec<_> = ids[..2]
            .iter()
            .map(|id| {
                let (proxy, id) = (proxy.clone(), id.clone());
                tokio::spawn(async move { proxy.chat(&id, "hola").await })
            })
            .collect();
        quickly(backend.entered.notified()).await;
        quickly(backend.entered.notified()).await;

        let third = quickly(proxy.chat(&ids[2], "mensajecanarioqzx")).await;

        assert!(matches!(third, Err(Error::Busy(_))), "{third:?}");
        let text = log.text();
        let l = text
            .lines()
            .find(|l| l.contains("la llamada se rechazó") && l.contains("Busy"))
            .unwrap_or_else(|| panic!("falta la línea:\n{text}"));
        assert!(l.contains("WARN"), "{l}");
        assert!(l.contains("method=\"Chat\""), "{l}");
        assert!(l.contains(&format!("sender=\"{A}\"")), "{l}");
        assert!(
            l.contains(&format!("conversation=\"{}\"", &ids[2][..8])),
            "{l}"
        );
        assert!(l.contains("error=\"Busy\""), "{l}");
        assert!(!text.contains("mensajecanarioqzx"));
        for _ in 0..2 {
            gate.notify_one();
        }
        for task in tasks {
            quickly(task).await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn borrar_las_conversaciones_con_chats_en_vuelo_no_deja_esquivar_el_tope_por_el_servicio()
    {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let ids: Vec<_> = (0..4).map(|_| service.open(A).unwrap()).collect();
        let first = start_chat(&service, A, &ids[0]);
        quickly(backend.entered.notified()).await;
        let second = start_chat(&service, A, &ids[1]);
        quickly(backend.entered.notified()).await;

        // El cliente borra las dos conversaciones que esperan y prueba con otras.
        service.delete_conversation_as(A, &ids[0]).await.unwrap();
        service.delete_conversation_as(A, &ids[1]).await.unwrap();
        let third = quickly(service.chat_as(A, &ids[2], "tres")).await;

        assert!(matches!(third, Err(Error::Busy(_))), "{third:?}");
        assert_eq!(backend.seen.lock().len(), 2);
        // Al terminar los pedidos (aunque sus conversaciones ya no estén), vuelve el lugar.
        for _ in 0..2 {
            gate.notify_one();
        }
        quickly(first).await.unwrap().unwrap();
        quickly(second).await.unwrap().unwrap();
        let again = start_chat(&service, A, &ids[3]);
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        quickly(again).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn al_irse_el_duenio_con_chats_colgados_sus_lugares_se_liberan_y_los_pedidos_viejos_no_rompen(
    ) {
        let (fake, gate) = FakeBackend::gated("hola");
        let backend = Arc::new(fake);
        let service = service(Arc::new(Limited::new(backend.clone())));
        let old: Vec<_> = (0..2).map(|_| service.open(A).unwrap()).collect();
        let first = start_chat(&service, A, &old[0]);
        quickly(backend.entered.notified()).await;
        let second = start_chat(&service, A, &old[1]);
        quickly(backend.entered.notified()).await;

        let released = service.conversations.lock().release_owner(A);
        assert_eq!(released, 2);
        // Un dueño con el mismo nombre arranca limpio: sus dos chats entran.
        let fresh: Vec<_> = (0..3).map(|_| service.open(A).unwrap()).collect();
        let f1 = start_chat(&service, A, &fresh[0]);
        quickly(backend.entered.notified()).await;
        let f2 = start_chat(&service, A, &fresh[1]);
        quickly(backend.entered.notified()).await;
        let third = quickly(service.chat_as(A, &fresh[2], "tres")).await;
        assert!(matches!(third, Err(Error::Busy(_))), "{third:?}");

        for _ in 0..4 {
            gate.notify_one();
        }
        // Los pedidos de la conexión que se fue terminan sin romper nada.
        for task in [first, second, f1, f2] {
            let _ = quickly(task).await.unwrap();
        }
    }

    #[test]
    fn el_cupo_de_modelos_lleno_llega_como_busy_y_cualquier_otro_unavailable_sigue_siendo_unavailable(
    ) {
        assert!(matches!(
            to_dbus(BackendError::Busy),
            Error::Busy(text) if text == crate::backend::METADATA_BUSY
        ));
        assert!(matches!(
            to_dbus(BackendError::Unavailable("apagado".into())),
            Error::Unavailable(text) if text == "apagado"
        ));
        assert!(matches!(
            to_dbus(BackendError::Failed(crate::backend::METADATA_BUSY.into())),
            Error::Backend(_)
        ));
    }

    #[test]
    fn to_dbus_decide_por_variante_y_no_por_el_texto() {
        assert!(matches!(
            to_dbus(BackendError::NotLocal),
            Error::Unavailable(text) if text == crate::backend::NOT_LOCAL
        ));
        assert!(matches!(
            to_dbus(BackendError::NotAllowed("en la nube".into())),
            Error::Unavailable(text) if text == "en la nube"
        ));
        // El texto del cupo lleno en un `Unavailable` no es el cupo lleno.
        assert!(matches!(
            to_dbus(BackendError::Unavailable(
                crate::backend::METADATA_BUSY.into()
            )),
            Error::Unavailable(_)
        ));
    }
}

/// La época se toma al comienzo del turno (#20): borrar, apagar o encender la
/// memoria mientras el modelo responde hace que de ese turno no se aprenda nada.
#[cfg(test)]
mod turn_epoch_tests {
    use super::*;
    use crate::test_support::{FakeBackend, FixedKey, FixedLabel, ScriptedBackend, TempDir};

    const OWNER: &str = ":1.1";
    const EXTRACTION: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#;

    struct Rig {
        _tmp: TempDir,
        service: Arc<AssistantService>,
    }

    async fn rig(backend: Arc<dyn ModelBackend>) -> Rig {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        memory.set_enabled(true).await.unwrap();
        let service = Arc::new(AssistantService {
            backend,
            conversations: Arc::default(),
            remote_allowed: false,
            secrets: keys,
            memory,
            labels: FixedLabel::none(),
        });
        Rig { _tmp: tmp, service }
    }

    async fn quickly<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), future)
            .await
            .expect("la llamada se colgó")
    }

    /// Un turno que pasa `worth_learning` y queda bloqueado en el modelo.
    fn start_turn(
        service: &Arc<AssistantService>,
        id: &str,
    ) -> tokio::task::JoinHandle<Result<String, Error>> {
        let (service, id) = (service.clone(), id.to_owned());
        tokio::spawn(async move {
            service
                .chat_with(OWNER, &id, "prefiero el mate amargo", true)
                .await
        })
    }

    async fn nothing_learned(rig: &Rig) {
        rig.service.memory.wait_until_idle().await;
        let notes = rig.service.memory.recall("mate amargo", true).await;
        assert!(
            notes.is_none_or(|block| !block.contains("mate amargo")),
            "se aprendió algo de un turno de otra época"
        );
    }

    #[tokio::test]
    async fn borrar_la_memoria_mientras_el_modelo_responde_no_aprende_nada_del_turno() {
        let (fake, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(fake);
        let rig = rig(backend.clone()).await;
        let id = rig.service.open(OWNER).unwrap();
        let turn = start_turn(&rig.service, &id);
        quickly(backend.entered.notified()).await;

        rig.service.memory.clear().await.unwrap();
        gate.notify_one();
        quickly(turn).await.unwrap().unwrap();
        rig.service.memory.wait_until_idle().await;

        // El chat sale; la extracción no: el backend recibió una sola llamada.
        assert_eq!(backend.seen.lock().len(), 1);
        nothing_learned(&rig).await;
        // La memoria nueva sigue encendida y vacía.
        assert!(rig.service.memory.status().await.available);
    }

    #[tokio::test]
    async fn apagar_y_encender_la_memoria_mientras_el_modelo_responde_tampoco_aprende() {
        let (fake, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(fake);
        let rig = rig(backend.clone()).await;
        let id = rig.service.open(OWNER).unwrap();
        let turn = start_turn(&rig.service, &id);
        quickly(backend.entered.notified()).await;

        rig.service.memory.set_enabled(false).await.unwrap();
        rig.service.memory.set_enabled(true).await.unwrap();
        gate.notify_one();
        quickly(turn).await.unwrap().unwrap();

        assert_eq!(backend.seen.lock().len(), 1);
        nothing_learned(&rig).await;
    }

    #[tokio::test]
    async fn solo_apagar_la_memoria_mientras_el_modelo_responde_tampoco_aprende() {
        let (fake, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(fake);
        let rig = rig(backend.clone()).await;
        let id = rig.service.open(OWNER).unwrap();
        let turn = start_turn(&rig.service, &id);
        quickly(backend.entered.notified()).await;

        rig.service.memory.set_enabled(false).await.unwrap();
        gate.notify_one();
        quickly(turn).await.unwrap().unwrap();
        rig.service.memory.wait_until_idle().await;

        assert_eq!(backend.seen.lock().len(), 1);
        rig.service.memory.set_enabled(true).await.unwrap();
        nothing_learned(&rig).await;
    }

    #[tokio::test]
    async fn sin_cambios_en_medio_el_turno_normal_sigue_aprendiendo_de_punta_a_punta() {
        let backend = ScriptedBackend::new(["Hola.", EXTRACTION]);
        let rig = rig(backend.clone()).await;
        let id = rig.service.open(OWNER).unwrap();

        rig.service
            .chat_with(OWNER, &id, "prefiero el mate amargo", true)
            .await
            .unwrap();
        rig.service.memory.wait_until_idle().await;

        assert_eq!(backend.seen.lock().len(), 2, "el chat y la extracción");
        let notes = rig.service.memory.recall("zzz", true).await.unwrap();
        assert!(notes.contains("Prefiere el mate amargo"), "{notes}");
    }

    #[tokio::test]
    async fn el_turno_siguiente_ya_con_la_epoca_nueva_aprende_con_normalidad() {
        let (fake, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(fake);
        let rig = rig(backend.clone()).await;
        let id = rig.service.open(OWNER).unwrap();
        let first = start_turn(&rig.service, &id);
        quickly(backend.entered.notified()).await;
        rig.service.memory.clear().await.unwrap();
        gate.notify_one();
        quickly(first).await.unwrap().unwrap();
        assert_eq!(backend.seen.lock().len(), 1);

        // Otro turno de la misma conversación, empezado después del borrado.
        let second = start_turn(&rig.service, &id);
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        quickly(second).await.unwrap().unwrap();
        // Ahora sí sale la extracción; hay que soltarla.
        quickly(backend.entered.notified()).await;
        gate.notify_one();
        rig.service.memory.wait_until_idle().await;

        assert_eq!(backend.seen.lock().len(), 3);
        let notes = rig.service.memory.recall("zzz", true).await.unwrap();
        assert!(notes.contains("Prefiere el mate amargo"), "{notes}");
    }
}
