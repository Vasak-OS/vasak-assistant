//! La habilidad de calendario: listar y leer calendarios, eventos y tareas por
//! `AccountsStore`, y preparar la creación de un evento. Crear es una acción
//! confirmable, como el envío del correo: `create_event` arma el evento pero
//! **no lo crea** hasta que la persona lo confirma.
//!
//! Todo va por `AccountsStore` (`ar.net.vasak.os.AccountsStore`), que ya exige
//! `store.calendar` por cuenta. El único control del lado de Lince es el permiso
//! propio, que el servicio comprueba antes de ejecutar la herramienta, más la
//! confirmación de la persona para crear.

use serde_json::{json, Value};
use zbus::Connection;

use super::dbus::{
    bool_arg, call_with_timeout, opt_string_arg, store_alive, store_proxy, string_arg,
    string_array_arg, CALL_TIMEOUT,
};
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_CALENDAR};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, PendingAction, DOMAIN_CALENDAR};

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
                name: "list_calendars".into(),
                description: "Lista los calendarios de todas las cuentas. Devuelve cada uno con \
                              su id, la cuenta («account_id»), el nombre y qué objetos admite \
                              («components»: VEVENT para eventos, VTODO para tareas). El id y la \
                              cuenta son los que toma «create_event»."
                    .into(),
                parameters: json!({ "type": "object", "properties": {} }),
            },
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
            ToolDef {
                name: "create_event".into(),
                description: "Prepara un evento nuevo en un calendario. No lo crea: la persona lo \
                              confirma antes. El calendario se nombra con «account_id» y \
                              «calendar_id», de list_calendars (uno que admita VEVENT)."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "account_id": {"type": "string", "description": "La cuenta del calendario, de list_calendars."},
                        "calendar_id": {"type": "string", "description": "El id del calendario, de list_calendars."},
                        "summary": {"type": "string", "description": "El título del evento."},
                        "start": {"type": "string", "description": "El comienzo. Con hora, una marca RFC 3339 en UTC («2026-01-01T09:00:00Z»); de día entero, una fecha «2026-01-01» con «all_day» en true."},
                        "end": {"type": "string", "description": "El fin, en el mismo formato que «start»."},
                        "all_day": {"type": "boolean", "description": "Si es un evento de día entero (fechas sin hora)."},
                        "description": {"type": "string", "description": "El detalle del evento; opcional."},
                        "location": {"type": "string", "description": "El lugar; opcional."}
                    },
                    "required": ["account_id", "calendar_id", "summary", "start", "end"]
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
                "list_calendars" => self.list_calendars().await,
                "list_events" => self.list_events(args).await,
                "read_event" => self.read_event(args).await,
                "list_tasks" => self.list_tasks(args).await,
                "create_event" => self.create_event(args, ctx).await,
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
    async fn list_calendars(&self) -> Result<ToolOutcome, Error> {
        let proxy = store_proxy(&self.connection).await?;
        let json: String =
            call_with_timeout(CALL_TIMEOUT, proxy.call("ListCalendars", &())).await?;
        Ok(ToolOutcome::Result(json))
    }

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

    /// Prepara un evento para confirmar. No crea nada acá: valida los argumentos
    /// y las fechas y, si cuadran, devuelve una acción pendiente cuyo `execute`
    /// llama a `PutEvent` (con `if_match` vacío: crear) recién al confirmar.
    async fn create_event(&self, args: &Value, ctx: &SkillCtx) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let calendar_id = string_arg(args, "calendar_id")?;
        let summary = string_arg(args, "summary")?;
        let all_day = bool_arg(args, "all_day", false)?;
        let (start_value, start_params) = ical_datetime(&string_arg(args, "start")?, all_day)?;
        let (end_value, end_params) = ical_datetime(&string_arg(args, "end")?, all_day)?;
        let description = opt_string_arg(args, "description")?;
        let location = opt_string_arg(args, "location")?;

        // El payload de `PutEvent`: las claves son las del contrato de cuentas.
        // Sin `id` (crear). `start`/`end` van como `{value, params}`, que el
        // almacén pone tal cual en la línea iCalendar. Los textos opcionales sólo
        // se incluyen si vienen: ausente no toca, y en uno nuevo eso es no fijarlo.
        let mut payload = json!({
            "summary": summary,
            "start": {"value": start_value, "params": start_params},
            "end": {"value": end_value, "params": end_params},
        });
        let map = payload.as_object_mut().expect("el payload es un objeto");
        if !description.is_empty() {
            map.insert("description".into(), json!(description));
        }
        if !location.is_empty() {
            map.insert("location".into(), json!(location));
        }
        let payload = payload.to_string();

        let action = PendingAction {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: ctx.conversation_id.clone(),
            kind: "create_event".into(),
            summary: format!("Crear el evento «{summary}» en el calendario «{calendar_id}»"),
            // Campos de correo: vacíos para una acción de calendario. El resumen
            // lo dice todo; la vista de confirmación rica es otra rebanada.
            to: String::new(),
            subject: String::new(),
            body_preview: String::new(),
        };

        let connection = self.connection.clone();
        let execute = Box::new(move || -> BoxFuture<'static, Result<(), Error>> {
            Box::pin(async move {
                let proxy = store_proxy(&connection).await?;
                // `if_match` vacío: crear. Devuelve `{event_id}`, que no se usa.
                let _created: String = call_with_timeout(
                    CALL_TIMEOUT,
                    proxy.call(
                        "PutEvent",
                        &(account_id, calendar_id, payload, String::new()),
                    ),
                )
                .await?;
                Ok(())
            })
        });

        Ok(ToolOutcome::Pending { action, execute })
    }
}

