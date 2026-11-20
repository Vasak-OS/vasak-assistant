//! Pruebas de la habilidad de calendario: los argumentos y el cable hacia
//! `AccountsStore`, contra un almacén de mentira servido en una conexión punto
//! a punto (sin bus).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::*;
use crate::skills::dbus::STORE_PATH;
use crate::test_support::p2p_pair_as;
use vasak_assistant_protocol::PendingAction;

type Log = Arc<Mutex<Vec<String>>>;

struct FakeStore {
    log: Log,
}

#[zbus::interface(name = "ar.net.vasak.os.AccountsStore")]
impl FakeStore {
    async fn get_status(&self) -> String {
        "{}".into()
    }

    async fn list_occurrences(
        &self,
        from: &str,
        to: &str,
        calendar_ids: Vec<String>,
        cursor: &str,
        limit: u32,
    ) -> String {
        self.log.lock().push(format!(
            "occ {from} {to} {calendar_ids:?} [{cursor}] {limit}"
        ));
        "[{\"evento\":1}]".into()
    }

    async fn list_calendars(&self) -> String {
        self.log.lock().push("calendars".into());
        "[{\"calendario\":1}]".into()
    }

    async fn put_event(
        &self,
        account_id: &str,
        calendar_id: &str,
        payload: &str,
        if_match: &str,
    ) -> String {
        self.log.lock().push(format!(
            "put {account_id} {calendar_id} {payload} [{if_match}]"
        ));
        "{\"event_id\":\"e1\"}".into()
    }

    async fn get_event(&self, event_id: &str, occurrence_id: &str) -> String {
        self.log
            .lock()
            .push(format!("event {event_id} [{occurrence_id}]"));
        "{\"titulo\":\"Reunión\"}".into()
    }

    async fn list_tasks(
        &self,
        calendar_ids: Vec<String>,
        include_completed: bool,
        cursor: &str,
        limit: u32,
    ) -> String {
        self.log.lock().push(format!(
            "tasks {calendar_ids:?} {include_completed} [{cursor}] {limit}"
        ));
        "[{\"tarea\":1}]".into()
    }
}

async fn store() -> (CalendarSkill, Log, zbus::Connection) {
    let log = Log::default();
    let fake = FakeStore { log: log.clone() };
    let (server, client) = p2p_pair_as(":1.5", |builder| builder.serve_at(STORE_PATH, fake))
        .await
        .unwrap();
    (CalendarSkill::new(client), log, server)
}

async fn run(skill: &CalendarSkill, name: &str, args: Value) -> Result<String, Error> {
    crate::test_support::tool_result(skill, name, args).await
}

/// Llama a `create_event` y devuelve la acción pendiente con su ejecución.
async fn prepare(
    skill: &CalendarSkill,
    args: Value,
) -> Result<(PendingAction, crate::skills::SendAction), Error> {
    crate::test_support::tool_pending(skill, "create_event", args).await
}

fn event_args() -> Value {
    json!({
        "account_id": "a1", "calendar_id": "c1", "summary": "Reunión",
        "start": "2026-12-01T09:00:00Z", "end": "2026-12-01T10:00:00Z"
    })
}

/// Crea el evento, lo confirma y devuelve el payload que llegó a `PutEvent`.
async fn put_payload(skill: &CalendarSkill, log: &Log, args: Value) -> Value {
    let (_action, execute) = prepare(skill, args).await.unwrap();
    execute().await.unwrap();
    let line = log.lock().last().cloned().unwrap();
    let json = line
        .strip_prefix("put a1 c1 ")
        .and_then(|rest| rest.strip_suffix(" []"))
        .unwrap_or_else(|| panic!("PutEvent mal llamado: {line}"));
    serde_json::from_str(json).unwrap()
}

#[tokio::test]
async fn la_habilidad_ofrece_sus_herramientas_con_su_esquema() {
    let (skill, _log, _server) = store().await;
    assert_eq!(skill.domain(), DOMAIN_CALENDAR);
    assert_eq!(skill.permission(), PERMISSION_CALENDAR);
    let tools = skill.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "list_calendars",
            "list_events",
            "read_event",
            "list_tasks",
            "create_event"
        ]
    );
    for tool in &tools {
        assert_eq!(tool.parameters["type"], "object", "{}", tool.name);
    }
    assert_eq!(tools[1].parameters["required"], json!(["from", "to"]));
    assert_eq!(tools[2].parameters["required"], json!(["event_id"]));
    assert_eq!(
        tools[4].parameters["required"],
        json!(["account_id", "calendar_id", "summary", "start", "end"])
    );
}

#[tokio::test]
async fn el_probe_es_verdadero_si_el_almacen_responde_y_falso_si_no() {
    let (skill, _log, _server) = store().await;
    assert!(skill.probe().await);
    // Sin el objeto del almacén del otro lado, la sonda da falso: no rota.
    let (server, client) = p2p_pair_as(":1.5", |builder| {
        builder.serve_at("/otro/objeto", FakeOther)
    })
    .await
    .unwrap();
    assert!(!CalendarSkill::new(client).probe().await);
    drop(server);
}

