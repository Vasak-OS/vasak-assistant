//! La clave del proveedor, del lado del broker.
//!
//! En esta PR el broker **no** lee el llavero: el demonio lee la clave (es el
//! dueño natural: `SetProviderKey`, su `SecretSource` sobre el Secret Service) y
//! se la pasa al broker en el sobre del IPC ([`vasak_assistant_ipc::wire`]). Acá
//! vive sólo lo que el backend de OpenAI necesita: el tipo [`ApiKey`] y un rasgo
//! angosto [`SecretSource`] con el único método que usa. (PR2 mueve la lectura
//! del llavero a este proceso.)

use vasak_assistant_ipc::BoxFuture;

/// El servicio del ítem de la clave en el llavero. Es también el texto que se le
/// muestra a la persona cuando falta la clave.
pub const ITEM_SERVICE: &str = "ar.net.vasak.os.Assistant";

/// Lo más largo que puede medir una clave: entra en una cabecera HTTP.
const MAX_KEY_BYTES: usize = 4096;

/// Una clave de proveedor. Sin `Display` y con un `Debug` que no la muestra,
/// para que no termine en el registro por accidente.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Valida lo que llegó: texto, sin espacios alrededor, sin caracteres de
    /// control (iría en una cabecera HTTP) y de un largo sensato.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?.trim();
        let valid =
            !text.is_empty() && text.len() <= MAX_KEY_BYTES && !text.chars().any(char::is_control);
        valid.then(|| Self(text.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(…)")
    }
}

/// De dónde sale la clave para un pedido. Un rasgo angosto para que el backend
/// de OpenAI no sepa de dónde viene (hoy del IPC, mañana del llavero) y para que
/// las pruebas pongan uno de mentira.
///
/// `url` es siempre la dirección ya normalizada
/// ([`vasak_assistant_ipc::config::normalize_url`]).
pub trait SecretSource: Send + Sync {
    /// La clave para esa dirección base, o `None` si no hay.
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>>;
}

/// La clave que llegó en el sobre del IPC, para un pedido. No mira la dirección:
/// el demonio ya leyó la clave que corresponde a la configuración común.
pub struct RequestKey(Option<ApiKey>);

impl RequestKey {
    /// Toma la clave en claro del sobre y la valida.
    pub fn new(key: Option<String>) -> Self {
        Self(key.as_deref().and_then(|key| ApiKey::parse(key.as_bytes())))
    }
}

impl SecretSource for RequestKey {
    fn api_key<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Option<ApiKey>> {
        let key = self.0.clone();
        Box::pin(async move { key })
    }
}
