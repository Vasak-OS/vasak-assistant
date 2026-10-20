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
//! `vasak.conf` hacia otro servidor, la clave de antes no viaja a ése. La
//! guarda el propio servicio ([`SecretSource::store_key`], el método
//! `SetProviderKey`), que es lo que usa el panel de Configuración, o a mano
//! `secret-tool store --label='Lince' service ar.net.vasak.os.Assistant url https://api.openai.com/v1/`.
//! Que la escriba el servicio y no el panel deja un solo ejecutable creando el
//! ítem, para cuando `vasak-keyring` lo restrinja a ése (vasak-keyring#55).
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

use vasak_assistant_protocol::Error;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use crate::backend::BoxFuture;

const SECRETS_NAME: &str = "org.freedesktop.secrets";
const SECRETS_PATH: &str = "/org/freedesktop/secrets";
const SECRETS_SERVICE: &str = "org.freedesktop.Secret.Service";
const SECRETS_SESSION: &str = "org.freedesktop.Secret.Session";
const SECRETS_COLLECTION: &str = "org.freedesktop.Secret.Collection";
/// La colección por omisión (`login`, en una sesión normal).
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";

/// El valor del atributo `service` de los ítems del asistente.
pub const ITEM_SERVICE: &str = "ar.net.vasak.os.Assistant";
/// La etiqueta con la que se ve el ítem en el llavero.
pub const ITEM_LABEL: &str = "Lince";

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

/// De dónde sale la clave y dónde se guarda. Un rasgo para que las pruebas
/// pongan uno de mentira.
///
/// `url` es siempre la dirección ya normalizada
/// ([`crate::config::normalize_url`]).
pub trait SecretSource: Send + Sync {
    /// La clave guardada para esa dirección base, o `None` si no hay, el
    /// llavero está bloqueado o no responde.
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>>;

    /// Guarda la clave para esa dirección, reemplazando la que hubiera.
    /// `Unavailable` si el llavero está bloqueado (no se abre ningún diálogo)
    /// o no responde. El error nunca lleva la clave.
    fn store_key<'a>(&'a self, url: &'a str, key: &'a ApiKey) -> BoxFuture<'a, Result<(), Error>>;

    /// Si hay una clave para esa dirección con el mismo criterio que
    /// [`Self::api_key`]: un solo ítem desbloqueado, o ninguno desbloqueado y
    /// uno solo bloqueado (está, pero hay que desbloquear el llavero). Con más
    /// de uno la lectura no usa ninguno, así que acá da `false`.
    fn has_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<bool, Error>>;
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

    fn store_key<'a>(&'a self, url: &'a str, key: &'a ApiKey) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.store(url, key).await.map_err(|error| {
                // El error es del bus (la clave viaja en el cuerpo del pedido,
                // nunca en un error); va al registro y a quien llamó.
                tracing::warn!("no se pudo guardar la clave en el llavero: {error}");
                Error::Unavailable(format!(
                    "No se pudo guardar la clave en el llavero: {error}"
                ))
            })?
        })
    }

    fn has_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let (unlocked, locked) = self.search(url).await.map_err(|error| {
                Error::Unavailable(format!("No se pudo consultar el llavero: {error}"))
            })?;
            Ok(has_usable_item(&unlocked, &locked))
        })
    }
}

type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

