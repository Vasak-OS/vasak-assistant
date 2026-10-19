//! El contrato entre el asistente de IA residente de VasakOS y quien le habla:
//! el escritorio, Configuración, la terminal y las aplicaciones.
//!
//! Todo lo que viaja por D-Bus se define acá una sola vez, así los lados no se
//! separan: un campo renombrado en una punta deja de compilar en la otra en lugar
//! de volverse, en silencio, una respuesta que nadie entiende.
//!
//! # Lo que hay en esta fase
//!
//! Conversación con un modelo, local por omisión (Ollama en la máquina) o, si la
//! persona lo permitió explícitamente, un proveedor remoto compatible con la API
//! de OpenAI. Ninguna capacidad sobre datos del usuario:
//! [`Assistant::capabilities`] las lista como **no disponibles**, nunca como un
//! error ni como un método que falla (decisión de producto,
//! vasak-accounts#24). Lo que falta se ve que falta.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

// ── Direcciones del bus ───────────────────────────────────────────────────────

/// El servicio corre en el bus de **sesión**, como el usuario con la sesión
/// abierta: las conversaciones son de quien está sentado en la máquina.
pub const SERVICE_NAME: &str = "ar.net.vasak.os.Assistant";
pub const SERVICE_PATH: &str = "/ar/net/vasak/os/Assistant";
pub const SERVICE_INTERFACE: &str = "ar.net.vasak.os.Assistant";

// ── Límites ───────────────────────────────────────────────────────────────────
//
// Están en el contrato y no escondidos en el servicio para que un cliente pueda
// cortar antes de mandar, en vez de enterarse por el error.

/// El tamaño máximo, en bytes UTF-8, de un mensaje que se le manda a `Chat`.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

/// Cuántas conversaciones abiertas puede haber a la vez.
pub const MAX_CONVERSATIONS: usize = 32;

/// Cuántos mensajes guarda una conversación. Pasado eso se olvidan los más
/// viejos; el último intercambio se conserva siempre.
pub const MAX_HISTORY_MESSAGES: usize = 100;

/// Cuántos bytes de texto guarda una conversación entre todos sus mensajes.
/// Pasado eso se olvidan los más viejos, igual que con [`MAX_HISTORY_MESSAGES`].
pub const MAX_HISTORY_BYTES: usize = 512 * 1024;

// ── Proveedores ───────────────────────────────────────────────────────────────

/// Ollama, con su API propia (`/api/chat`, `/api/tags`).
pub const PROVIDER_OLLAMA: &str = "ollama";
/// Cualquier servidor compatible con la API de OpenAI (`/chat/completions`,
/// `/models`): el de OpenAI y la mayoría de los proveedores, pero también
/// llama.cpp server o LM Studio en la máquina.
pub const PROVIDER_OPENAI: &str = "openai";

// ── Dominios ──────────────────────────────────────────────────────────────────

/// Los dominios sobre los que el asistente puede llegar a tener una capacidad.
/// Viajan como texto y no como enumeración para que agregar uno en una fase
/// nueva no rompa a un cliente viejo que no lo conoce.
pub const DOMAIN_MAIL: &str = "mail";
pub const DOMAIN_CALENDAR: &str = "calendar";
pub const DOMAIN_CONTACTS: &str = "contacts";
pub const DOMAIN_FILES: &str = "files";
pub const DOMAIN_WEB: &str = "web";
pub const DOMAIN_MEMORY: &str = "memory";
pub const DOMAIN_REMOTE_BACKENDS: &str = "remote_backends";

/// Todos los dominios, en el orden en que se informan.
pub const DOMAINS: [&str; 7] = [
    DOMAIN_MAIL,
    DOMAIN_CALENDAR,
    DOMAIN_CONTACTS,
    DOMAIN_FILES,
    DOMAIN_WEB,
    DOMAIN_MEMORY,
    DOMAIN_REMOTE_BACKENDS,
];

// ── La conversación ───────────────────────────────────────────────────────────

/// Quién dijo un mensaje. Viaja como texto: `system`, `user` o `assistant`,
/// los mismos nombres que usan los backends de modelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
#[zvariant(signature = "s")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Un mensaje de una conversación.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

// ── El estado ─────────────────────────────────────────────────────────────────

