//! La clave del proveedor, desde `vasak-keyring`.
//!
//! `vasak-keyring` es el Secret Service de la sesión (`org.freedesktop.secrets`),
//! así que el asistente le pide la clave por la API estándar, igual que
//! cualquier programa de la sesión: abre una sesión, busca el ítem por sus
//! atributos y lee el secreto. Nada nuevo en el llavero, nada en disco ni en el
//! entorno.
//!
//! El ítem se busca por dos atributos:
//!
//! - `service` = `ar.net.vasak.os.Assistant`
//! - `url` = la dirección base del backend tal como la normaliza el servicio
//!   (terminada en `/`, p. ej. `https://api.openai.com/v1/`)
//!
//! La clave queda atada a la dirección: si alguien cambia la dirección en
//! `vasak.conf` hacia otro servidor, la clave de antes no viaja a ése. Hasta que
//! esté el panel (fase 3) se guarda con
//! `secret-tool store --label='Lince' service ar.net.vasak.os.Assistant url https://api.openai.com/v1/`.
//!
//! Lo que esto **no** da: el ítem no lleva un esquema protegido como los tokens
//! de `vasak-accounts`, así que otro proceso de la misma sesión puede leerlo,
//! como cualquier secreto del llavero. Restringirlo a este binario pide un
//! cambio en `vasak-keyring` (otro esquema protegido con su ejecutable
//! autorizado).
//!
//! La sesión es `plain`: el secreto viaja sin cifrar por el bus de sesión. No es
//! más débil que el modelo de acceso de arriba —quien puede espiar el bus de la
//! sesión también puede pedir el ítem—, y la sesión cifrada (Diffie-Hellman)
//! traería una pila de cripto para nada.
//!
//! La clave nunca se escribe en el registro: [`ApiKey`] no se puede imprimir.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use crate::backend::BoxFuture;

const SECRETS_NAME: &str = "org.freedesktop.secrets";
const SECRETS_PATH: &str = "/org/freedesktop/secrets";
const SECRETS_SERVICE: &str = "org.freedesktop.Secret.Service";
const SECRETS_SESSION: &str = "org.freedesktop.Secret.Session";

/// El valor del atributo `service` de los ítems del asistente.
pub const ITEM_SERVICE: &str = "ar.net.vasak.os.Assistant";

/// El tope de una clave. Ninguna de verdad se le acerca.
const MAX_KEY_BYTES: usize = 4096;

/// Una clave de proveedor. Sin `Display` y con un `Debug` que no la muestra,
/// para que no termine en el registro por accidente.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Valida lo que vino del llavero: texto, sin espacios alrededor, sin
    /// caracteres de control (iría en una cabecera HTTP) y de un largo sensato.
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

/// De dónde sale la clave. Un rasgo para que las pruebas pongan una de mentira.
pub trait SecretSource: Send + Sync {
    /// La clave guardada para esa dirección base, o `None` si no hay, el
    /// llavero está bloqueado o no responde.
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>>;
}

/// El llavero de la sesión, por la API del Secret Service.
pub struct SecretService {
    pub connection: Connection,
}

impl SecretSource for SecretService {
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>> {
        Box::pin(async move {
            match self.lookup(url).await {
                Ok(key) => key,
                Err(error) => {
                    // El error de zbus no lleva el secreto: es del bus.
                    tracing::warn!("no se pudo leer la clave del llavero: {error}");
                    None
                }
            }
        })
    }
}

type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

impl SecretService {
    async fn lookup(&self, url: &str) -> zbus::Result<Option<ApiKey>> {
        let attributes = HashMap::from([("service", ITEM_SERVICE), ("url", url)]);
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "SearchItems",
                &attributes,
            )
            .await?
            .body()
            .deserialize()?;
        let Some(item) = unlocked.into_iter().next() else {
            if !locked.is_empty() {
                // Desbloquearlo pediría un diálogo; eso es de quien usa el
                // sistema, no de un servicio en segundo plano.
                tracing::info!("la clave del proveedor está en un llavero bloqueado");
            }
            return Ok(None);
        };

