//! La habilidad de contactos: buscar y leer contactos por `AccountsStore`.
//! **Sólo lectura**: ninguna herramienta crea, edita ni borra nada —eso sería
//! una acción confirmable, como el envío del correo, y queda para otra
//! rebanada—.
//!
//! Las lecturas van por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que
//! ya exige `store.contacts` por cuenta. El único control del lado de Lince es
//! el permiso propio, que el servicio comprueba antes de ejecutar la
//! herramienta.

use serde_json::{json, Value};
use zbus::Connection;

use super::dbus::{call_with_timeout, store_alive, store_proxy, string_arg, CALL_TIMEOUT};
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_CONTACTS};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, DOMAIN_CONTACTS};

/// Cuántos contactos trae una búsqueda. El almacén además acota el tamaño de la
/// respuesta y topea el límite, así que esto es sólo para no pedir de más.
const SEARCH_LIMIT: u32 = 20;

/// La habilidad de contactos. Guarda la conexión al bus de sesión para hablarle
/// al almacén de cuentas; los proxies se arman por llamada, que es barato.
pub struct ContactsSkill {
    connection: Connection,
}

impl ContactsSkill {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }
}

impl Skill for ContactsSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_CONTACTS
    }

    fn permission(&self) -> &'static str {
        PERMISSION_CONTACTS
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "search_contacts".into(),
                description: "Busca contactos de una cuenta por texto (nombre, correo, teléfono \
                              u organización). Devuelve un resumen de cada uno con su id."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "El identificador de la cuenta."},
                        "query": {"type": "string", "description": "Las palabras a buscar."}
                    },
                    "required": ["account_id", "query"]
                }),
            },
            ToolDef {
                name: "read_contact".into(),
                description: "Lee un contacto entero de una cuenta por su id (nombre, correos, \
                              teléfonos, direcciones, organización, notas y relaciones)."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "El identificador de la cuenta."},
                        "contact_id": {"type": "string", "description": "El id del contacto."}
                    },
                    "required": ["account_id", "contact_id"]
                }),
            },
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        Box::pin(async move {
            match name {
                "search_contacts" => self.search_contacts(args).await,
                "read_contact" => self.read_contact(args).await,
                other => Err(Error::InvalidArgs(format!(
                    "los contactos no tienen una herramienta «{other}»"
                ))),
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        Box::pin(store_alive(&self.connection))
    }
}

impl ContactsSkill {
    async fn search_contacts(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let query = string_arg(args, "query")?;
        let proxy = store_proxy(&self.connection).await?;
        // Sin cursor, la primera página.
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call(
                "SearchContacts",
                &(account_id, query, String::new(), SEARCH_LIMIT),
            ),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn read_contact(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let contact_id = string_arg(args, "contact_id")?;
        let proxy = store_proxy(&self.connection).await?;
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call("GetContact", &(account_id, contact_id)),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }
}

#[cfg(test)]
#[path = "contacts_tests.rs"]
mod tests;
