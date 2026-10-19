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
