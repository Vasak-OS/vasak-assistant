//! La habilidad de correo: leer el correo por `AccountsStore` y redactar envíos
//! por `AccountsSync`, siempre con la confirmación de la persona para mandar.
//!
//! Las lecturas van por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que
//! ya exige `store.email` por cuenta. El envío va por
//! `AccountsSync.send_message`, que **no** gatea por permiso: por eso el único
//! control del lado de Lince es el permiso propio (lo comprueba el servicio
//! antes de ejecutar la herramienta) más la confirmación de la persona.

use std::time::Duration;

use serde_json::{json, Value};
use zbus::Connection;

use super::dbus::{
    backend_error, call_with_timeout, int_arg, store_alive, store_proxy, string_arg,
    ACCOUNTS_SERVICE, CALL_TIMEOUT,
};
use super::{PermissionCheck, Skill, SkillCtx, ToolOutcome, PERMISSION_EMAIL};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, PendingAction, DOMAIN_MAIL};

// ── Direcciones de la sincronización ──────────────────────────────────────────
//
// El almacén local y la sincronización los sirve el mismo servicio, dueño del
// nombre `ar.net.vasak.os.AccountsSync` (ver `dbus`), en dos objetos distintos.

const SYNC_PATH: &str = "/ar/net/vasak/os/AccountsSync";
const SYNC_INTERFACE: &str = "ar.net.vasak.os.AccountsSync";

/// Cuántos mensajes trae una búsqueda. El almacén además acota el tamaño de la
/// respuesta, así que esto es sólo para no pedir de más.
const SEARCH_LIMIT: u32 = 20;

/// Cuánto del cuerpo se le muestra a la persona en el resumen de un envío.
const BODY_PREVIEW_BYTES: usize = 240;

/// Plazo del envío (`SendMessage`), algo más holgado que una lectura. Si se
/// vence, el resultado queda **desconocido**: el correo pudo haber salido, así
/// que no se reintenta (la acción ya se consumió del registro).
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// La habilidad de correo. Guarda la conexión al bus de sesión para hablarle al
/// servicio de cuentas; los proxies se arman por llamada, que es barato.
pub struct MailSkill {
    connection: Connection,
}

impl MailSkill {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }
}

impl Skill for MailSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_MAIL
    }

    fn permission(&self) -> &'static str {
        PERMISSION_EMAIL
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "search_mail".into(),
                description: "Busca mensajes de correo de una cuenta por texto. \
                              Devuelve metadatos (remitente, asunto, fecha, id)."
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
                name: "read_mail".into(),
                description: "Lee un mensaje entero de una cuenta por su id.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "El identificador de la cuenta."},
                        "message_id": {"type": "integer", "description": "El id del mensaje."}
                    },
                    "required": ["account_id", "message_id"]
                }),
            },
            ToolDef {
                name: "send_mail".into(),
                description: "Prepara un correo para enviar. No lo manda: la persona lo \
                              confirma antes de que salga."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "El identificador de la cuenta desde la que se manda."},
                        "to": {"type": "string", "description": "Los destinatarios, separados por coma."},
                        "subject": {"type": "string", "description": "El asunto."},
                        "body": {"type": "string", "description": "El cuerpo del mensaje."}
                    },
                    "required": ["account_id", "to", "subject", "body"]
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
                "search_mail" => self.search_mail(args).await,
                "read_mail" => self.read_mail(args).await,
                "send_mail" => self.send_mail(args, ctx).await,
                other => Err(Error::InvalidArgs(format!(
                    "el correo no tiene una herramienta «{other}»"
                ))),
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        Box::pin(store_alive(&self.connection))
    }
}