/// El modelo con el que responde el asistente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ModelInfo {
    /// El proveedor: uno de los `PROVIDER_*`.
    pub provider: String,
    /// El nombre del modelo, tal como lo conoce el proveedor
    /// (`llama3.2:latest`). Vacío si todavía no hay ninguno.
    pub name: String,
    /// Si corre en la máquina: nada de la conversación sale de la caja. Sale de
    /// la dirección efectiva del backend (el host es de loopback), no de nada
    /// que alguien haya declarado. Es el dato para el indicador de «esto sale de
    /// la caja»: `false` quiere decir que cada mensaje viaja a [`Self::host`].
    pub local: bool,
    /// El host al que se le habla (`127.0.0.1`, `api.openai.com`). Vacío si la
    /// dirección configurada no se pudo leer.
    pub host: String,
}

/// Cómo está el servicio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServiceStatus {
    /// Si hay un modelo listo para responder.
    pub backend_available: bool,
    /// Por qué no, en palabras para mostrarle a la persona. Vacío si está
    /// disponible.
    pub reason: String,
    /// El modelo activo.
    pub model: ModelInfo,
    /// Cuántas conversaciones hay abiertas.
    pub conversations: u32,
}

/// Una capacidad del asistente sobre un dominio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Capability {
    /// Uno de los `DOMAIN_*`.
    pub domain: String,
    /// Si el asistente la tiene.
    pub available: bool,
    /// Por qué no, en palabras para mostrarle a la persona. Vacío si está
    /// disponible.
    pub reason: String,
}

// ── Los errores ───────────────────────────────────────────────────────────────

/// Los errores que devuelve el servicio, con nombre propio en el bus
/// (`ar.net.vasak.os.Assistant.Error.Unavailable`, …) para que quien llama
/// distinga «no hay modelo» de «mandaste algo inválido» sin leer el texto.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "ar.net.vasak.os.Assistant.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// No hay un modelo que pueda responder: el backend no corre, no tiene el
    /// modelo, pide una clave que no está en el llavero, o la configuración
    /// eligió un remoto sin permitirlo. El texto lo explica para la persona.
    Unavailable(String),
    /// El modelo respondió algo que no se pudo usar, o tardó demasiado.
    Backend(String),
    /// Un argumento inválido: id mal formado, mensaje vacío o demasiado largo.
    InvalidArgs(String),
    /// No hay ninguna conversación con ese id.
    NotFound(String),
    /// Se llegó a [`MAX_CONVERSATIONS`].
    LimitExceeded(String),
    /// La conversación ya está esperando una respuesta.
    Busy(String),
}

// ── El proxy, para quien esté en zbus 5 ───────────────────────────────────────

/// El lado cliente del servicio.
///
/// Lo usa quien esté en la misma versión de zbus que el servicio. Quien esté
/// clavado en zbus 4 (el escritorio) replica estas firmas a mano.
#[zbus::proxy(
    interface = "ar.net.vasak.os.Assistant",
    default_service = "ar.net.vasak.os.Assistant",
    default_path = "/ar/net/vasak/os/Assistant",
    gen_blocking = false
)]
pub trait Assistant {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    fn status(&self) -> Result<ServiceStatus, Error>;

    /// Las capacidades sobre cada dominio de [`DOMAINS`]. En esta fase las de
    /// datos del usuario vienen como no disponibles; `remote_backends` dice si
    /// la configuración permite un proveedor remoto.
    fn capabilities(&self) -> Result<Vec<Capability>, Error>;

    /// Los modelos instalados en el backend. `Unavailable` si no responde.
    fn models(&self) -> Result<Vec<String>, Error>;

    /// Abre una conversación nueva y devuelve su id.
    fn new_conversation(&self) -> Result<String, Error>;

    /// Las conversaciones abiertas.
    fn conversations(&self) -> Result<Vec<String>, Error>;

    /// Los mensajes de una conversación, del más viejo al más nuevo.
    fn history(&self, conversation_id: &str) -> Result<Vec<Message>, Error>;

    /// Le manda un mensaje a la conversación y devuelve la respuesta del
    /// modelo. Si falla, la conversación queda como estaba.
    fn chat(&self, conversation_id: &str, content: &str) -> Result<String, Error>;

    /// Cierra una conversación y olvida sus mensajes.
    fn delete_conversation(&self, conversation_id: &str) -> Result<(), Error>;
}
