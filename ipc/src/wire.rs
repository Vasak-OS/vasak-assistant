//! El protocolo del puente de red: un pedido por método del rasgo
//! [`crate::backend::ModelBackend`], y su respuesta.
//!
//! **Regla dura de seguridad:** ningún pedido **de modelo** lleva URL ni host. El
//! demonio no puede nombrar un destino para la conversación; el único que arma la
//! dirección del modelo es el broker, desde su propia configuración. Así, ante una
//! inyección de instrucciones, no hay a dónde desviar el pedido del modelo.
//!
//! La **única excepción sancionada** es [`BrokerRequest::WebFetch`], la habilidad
//! web: ahí el demonio sí nombra una URL, pero el broker **no la obedece a ciegas**
//! —la valida contra un allowlist de hosts de su propia configuración y una defensa
//! SSRF (resuelve el host, rechaza IPs internas y fija la conexión a la IP ya
//! verificada) antes de salir—. Es sólo lectura: GET, sin credenciales ni cuerpo.
//! El contenido web es público; un host fuera del allowlist no sale.

use serde::{Deserialize, Serialize};

use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, ChatMessage, ModelTurn, ToolDef};

/// Un pedido al broker: una variante por método del rasgo `ModelBackend`, más
/// [`BrokerRequest::WebFetch`] (la habilidad web). Ninguna de las de **modelo**
/// nombra un destino; `WebFetch` sí lleva una URL, la excepción explícita que el
/// broker valida con allowlist + SSRF (ver el encabezado del módulo). La clave del
/// proveedor viaja aparte, en [`BrokerEnvelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrokerRequest {
    /// Lo que el demonio sirve de su propia config; el broker igual sabe
    /// responderlo desde la suya.
    Info,
    Models,
    ActiveModel,
    SupportsTools,
    ActiveSupportsTools,
    Chat {
        messages: Vec<Message>,
    },
    ChatLocalOnly {
        messages: Vec<Message>,
    },
    ChatTools {
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    },
    ChatToolsLocalOnly {
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    },
    /// La habilidad web: leer una página. **No es un pedido de modelo**: no pasa
    /// por ningún `ModelBackend`. Lleva la URL (la excepción a la regla «sin
    /// destino»), que el broker valida contra su allowlist y la defensa SSRF antes
    /// de salir. Sólo lectura: GET, sin credenciales ni cuerpo.
    WebFetch {
        url: String,
    },
}

/// Lo que trae una lectura web: la dirección final (sin redirecciones, igual a la
/// pedida), el tipo de contenido declarado por el servidor y el cuerpo ya recortado
/// a [`crate::MAX_RESPONSE_BYTES`] (ver `read_capped` del broker). El cuerpo es
/// texto; un binario llega como su mejor interpretación UTF-8.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebContent {
    pub final_url: String,
    pub content_type: String,
    pub body: String,
}

/// Lo que el demonio escribe en el socket: el pedido y, sólo si el proveedor la
/// necesita (OpenAI), la clave que el demonio leyó del llavero. La clave va en
/// el sobre y no dentro de [`BrokerRequest`] para que el pedido no tenga ningún
/// campo que nombre un destino ni cargue un secreto; el broker la usa sólo si su
/// configuración resuelve a un proveedor que la pide.
///
/// `key_base_url` es la dirección base (normalizada) **para la que el demonio
/// resolvió la clave**: a qué proveedor corresponde. No es un destino —el broker
/// jamás encamina por ella, el destino lo sigue decidiendo sólo su propia
/// configuración—; el broker sólo la **compara** con su propia URL antes de usar
/// la clave. Si no coinciden (la config cambió y reinició uno solo de los dos
/// procesos), la clave del proveedor nuevo no viaja a la URL del viejo: el
/// broker trata el pedido como sin clave.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// La dirección base para la que el demonio resolvió `api_key`. El broker la
    /// compara con la suya; nunca encamina por ella.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_base_url: Option<String>,
    pub request: BrokerRequest,
}

// `Debug` a mano: la clave nunca se imprime. Sin esto, un `debug!(?envelope)`
// que alguien agregue más adelante filtraría la clave del proveedor al journal.
// `PartialEq` sigue derivado para que las pruebas puedan comparar sobres.
impl std::fmt::Debug for BrokerEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerEnvelope")
            .field("api_key", &self.api_key.as_ref().map(|_| "<oculta>"))
            .field("key_base_url", &self.key_base_url)
            .field("request", &self.request)
            .finish()
    }
}

/// La respuesta del broker, agrupada por tipo de retorno. El demonio sabe qué
/// pidió y espera la variante que le toca; cualquier otra la trata como un
/// error del puente ([`BackendError::Unavailable`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrokerResponse {
    /// `info` (infalible: sale de la `Selection` de la config).
    Info(ModelInfo),
    Models(Result<Vec<String>, BackendError>),
    ActiveModel(Result<ModelInfo, BackendError>),
    SupportsTools(bool),
    /// `active_supports_tools`.
    Bool(Result<bool, BackendError>),
    /// `chat` y `chat_local_only`.
    Text(Result<String, BackendError>),
    /// `chat_tools` y `chat_tools_local_only`.
    Turn(Result<ModelTurn, BackendError>),
    /// `web_fetch`: el contenido de la página, o por qué no se pudo (host fuera del
    /// allowlist, IP interna, esquema inválido, el servidor falló).
    Web(Result<WebContent, BackendError>),
}
