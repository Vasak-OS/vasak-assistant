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
//!
//! # La memoria
//!
//! El asistente puede tener una memoria propia: una base de grafos cifrada en
//! la carpeta de estado del servicio, con la llave en el llavero de la sesión.
//! **Viene apagada**: existe sólo desde que la persona la enciende
//! ([`Assistant::set_memory_enabled`]). Apagarla no borra nada: deja de usarla
//! y encenderla de nuevo la reabre. Borrar es sólo [`Assistant::clear_memory`].
//! [`Assistant::memory_status`] dice si está encendida, si se puede usar y
//! cuánto ocupa.
//!
//! Qué guarda: lo que la persona dice de sí misma —preferencias, cómo quiere
//! que le hablen, hábitos, temas, correcciones, nombres que usa— y resúmenes
//! breves de conversaciones. Lo aprende solo después de responder, y lo usa
//! como notas en el contexto del modelo, nunca en el historial. **No guarda
//! datos sensibles que no son suyos** (correo, calendario, contactos,
//! archivos, cuentas, claves): esos se leen de su servicio en el momento. Con
//! un proveedor remoto, la memoria no se lee ni se escribe salvo que la
//! persona lo permita (`share_memory_with_remote` en `vasak.conf`); si no,
//! `MemoryStatus` y `Capabilities` dicen «La memoria no se comparte con un
//! proveedor remoto». No hay métodos para leer o editar recuerdos sueltos:
//! sólo encender, apagar y borrar.

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

/// Cuántas conversaciones abiertas puede haber a la vez, entre todos los
/// clientes.
pub const MAX_CONVERSATIONS: usize = 32;

/// Cuántas conversaciones abiertas puede tener una misma conexión.
pub const MAX_CONVERSATIONS_PER_OWNER: usize = 8;

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
    /// Cuántas conversaciones tiene abiertas quien pregunta.
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

/// Cómo está la memoria del asistente. Firma `(bbst)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MemoryStatus {
    /// Si la persona la encendió, se pueda abrir o no.
    pub enabled: bool,
    /// Si está encendida y abierta, lista para usarse.
    pub available: bool,
    /// Por qué no está disponible, en palabras para mostrarle a la persona
    /// («La memoria está apagada», «El llavero está bloqueado», …). Vacío si
    /// está disponible.
    pub reason: String,
    /// Cuánto ocupa en disco, en bytes, encendida o apagada. Se mide sin
    /// abrirla; 0 si no hay base.
    pub bytes: u64,
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
    /// Se llegó a [`MAX_CONVERSATIONS`] o a [`MAX_CONVERSATIONS_PER_OWNER`].
    LimitExceeded(String),
    /// La conversación ya está esperando una respuesta.
    Busy(String),
}

// ── El proxy, para quien esté en zbus 5 ───────────────────────────────────────

