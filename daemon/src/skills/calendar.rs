//! La habilidad de calendario: leer calendarios, eventos y tareas por
//! `AccountsStore`. **Sólo lectura**: ninguna herramienta crea ni mueve nada
//! —eso sería una acción confirmable, como el envío del correo, y queda para
//! otra rebanada—.
//!
//! Las lecturas van por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que
//! ya exige `store.calendar` por cuenta. El único control del lado de Lince es
//! el permiso propio, que el servicio comprueba antes de ejecutar la
//! herramienta.

use serde_json::{json, Value};
use zbus::Connection;

use super::dbus::{
    bool_arg, call_with_timeout, opt_string_arg, store_alive, store_proxy, string_arg,
    string_array_arg, CALL_TIMEOUT,
};
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_CALENDAR};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, DOMAIN_CALENDAR};

/// Cuántos eventos o tareas trae una página. El almacén además acota el tamaño
/// de la respuesta y topea el límite, así que esto es sólo para no pedir de más.
const PAGE_LIMIT: u32 = 100;

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
        Box::pin(store_alive(&self.connection))
    }
}

impl CalendarSkill {
    async fn list_events(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let from = string_arg(args, "from")?;
        let to = string_arg(args, "to")?;
        let calendar_ids = string_array_arg(args, "calendar_ids")?;
        let proxy = store_proxy(&self.connection).await?;
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
        let occurrence_id = opt_string_arg(args, "occurrence_id")?;
        let proxy = store_proxy(&self.connection).await?;
        let json: String = call_with_timeout(
            CALL_TIMEOUT,
            proxy.call("GetEvent", &(event_id, occurrence_id)),
        )
        .await?;
        Ok(ToolOutcome::Result(json))
    }

    async fn list_tasks(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let calendar_ids = string_array_arg(args, "calendar_ids")?;
        let include_completed = bool_arg(args, "include_completed", false)?;
        let proxy = store_proxy(&self.connection).await?;
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

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
