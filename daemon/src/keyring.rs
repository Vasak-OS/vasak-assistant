//! La clave del proveedor, desde `vasak-keyring`.
//!
//! `vasak-keyring` es el Secret Service de la sesión (`org.freedesktop.secrets`),
//! así que el asistente le pide la clave por la API estándar, igual que
//! cualquier programa de la sesión: abre una sesión, busca el ítem por sus
//! atributos y lee el secreto. Nada nuevo en el llavero, nada en disco ni en el
//! entorno.
//!
//! El ítem se busca y se crea con **el mismo mapa** de atributos:
//!
//! - `service` = `ar.net.vasak.os.Assistant`
//! - `url` = la dirección base del backend tal como la normaliza el servicio
//!   (terminada en `/`, p. ej. `https://api.openai.com/v1/`)
//! - `purpose` = `provider`
//! - `xdg:schema` = `ar.net.vasak.os.Assistant`
//!
//! La clave queda atada a la dirección: si alguien cambia la dirección en
//! `vasak.conf` hacia otro servidor, la clave de antes no viaja a ése. La
//! guarda el propio servicio ([`SecretSource::store_key`], el método
//! `SetProviderKey`), que es lo que usa el panel de Configuración.
//!
//! # El esquema protegido
//!
//! `vasak-keyring` restringe los ítems con `xdg:schema=ar.net.vasak.os.Assistant`
//! al ejecutable `/usr/bin/vasak-assistant` (vasak-keyring#55): ningún otro
//! proceso de la sesión los lee, los reemplaza ni los planta. Por eso el
//! esquema va también en la **búsqueda**: si se buscara sin él, un ítem común
//! con los mismos atributos plantado por otro proceso entraría en la
//! búsqueda, y con la llave de la memoria eso quiere decir cifrar con una
//! llave que conoce quien la plantó.
//!
//! Un ítem sin el esquema, aunque tenga los mismos `service` y `url`, no se
//! cuenta ni se usa: no hay migración desde antes del esquema (la 0.2.0 nunca
//! se empaquetó).
//!
//! La sesión es `plain`: el secreto viaja sin cifrar por el bus de sesión hasta
//! este proceso. Con el esquema protegido, el riesgo que queda es quien pueda
//! espiar el bus de la sesión; la sesión cifrada (Diffie-Hellman) traería una
//! pila de cripto para eso, y no se usa.
//!
//! La clave nunca se escribe en el registro: [`ApiKey`] no se puede imprimir.
//!
//! # La llave de la memoria
//!
//! La base de la memoria (`crate::memory`) se cifra con una llave cruda de 32
//! bytes que vive en el mismo llavero, en un ítem aparte:
//!
//! - `service` = `ar.net.vasak.os.Assistant`
//! - `purpose` = `memory`
//! - `xdg:schema` = `ar.net.vasak.os.Assistant`
//!
//! La genera sólo el servicio, la primera vez que se enciende la memoria, y la
//! borra sólo al borrar la memoria (`ClearMemory`); nunca sola al arrancar.
//! Tiene el mismo modelo de acceso que la clave del proveedor —el esquema
//! protegido: sólo `/usr/bin/vasak-assistant` la lee o la escribe
//! (vasak-keyring#55)— y el mismo criterio de duplicados: con más de una no se
//! usa ninguna.

use std::collections::HashMap;

use vasak_assistant_protocol::Error;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;
use zeroize::Zeroizing;

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
/// El valor del atributo `purpose` del ítem de la clave del proveedor.
pub const PROVIDER_PURPOSE: &str = "provider";
/// El valor del atributo `purpose` del ítem de la llave de la memoria.
pub const MEMORY_PURPOSE: &str = "memory";
/// La etiqueta del ítem de la llave de la memoria.
pub const MEMORY_LABEL: &str = "Lince: llave de la memoria";
/// El largo de la llave de la memoria: AES-256.
pub const MEMORY_KEY_BYTES: usize = 32;

/// Los atributos con los que se crea el ítem de la llave de la memoria.
/// Se usan igual para buscar, crear y borrar.
fn memory_attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("service", ITEM_SERVICE),
        ("purpose", MEMORY_PURPOSE),
        ("xdg:schema", ITEM_SERVICE),
    ])
}

