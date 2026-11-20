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
    bool_arg, call_with_timeout, opt_string_arg, store_alive, store_create, store_proxy,
    string_arg, string_array_arg, CALL_TIMEOUT,
};
use super::{tool, Skill, SkillCtx, ToolOutcome, PERMISSION_CALENDAR};
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
            tool(
                "list_calendars",
                "Lista los calendarios de todas las cuentas. Devuelve cada uno con su id, la \
                 cuenta («account_id»), el nombre y qué objetos admite («components»: VEVENT para \
                 eventos, VTODO para tareas). El id y la cuenta son los que toma «create_event».",
                json!({}),
                &[],
            ),
            tool(
                "list_events",
                "Lista las veces de los eventos del calendario en un rango de fechas. Devuelve \
                 metadatos (título, comienzo, fin, calendario, id del evento y de la vez).",
                json!({
                    "from": {"type": "string", "description": "El comienzo del rango, en ISO-8601 (RFC 3339)."},
                    "to": {"type": "string", "description": "El fin del rango, en ISO-8601 (RFC 3339)."},
                    "calendar_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Los calendarios a mirar; vacío u omitido, todos."
                    }
                }),
                &["from", "to"],
            ),
            tool(
                "read_event",
                "Lee un evento entero por su id. Con «occurrence_id» lee esa vez puntual; sin él, \
                 el evento.",
                json!({
                    "event_id": {"type": "string", "description": "El id del evento."},
                    "occurrence_id": {"type": "string", "description": "La vez puntual; omitido, el evento."}
                }),
                &["event_id"],
            ),
            tool(
                "list_tasks",
                "Lista las tareas del calendario, por vencimiento. Sin las hechas salvo con \
                 «include_completed».",
                json!({
                    "calendar_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Los calendarios a mirar; vacío u omitido, todos."
                    },
                    "include_completed": {"type": "boolean", "description": "Incluir las tareas ya hechas."}
                }),
                &[],
            ),
            tool(
                "create_event",
                "Prepara un evento nuevo en un calendario. No lo crea: la persona lo confirma \
                 antes. El calendario se nombra con «account_id» y «calendar_id», de \
                 list_calendars (uno que admita VEVENT).",
                json!({
                    "account_id": {"type": "string", "description": "La cuenta del calendario, de list_calendars."},
                    "calendar_id": {"type": "string", "description": "El id del calendario, de list_calendars."},
                    "summary": {"type": "string", "description": "El título del evento."},
                    "start": {"type": "string", "description": "El comienzo. Con hora, una marca RFC 3339 en UTC («2026-01-01T09:00:00Z»); de día entero, una fecha «2026-01-01» con «all_day» en true."},
                    "end": {"type": "string", "description": "El fin, en el mismo formato que «start»."},
                    "all_day": {"type": "boolean", "description": "Si es un evento de día entero (fechas sin hora)."},
                    "description": {"type": "string", "description": "El detalle del evento; opcional."},
                    "location": {"type": "string", "description": "El lugar; opcional."}
                }),
                &["account_id", "calendar_id", "summary", "start", "end"],
            ),
            tool(
                "create_task",
                "Prepara una tarea nueva en un calendario. No la crea: la persona la confirma \
                 antes. El calendario se nombra con «account_id» y «calendar_id», de \
                 list_calendars (uno que admita VTODO).",
                json!({
                    "account_id": {"type": "string", "description": "La cuenta del calendario, de list_calendars."},
                    "calendar_id": {"type": "string", "description": "El id del calendario, de list_calendars."},
                    "summary": {"type": "string", "description": "El título de la tarea."},
                    "description": {"type": "string", "description": "El detalle de la tarea; opcional."},
                    "due": {"type": "string", "description": "El vencimiento; opcional. Con hora, una marca RFC 3339 en UTC («2026-01-01T09:00:00Z»); de día entero, una fecha «2026-01-01» con «all_day» en true."},
                    "all_day": {"type": "boolean", "description": "Si el vencimiento es una fecha sin hora."},
                    "status": {"type": "string", "description": "Estado; opcional: NEEDS-ACTION, IN-PROCESS, COMPLETED o CANCELLED."},
                    "priority": {"type": "integer", "description": "Prioridad 0–9; opcional (0 es sin prioridad)."}
                }),
                &["account_id", "calendar_id", "summary"],
            ),
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
                "create_task" => self.create_task(args, ctx).await,
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

        let action = PendingAction::simple(
            uuid::Uuid::new_v4().to_string(),
            ctx.conversation_id.clone(),
            "create_event",
            format!("Crear el evento «{summary}» en el calendario «{calendar_id}»"),
        );
        // `if_match` vacío: crear. Se ejecuta recién al confirmar.
        let execute = store_create(
            self.connection.clone(),
            "PutEvent",
            (account_id, calendar_id, payload, String::new()),
        );
        Ok(ToolOutcome::Pending { action, execute })
    }

    /// Prepara una tarea para confirmar. Como [`Self::create_event`], pero contra
    /// `PutTask`: valida los argumentos y, si cuadran, devuelve una acción
    /// pendiente cuyo `execute` crea la tarea (con `if_match` vacío) al confirmar.
    async fn create_task(&self, args: &Value, ctx: &SkillCtx) -> Result<ToolOutcome, Error> {
        let account_id = string_arg(args, "account_id")?;
        let calendar_id = string_arg(args, "calendar_id")?;
        let summary = string_arg(args, "summary")?;
        let description = opt_string_arg(args, "description")?;

        // El payload de `PutTask`. Sin `id` (crear). Los opcionales sólo se
        // incluyen si vienen: ausente no toca, y en una nueva eso es no fijarlo.
        let mut payload = json!({ "summary": summary });
        let map = payload.as_object_mut().expect("el payload es un objeto");
        if !description.is_empty() {
            map.insert("description".into(), json!(description));
        }
        // El vencimiento, si viene, va como `{value, params}` igual que las fechas
        // de un evento.
        let due = opt_string_arg(args, "due")?;
        if !due.is_empty() {
            let all_day = bool_arg(args, "all_day", false)?;
            let (value, params) = ical_datetime(&due, all_day)?;
            map.insert("due".into(), json!({"value": value, "params": params}));
        }
        if let Some(status) = opt_status(args)? {
            map.insert("status".into(), json!(status));
        }
        if let Some(priority) = opt_priority(args)? {
            map.insert("priority".into(), json!(priority));
        }
        let payload = payload.to_string();

        let action = PendingAction::simple(
            uuid::Uuid::new_v4().to_string(),
            ctx.conversation_id.clone(),
            "create_task",
            format!("Crear la tarea «{summary}» en el calendario «{calendar_id}»"),
        );
        let execute = store_create(
            self.connection.clone(),
            "PutTask",
            (account_id, calendar_id, payload, String::new()),
        );
        Ok(ToolOutcome::Pending { action, execute })
    }
}