struct FakeOther;

#[zbus::interface(name = "ar.net.vasak.os.Otra")]
impl FakeOther {
    async fn nada(&self) -> String {
        String::new()
    }
}

#[tokio::test]
async fn list_events_consulta_ocurrencias_con_rango_calendarios_y_tope() {
    let (skill, log, _server) = store().await;
    let args = json!({"from": "2026-11-01T00:00:00Z", "to": "2026-11-30T00:00:00Z", "calendar_ids": ["c1", "c2"]});
    assert_eq!(
        run(&skill, "list_events", args).await.unwrap(),
        "[{\"evento\":1}]"
    );
    assert_eq!(
        log.lock().as_slice(),
        ["occ 2026-11-01T00:00:00Z 2026-11-30T00:00:00Z [\"c1\", \"c2\"] [] 100"]
    );
    // Sin calendar_ids son todos: la lista va vacía.
    log.lock().clear();
    run(&skill, "list_events", json!({"from": "a", "to": "b"}))
        .await
        .unwrap();
    assert_eq!(log.lock().as_slice(), ["occ a b [] [] 100"]);
}

#[tokio::test]
async fn read_event_pide_el_evento_con_o_sin_vez_puntual() {
    let (skill, log, _server) = store().await;
    let ok = run(&skill, "read_event", json!({"event_id": "e1"}))
        .await
        .unwrap();
    assert_eq!(ok, "{\"titulo\":\"Reunión\"}");
    run(
        &skill,
        "read_event",
        json!({"event_id": "e1", "occurrence_id": "o9"}),
    )
    .await
    .unwrap();
    assert_eq!(log.lock().as_slice(), ["event e1 []", "event e1 [o9]"]);
}

#[tokio::test]
async fn list_tasks_pide_las_pendientes_salvo_con_include_completed() {
    let (skill, log, _server) = store().await;
    assert_eq!(
        run(&skill, "list_tasks", json!({})).await.unwrap(),
        "[{\"tarea\":1}]"
    );
    run(
        &skill,
        "list_tasks",
        json!({"calendar_ids": ["c1"], "include_completed": true}),
    )
    .await
    .unwrap();
    assert_eq!(
        log.lock().as_slice(),
        ["tasks [] false [] 100", "tasks [\"c1\"] true [] 100"]
    );
}

#[tokio::test]
async fn los_argumentos_invalidos_dan_invalid_args_y_no_tocan_el_almacen() {
    let (skill, log, _server) = store().await;
    for (name, args) in [
        ("list_events", json!({"from": "a"})),
        ("list_events", json!({"from": "a", "to": 5})),
        (
            "list_events",
            json!({"from": "a", "to": "b", "calendar_ids": "c1"}),
        ),
        ("read_event", json!({})),
        // Un número no es «el evento entero»: no se lee la serie.
        ("read_event", json!({"event_id": "e1", "occurrence_id": 5})),
        // El texto «true» no es el booleano: no se lista sin las hechas.
        ("list_tasks", json!({"include_completed": "true"})),
        ("list_tasks", json!({"calendar_ids": [1]})),
    ] {
        let result = run(&skill, name, args.clone()).await;
        assert!(
            matches!(result, Err(Error::InvalidArgs(_))),
            "{name} {args}"
        );
    }
    assert!(log.lock().is_empty());
}

#[tokio::test]
async fn una_herramienta_desconocida_es_invalid_args() {
    let (skill, _log, _server) = store().await;
    let result = run(&skill, "delete_event", json!({})).await;
    assert!(matches!(result, Err(Error::InvalidArgs(m)) if m.contains("delete_event")));
}

#[tokio::test]
async fn si_el_almacen_no_esta_la_lectura_da_error_de_backend() {
    let (server, client) = p2p_pair_as(":1.5", |builder| {
        builder.serve_at("/otro/objeto", FakeOther)
    })
    .await
    .unwrap();
    let skill = CalendarSkill::new(client);
    let result = run(&skill, "list_tasks", json!({})).await;
    assert!(matches!(result, Err(Error::Backend(_))), "{result:?}");
    drop(server);
}

#[tokio::test]
async fn list_calendars_consulta_los_calendarios_y_pasa_el_json() {
    let (skill, log, _server) = store().await;
    assert_eq!(
        run(&skill, "list_calendars", json!({})).await.unwrap(),
        "[{\"calendario\":1}]"
    );
    assert_eq!(log.lock().as_slice(), ["calendars"]);
}

#[tokio::test]
async fn create_event_prepara_una_accion_y_no_escribe_hasta_confirmar() {
    let (skill, log, _server) = store().await;
    let (action, execute) = prepare(&skill, event_args()).await.unwrap();
    assert_eq!(action.kind, "create_event");
    assert_eq!(action.conversation_id, "conv-1");
    assert!(action.summary.contains("Reunión"), "{}", action.summary);
    assert!(log.lock().is_empty(), "preparar no debe llamar a PutEvent");
    execute().await.unwrap();
    assert_eq!(log.lock().len(), 1);
}