/// La llave de la memoria tal como se encontró en el llavero.
pub enum MemoryKey {
    Found(Zeroizing<[u8; MEMORY_KEY_BYTES]>),
    /// No hay ningún ítem.
    Missing,
    /// Está, pero en un llavero bloqueado.
    Locked,
    /// Hay más de uno: no se usa ninguno.
    Duplicated,
    /// Hay uno, pero no son 32 bytes.
    Invalid,
}

impl std::fmt::Debug for MemoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Found(_) => "Found(…)",
            Self::Missing => "Missing",
            Self::Locked => "Locked",
            Self::Duplicated => "Duplicated",
            Self::Invalid => "Invalid",
        })
    }
}

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

    /// Si la colección por omisión está bloqueada. `Unavailable` si el
    /// llavero no responde.
    fn is_locked(&self) -> BoxFuture<'_, Result<bool, Error>>;

    /// La llave de la memoria. `Unavailable` sólo si el llavero no responde.
    fn memory_key(&self) -> BoxFuture<'_, Result<MemoryKey, Error>>;

    /// Guarda la llave de la memoria, reemplazando la que hubiera.
    /// `Unavailable` si el llavero está bloqueado o no responde.
    fn store_memory_key<'a>(
        &'a self,
        key: &'a [u8; MEMORY_KEY_BYTES],
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Borra todos los ítems de la llave de la memoria. `Unavailable` si
    /// quedó alguno (llavero bloqueado) o el llavero no responde.
    fn delete_memory_key(&self) -> BoxFuture<'_, Result<(), Error>>;
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
            let (unlocked, locked) = self
                .search(&provider_attributes(url))
                .await
                .map_err(unreachable)?;
            Ok(has_usable_item(&unlocked, &locked))
        })
    }

    fn is_locked(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move { self.collection_locked().await.map_err(unreachable) })
    }

    fn memory_key(&self) -> BoxFuture<'_, Result<MemoryKey, Error>> {
        Box::pin(async move { self.read_memory_key().await.map_err(unreachable) })
    }

    fn store_memory_key<'a>(
        &'a self,
        key: &'a [u8; MEMORY_KEY_BYTES],
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.store_secret(
                memory_attributes(),
                MEMORY_LABEL,
                key,
                "application/octet-stream",
            )
            .await
            .map_err(|error| {
                Error::Unavailable(format!(
                    "No se pudo guardar la llave de la memoria en el llavero: {error}"
                ))
            })?
        })
    }

    fn delete_memory_key(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.delete(&memory_attributes())
                .await
                .map_err(unreachable)?
        })
    }
}

type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// Los atributos del ítem de la clave del proveedor de esa dirección.
/// Se usan igual para buscar y para crear.
fn provider_attributes(url: &str) -> HashMap<&str, &str> {
    HashMap::from([
        ("service", ITEM_SERVICE),
        ("url", url),
        ("purpose", PROVIDER_PURPOSE),
        ("xdg:schema", ITEM_SERVICE),
    ])
}

fn unreachable(error: zbus::Error) -> Error {
    Error::Unavailable(format!("No se pudo consultar el llavero: {error}"))
}

impl SecretService {
    /// Los ítems con esos atributos: (desbloqueados, bloqueados).
    async fn search(
        &self,
        attributes: &HashMap<&str, &str>,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        self.connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "SearchItems",
                attributes,
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
        let (unlocked, locked) = self.search(&provider_attributes(url)).await?;
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

        let value = self.read_secret(&item).await?;
        Ok(value.and_then(|value| ApiKey::parse(&value)))
    }