/// Los estados que acepta una tarea (`VTODO`), tal como los nombra el contrato de
/// cuentas.
const TASK_STATUSES: [&str; 4] = ["NEEDS-ACTION", "IN-PROCESS", "COMPLETED", "CANCELLED"];

/// El `status` de una tarea, si vino: tiene que ser uno de [`TASK_STATUSES`].
/// `None` si falta; `InvalidArgs` si es un valor desconocido —el modelo lo
/// corrige, no se manda algo que el almacén rechazaría—.
fn opt_status(args: &Value) -> Result<Option<String>, Error> {
    let status = opt_string_arg(args, "status")?;
    if status.is_empty() {
        return Ok(None);
    }
    if !TASK_STATUSES.contains(&status.as_str()) {
        return Err(Error::InvalidArgs(format!(
            "«status» es uno de {TASK_STATUSES:?}: «{status}»"
        )));
    }
    Ok(Some(status))
}

/// La `priority` de una tarea, si vino: un entero de 0 a 9. `None` si falta;
/// `InvalidArgs` si no es un entero en rango.
fn opt_priority(args: &Value) -> Result<Option<i64>, Error> {
    match args.get("priority") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_i64() {
            Some(priority) if (0..=9).contains(&priority) => Ok(Some(priority)),
            _ => Err(Error::InvalidArgs(
                "«priority» es un entero de 0 a 9".into(),
            )),
        },
    }
}