#[tokio::test]
async fn al_confirmar_el_evento_se_crea_con_if_match_vacio_y_el_payload_correcto() {
    let (skill, log, _server) = store().await;
    let (_action, execute) = prepare(&skill, event_args()).await.unwrap();
    execute().await.unwrap();
    let expected = json!({
        "summary": "Reunión",
        "start": {"value": "20261201T090000Z", "params": []},
        "end": {"value": "20261201T100000Z", "params": []},
    });
    assert_eq!(log.lock().as_slice(), [format!("put a1 c1 {expected} []")]);
}

#[tokio::test]
async fn description_y_location_van_al_payload_solo_si_vienen_no_vacios() {
    let (skill, log, _server) = store().await;
    let mut args = event_args();
    args["description"] = json!("Revisar el plan");
    args["location"] = json!("Sala 2");
    let payload = put_payload(&skill, &log, args).await;
    assert_eq!(payload["description"], "Revisar el plan");
    assert_eq!(payload["location"], "Sala 2");

    let mut args = event_args();
    args["description"] = json!("");
    let payload = put_payload(&skill, &log, args).await;
    assert!(payload.get("description").is_none(), "{payload}");
    assert!(payload.get("location").is_none(), "{payload}");
}

#[tokio::test]
async fn un_evento_de_dia_entero_va_como_fecha_con_value_date() {
    let (skill, log, _server) = store().await;
    let mut args = event_args();
    args["start"] = json!("2026-12-24");
    args["end"] = json!("2026-12-25");
    args["all_day"] = json!(true);
    let payload = put_payload(&skill, &log, args).await;
    assert_eq!(
        payload["start"],
        json!({"value": "20261224", "params": ["VALUE=DATE"]})
    );
    assert_eq!(
        payload["end"],
        json!({"value": "20261225", "params": ["VALUE=DATE"]})
    );
}

#[tokio::test]
async fn un_dia_bisiesto_valido_se_acepta() {
    let (skill, log, _server) = store().await;
    let mut args = event_args();
    // 2028 es bisiesto: el 29 de febrero existe y no se rechaza.
    args["start"] = json!("2028-02-29");
    args["end"] = json!("2028-03-01");
    args["all_day"] = json!(true);
    let payload = put_payload(&skill, &log, args).await;
    assert_eq!(payload["start"]["value"], "20280229");
}

#[tokio::test]
async fn la_fraccion_de_segundo_de_la_hora_utc_se_recorta() {
    let (skill, log, _server) = store().await;
    let mut args = event_args();
    args["start"] = json!("2026-12-01T09:00:00.123Z");
    let payload = put_payload(&skill, &log, args).await;
    assert_eq!(payload["start"]["value"], "20261201T090000Z");
}

#[tokio::test]
async fn una_fecha_invalida_en_create_event_es_invalid_args_y_no_deja_accion() {
    let (skill, log, _server) = store().await;
    let casos = [
        // Hora con huso en vez de UTC.
        ("start", json!("2026-12-01T09:00:00+01:00"), false),
        // Hora sin huso.
        ("end", json!("2026-12-01T10:00:00"), false),
        // Hora mal formada.
        ("start", json!("mañana a las nueve Z"), false),
        // Relleno en vez de los dos puntos: no se compacta a una hora válida.
        ("start", json!("2026-12-01T09x00x00Z"), false),
        // Fracción de segundo que no es numérica.
        ("start", json!("2026-12-01T09:00:00.xyzZ"), false),
        // Hora fuera de rango.
        ("start", json!("2026-12-01T29:00:00Z"), false),
        // Minuto fuera de rango.
        ("end", json!("2026-12-01T10:70:00Z"), false),
        // Día entero con una hora, o con la fecha rota.
        ("start", json!("2026-12-01T09:00:00Z"), true),
        ("end", json!("2026-12"), true),
        // Día entero imposible (febrero no tiene 30).
        ("start", json!("2026-02-30"), true),
        // Día entero con un separador fuera de lugar.
        ("end", json!("2026--2-01"), true),
        // Día entero no bisiesto: 2026 no tiene 29 de febrero.
        ("start", json!("2026-02-29"), true),
    ];
    for (campo, valor, all_day) in casos {
        let mut args = event_args();
        args[campo] = valor.clone();
        if all_day {
            args["all_day"] = json!(true);
        }
        let result = prepare(&skill, args).await;
        assert!(
            matches!(result, Err(Error::InvalidArgs(_))),
            "{campo}={valor} all_day={all_day}"
        );
    }
    assert!(log.lock().is_empty());
}

#[tokio::test]
async fn create_event_sin_argumentos_obligatorios_es_invalid_args() {
    let (skill, _log, _server) = store().await;
    for campo in ["account_id", "calendar_id", "summary", "start", "end"] {
        let mut args = event_args();
        args.as_object_mut().unwrap().remove(campo);
        let result = prepare(&skill, args).await;
        assert!(matches!(result, Err(Error::InvalidArgs(_))), "{campo}");
    }
}