impl SecretService {
    /// Los ítems del asistente para esa dirección: (desbloqueados, bloqueados).
    async fn search(
        &self,
        url: &str,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        let attributes = HashMap::from([("service", ITEM_SERVICE), ("url", url)]);
        self.connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "SearchItems",
                &attributes,
            )
            .await?
            .body()
            .deserialize()
    }

    async fn open_session(&self) -> zbus::Result<OwnedObjectPath> {
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
        Ok(session)
    }

    async fn close_session(&self, session: &OwnedObjectPath) {
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
    }

    async fn lookup(&self, url: &str) -> zbus::Result<Option<ApiKey>> {
        let (unlocked, locked) = self.search(url).await?;
        // Más de un ítem con los mismos atributos: cualquiera de la sesión puede
        // crear uno, así que otro proceso pudo haber plantado su clave al lado de
        // la verdadera. No se adivina cuál es cuál: no se usa ninguna.
        // El mismo criterio que `has_usable_item`, que usa `has_key`.
        if unlocked.len() > 1 {
            tracing::warn!(
                "hay {} claves en el llavero para la misma dirección; no se usa ninguna hasta que quede una sola",
                unlocked.len()
            );
            return Ok(None);
        }
        let Some(item) = unlocked.into_iter().next() else {
            if locked.len() > 1 {
                // Desbloquear no alcanzaría: con más de una tampoco se usaría.
                tracing::warn!(
                    "hay {} claves en el llavero para la misma dirección; no se usa ninguna hasta que quede una sola",
                    locked.len()
                );
            } else if !locked.is_empty() {
                // Desbloquearlo pediría un diálogo; eso es de quien usa el
                // sistema, no de un servicio en segundo plano.
                tracing::info!("la clave del proveedor está en un llavero bloqueado");
            }
            return Ok(None);
        };

        let session = self.open_session().await?;
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
        self.close_session(&session).await;
        Ok(secrets?
            .remove(&item)
            .and_then(|(_, _, value, _)| ApiKey::parse(&value)))
    }

    /// `CreateItem` en la colección por omisión con `replace`. El `Err` de
    /// afuera es del bus; el de adentro, el motivo ya armado para la persona.
    async fn store(&self, url: &str, key: &ApiKey) -> zbus::Result<Result<(), Error>> {
        // Se pregunta antes de escribir: con la colección bloqueada,
        // `CreateItem` devolvería un diálogo de desbloqueo, y abrirlo es de
        // quien usa el sistema, no de un servicio en segundo plano.
        let locked: OwnedValue = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                DEFAULT_COLLECTION,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(SECRETS_COLLECTION, "Locked"),
            )
            .await?
            .body()
            .deserialize()?;
        if bool::try_from(locked).unwrap_or(true) {
            return Ok(Err(locked_error()));
        }

        let attributes = HashMap::from([("service", ITEM_SERVICE), ("url", url)]);
        let properties = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(ITEM_LABEL)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes),
            ),
        ]);
        let session = self.open_session().await?;
        let secret = (
            &session,
            Vec::<u8>::new(),
            key.expose().as_bytes(),
            "text/plain",
        );
        let created = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                DEFAULT_COLLECTION,
                Some(SECRETS_COLLECTION),
                "CreateItem",
                &(properties, secret, true),
            )
            .await
            .and_then(|reply| {
                reply
                    .body()
                    .deserialize::<(OwnedObjectPath, OwnedObjectPath)>()
            });
        self.close_session(&session).await;
        // vasak-keyring contesta `IsLocked` en vez de un prompt cuando la
        // colección se bloqueó entre la consulta y la escritura.
        let (_, prompt) = match created {
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.Secret.Error.IsLocked" =>
            {
                return Ok(Err(locked_error()));
            }
            created => created?,
        };
        // Un prompt quiere decir que hace falta desbloquear (la colección se
        // bloqueó entre la consulta y la escritura). No se llama a `Prompt`.
        // shortcut: el objeto prompt queda sin `Dismiss`; el llavero lo descarta
        // con la sesión del cliente. Si alguno lo mostrara solo, sumar `Dismiss`.
        if prompt.as_str() != "/" {
            return Ok(Err(locked_error()));
        }
        Ok(Ok(()))
    }
}

/// El criterio de qué ítems sirven, el mismo para leer la clave y para decir
/// si hay una: más de uno desbloqueado no sirve (otro proceso pudo plantar el
/// suyo al lado del verdadero y no se adivina cuál es cuál). Sin ninguno
/// desbloqueado, tiene que haber exactamente uno bloqueado: con más, al
/// desbloquear la lectura tampoco usaría ninguno.
fn has_usable_item(unlocked: &[OwnedObjectPath], locked: &[OwnedObjectPath]) -> bool {
    match unlocked.len() {
        0 => locked.len() == 1,
        1 => true,
        _ => false,
    }
}

