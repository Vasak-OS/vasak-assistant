//! Pruebas de la habilidad de correo: los argumentos, el recorte del cuerpo y
//! el cable hacia `AccountsStore`/`AccountsSync`/`Permissions`, contra
//! servicios de mentira servidos en una conexión punto a punto (sin bus).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::*;
use crate::test_support::p2p_pair_as;

#[test]
fn los_argumentos_faltantes_o_mal_tipados_son_invalid_args() {
    let args = json!({"s": "x", "n": 5, "txt": "no"});
    assert_eq!(string_arg(&args, "s").unwrap(), "x");
    assert!(matches!(
        string_arg(&args, "falta"),
        Err(Error::InvalidArgs(_))
    ));
    assert!(matches!(string_arg(&args, "n"), Err(Error::InvalidArgs(_))));
    assert_eq!(int_arg(&args, "n").unwrap(), 5);
    assert!(matches!(int_arg(&args, "txt"), Err(Error::InvalidArgs(_))));
    assert!(matches!(
        int_arg(&args, "falta"),
        Err(Error::InvalidArgs(_))
    ));
}

#[test]
fn el_recorte_del_cuerpo_respeta_el_limite_y_los_caracteres() {
    assert_eq!(preview("corto"), "corto");
    let recorte = preview(&"a".repeat(500));
    assert!(recorte.ends_with('…'));
    assert_eq!(recorte.len(), BODY_PREVIEW_BYTES + '…'.len_utf8());
    // Un carácter de dos bytes cruzando el límite no parte el texto.
    let recorte = preview(&"é".repeat(200));
    assert!(recorte.ends_with('…'));
    assert!(recorte.trim_end_matches('…').chars().all(|c| c == 'é'));
}

#[test]
fn el_error_del_bus_dice_que_el_servicio_de_cuentas_no_respondio() {
    let error = backend_error("boom");
    assert!(matches!(&error, Error::Backend(m) if m.contains("boom") && m.contains("cuentas")));
}

// ── Servicios de mentira ────────────────────────────────────────────────────────

type Log = Arc<Mutex<Vec<String>>>;

struct FakeStore {
    log: Log,
}

#[zbus::interface(name = "ar.net.vasak.os.AccountsStore")]
impl FakeStore {
    async fn get_status(&self) -> String {
        "{}".into()
    }

    async fn search_messages(
        &self,
        account: &str,
        query: &str,
        mailbox: i64,
        cursor: &str,
        limit: u32,
    ) -> String {
        self.log.lock().push(format!(
            "search {account} {query} {mailbox} [{cursor}] {limit}"
        ));
        "[{\"id\":1}]".into()
    }

    async fn get_message(&self, account: &str, id: i64) -> String {
        self.log.lock().push(format!("get {account} {id}"));
        "{\"cuerpo\":\"hola\"}".into()
    }
}

struct FakeSync {
    log: Log,
}

#[zbus::interface(name = "ar.net.vasak.os.AccountsSync")]
impl FakeSync {
    async fn send_message(&self, account: &str, draft: &str, not_before: &str) -> String {
        self.log
            .lock()
            .push(format!("send {account} {draft} [{not_before}]"));
        "queued-1".into()
    }
}

struct FakePermissions {
    grant: bool,
    log: Log,
}

#[zbus::interface(name = "ar.net.vasak.os.Permissions")]
impl FakePermissions {
    async fn check_permission(&self, resource: &str, detail: &str) -> bool {
        self.log.lock().push(format!("perm {resource} {detail}"));
        self.grant
    }
}

async fn accounts() -> (MailSkill, Log, zbus::Connection) {
    let log = Log::default();
    let (store, sync) = (
        FakeStore { log: log.clone() },
        FakeSync { log: log.clone() },
    );
    let (server, client) = p2p_pair_as(":1.5", |builder| {
        builder
            .serve_at(STORE_PATH, store)?
            .serve_at(SYNC_PATH, sync)
    })
    .await
    .unwrap();
    (MailSkill::new(client), log, server)
}

/// Un par donde el otro lado sirve un objeto cualquiera pero ninguno de los de
/// cuentas ni permisos: contesta "objeto desconocido" en vez de colgarse.
async fn silent_peer(sender: &str) -> (zbus::Connection, zbus::Connection) {
    let service = FakePermissions {
        grant: false,
        log: Log::default(),
    };
    p2p_pair_as(sender, |builder| builder.serve_at("/otro/objeto", service))
        .await
        .unwrap()
}

fn ctx() -> SkillCtx {
    SkillCtx {
        conversation_id: "conv-1".into(),
    }
}