        let (_, session): (OwnedValue, OwnedObjectPath) = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "OpenSession",
                &("plain", Value::from("")),
            )
            .await?
            .body()
            .deserialize()?;
        let secrets = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "GetSecrets",
                &(vec![&item], &session),
            )
            .await
            .and_then(|reply| {
                reply
                    .body()
                    .deserialize::<HashMap<OwnedObjectPath, Secret>>()
            });
        // La sesión se cierra pase lo que pase con la lectura.
        let _ = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                session.as_str(),
                Some(SECRETS_SESSION),
                "Close",
                &(),
            )
            .await;
        Ok(secrets?
            .remove(&item)
            .and_then(|(_, _, value, _)| ApiKey::parse(&value)))
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

    // ── La parte D-Bus, contra un Secret Service de mentira ───────────────────
    //
    // Una conexión punto a punto dentro de la prueba: nunca se habla con el
    // `org.freedesktop.secrets` real de la sesión.

    use std::sync::Arc;

    use parking_lot::Mutex;
    use zbus::interface;
    use zbus::zvariant::ObjectPath;

    use crate::test_support::p2p_pair;

    const ITEM: &str = "/org/freedesktop/secrets/collection/login/1";
    const SESSION: &str = "/org/freedesktop/secrets/session/s1";

    #[derive(Clone, Copy, PartialEq)]
    enum Item {
        Unlocked,
        Locked,
    }

    struct FakeSecrets {
        /// La dirección que tiene guardada y el secreto que devuelve.
        url: String,
        secret: Vec<u8>,
        item: Item,
        sessions_opened: Arc<Mutex<Vec<String>>>,
    }

    #[interface(name = "org.freedesktop.Secret.Service")]
    impl FakeSecrets {
        fn search_items(
            &self,
            attributes: HashMap<String, String>,
        ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
            let coincide = attributes.len() == 2
                && attributes.get("service").map(String::as_str) == Some(ITEM_SERVICE)
                && attributes.get("url") == Some(&self.url);
            let item = OwnedObjectPath::try_from(ITEM).unwrap();
            match (coincide, self.item) {
                (true, Item::Unlocked) => (vec![item], vec![]),
                (true, Item::Locked) => (vec![], vec![item]),
                (false, _) => (vec![], vec![]),
            }
        }

        fn open_session(
            &self,
            algorithm: String,
            _input: zbus::zvariant::Value<'_>,
        ) -> (OwnedValue, OwnedObjectPath) {
            self.sessions_opened.lock().push(algorithm);
            (
                OwnedValue::try_from(Value::from("")).unwrap(),
                OwnedObjectPath::try_from(SESSION).unwrap(),
            )
        }

        fn get_secrets(
            &self,
            items: Vec<ObjectPath<'_>>,
            session: ObjectPath<'_>,
        ) -> HashMap<OwnedObjectPath, Secret> {
            assert_eq!(session.as_str(), SESSION);
            items
                .into_iter()
                .map(|item| {
                    let secret: Secret = (
                        OwnedObjectPath::try_from(SESSION).unwrap(),
                        vec![],
                        self.secret.clone(),
                        "text/plain".into(),
                    );
                    (OwnedObjectPath::from(item), secret)
                })
                .collect()
        }
    }

    struct FakeSession(Arc<Mutex<u32>>);

    #[interface(name = "org.freedesktop.Secret.Session")]
    impl FakeSession {
        fn close(&self) {
            *self.0.lock() += 1;
        }
    }

    struct Setup {
        service: SecretService,
        closed: Arc<Mutex<u32>>,
        sessions_opened: Arc<Mutex<Vec<String>>>,
        _server: Connection,
    }

    async fn secret_service(item: Item, secret: &[u8]) -> Setup {
        let closed = Arc::new(Mutex::new(0));
        let sessions_opened = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeSecrets {
            url: "https://api.ejemplo.com/v1/".into(),
            secret: secret.to_vec(),
            item,
            sessions_opened: sessions_opened.clone(),
        };
        let session = FakeSession(closed.clone());
        let (server, client) = p2p_pair(|builder| {
            builder
                .serve_at(SECRETS_PATH, fake)?
                .serve_at(SESSION, session)
        })
        .await
        .unwrap();
        Setup {
            service: SecretService { connection: client },
            closed,
            sessions_opened,
            _server: server,
        }
    }

    const URL: &str = "https://api.ejemplo.com/v1/";

    #[tokio::test]
    async fn la_clave_se_busca_por_servicio_y_direccion_y_se_lee_con_sesion_plain() {
        let setup = secret_service(Item::Unlocked, b"sk-del-llavero\n").await;

        let key = setup.service.api_key(URL).await.unwrap();

        assert_eq!(key.expose(), "sk-del-llavero");
        assert_eq!(*setup.sessions_opened.lock(), ["plain"]);
    }

    #[tokio::test]
    async fn la_sesion_del_llavero_se_cierra_despues_de_leer() {
        let setup = secret_service(Item::Unlocked, b"sk-del-llavero").await;
        setup.service.api_key(URL).await.unwrap();
        assert_eq!(*setup.closed.lock(), 1);
    }

    #[tokio::test]
    async fn la_clave_de_otra_direccion_no_se_entrega() {
        let setup = secret_service(Item::Unlocked, b"sk-del-llavero").await;
        assert!(setup
            .service
            .api_key("https://otro.ejemplo.com/v1/")
            .await
            .is_none());
        assert!(setup.sessions_opened.lock().is_empty());
    }

    #[tokio::test]
    async fn un_llavero_bloqueado_no_entrega_clave_ni_abre_sesion() {
        let setup = secret_service(Item::Locked, b"sk-del-llavero").await;
        assert!(setup.service.api_key(URL).await.is_none());
        assert!(setup.sessions_opened.lock().is_empty());
    }

    #[tokio::test]
    async fn un_secreto_invalido_en_el_llavero_no_se_usa() {
        let setup = secret_service(Item::Unlocked, b"sk-\r\nX-Otra: 1").await;
        assert!(setup.service.api_key(URL).await.is_none());
    }

    #[tokio::test]
    async fn si_el_llavero_no_responde_no_hay_clave_y_no_hay_panico() {
        // Un servidor que sirve otra cosa: pedirle el llavero da «objeto
        // desconocido».
        let (_server, client) =
            p2p_pair(|builder| builder.serve_at(SESSION, FakeSession(Arc::default())))
                .await
                .unwrap();
        let service = SecretService { connection: client };
        assert!(service.api_key(URL).await.is_none());
    }
}
