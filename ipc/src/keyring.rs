//! Las piezas del llavero que comparten el demonio y el broker: el tipo de la
//! clave del proveedor y el contrato angosto de dónde sale.
//!
//! El demonio es el dueño natural del llavero (lo lee por el Secret Service y lo
//! guarda desde Configuración); su `SecretService` y el rasgo ancho con
//! `store_key`/`memory_key`/… viven allá ([`crate`] no los necesita). Acá queda
//! lo que los dos lados usan con el mismo código: [`ApiKey`] (con su `Debug`
//! redactado y `parse`), el rasgo angosto [`SecretSource`] —el único método que
//! usa el backend de OpenAI del broker—, [`ITEM_SERVICE`] y [`RequestKey`] (la
//! clave que el demonio leyó y le pasó al broker en el sobre del IPC).

use crate::backend::BoxFuture;

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
    /// Valida lo que llegó (del llavero o del sobre del IPC): texto, sin
    /// espacios alrededor, sin caracteres de control (iría en una cabecera HTTP)
    /// y de un largo sensato.
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
/// de OpenAI del broker no sepa de dónde viene (hoy del IPC, mañana del llavero)
/// y para que las pruebas pongan uno de mentira.
///
/// `url` es siempre la dirección ya normalizada
/// ([`crate::config::normalize_url`]).
pub trait SecretSource: Send + Sync {
    /// La clave para esa dirección base, o `None` si no hay.
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>>;
}

/// La clave que llegó en el sobre del IPC, para un pedido. No mira la dirección:
/// el demonio ya leyó la clave que corresponde a la configuración común, y el
/// broker ya comprobó que esa dirección coincide con la suya antes de usarla.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn una_clave_normal_se_acepta_y_se_recorta() {
        let key = ApiKey::parse(b"  sk-abc123\n").unwrap();
        assert_eq!(key.expose(), "sk-abc123");
    }

    #[test]
    fn una_clave_vacia_o_de_espacios_no_vale() {
        for bytes in [&b""[..], b"   ", b"\n"] {
            assert!(ApiKey::parse(bytes).is_none(), "{bytes:?}");
        }
    }

    #[test]
    fn una_clave_con_caracteres_de_control_no_vale_porque_iria_en_una_cabecera() {
        for bytes in [
            &b"sk-abc\r\nX-Otra: 1"[..],
            b"sk-\0abc",
            b"sk-a\tbc",
            b"sk-\x7fabc",
        ] {
            assert!(ApiKey::parse(bytes).is_none(), "{bytes:?}");
        }
    }

    #[test]
    fn una_clave_que_no_es_utf8_no_vale() {
        assert!(ApiKey::parse(&[0xff, 0xfe, 0x41]).is_none());
    }

    #[test]
    fn una_clave_acepta_justo_el_tope_y_rechaza_un_byte_mas() {
        assert!(ApiKey::parse("k".repeat(MAX_KEY_BYTES).as_bytes()).is_some());
        assert!(ApiKey::parse("k".repeat(MAX_KEY_BYTES + 1).as_bytes()).is_none());
    }

    #[test]
    fn el_debug_de_la_clave_no_la_muestra() {
        let key = ApiKey::parse(b"sk-secreta").unwrap();
        let texto = format!("{key:?}");
        assert!(!texto.contains("secreta"));
        let anidada = format!("{:#?}", Some(&key));
        assert!(!anidada.contains("secreta"));
    }
}