fn locked_error() -> Error {
    Error::Unavailable(
        "El llavero está bloqueado: desbloquealo (al iniciar sesión, o desde el llavero) y volvé a guardar la clave."
            .into(),
    )
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
    const ITEM_2: &str = "/org/freedesktop/secrets/collection/login/2";
    const SESSION: &str = "/org/freedesktop/secrets/session/s1";

    #[derive(Clone, Copy, PartialEq)]
    enum Item {
        Unlocked,
        Locked,
        /// Dos ítems con los mismos atributos: uno plantado al lado del verdadero.
        Duplicated,
        /// Dos ítems bloqueados con los mismos atributos.
        LockedTwice,
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
                (true, Item::Duplicated) => (
                    vec![item, OwnedObjectPath::try_from(ITEM_2).unwrap()],
                    vec![],
                ),
                (true, Item::LockedTwice) => (
                    vec![],
                    vec![item, OwnedObjectPath::try_from(ITEM_2).unwrap()],
                ),
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

    /// Cómo está la colección por omisión para `CreateItem`.
    #[derive(Clone, Copy, PartialEq)]
    enum Collection {
        Unlocked,
        /// `Locked` da `true`: no se debería llegar a `CreateItem`.
        Locked,
        /// `Locked` da `false` pero `CreateItem` contesta con un prompt (se
        /// bloqueó entre la consulta y la escritura).
        Prompts,
        /// `CreateItem` falla con `org.freedesktop.Secret.Error.IsLocked`.
        IsLocked,
        /// `CreateItem` falla con otro error de método.
        Fails,
    }

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.Secret.Error")]
    enum FakeError {
        #[zbus(error)]
        ZBus(zbus::Error),
        IsLocked(String),
        #[zbus(name = "org.freedesktop.DBus.Error.Failed")]
        Failed(String),
    }

    /// Un `CreateItem` tal como llegó.
    #[derive(Debug, Clone)]
    struct Created {
        label: String,
        attributes: HashMap<String, String>,
        session: String,
        value: Vec<u8>,
        content_type: String,
        replace: bool,
    }

    /// La colección por omisión: anota cada `CreateItem`.
    struct FakeCollection {
        state: Collection,
        created: Arc<Mutex<Vec<Created>>>,
    }

    #[interface(name = "org.freedesktop.Secret.Collection")]
    impl FakeCollection {
        #[zbus(property)]
        fn locked(&self) -> bool {
            self.state == Collection::Locked
        }

        fn create_item(
            &self,
            properties: HashMap<String, OwnedValue>,
            secret: Secret,
            replace: bool,
        ) -> Result<(OwnedObjectPath, OwnedObjectPath), FakeError> {
            let label = properties
                .get("org.freedesktop.Secret.Item.Label")
                .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
                .unwrap_or_default();
            let attributes = properties
                .get("org.freedesktop.Secret.Item.Attributes")
                .and_then(|value| HashMap::<String, String>::try_from(value.try_clone().ok()?).ok())
                .unwrap_or_default();
            let (session, _, value, content_type) = secret;
            self.created.lock().push(Created {
                label,
                attributes,
                session: session.to_string(),
                value,
                content_type,
                replace,
            });
            let prompt = match self.state {
                Collection::Prompts => "/org/freedesktop/secrets/prompt/p1",
                Collection::IsLocked => return Err(FakeError::IsLocked("bloqueada".into())),
                Collection::Fails => return Err(FakeError::Failed("falló".into())),
                _ => "/",
            };
            Ok((
                OwnedObjectPath::try_from(ITEM).unwrap(),
                OwnedObjectPath::try_from(prompt).unwrap(),
            ))
        }
    }

    struct Setup {
        service: SecretService,
        closed: Arc<Mutex<u32>>,
        sessions_opened: Arc<Mutex<Vec<String>>>,
        created: Arc<Mutex<Vec<Created>>>,
        _server: Connection,
    }

    async fn secret_service(item: Item, secret: &[u8]) -> Setup {
        secret_service_with(item, secret, Collection::Unlocked).await
    }

    /// El llavero de mentira con la colección por omisión en `collection`.
    async fn secret_service_with(item: Item, secret: &[u8], collection: Collection) -> Setup {
        let closed = Arc::new(Mutex::new(0));
        let sessions_opened = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeSecrets {
            url: "https://api.ejemplo.com/v1/".into(),
            secret: secret.to_vec(),
            item,
            sessions_opened: sessions_opened.clone(),
        };
        let session = FakeSession(closed.clone());
        let collection = FakeCollection {
            state: collection,
            created: created.clone(),
        };
        let (server, client) = p2p_pair(|builder| {
            builder
                .serve_at(SECRETS_PATH, fake)?
                .serve_at(SESSION, session)?
                .serve_at(DEFAULT_COLLECTION, collection)
        })
        .await
        .unwrap();
        Setup {
            service: SecretService { connection: client },
            closed,
            sessions_opened,
            created,
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

    #[tokio::test]
    async fn con_mas_de_una_clave_para_la_misma_direccion_no_se_usa_ninguna() {
        let setup = secret_service(Item::Duplicated, b"sk-del-llavero").await;
        assert!(setup.service.api_key(URL).await.is_none());
        // Ni siquiera se abre sesión para leer una de las dos.
        assert!(setup.sessions_opened.lock().is_empty());
    }

    const CLAVE: &str = "sk-clave-que-no-debe-filtrarse";

    fn key() -> ApiKey {
        ApiKey::parse(CLAVE.as_bytes()).unwrap()
    }

    #[tokio::test]
    async fn guardar_crea_el_item_con_etiqueta_atributos_y_reemplazo() {
        let setup = secret_service(Item::Unlocked, b"").await;

        setup.service.store(URL, &key()).await.unwrap().unwrap();

        let created = setup.created.lock();
        assert_eq!(created.len(), 1);
        let item = &created[0];
        assert_eq!(item.label, ITEM_LABEL);
        assert_eq!(
            item.attributes,
            HashMap::from([
                ("service".to_owned(), ITEM_SERVICE.to_owned()),
                ("url".to_owned(), URL.to_owned()),
            ])
        );
        assert_eq!(item.value, CLAVE.as_bytes());
        assert_eq!(item.content_type, "text/plain");
        assert_eq!(item.session, SESSION);
        assert!(item.replace);
        assert_eq!(*setup.sessions_opened.lock(), ["plain"]);
    }

    #[tokio::test]
    async fn guardar_cierra_la_sesion_una_sola_vez() {
        let setup = secret_service(Item::Unlocked, b"").await;
        setup.service.store_key(URL, &key()).await.unwrap();
        assert_eq!(*setup.closed.lock(), 1);
    }

    #[tokio::test]
    async fn lo_que_se_guarda_se_encuentra_con_los_mismos_atributos_que_usa_la_lectura() {
        let setup = secret_service(Item::Unlocked, CLAVE.as_bytes()).await;
        setup.service.store_key(URL, &key()).await.unwrap();
        let attributes = setup.created.lock()[0].attributes.clone();
        // La lectura busca exactamente `service` y `url`, y esa búsqueda acierta.
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes["url"], URL);
        assert_eq!(setup.service.api_key(URL).await.unwrap().expose(), CLAVE);
    }

    #[tokio::test]
    async fn con_la_coleccion_bloqueada_no_se_crea_nada_y_el_error_dice_que_hacer() {
        let setup = secret_service_with(Item::Unlocked, b"", Collection::Locked).await;

        let Err(Error::Unavailable(text)) = setup.service.store_key(URL, &key()).await else {
            panic!("tenía que ser Unavailable");
        };

        assert!(text.contains("bloqueado"), "{text}");
        assert!(!text.contains(CLAVE));
        assert!(setup.created.lock().is_empty());
        assert!(setup.sessions_opened.lock().is_empty());
    }

    #[tokio::test]
    async fn si_el_llavero_responde_con_un_prompt_es_unavailable_y_la_sesion_se_cierra() {
        let setup = secret_service_with(Item::Unlocked, b"", Collection::Prompts).await;

        let Err(Error::Unavailable(text)) = setup.service.store_key(URL, &key()).await else {
            panic!("tenía que ser Unavailable");
        };

        assert!(text.contains("bloqueado"), "{text}");
        assert!(!text.contains(CLAVE));
        assert_eq!(*setup.closed.lock(), 1);
    }

    #[tokio::test]
    async fn sin_llavero_guardar_es_unavailable_y_el_texto_no_lleva_la_clave() {
        let (_server, client) =
            p2p_pair(|builder| builder.serve_at(SESSION, FakeSession(Arc::default())))
                .await
                .unwrap();
        let service = SecretService { connection: client };

        let Err(Error::Unavailable(text)) = service.store_key(URL, &key()).await else {
            panic!("tenía que ser Unavailable");
        };

        assert!(text.contains("llavero"), "{text}");
        assert!(!text.contains(CLAVE), "{text}");
    }

    #[tokio::test]
    async fn has_key_dice_si_hay_un_item_para_la_direccion_aunque_este_bloqueado() {
        for item in [Item::Unlocked, Item::Locked] {
            let setup = secret_service(item, b"x").await;
            assert!(setup.service.has_key(URL).await.unwrap());
            assert!(!setup
                .service
                .has_key("https://otro.ejemplo.com/v1/")
                .await
                .unwrap());
            // Preguntar si hay clave no la lee: no abre sesión.
            assert!(setup.sessions_opened.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn sin_llavero_has_key_es_unavailable() {
        let (_server, client) =
            p2p_pair(|builder| builder.serve_at(SESSION, FakeSession(Arc::default())))
                .await
                .unwrap();
        let service = SecretService { connection: client };
        assert!(matches!(
            service.has_key(URL).await,
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn si_create_item_falla_con_is_locked_el_motivo_es_el_de_llavero_bloqueado() {
        let setup = secret_service_with(Item::Unlocked, b"", Collection::IsLocked).await;

        let Err(Error::Unavailable(text)) = setup.service.store_key(URL, &key()).await else {
            panic!("tenía que ser Unavailable");
        };

        assert_eq!(text, locked_error_text());
        assert!(!text.contains(CLAVE));
        assert_eq!(*setup.closed.lock(), 1);
    }

    #[tokio::test]
    async fn otro_error_de_create_item_da_el_unavailable_generico_sin_la_clave() {
        let setup = secret_service_with(Item::Unlocked, b"", Collection::Fails).await;

        let Err(Error::Unavailable(text)) = setup.service.store_key(URL, &key()).await else {
            panic!("tenía que ser Unavailable");
        };

        assert!(
            text.starts_with("No se pudo guardar la clave en el llavero"),
            "{text}"
        );
        assert_ne!(text, locked_error_text());
        assert!(!text.contains(CLAVE));
        assert_eq!(*setup.closed.lock(), 1);
    }

    fn locked_error_text() -> String {
        let Error::Unavailable(text) = locked_error() else {
            unreachable!()
        };
        text
    }

    #[tokio::test]
    async fn con_mas_de_un_item_desbloqueado_has_key_da_false() {
        let setup = secret_service(Item::Duplicated, b"x").await;
        assert!(!setup.service.has_key(URL).await.unwrap());
    }

    #[tokio::test]
    async fn has_provider_key_de_punta_a_punta_da_false_con_la_clave_duplicada() {
        use crate::test_support::FakeBackend;
        for (item, esperado) in [
            (Item::Duplicated, false),
            (Item::Unlocked, true),
            (Item::Locked, true),
        ] {
            let setup = secret_service(item, b"x").await;
            let service = crate::service::AssistantService {
                backend: Arc::new(FakeBackend::replying("")),
                conversations: Arc::default(),
                remote_allowed: false,
                secrets: Arc::new(setup.service),
            };
            assert_eq!(
                service
                    .has_provider_key_for("https://api.ejemplo.com/v1")
                    .await
                    .unwrap(),
                esperado
            );
        }
    }

    #[test]
    fn has_usable_item_es_true_con_un_desbloqueado_o_solo_bloqueados() {
        let paths = |n: usize| vec![OwnedObjectPath::try_from(ITEM).unwrap(); n];
        for (unlocked, locked, esperado) in [
            (0, 0, false),
            (0, 1, true),
            (1, 0, true),
            (1, 1, true),
            (0, 2, false),
            (2, 0, false),
            (2, 1, false),
        ] {
            assert_eq!(
                has_usable_item(&paths(unlocked), &paths(locked)),
                esperado,
                "{unlocked} desbloqueados, {locked} bloqueados"
            );
        }
    }

    /// Un destino de `tracing` que junta lo que se escribe.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    #[tokio::test]
    async fn con_dos_claves_bloqueadas_no_hay_clave_y_el_aviso_dice_que_hay_varias() {
        let log = Captured::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(log.clone())
                .with_ansi(false)
                .finish(),
        );
        let setup = secret_service(Item::LockedTwice, b"x").await;

        assert!(setup.service.api_key(URL).await.is_none());
        assert!(!setup.service.has_key(URL).await.unwrap());
        assert!(setup.sessions_opened.lock().is_empty());

        let text = String::from_utf8(log.0.lock().clone()).unwrap();
        assert!(text.contains("hay 2 claves"), "{text}");
        assert!(!text.contains("llavero bloqueado"), "{text}");
    }

    #[tokio::test]
    async fn con_una_sola_clave_bloqueada_el_aviso_es_el_de_llavero_bloqueado() {
        let log = Captured::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(log.clone())
                .with_ansi(false)
                .finish(),
        );
        let setup = secret_service(Item::Locked, b"x").await;

        assert!(setup.service.api_key(URL).await.is_none());

        let text = String::from_utf8(log.0.lock().clone()).unwrap();
        assert!(text.contains("llavero bloqueado"), "{text}");
        assert!(!text.contains("hay 2 claves"), "{text}");
    }
}