impl MailSkill {
    async fn search_mail(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let query = string_arg(args, "query")?;
        let proxy = store_proxy(&self.connection).await?;
        // `mailbox_id` 0 es "todas las casillas"; sin cursor, la primera página.
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call(
                "SearchMessages",
                &(account_id, query, 0i64, String::new(), SEARCH_LIMIT),
            ),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn read_mail(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let message_id = int_arg(args, "message_id")?;
        let proxy = store_proxy(&self.connection).await?;
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call("GetMessage", &(account_id, message_id)),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn send_mail(&self, args: &Value, ctx: &SkillCtx) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let to = string_arg(args, "to")?;
        let subject = string_arg(args, "subject")?;
        let body = string_arg(args, "body")?;

        // El borrador para `AccountsSync.send_message`: las claves son las del
        // contrato de cuentas (en español). El `de` y el nombre los pone el
        // servicio de cuentas desde la cuenta; acá no se eligen.
        let recipients: Vec<String> = to
            .split(',')
            .map(|address| address.trim().to_owned())
            .filter(|address| !address.is_empty())
            .collect();
        // Una lista que se normaliza a vacía (`" , , "`) no es un envío: sin
        // destinatarios no hay a quién mandarle.
        if recipients.is_empty() {
            return Err(Error::InvalidArgs(
                "no hay destinatarios: la lista «to» quedó vacía".into(),
            ));
        }
        let draft = json!({
            "para": recipients,
            "asunto": subject,
            "cuerpo": body,
        })
        .to_string();

        let action = PendingAction {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: ctx.conversation_id.clone(),
            kind: "send_mail".into(),
            // La cuenta que envía entra en el resumen: dos borradores con los
            // mismos destinatarios y asunto pero distinta cuenta (personal y
            // trabajo) no se ven iguales al confirmar.
            summary: format!(
                "Enviar un correo a {to} con el asunto «{subject}» desde la cuenta «{account_id}»"
            ),
            to,
            subject,
            body_preview: preview(&body),
        };

        // El envío se corre recién al confirmar. Captura una copia de la
        // conexión y el borrador completo; el borrador no viaja a la persona.
        let connection = self.connection.clone();
        let execute = Box::new(move || -> BoxFuture<'static, Result<(), Error>> {
            Box::pin(async move {
                let proxy =
                    zbus::Proxy::new(&connection, ACCOUNTS_SERVICE, SYNC_PATH, SYNC_INTERFACE)
                        .await
                        .map_err(backend_error)?;
                // `not_before` vacío: "cuando se pueda". Con plazo: si se vence,
                // el resultado es desconocido (el correo pudo haber salido) y no
                // se reintenta.
                let args = (account_id, draft, String::new());
                let send = proxy.call::<_, _, String>("SendMessage", &args);
                let _queued: String = match tokio::time::timeout(SEND_TIMEOUT, send).await {
                    Ok(result) => result.map_err(backend_error)?,
                    Err(_) => {
                        return Err(Error::Backend(
                            "no se sabe si el correo salió: el envío no respondió a tiempo; no se reintentará."
                                .into(),
                        ))
                    }
                };
                Ok(())
            })
        });

        Ok(ToolOutcome::Pending { action, execute })
    }
}

/// Un verificador de permisos que le pregunta a `vasak-permissions` como Lince.
/// Consulta `CheckPermission` en cada llamada: quien recuerda la decisión de la
/// persona es el servicio de permisos, no esto. Re-preguntar siempre es lo más
/// seguro; si alguna vez pesa, se le pone una caché corta como la de `access.rs`.
///
/// La conexión es la del bus donde vive `vasak-permissions` —el del **sistema**
/// en producción (ver `dbus::permission_bus`)—, no la del almacén de cuentas.
pub struct DbusPermission {
    connection: Connection,
}

impl DbusPermission {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }
}

impl PermissionCheck for DbusPermission {
    fn check<'a>(
        &'a self,
        resource: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let proxy = zbus::Proxy::new(
                &self.connection,
                "ar.net.vasak.os.Permissions",
                "/ar/net/vasak/os/Permissions",
                "ar.net.vasak.os.Permissions",
            )
            .await
            .map_err(backend_error)?;
            // Sin plazo: `CheckPermission` abre un diálogo la primera vez y la
            // respuesta llega recién cuando la persona contesta, lo que puede
            // tardar minutos. Un plazo corto lo tomaría por una negativa.
            proxy
                .call("CheckPermission", &(resource, detail))
                .await
                .map_err(backend_error)
        })
    }
}

/// Un recorte del cuerpo para mostrarle a la persona, cortado en un límite de
/// carácter.
fn preview(body: &str) -> String {
    if body.len() <= BODY_PREVIEW_BYTES {
        return body.to_owned();
    }
    let mut end = BODY_PREVIEW_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

#[cfg(test)]
#[path = "mail_tests.rs"]
mod tests;