    /// El secreto de un ítem, por una sesión `plain` que se cierra pase lo que
    /// pase con la lectura. `Zeroizing` porque puede ser la llave de la memoria.
    async fn read_secret(
        &self,
        item: &OwnedObjectPath,
    ) -> zbus::Result<Option<Zeroizing<Vec<u8>>>> {
        let session = self.open_session().await?;
        let secrets = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                SECRETS_PATH,
                Some(SECRETS_SERVICE),
                "GetSecrets",
                &(vec![item], &session),
            )
            .await
            .and_then(|reply| {
                reply
                    .body()
                    .deserialize::<HashMap<OwnedObjectPath, Secret>>()
            });
        self.close_session(&session).await;
        Ok(secrets?
            .remove(item)
            .map(|(_, _, value, _)| Zeroizing::new(value)))
    }

    /// La llave de la memoria, con el mismo criterio de duplicados que la
    /// clave del proveedor ([`has_usable_item`]).
    async fn read_memory_key(&self) -> zbus::Result<MemoryKey> {
        let (unlocked, locked) = self.search(&memory_attributes()).await?;
        let item = match (unlocked.as_slice(), locked.len()) {
            ([item], _) => item,
            ([], 0) => return Ok(MemoryKey::Missing),
            ([], 1) => return Ok(MemoryKey::Locked),
            _ => {
                tracing::warn!(
                    "hay más de una llave de la memoria en el llavero; no se usa ninguna"
                );
                return Ok(MemoryKey::Duplicated);
            }
        };
        let Some(value) = self.read_secret(item).await? else {
            return Ok(MemoryKey::Missing);
        };
        let Ok(bytes) = <[u8; MEMORY_KEY_BYTES]>::try_from(value.as_slice()) else {
            return Ok(MemoryKey::Invalid);
        };
        Ok(MemoryKey::Found(Zeroizing::new(bytes)))
    }

    /// `Locked` de la colección por omisión. Si no se puede leer como booleano,
    /// se la da por bloqueada.
    async fn collection_locked(&self) -> zbus::Result<bool> {
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
        Ok(bool::try_from(locked).unwrap_or(true))
    }

    /// La clave del proveedor de esa dirección, con [`Self::store_secret`].
    async fn store(&self, url: &str, key: &ApiKey) -> zbus::Result<Result<(), Error>> {
        self.store_secret(
            provider_attributes(url),
            ITEM_LABEL,
            key.expose().as_bytes(),
            "text/plain",
        )
        .await
    }

    /// Borra cada ítem desbloqueado con esos atributos (`Item.Delete`). Los
    /// bloqueados no se tocan: borrarlos pediría desbloquear. El `Err` de
    /// afuera es del bus; el de adentro, que quedó alguno.
    async fn delete(&self, attributes: &HashMap<&str, &str>) -> zbus::Result<Result<(), Error>> {
        let (unlocked, locked) = self.search(attributes).await?;
        let mut left = !locked.is_empty();
        for item in &unlocked {
            left |= !self.delete_item(item).await?;
        }
        Ok(if left { Err(locked_error()) } else { Ok(()) })
    }

    /// `Item.Delete` de un ítem. `false` si no se borró porque el llavero se
    /// bloqueó (contestó un prompt o `IsLocked`).
    async fn delete_item(&self, item: &OwnedObjectPath) -> zbus::Result<bool> {
        let deleted = self
            .connection
            .call_method(
                Some(SECRETS_NAME),
                item.as_str(),
                Some("org.freedesktop.Secret.Item"),
                "Delete",
                &(),
            )
            .await
            .and_then(|reply| reply.body().deserialize::<OwnedObjectPath>());
        match deleted {
            Ok(prompt) => Ok(prompt.as_str() == "/"),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.Secret.Error.IsLocked" =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// `CreateItem` en la colección por omisión con `replace`. El `Err` de
    /// afuera es del bus; el de adentro, el motivo ya armado para la persona.
    async fn store_secret(
        &self,
        attributes: HashMap<&str, &str>,
        label: &str,
        value: &[u8],
        content_type: &str,
    ) -> zbus::Result<Result<(), Error>> {
        // Se pregunta antes de escribir: con la colección bloqueada,
        // `CreateItem` devolvería un diálogo de desbloqueo, y abrirlo es de
        // quien usa el sistema, no de un servicio en segundo plano.
        if self.collection_locked().await? {
            return Ok(Err(locked_error()));
        }

        let properties = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(label)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes),
            ),
        ]);
        let session = self.open_session().await?;
        let secret = (&session, Vec::<u8>::new(), value, content_type);
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
    /// Donde está la clave de proveedor creada con `CreateItem`, si se creó.
    const CREATED_ITEM: &str = "/org/freedesktop/secrets/collection/login/created";
    /// Donde está el ítem de la llave de la memoria, si se creó.
    const MEMORY_ITEM: &str = "/org/freedesktop/secrets/collection/login/memory";

    /// Las llamadas que recibe el llavero de mentira, en orden, por nombre del
    /// método D-Bus (`SearchItems`, `OpenSession`, `GetSecrets`, `Locked`,
    /// `CreateItem`, `Delete`). Infraestructura para probar el orden.
    type Calls = Arc<Mutex<Vec<&'static str>>>;

    /// Si un `CreateItem` fue el de la llave de la memoria.
    fn is_memory(created: &Created) -> bool {
        created.attributes.get("purpose").map(String::as_str) == Some(MEMORY_PURPOSE)
    }

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
        /// Lo creado con `CreateItem`: la llave de la memoria se busca y se lee
        /// de acá, como en un llavero de verdad (con `replace`, una sola).
        created: Arc<Mutex<Vec<Created>>>,
        /// Para saber si la llave de la memoria está en un llavero bloqueado.
        collection: Collection,
        calls: Calls,
        /// Si los ítems de `item` llevan el esquema protegido (`purpose` y
        /// `xdg:schema`) o son de antes (`{service, url}` solos).
        schema: bool,
        /// Los ítems de `item` borrados con `Item.Delete`.
        deleted: Arc<Mutex<Vec<String>>>,
    }

    /// Los atributos de los ítems de `item`, con o sin el esquema protegido.
    fn item_attributes(url: &str, schema: bool) -> HashMap<String, String> {
        let mut attributes = HashMap::from([
            ("service".to_owned(), ITEM_SERVICE.to_owned()),
            ("url".to_owned(), url.to_owned()),
        ]);
        if schema {
            attributes.insert("purpose".into(), PROVIDER_PURPOSE.into());
            attributes.insert("xdg:schema".into(), ITEM_SERVICE.into());
        }
        attributes
    }

    /// Si un ítem con `item` responde a una búsqueda con `query` (como en el
    /// Secret Service: la búsqueda es un subconjunto de sus atributos).
    fn matches(query: &HashMap<String, String>, item: &HashMap<String, String>) -> bool {
        query
            .iter()
            .all(|(key, value)| item.get(key) == Some(value))
    }

    #[interface(name = "org.freedesktop.Secret.Service")]
    impl FakeSecrets {
        fn search_items(
            &self,
            attributes: HashMap<String, String>,
        ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
            self.calls.lock().push("SearchItems");
            // La llave de la memoria: quien busca con un subconjunto de sus
            // atributos la encuentra, si se creó y no se borró.
            if attributes.get("purpose").map(String::as_str) == Some(MEMORY_PURPOSE) {
                let found = self.created.lock().iter().any(|created| {
                    is_memory(created)
                        && attributes
                            .iter()
                            .all(|(key, value)| created.attributes.get(key) == Some(value))
                });
                let item = OwnedObjectPath::try_from(MEMORY_ITEM).unwrap();
                return match (found, self.collection == Collection::Locked) {
                    (false, _) => (vec![], vec![]),
                    (true, false) => (vec![item], vec![]),
                    (true, true) => (vec![], vec![item]),
                };
            }
            let coincide = matches(&attributes, &item_attributes(&self.url, self.schema));
            let deleted = self.deleted.lock().clone();
            let path = |path: &str| {
                (!deleted.iter().any(|gone| gone == path))
                    .then(|| OwnedObjectPath::try_from(path).unwrap())
            };
            let (mut unlocked, mut locked): (Vec<_>, Vec<_>) = match (coincide, self.item) {
                (true, Item::Unlocked) => (path(ITEM).into_iter().collect(), vec![]),
                (true, Item::Locked) => (vec![], path(ITEM).into_iter().collect()),
                (true, Item::Duplicated) => {
                    (path(ITEM).into_iter().chain(path(ITEM_2)).collect(), vec![])
                }
                (true, Item::LockedTwice) => {
                    (vec![], path(ITEM).into_iter().chain(path(ITEM_2)).collect())
                }
                (false, _) => (vec![], vec![]),
            };
            // Una clave de proveedor creada con `CreateItem` (la migración, o
            // guardar y después leer) también se encuentra.
            let created = self.created.lock().iter().any(|created| {
                created.attributes.get("purpose").map(String::as_str) == Some(PROVIDER_PURPOSE)
                    && matches(&attributes, &created.attributes)
            });
            if created {
                // `CreateItem` con `replace` reemplaza al ítem con los mismos
                // atributos: el de `item` con esquema deja de estar.
                if self.schema {
                    unlocked.clear();
                    locked.clear();
                }
                let item = OwnedObjectPath::try_from(CREATED_ITEM).unwrap();
                if self.collection == Collection::Locked {
                    locked.push(item);
                } else {
                    unlocked.push(item);
                }
            }
            (unlocked, locked)
        }

        fn open_session(
            &self,
            algorithm: String,
            _input: zbus::zvariant::Value<'_>,
        ) -> (OwnedValue, OwnedObjectPath) {
            self.calls.lock().push("OpenSession");
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
            self.calls.lock().push("GetSecrets");
            assert_eq!(session.as_str(), SESSION);
            items
                .into_iter()
                .map(|item| {
                    let value = if item.as_str() == MEMORY_ITEM || item.as_str() == CREATED_ITEM {
                        let purpose = if item.as_str() == MEMORY_ITEM {
                            MEMORY_PURPOSE
                        } else {
                            PROVIDER_PURPOSE
                        };
                        let created = self.created.lock();
                        let last = created.iter().rev().find(|created| {
                            created.attributes.get("purpose").map(String::as_str) == Some(purpose)
                        });
                        last.map(|created| created.value.clone())
                            .unwrap_or_default()
                    } else {
                        self.secret.clone()
                    };
                    let secret: Secret = (
                        OwnedObjectPath::try_from(SESSION).unwrap(),
                        vec![],
                        value,
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
        calls: Calls,
    }

    #[interface(name = "org.freedesktop.Secret.Collection")]
    impl FakeCollection {
        #[zbus(property)]
        fn locked(&self) -> bool {
            self.calls.lock().push("Locked");
            self.state == Collection::Locked
        }

        fn create_item(
            &self,
            properties: HashMap<String, OwnedValue>,
            secret: Secret,
            replace: bool,
        ) -> Result<(OwnedObjectPath, OwnedObjectPath), FakeError> {
            self.calls.lock().push("CreateItem");
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

    /// El ítem de la llave de la memoria: `Delete` la saca de `created`, salvo
    /// con la colección en `Prompts` (contesta un prompt) o `IsLocked`.
    struct FakeItem {
        state: Collection,
        created: Arc<Mutex<Vec<Created>>>,
        calls: Calls,
    }

    #[interface(name = "org.freedesktop.Secret.Item")]
    impl FakeItem {
        fn delete(&self) -> Result<OwnedObjectPath, FakeError> {
            self.calls.lock().push("Delete");
            let prompt = match self.state {
                Collection::Prompts => "/org/freedesktop/secrets/prompt/p2",
                Collection::IsLocked => return Err(FakeError::IsLocked("bloqueada".into())),
                Collection::Fails => return Err(FakeError::Failed("falló".into())),
                _ => {
                    self.created.lock().retain(|created| !is_memory(created));
                    "/"
                }
            };
            Ok(OwnedObjectPath::try_from(prompt).unwrap())
        }
    }

    /// Un ítem de `item` (en `ITEM` o `ITEM_2`): sus atributos, para saber si
    /// lleva el esquema, y `Delete`, para la migración.
    struct FakeProviderItem {
        path: &'static str,
        attributes: HashMap<String, String>,
        state: Collection,
        deleted: Arc<Mutex<Vec<String>>>,
        calls: Calls,
    }

    #[interface(name = "org.freedesktop.Secret.Item")]
    impl FakeProviderItem {
        #[zbus(property)]
        fn attributes(&self) -> HashMap<String, String> {
            self.attributes.clone()
        }

        fn delete(&self) -> Result<OwnedObjectPath, FakeError> {
            self.calls.lock().push("Delete");
            let prompt = match self.state {
                Collection::Prompts => "/org/freedesktop/secrets/prompt/p3",
                Collection::IsLocked => return Err(FakeError::IsLocked("bloqueada".into())),
                _ => {
                    self.deleted.lock().push(self.path.to_owned());
                    "/"
                }
            };
            Ok(OwnedObjectPath::try_from(prompt).unwrap())
        }
    }

    struct Setup {
        service: SecretService,
        /// Las llamadas al llavero, en orden.
        calls: Calls,
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
        secret_service_full(item, secret, collection, true).await
    }

    async fn secret_service_full(
        item: Item,
        secret: &[u8],
        collection: Collection,
        schema: bool,
    ) -> Setup {
        let closed = Arc::new(Mutex::new(0));
        let sessions_opened = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(Vec::new()));
        let calls = Calls::default();
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeSecrets {
            url: "https://api.ejemplo.com/v1/".into(),
            secret: secret.to_vec(),
            item,
            sessions_opened: sessions_opened.clone(),
            created: created.clone(),
            collection,
            calls: calls.clone(),
            schema,
            deleted: deleted.clone(),
        };
        let provider_item = |path: &'static str| FakeProviderItem {
            path,
            attributes: item_attributes("https://api.ejemplo.com/v1/", schema),
            state: collection,
            deleted: deleted.clone(),
            calls: calls.clone(),
        };
        let (item_1, item_2) = (provider_item(ITEM), provider_item(ITEM_2));
        let session = FakeSession(closed.clone());
        let memory_item = FakeItem {
            state: collection,
            created: created.clone(),
            calls: calls.clone(),
        };
        let collection = FakeCollection {
            state: collection,
            created: created.clone(),
            calls: calls.clone(),
        };
        let (server, client) = p2p_pair(|builder| {
            builder
                .serve_at(SECRETS_PATH, fake)?
                .serve_at(SESSION, session)?
                .serve_at(DEFAULT_COLLECTION, collection)?
                .serve_at(MEMORY_ITEM, memory_item)?
                .serve_at(ITEM, item_1)?
                .serve_at(ITEM_2, item_2)
        })
        .await
        .unwrap();
        Setup {
            service: SecretService { connection: client },
            calls,
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
                ("purpose".to_owned(), PROVIDER_PURPOSE.to_owned()),
                ("xdg:schema".to_owned(), ITEM_SERVICE.to_owned()),
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
        // La lectura busca con el mismo mapa (con el esquema protegido), y
        // esa búsqueda acierta.
        assert_eq!(attributes.len(), 4);
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
                memory: crate::test_support::memory_off(),
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

    // ── La llave de la memoria y el esquema protegido ─────────────────────────

    /// Planta un ítem como lo haría otro proceso de la sesión: con los
    /// atributos que se le antojen.
    async fn plant(setup: &Setup, attributes: &[(&str, &str)], value: &[u8]) {
        setup
            .service
            .store_secret(
                attributes.iter().copied().collect(),
                "plantado",
                value,
                "application/octet-stream",
            )
            .await
            .unwrap()
            .unwrap();
    }

    fn memory_key_bytes(key: MemoryKey) -> Option<[u8; MEMORY_KEY_BYTES]> {
        match key {
            MemoryKey::Found(key) => Some(*key),
            _ => None,
        }
    }

    fn owned(attributes: HashMap<&str, &str>) -> HashMap<String, String> {
        attributes
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn planted_memory_key(value: Vec<u8>) -> Created {
        Created {
            label: MEMORY_LABEL.into(),
            attributes: owned(memory_attributes()),
            session: SESSION.into(),
            value,
            content_type: String::new(),
            replace: true,
        }
    }

    #[tokio::test]
    async fn la_llave_de_la_memoria_se_crea_con_los_tres_atributos_etiqueta_y_reemplazo() {
        let setup = secret_service(Item::Unlocked, b"").await;

        setup.service.store_memory_key(&[9u8; 32]).await.unwrap();

        let created = setup.created.lock();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].label, MEMORY_LABEL);
        assert_eq!(
            created[0].attributes,
            HashMap::from([
                ("service".to_owned(), ITEM_SERVICE.to_owned()),
                ("purpose".to_owned(), MEMORY_PURPOSE.to_owned()),
                ("xdg:schema".to_owned(), ITEM_SERVICE.to_owned()),
            ])
        );
        assert_eq!(created[0].value, [9u8; 32]);
        assert_eq!(created[0].content_type, "application/octet-stream");
        assert!(created[0].replace);
    }

    #[tokio::test]
    async fn la_llave_que_se_guarda_se_encuentra_con_los_mismos_atributos_y_vuelve_igual() {
        let setup = secret_service(Item::Unlocked, b"").await;
        assert!(matches!(
            setup.service.memory_key().await.unwrap(),
            MemoryKey::Missing
        ));

        setup.service.store_memory_key(&[9u8; 32]).await.unwrap();

        let key = setup.service.memory_key().await.unwrap();
        assert_eq!(memory_key_bytes(key), Some([9u8; 32]));
        // Una sesión para guardar y otra para leer, las dos cerradas.
        assert_eq!(*setup.closed.lock(), 2);
    }

    #[tokio::test]
    async fn un_item_plantado_sin_esquema_con_service_y_purpose_no_se_usa_como_llave() {
        let setup = secret_service(Item::Unlocked, b"").await;
        plant(
            &setup,
            &[("service", ITEM_SERVICE), ("purpose", MEMORY_PURPOSE)],
            &[6u8; 32],
        )
        .await;

        let key = setup.service.memory_key().await.unwrap();

        assert!(matches!(key, MemoryKey::Missing), "{key:?}");
    }

    #[tokio::test]
    async fn con_la_llave_de_verdad_y_una_plantada_sin_esquema_gana_la_de_verdad() {
        let setup = secret_service(Item::Unlocked, b"").await;
        plant(
            &setup,
            &[("service", ITEM_SERVICE), ("purpose", MEMORY_PURPOSE)],
            &[6u8; 32],
        )
        .await;
        setup.service.store_memory_key(&[9u8; 32]).await.unwrap();

        let key = setup.service.memory_key().await.unwrap();

        assert_eq!(memory_key_bytes(key), Some([9u8; 32]));
    }

    #[tokio::test]
    async fn una_llave_que_no_son_32_bytes_es_invalida_y_con_el_llavero_bloqueado_esta_bloqueada() {
        let setup = secret_service(Item::Unlocked, b"").await;
        plant(
            &setup,
            &[
                ("service", ITEM_SERVICE),
                ("purpose", MEMORY_PURPOSE),
                ("xdg:schema", ITEM_SERVICE),
            ],
            &[1, 2, 3],
        )
        .await;
        assert!(matches!(
            setup.service.memory_key().await.unwrap(),
            MemoryKey::Invalid
        ));

        let locked = secret_service_with(Item::Unlocked, b"", Collection::Locked).await;
        locked
            .created
            .lock()
            .push(planted_memory_key(vec![9u8; 32]));
        assert!(matches!(
            locked.service.memory_key().await.unwrap(),
            MemoryKey::Locked
        ));
        // Un llavero bloqueado no entrega la llave ni abre una sesión.
        assert!(locked.sessions_opened.lock().is_empty());
    }

    #[tokio::test]
    async fn borrar_la_llave_la_saca_y_con_el_llavero_bloqueado_da_unavailable() {
        let setup = secret_service(Item::Unlocked, b"").await;
        setup.service.store_memory_key(&[9u8; 32]).await.unwrap();
        setup.service.delete_memory_key().await.unwrap();
        assert!(matches!(
            setup.service.memory_key().await.unwrap(),
            MemoryKey::Missing
        ));
        // Borrar lo que no está tampoco falla.
        setup.service.delete_memory_key().await.unwrap();

        let prompts = secret_service_with(Item::Unlocked, b"", Collection::Prompts).await;
        prompts
            .created
            .lock()
            .push(planted_memory_key(vec![9u8; 32]));
        assert!(matches!(
            prompts.service.delete_memory_key().await,
            Err(Error::Unavailable(text)) if text.contains("bloqueado")
        ));
    }

    #[tokio::test]
    async fn is_locked_sigue_a_la_coleccion_y_sin_llavero_es_unavailable() {
        for (collection, esperado) in [(Collection::Unlocked, false), (Collection::Locked, true)] {
            let setup = secret_service_with(Item::Unlocked, b"", collection).await;
            assert_eq!(setup.service.is_locked().await.unwrap(), esperado);
        }
        let (_server, client) =
            p2p_pair(|builder| builder.serve_at(SESSION, FakeSession(Arc::default())))
                .await
                .unwrap();
        let service = SecretService { connection: client };
        assert!(matches!(
            service.is_locked().await,
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            service.memory_key().await,
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            service.delete_memory_key().await,
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn el_debug_de_la_llave_de_la_memoria_no_la_muestra() {
        let key = MemoryKey::Found(Zeroizing::new([0xABu8; 32]));
        assert_eq!(format!("{key:?}"), "Found(…)");
        for (other, text) in [
            (MemoryKey::Missing, "Missing"),
            (MemoryKey::Locked, "Locked"),
            (MemoryKey::Duplicated, "Duplicated"),
            (MemoryKey::Invalid, "Invalid"),
        ] {
            assert_eq!(format!("{other:?}"), text);
        }
    }

    #[tokio::test]
    async fn la_clave_con_esquema_gana_y_no_dispara_ninguna_migracion() {
        let setup = secret_service(Item::Unlocked, b"sk-nueva").await;

        assert_eq!(
            setup.service.api_key(URL).await.unwrap().expose(),
            "sk-nueva"
        );

        assert!(setup.created.lock().is_empty());
        assert!(!setup.calls.lock().contains(&"Delete"));
    }
}