/// El lado cliente del servicio.
///
/// Lo usa quien esté en la misma versión de zbus que el servicio. Quien esté
/// clavado en zbus 4 (el escritorio) replica estas firmas a mano.
///
/// # Las conversaciones son de la conexión que las abrió
///
/// `NewConversation` deja como dueña a la conexión que llama (su nombre único
/// en el bus). Sólo ella la ve en `Conversations`, y sólo ella puede usar
/// `History`, `Chat` y `DeleteConversation` con ese id; para cualquier otra, el
/// id da `NotFound`, igual que uno que no existe. Cuando la conexión se cierra,
/// sus conversaciones se olvidan. `Status` cuenta sólo las de quien pregunta.
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
    /// la configuración permite un proveedor remoto, y `memory` está
    /// disponible si la memoria está encendida y abierta (si no, el motivo es
    /// el de [`Self::memory_status`], o «La memoria está apagada»).
    fn capabilities(&self) -> Result<Vec<Capability>, Error>;

    /// Los modelos instalados en el backend. `Unavailable` si no responde.
    fn models(&self) -> Result<Vec<String>, Error>;

    /// Abre una conversación nueva, de quien llama, y devuelve su id.
    fn new_conversation(&self) -> Result<String, Error>;

    /// Las conversaciones abiertas de quien llama.
    fn conversations(&self) -> Result<Vec<String>, Error>;

    /// Los mensajes de una conversación, del más viejo al más nuevo.
    fn history(&self, conversation_id: &str) -> Result<Vec<Message>, Error>;

    /// Le manda un mensaje a la conversación y devuelve la respuesta del
    /// modelo. Si falla, la conversación queda como estaba.
    fn chat(&self, conversation_id: &str, content: &str) -> Result<String, Error>;

    /// Cierra una conversación y olvida sus mensajes.
    fn delete_conversation(&self, conversation_id: &str) -> Result<(), Error>;

    /// Guarda en el llavero de la sesión la clave del proveedor de `url`,
    /// reemplazando la que hubiera. Es lo que usa el panel de Configuración:
    /// así sólo el servicio escribe el ítem del llavero.
    ///
    /// `url` se normaliza igual que la de `vasak.conf` (`http` o `https`, sin
    /// consulta ni fragmento, con `/` final), así que
    /// `https://api.openai.com/v1` y `https://api.openai.com/v1/` son la misma
    /// clave. La clave se recorta y tiene que tener de 1 a 4096 bytes sin
    /// caracteres de control.
    ///
    /// - `InvalidArgs`: la dirección no se puede usar (ilegible, otro esquema,
    ///   sin host, con usuario o contraseña, o un host remoto que no es
    ///   `https`) o la clave no es válida.
    /// - `Unavailable`: el llavero está bloqueado o no responde. No se abre
    ///   ningún diálogo de desbloqueo: el motivo dice qué hacer.
    ///
    /// La clave nunca vuelve en un error ni va al registro.
    fn set_provider_key(&self, url: &str, key: &str) -> Result<(), Error>;

    /// Si hay una clave guardada en el llavero para `url` (normalizada igual
    /// que en [`Self::set_provider_key`]), aunque el llavero esté bloqueado.
    /// Si hay más de una para la misma dirección da `false`: el servicio no usa
    /// ninguna hasta que quede una sola. Nunca devuelve la clave. `InvalidArgs` si la dirección no se puede usar,
    /// `Unavailable` si el llavero no responde.
    fn has_provider_key(&self, url: &str) -> Result<bool, Error>;

    /// Cómo está la memoria. Nunca falla (salvo el bus): cualquier problema
    /// llega como `available: false` con el motivo (también el de un
    /// proveedor remoto sin permiso). `bytes` se mide sin abrir la base.
    fn memory_status(&self) -> Result<MemoryStatus, Error>;

    /// Enciende o apaga la memoria.
    ///
    /// - Encender reabre la base que haya, con la llave que está en el
    ///   llavero; si no hay, genera una llave nueva, la guarda en el llavero y
    ///   crea la base vacía y cifrada. `Unavailable` si el llavero está
    ///   bloqueado o no responde (nunca se abre un diálogo), si la llave falta
    ///   o no abre la base existente, o si no se pudo crear. Si ya estaba
    ///   encendida y abierta, no hace nada.
    /// - Apagar **no borra nada**: deja de usarla y de escribirla; los archivos
    ///   y la llave quedan.
    fn set_memory_enabled(&self, enabled: bool) -> Result<(), Error>;

    /// Borra la memoria: primero la llave del llavero y después los archivos.
    /// Si estaba encendida, la recrea vacía con una llave nueva. `Unavailable`
    /// si el llavero está bloqueado (entonces no se toca nada) o no responde.
    /// Es la salida cuando la llave falta o no abre la base.
    fn clear_memory(&self) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{serialized::Context, to_bytes, LE};
    use zbus::DBusError;

    fn signature<T: Type>() -> String {
        T::SIGNATURE.to_string()
    }

    /// Ida y vuelta por la codificación de D-Bus, que es la del cable.
    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de> + Type,
    {
        let context = Context::new_dbus(LE, 0);
        let bytes = to_bytes(context, value).unwrap();
        bytes.deserialize().unwrap().0
    }

    #[test]
    fn la_firma_de_cada_tipo_es_la_del_contrato() {
        // Quien replica estas firmas a mano (el escritorio, en zbus 4) depende
        // de que no cambien sin avisar.
        assert_eq!(signature::<Role>(), "s");
        assert_eq!(signature::<Message>(), "(ss)");
        assert_eq!(signature::<ModelInfo>(), "(ssbs)");
        assert_eq!(signature::<ServiceStatus>(), "(bs(ssbs)u)");
        assert_eq!(signature::<Capability>(), "(sbs)");
    }

    #[test]
    fn los_roles_viajan_como_texto_en_minuscula() {
        for (role, name) in [
            (Role::System, "system"),
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
        ] {
            let bytes = to_bytes(Context::new_dbus(LE, 0), &role).unwrap();
            // Un string D-Bus: largo (u32), texto y NUL final.
            let mut expected = (name.len() as u32).to_le_bytes().to_vec();
            expected.extend_from_slice(name.as_bytes());
            expected.push(0);
            assert_eq!(&*bytes, expected.as_slice(), "{role:?}");
            assert_eq!(round_trip(&role), role);
        }
    }

    #[test]
    fn los_tipos_sobreviven_a_la_codificacion_de_dbus() {
        let message = Message {
            role: Role::Assistant,
            content: "Hola, ¿qué tal? 👋".into(),
        };
        assert_eq!(round_trip(&message), message);

        let status = ServiceStatus {
            backend_available: true,
            reason: String::new(),
            model: ModelInfo {
                provider: PROVIDER_OLLAMA.into(),
                name: "llama3.2:latest".into(),
                local: true,
                host: "127.0.0.1".into(),
            },
            conversations: 3,
        };
        assert_eq!(round_trip(&status), status);

        let capabilities: Vec<Capability> = DOMAINS
            .iter()
            .map(|domain| Capability {
                domain: (*domain).into(),
                available: false,
                reason: "todavía no".into(),
            })
            .collect();
        assert_eq!(round_trip(&capabilities), capabilities);
    }

    #[test]
    fn cada_error_tiene_su_nombre_propio_en_el_bus() {
        let prefix = "ar.net.vasak.os.Assistant.Error";
        let casos = [
            (Error::Unavailable(String::new()), "Unavailable"),
            (Error::Backend(String::new()), "Backend"),
            (Error::InvalidArgs(String::new()), "InvalidArgs"),
            (Error::NotFound(String::new()), "NotFound"),
            (Error::LimitExceeded(String::new()), "LimitExceeded"),
            (Error::Busy(String::new()), "Busy"),
        ];
        for (error, name) in casos {
            assert_eq!(error.name().as_str(), format!("{prefix}.{name}"));
        }
    }

    #[test]
    fn el_texto_del_error_viaja_como_descripcion() {
        assert_eq!(
            Error::Busy("esperando".into()).description(),
            Some("esperando")
        );
    }

    #[test]
    fn las_direcciones_del_servicio_son_coherentes_entre_si() {
        assert_eq!(SERVICE_INTERFACE, SERVICE_NAME);
        assert_eq!(SERVICE_PATH, format!("/{}", SERVICE_NAME.replace('.', "/")));
    }

    #[test]
    fn los_dominios_son_siete_sin_repetirse_y_terminan_en_remote_backends() {
        assert_eq!(DOMAINS.len(), 7);
        let mut sorted = DOMAINS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), DOMAINS.len());
        assert_eq!(DOMAINS.last(), Some(&DOMAIN_REMOTE_BACKENDS));
        assert!(DOMAINS.contains(&DOMAIN_MAIL));
    }

    #[test]
    fn los_limites_son_coherentes_para_que_un_mensaje_siempre_quepa_en_el_historial() {
        const {
            assert!(MAX_MESSAGE_BYTES * 2 <= MAX_HISTORY_BYTES);
            assert!(MAX_HISTORY_MESSAGES >= 2);
            assert!(MAX_CONVERSATIONS > 0);
        }
    }
}