/// Convierte una fecha que da el modelo al `(value, params)` iCalendar que espera
/// el almacén (ambos van tal cual a la línea). Un evento de día entero toma una
/// fecha `YYYY-MM-DD` → `YYYYMMDD` con `VALUE=DATE`; uno con hora, una marca
/// RFC 3339 en UTC (`...Z`) → la forma básica `YYYYMMDDTHHMMSSZ`. Se exige UTC:
/// una hora con huso (`+01:00`) necesitaría un `TZID`, que es otra rebanada, y se
/// rechaza para no guardar una hora ambigua. Una fecha mal formada da
/// `InvalidArgs` para que el modelo la corrija, nunca un valor inventado.
fn ical_datetime(input: &str, all_day: bool) -> Result<(String, Vec<String>), Error> {
    let input = input.trim();
    if all_day {
        // `YYYY-MM-DD`: diez caracteres, sólo dígitos y guiones, ocho dígitos.
        let digits: String = input.chars().filter(char::is_ascii_digit).collect();
        let only_date = input.chars().all(|c| c.is_ascii_digit() || c == '-');
        if digits.len() != 8 || !only_date {
            return Err(Error::InvalidArgs(format!(
                "la fecha de un evento de día entero es «YYYY-MM-DD»: «{input}»"
            )));
        }
        return Ok((digits, vec!["VALUE=DATE".into()]));
    }
    // Con hora: RFC 3339 en UTC. Se recorta la fracción de segundo si viene.
    let Some(body) = input.strip_suffix('Z').or_else(|| input.strip_suffix('z')) else {
        return Err(Error::InvalidArgs(format!(
            "la hora tiene que estar en UTC y terminar en «Z»: «{input}»"
        )));
    };
    let body = body.split('.').next().unwrap_or(body);
    let compact: String = body
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();
    // `YYYYMMDD` + `T` + `HHMMSS` = 15, con la `T` en el medio.
    if compact.len() != 15 || compact.as_bytes()[8] != b'T' {
        return Err(Error::InvalidArgs(format!(
            "la hora es una marca RFC 3339 en UTC, p. ej. «2026-01-01T09:00:00Z»: «{input}»"
        )));
    }
    Ok((format!("{compact}Z"), Vec::new()))
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
