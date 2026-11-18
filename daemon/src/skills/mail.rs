//! La habilidad de correo: leer el correo por `AccountsStore` y redactar envíos
//! por `AccountsSync`, siempre con la confirmación de la persona para mandar.
//!
//! Las lecturas van por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que
//! ya exige `store.email` por cuenta. El envío va por
//! `AccountsSync.send_message`, que **no** gatea por permiso: por eso el único
//! control del lado de Lince es el permiso propio (lo comprueba el servicio
//! antes de ejecutar la herramienta) más la confirmación de la persona.

use serde_json::{json, Value};
use zbus::Connection;

use super::{PermissionCheck, Skill, SkillCtx, ToolOutcome, PERMISSION_EMAIL};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, PendingAction, DOMAIN_MAIL};

// ── Direcciones del bus de cuentas ──────────────────────────────────────────────
//
// El almacén local y la sincronización los sirve el mismo servicio, dueño del
// nombre `ar.net.vasak.os.AccountsSync`, en dos objetos distintos.

const ACCOUNTS_SERVICE: &str = "ar.net.vasak.os.AccountsSync";
const STORE_PATH: &str = "/ar/net/vasak/os/AccountsStore";
const STORE_INTERFACE: &str = "ar.net.vasak.os.AccountsStore";
const SYNC_PATH: &str = "/ar/net/vasak/os/AccountsSync";
const SYNC_INTERFACE: &str = "ar.net.vasak.os.AccountsSync";

/// Cuántos mensajes trae una búsqueda. El almacén además acota el tamaño de la
/// respuesta, así que esto es sólo para no pedir de más.
const SEARCH_LIMIT: u32 = 20;

/// Cuánto del cuerpo se le muestra a la persona en el resumen de un envío.
const BODY_PREVIEW_BYTES: usize = 240;

/// La habilidad de correo. Guarda la conexión al bus de sesión para hablarle al
/// servicio de cuentas; los proxies se arman por llamada, que es barato.
pub struct MailSkill {
    connection: Connection,
}

impl MailSkill {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn store_proxy(&self) -> Result<zbus::Proxy<'_>, Error> {
        zbus::Proxy::new(
            &self.connection,
            ACCOUNTS_SERVICE,
            STORE_PATH,
            STORE_INTERFACE,
        )
        .await
        .map_err(backend_error)
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
        Box::pin(async move {
            // `GetStatus` no abre ningún diálogo ni pide permiso: si contesta,
            // el almacén está en pie.
            let Ok(proxy) = self.store_proxy().await else {
                return false;
            };
            proxy.call::<_, _, String>("GetStatus", &()).await.is_ok()
        })
    }
}

impl MailSkill {
    async fn search_mail(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let query = string_arg(args, "query")?;
        let proxy = self.store_proxy().await?;
        // `mailbox_id` 0 es "todas las casillas"; sin cursor, la primera página.
        let json: String = proxy
            .call(
                "SearchMessages",
                &(account_id, query, 0i64, String::new(), SEARCH_LIMIT),
            )
            .await
            .map_err(backend_error)?;
        Ok(ToolOutcome::Result(json))
    }

    async fn read_mail(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let message_id = int_arg(args, "message_id")?;
        let proxy = self.store_proxy().await?;
        let json: String = proxy
            .call("GetMessage", &(account_id, message_id))
            .await
            .map_err(backend_error)?;
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
            summary: format!("Enviar un correo a {to} con el asunto «{subject}»"),
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
                // `not_before` vacío: "cuando se pueda".
                let _queued: String = proxy
                    .call("SendMessage", &(account_id, draft, String::new()))
                    .await
                    .map_err(backend_error)?;
                Ok(())
            })
        });

        Ok(ToolOutcome::Pending { action, execute })
    }
}

/// Un verificador de permisos que le pregunta a `vasak-permissions` como Lince.
/// Pregunta la primera vez y recuerda la respuesta.
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
            proxy
                .call("CheckPermission", &(resource, detail))
                .await
                .map_err(backend_error)
        })
    }
}

/// El valor de un argumento de texto, o `InvalidArgs` si falta o no es texto.
fn string_arg(args: &Value, key: &str) -> Result<String, Error> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidArgs(format!("falta el argumento «{key}»")))
}

/// El valor de un argumento entero.
fn int_arg(args: &Value, key: &str) -> Result<i64, Error> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::InvalidArgs(format!("falta el argumento «{key}» o no es un número")))
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

/// Un error del bus de cuentas o permisos, para la persona, sin filtrar nada
/// delicado del cable.
fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("el servicio de cuentas no respondió: {error}"))
}