/// Convierte una fecha que da el modelo al `(value, params)` iCalendar que espera
/// el almacén (ambos van tal cual a la línea). Un evento de día entero toma una
/// fecha `YYYY-MM-DD` → `YYYYMMDD` con `VALUE=DATE`; uno con hora, una marca
/// RFC 3339 en UTC (`...Z`) → la forma básica `YYYYMMDDTHHMMSSZ`. Se exige UTC:
/// una hora con huso (`+01:00`) necesitaría un `TZID`, que es otra rebanada, y se
/// rechaza para no guardar una hora ambigua.
///
/// La fecha se parsea entera y se validan los campos reales (mes 1–12, día del
/// mes con años bisiestos, hora/minuto/segundo en rango): una fecha imposible
/// (`2026-02-30`), un separador fuera de lugar (`2026--2-01`) o un carácter de
/// relleno (`2026-12-01T09x00x00Z`) dan `InvalidArgs`, nunca un valor inventado.
fn ical_datetime(input: &str, all_day: bool) -> Result<(String, Vec<String>), Error> {
    let input = input.trim();
    if all_day {
        let (y, m, d) = parse_date(input).ok_or_else(|| {
            Error::InvalidArgs(format!(
                "la fecha de un evento de día entero es «YYYY-MM-DD»: «{input}»"
            ))
        })?;
        return Ok((format!("{y:04}{m:02}{d:02}"), vec!["VALUE=DATE".into()]));
    }
    // Con hora: RFC 3339 en UTC, `YYYY-MM-DDTHH:MM:SS` con una fracción de
    // segundo opcional, y la `Z` final.
    let invalid = || {
        Error::InvalidArgs(format!(
            "la hora es una marca RFC 3339 en UTC, p. ej. «2026-01-01T09:00:00Z»: «{input}»"
        ))
    };
    let body = input
        .strip_suffix('Z')
        .or_else(|| input.strip_suffix('z'))
        .ok_or_else(invalid)?;
    // Se recorta la fracción de segundo si viene; tiene que ser dígitos.
    let clock = match body.split_once('.') {
        Some((clock, frac)) if !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit()) => {
            clock
        }
        Some(_) => return Err(invalid()),
        None => body,
    };
    let (date, time) = clock.split_once('T').ok_or_else(invalid)?;
    let (y, m, d) = parse_date(date).ok_or_else(invalid)?;
    let (hh, mm, ss) = parse_time(time).ok_or_else(invalid)?;
    Ok((
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        Vec::new(),
    ))
}

/// Parsea un `YYYY-MM-DD` exacto y valida los campos (mes 1–12, día del mes con
/// bisiestos). `None` si la forma o la fecha no son válidas.
fn parse_date(s: &str) -> Option<(u16, u8, u8)> {
    let b = s.as_bytes();
    if s.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: u16 = two_or_four_digits(&s[0..4])?;
    let m: u8 = two_or_four_digits(&s[5..7])? as u8;
    let d: u8 = two_or_four_digits(&s[8..10])? as u8;
    if m < 1 || m > 12 || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

/// Parsea un `HH:MM:SS` exacto y valida los campos (hora 0–23, minuto 0–59,
/// segundo 0–60: el 60 es el segundo intercalar de RFC 3339). `None` si no.
fn parse_time(s: &str) -> Option<(u8, u8, u8)> {
    let b = s.as_bytes();
    if s.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let hh = two_or_four_digits(&s[0..2])? as u8;
    let mm = two_or_four_digits(&s[3..5])? as u8;
    let ss = two_or_four_digits(&s[6..8])? as u8;
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some((hh, mm, ss))
}

/// Un grupo de dígitos ASCII a número. Sólo dígitos: `str::parse` aceptaría un
/// signo (`+026`), que no es una fecha. `None` si hay algo que no es un dígito.
fn two_or_four_digits(s: &str) -> Option<u16> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Cuántos días tiene el mes `m` del año `y` (con bisiestos). `m` fuera de 1–12
/// da 0, que rechaza cualquier día.
fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
