//! Pruebas de la habilidad de calendario: los argumentos y el cable hacia
//! `AccountsStore`, contra un almacén de mentira servido en una conexión punto
//! a punto (sin bus).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::*;
use crate::test_support::p2p_pair_as;

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

fn ctx() -> SkillCtx {
    SkillCtx {
        conversation_id: "conv-1".into(),
    }
}

async fn run(skill: &CalendarSkill, name: &str, args: Value) -> Result<String, Error> {
    match skill.call(name, &args, &ctx()).await? {
        ToolOutcome::Result(text) => Ok(text),
        ToolOutcome::Pending { .. } => panic!("el calendario no deja acciones pendientes"),
    }
}

#[test]
fn los_argumentos_faltantes_o_mal_tipados_son_invalid_args() {
    let args = json!({"s": "x", "n": 5, "ids": ["a", "b"], "mal": [1], "no_lista": "a", "nulo": null, "b": true});
    assert_eq!(string_arg(&args, "s").unwrap(), "x");
    assert!(matches!(
        string_arg(&args, "falta"),
        Err(Error::InvalidArgs(_))
    ));
    assert!(matches!(string_arg(&args, "n"), Err(Error::InvalidArgs(_))));
    assert_eq!(opt_string_arg(&args, "s"), "x");
    assert_eq!(opt_string_arg(&args, "n"), "");
    assert_eq!(opt_string_arg(&args, "falta"), "");
    assert!(bool_arg(&args, "b", false));
    assert!(bool_arg(&args, "falta", true));
    assert!(!bool_arg(&args, "s", false));
    assert_eq!(string_array_arg(&args, "ids").unwrap(), ["a", "b"]);
    assert!(string_array_arg(&args, "falta").unwrap().is_empty());
    assert!(string_array_arg(&args, "nulo").unwrap().is_empty());
    assert!(matches!(
        string_array_arg(&args, "mal"),
        Err(Error::InvalidArgs(m)) if m.contains("elemento")
    ));
    assert!(matches!(
        string_array_arg(&args, "no_lista"),
        Err(Error::InvalidArgs(m)) if m.contains("lista")
    ));
}

#[tokio::test]
async fn una_llamada_que_no_responde_a_tiempo_da_error_de_backend_en_vez_de_colgarse() {
    let colgada = std::future::pending::<zbus::Result<String>>();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        call_with_timeout(Duration::from_millis(20), colgada),
    )
    .await
    .expect("la llamada no puede quedarse esperando para siempre");
    assert!(
        matches!(&result, Err(Error::Backend(m)) if m.contains("a tiempo")),
        "{result:?}"
    );
    let ok = call_with_timeout(Duration::from_secs(5), async { Ok::<_, zbus::Error>(7) }).await;
    assert_eq!(ok.unwrap(), 7);
}

#[test]
fn el_error_del_bus_dice_que_el_servicio_de_cuentas_no_respondio() {
    let error = backend_error("boom");
    assert!(matches!(&error, Error::Backend(m) if m.contains("boom") && m.contains("cuentas")));
}

#[tokio::test]
async fn la_habilidad_ofrece_las_tres_herramientas_de_solo_lectura_con_su_esquema() {
    let (skill, _log, _server) = store().await;
    assert_eq!(skill.domain(), DOMAIN_CALENDAR);
    assert_eq!(skill.permission(), PERMISSION_CALENDAR);
    let tools = skill.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["list_events", "read_event", "list_tasks"]);
    for tool in &tools {
        assert_eq!(tool.parameters["type"], "object", "{}", tool.name);
    }
    assert_eq!(tools[0].parameters["required"], json!(["from", "to"]));
    assert_eq!(tools[1].parameters["required"], json!(["event_id"]));
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
    let result = run(&skill, "create_event", json!({})).await;
    assert!(matches!(result, Err(Error::InvalidArgs(m)) if m.contains("create_event")));
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
