//! La habilidad de calendario: leer calendarios, eventos y tareas por
//! `AccountsStore`. **Sólo lectura**: ninguna herramienta crea ni mueve nada
//! —eso sería una acción confirmable, como el envío del correo, y queda para
//! otra rebanada—.
//!
//! Las lecturas van por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que
//! ya exige `store.calendar` por cuenta. El único control del lado de Lince es
//! el permiso propio, que el servicio comprueba antes de ejecutar la
//! herramienta.

use std::time::Duration;

use serde_json::{json, Value};
use zbus::Connection;

use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_CALENDAR};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, DOMAIN_CALENDAR};

// ── Direcciones del bus de cuentas ──────────────────────────────────────────────
//
// El almacén local lo sirve el servicio dueño del nombre
// `ar.net.vasak.os.AccountsSync`, en el objeto `AccountsStore`.

const ACCOUNTS_SERVICE: &str = "ar.net.vasak.os.AccountsSync";
const STORE_PATH: &str = "/ar/net/vasak/os/AccountsStore";
const STORE_INTERFACE: &str = "ar.net.vasak.os.AccountsStore";

/// Cuántos eventos o tareas trae una página. El almacén además acota el tamaño
/// de la respuesta y topea el límite, así que esto es sólo para no pedir de más.
const PAGE_LIMIT: u32 = 100;

/// Plazo de una lectura o de `GetStatus`: zbus 5 no tiene timeout por método, y
/// un servicio conectado pero que no responde dejaría la llamada —y el lugar de
/// conversación del turno— esperando para siempre.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// La habilidad de calendario. Guarda la conexión al bus de sesión para
/// hablarle al almacén de cuentas; los proxies se arman por llamada, que es
/// barato.
pub struct CalendarSkill {
    connection: Connection,
}

impl CalendarSkill {
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

impl Skill for CalendarSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_CALENDAR
    }

    fn permission(&self) -> &'static str {
        PERMISSION_CALENDAR
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "list_events".into(),
                description: "Lista las veces de los eventos del calendario en un rango de \
                              fechas. Devuelve metadatos (título, comienzo, fin, calendario, \
                              id del evento y de la vez)."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "from": {"type": "string", "description": "El comienzo del rango, en ISO-8601 (RFC 3339)."},
                        "to": {"type": "string", "description": "El fin del rango, en ISO-8601 (RFC 3339)."},
                        "calendar_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Los calendarios a mirar; vacío u omitido, todos."
                        }
                    },
                    "required": ["from", "to"]
                }),
            },
            ToolDef {
                name: "read_event".into(),
                description: "Lee un evento entero por su id. Con «occurrence_id» lee esa vez \
                              puntual; sin él, el evento."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "event_id": {"type": "string", "description": "El id del evento."},
                        "occurrence_id": {"type": "string", "description": "La vez puntual; omitido, el evento."}
                    },
                    "required": ["event_id"]
                }),
            },
            ToolDef {
                name: "list_tasks".into(),
                description: "Lista las tareas del calendario, por vencimiento. Sin las hechas \
                              salvo con «include_completed»."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "calendar_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Los calendarios a mirar; vacío u omitido, todos."
                        },
                        "include_completed": {"type": "boolean", "description": "Incluir las tareas ya hechas."}
                    }
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
                "list_events" => self.list_events(args).await,
                "read_event" => self.read_event(args).await,
                "list_tasks" => self.list_tasks(args).await,
                other => Err(Error::InvalidArgs(format!(
                    "el calendario no tiene una herramienta «{other}»"
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
            let status = proxy.call::<_, _, String>("GetStatus", &());
            matches!(tokio::time::timeout(CALL_TIMEOUT, status).await, Ok(Ok(_)))
        })
    }
}

impl CalendarSkill {
    async fn list_events(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let from = string_arg(args, "from")?;
        let to = string_arg(args, "to")?;
        let calendar_ids = string_array_arg(args, "calendar_ids")?;
        let proxy = self.store_proxy().await?;
        // Sin cursor, la primera página; `calendar_ids` vacío son todos.
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call(
                "ListOccurrences",
                &(from, to, calendar_ids, String::new(), PAGE_LIMIT),
            ),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn read_event(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let event_id = string_arg(args, "event_id")?;
        // `occurrence_id` vacío es "el evento, no una vez puntual".
        let occurrence_id = opt_string_arg(args, "occurrence_id");
        let proxy = self.store_proxy().await?;
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call("GetEvent", &(event_id, occurrence_id)),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn list_tasks(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let calendar_ids = string_array_arg(args, "calendar_ids")?;
        let include_completed = bool_arg(args, "include_completed", false);
        let proxy = self.store_proxy().await?;
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call(
                "ListTasks",
                &(calendar_ids, include_completed, String::new(), PAGE_LIMIT),
            ),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }
}

/// Envuelve una llamada D-Bus en un plazo y traduce su error: zbus 5 no tiene
/// timeout por método. Si se vence, el servicio está conectado pero no
/// responde, y la llamada no espera para siempre.
async fn call_with_timeout<T>(
    timeout: Duration,
    call: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, Error> {
    match tokio::time::timeout(timeout, call).await {
        Ok(result) => result.map_err(backend_error),
        Err(_) => Err(Error::Backend(
            "el servicio de cuentas no respondió a tiempo".into(),
        )),
    }
}

/// El valor de un argumento de texto, o `InvalidArgs` si falta o no es texto.
fn string_arg(args: &Value, key: &str) -> Result<String, Error> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidArgs(format!("falta el argumento «{key}»")))
}

/// El valor de un argumento de texto opcional: vacío si falta o no es texto.
fn opt_string_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Un argumento booleano opcional, con su valor por omisión si falta.
fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Un argumento de lista de textos opcional: vacío si falta. `InvalidArgs` si
/// está pero no es una lista, o si tiene un elemento que no es texto.
fn string_array_arg(args: &Value, key: &str) -> Result<Vec<String>, Error> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::InvalidArgs(format!("«{key}» tiene un elemento que no es texto"))
                })
            })
            .collect(),
        Some(_) => Err(Error::InvalidArgs(format!("«{key}» no es una lista"))),
    }
}

/// Un error del bus de cuentas, para la persona, sin filtrar nada delicado del
/// cable.
fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("el servicio de cuentas no respondió: {error}"))
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