#[tokio::test]
async fn la_habilidad_ofrece_las_tres_herramientas_con_su_esquema() {
    let (skill, _log, _server) = accounts().await;
    assert_eq!(skill.domain(), DOMAIN_MAIL);
    assert_eq!(skill.permission(), PERMISSION_EMAIL);
    let tools = skill.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search_mail", "read_mail", "send_mail"]);
    for tool in &tools {
        assert_eq!(tool.parameters["type"], "object", "{}", tool.name);
        assert!(!tool.parameters["required"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn el_probe_es_verdadero_si_el_almacen_responde_y_falso_si_no() {
    let (skill, _log, _server) = accounts().await;
    assert!(skill.probe().await);
    // Sin ningún servicio del otro lado, la sonda da falso: no rota.
    let (server, client) = silent_peer(":1.5").await;
    let skill = MailSkill::new(client);
    assert!(!skill.probe().await);
    drop(server);
}

#[tokio::test]
async fn search_mail_consulta_el_almacen_con_cuenta_texto_y_tope() {
    let (skill, log, _server) = accounts().await;
    let outcome = skill
        .call(
            "search_mail",
            &json!({"account_id": "a1", "query": "factura"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ToolOutcome::Result(json) if json == "[{\"id\":1}]"));
    assert_eq!(
        *log.lock(),
        [format!("search a1 factura 0 [] {SEARCH_LIMIT}")]
    );
}

#[tokio::test]
async fn read_mail_pide_el_mensaje_por_su_id() {
    let (skill, log, _server) = accounts().await;
    let outcome = skill
        .call(
            "read_mail",
            &json!({"account_id": "a1", "message_id": 42}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ToolOutcome::Result(json) if json.contains("hola")));
    assert_eq!(*log.lock(), ["get a1 42"]);
}

#[tokio::test]
async fn las_lecturas_sin_argumentos_validos_fallan_sin_tocar_el_bus() {
    let (skill, log, _server) = accounts().await;
    for (tool, args) in [
        ("search_mail", json!({"account_id": "a1"})),
        ("read_mail", json!({"account_id": "a1", "message_id": "x"})),
        (
            "send_mail",
            json!({"account_id": "a1", "to": "a@b.c", "subject": "s"}),
        ),
    ] {
        let result = skill.call(tool, &args, &ctx()).await;
        assert!(matches!(result, Err(Error::InvalidArgs(_))), "{tool}");
    }
    assert!(log.lock().is_empty());
}

#[tokio::test]
async fn una_herramienta_desconocida_es_invalid_args() {
    let (skill, _log, _server) = accounts().await;
    let result = skill.call("borrar", &json!({}), &ctx()).await;
    assert!(matches!(result, Err(Error::InvalidArgs(_))));
}

#[tokio::test]
async fn send_mail_no_manda_nada_hasta_que_se_ejecuta_la_accion() {
    let (skill, log, _server) = accounts().await;
    let args = json!({
        "account_id": "a1",
        "to": " ana@x.org , bea@x.org,, ",
        "subject": "Hola",
        "body": "c".repeat(500),
    });
    let ToolOutcome::Pending { action, execute } =
        skill.call("send_mail", &args, &ctx()).await.unwrap()
    else {
        panic!("send_mail tiene que devolver una acción pendiente");
    };
    assert_eq!(action.kind, "send_mail");
    assert_eq!(action.conversation_id, "conv-1");
    assert_eq!(action.to, " ana@x.org , bea@x.org,, ");
    assert_eq!(action.subject, "Hola");
    assert!(
        action.body_preview.ends_with('…'),
        "el cuerpo viaja recortado"
    );
    assert!(uuid::Uuid::parse_str(&action.id).is_ok());
    assert!(log.lock().is_empty(), "todavía no se mandó nada");

    execute().await.unwrap();
    let sent = log.lock().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].starts_with("send a1 "), "{sent:?}");
    assert!(sent[0].ends_with(" []"), "{sent:?}");
    let draft: serde_json::Value = serde_json::from_str(
        sent[0]
            .trim_start_matches("send a1 ")
            .trim_end_matches(" []"),
    )
    .unwrap();
    // Destinatarios separados y sin vacíos; el cuerpo completo, no el recorte.
    assert_eq!(draft["para"], json!(["ana@x.org", "bea@x.org"]));
    assert_eq!(draft["asunto"], "Hola");
    assert_eq!(draft["cuerpo"].as_str().unwrap().len(), 500);
}

#[tokio::test]
async fn si_el_servicio_de_cuentas_no_esta_ejecutar_la_accion_falla_con_backend() {
    let (server, client) = silent_peer(":1.5").await;
    let skill = MailSkill::new(client);
    let args = json!({"account_id": "a1", "to": "a@b.c", "subject": "s", "body": "b"});
    let ToolOutcome::Pending { execute, .. } =
        skill.call("send_mail", &args, &ctx()).await.unwrap()
    else {
        panic!("pendiente");
    };
    assert!(matches!(execute().await, Err(Error::Backend(_))));
    let read = skill
        .call(
            "search_mail",
            &json!({"account_id": "a", "query": "q"}),
            &ctx(),
        )
        .await;
    assert!(matches!(read, Err(Error::Backend(_))));
    drop(server);
}

async fn permissions(grant: bool) -> (DbusPermission, Log, zbus::Connection) {
    let log = Log::default();
    let service = FakePermissions {
        grant,
        log: log.clone(),
    };
    let (server, client) = p2p_pair_as(":1.6", |builder| {
        builder.serve_at("/ar/net/vasak/os/Permissions", service)
    })
    .await
    .unwrap();
    (DbusPermission::new(client), log, server)
}

#[tokio::test]
async fn el_verificador_de_permisos_pregunta_a_vasak_permissions_y_devuelve_su_respuesta() {
    for grant in [true, false] {
        let (check, log, _server) = permissions(grant).await;
        assert_eq!(check.check("store.email", "a1").await.unwrap(), grant);
        assert_eq!(*log.lock(), ["perm store.email a1"]);
    }
}

#[tokio::test]
async fn si_vasak_permissions_no_responde_el_verificador_da_error_y_no_concede() {
    let (server, client) = silent_peer(":1.6").await;
    let result = DbusPermission::new(client).check("store.email", "").await;
    assert!(matches!(result, Err(Error::Backend(_))), "{result:?}");
    drop(server);
}
