//! La habilidad de contactos: buscar, leer y listar libretas por
//! `AccountsStore`, y preparar la creación de un contacto. Crear es una acción
//! confirmable, como el envío del correo: `create_contact` arma el contacto pero
//! **no lo crea** hasta que la persona lo confirma.
//!
//! Todo va por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que ya exige
//! `store.contacts` por cuenta. El único control del lado de Lince es el permiso
//! propio, que el servicio comprueba antes de ejecutar la herramienta, más la
//! confirmación de la persona para crear.

use serde_json::{json, Value};
use zbus::Connection;

use super::dbus::{
    call_with_timeout, opt_string_arg, store_alive, store_proxy, string_arg, string_array_arg,
    CALL_TIMEOUT,
};
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_CONTACTS};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, PendingAction, DOMAIN_CONTACTS};

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
                name: "list_address_books".into(),
                description: "Lista las libretas de contactos de una cuenta. Devuelve cada una \
                              con su id, que es el que toma «create_contact»."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "El identificador de la cuenta."}
                    },
                    "required": ["account_id"]
                }),
            },
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
            ToolDef {
                name: "create_contact".into(),
                description: "Prepara un contacto nuevo en una libreta. No lo crea: la persona lo \
                              confirma antes. La libreta se nombra con «account_id» y \
                              «address_book_id», de list_address_books."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "La cuenta de la libreta."},
                        "address_book_id": {"type": "string", "description": "El id de la libreta, de list_address_books."},
                        "full_name": {"type": "string", "description": "El nombre completo del contacto."},
                        "organization": {"type": "string", "description": "La organización; opcional."},
                        "note": {"type": "string", "description": "Una nota; opcional."},
                        "emails": {"type": "array", "items": {"type": "string"}, "description": "Las direcciones de correo; opcional."},
                        "phones": {"type": "array", "items": {"type": "string"}, "description": "Los teléfonos; opcional."}
                    },
                    "required": ["account_id", "address_book_id", "full_name"]
                }),
            },
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        Box::pin(async move {
            match name {
                "list_address_books" => self.list_address_books(args).await,
                "search_contacts" => self.search_contacts(args).await,
                "read_contact" => self.read_contact(args).await,
                "create_contact" => self.create_contact(args, ctx).await,
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
    async fn list_address_books(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let proxy = store_proxy(&self.connection).await?;
        let json: String =
            call_with_timeout(CALL_TIMEOUT, proxy.call("ListAddressBooks", &(account_id,))).await?;
        Ok(ToolOutcome::Result(json))
    }

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

    /// Prepara un contacto para confirmar. No crea nada acá: arma el payload de
    /// `PutContact` y devuelve una acción pendiente cuyo `execute` lo crea (con
    /// `if_match` vacío) recién al confirmar.
    async fn create_contact(&self, args: &Value, ctx: &SkillCtx) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let address_book_id = string_arg(args, "address_book_id")?;
        let full_name = string_arg(args, "full_name")?;
        let organization = opt_string_arg(args, "organization")?;
        let note = opt_string_arg(args, "note")?;
        let emails = string_array_arg(args, "emails")?;
        let phones = string_array_arg(args, "phones")?;

        // El payload de `PutContact`: `full_name`/`organization`/`note` de texto;
        // `emails`/`phones` como listas de `{value}` (la etiqueta es opcional y
        // no se fija acá). Sin `id` (crear). Los opcionales sólo se incluyen si
        // vienen: ausente no toca, y en uno nuevo eso es no fijarlo.
        let mut payload = json!({ "full_name": full_name });
        let map = payload.as_object_mut().expect("el payload es un objeto");
        if !organization.is_empty() {
            map.insert("organization".into(), json!(organization));
        }
        if !note.is_empty() {
            map.insert("note".into(), json!(note));
        }
        if !emails.is_empty() {
            map.insert("emails".into(), labeled(&emails));
        }
        if !phones.is_empty() {
            map.insert("phones".into(), labeled(&phones));
        }
        let payload = payload.to_string();

        let action = PendingAction {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: ctx.conversation_id.clone(),
            kind: "create_contact".into(),
            summary: format!("Crear el contacto «{full_name}» en la libreta «{address_book_id}»"),
            // Campos de correo: vacíos para una acción de contactos.
            to: String::new(),
            subject: String::new(),
            body_preview: String::new(),
        };

        let connection = self.connection.clone();
        let execute = Box::new(move || -> BoxFuture<'static, Result<(), Error>> {
            Box::pin(async move {
                let proxy = store_proxy(&connection).await?;
                // `if_match` vacío: crear. Devuelve `{contact_id}`, que no se usa.
                let _created: String = call_with_timeout(
                    CALL_TIMEOUT,
                    proxy.call(
                        "PutContact",
                        &(account_id, address_book_id, payload, String::new()),
                    ),
                )
                .await?;
                Ok(())
            })
        });

        Ok(ToolOutcome::Pending { action, execute })
    }
}

/// Una lista de valores de texto (correos, teléfonos) en la forma `{value}` que
/// toma `PutContact`. La etiqueta es opcional y no se fija acá.
fn labeled(values: &[String]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| json!({ "value": value }))
            .collect(),
    )
}

#[cfg(test)]
#[path = "contacts_tests.rs"]
mod tests;
