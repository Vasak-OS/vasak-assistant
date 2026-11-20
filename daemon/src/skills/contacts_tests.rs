//! Pruebas de la habilidad de contactos: los argumentos y el cable hacia
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

    async fn search_contacts(
        &self,
        account_id: &str,
        query: &str,
        cursor: &str,
        limit: u32,
    ) -> String {
        self.log
            .lock()
            .push(format!("search {account_id} {query} [{cursor}] {limit}"));
        "[{\"contacto\":1}]".into()
    }

    async fn list_address_books(&self, account_id: &str) -> String {
        self.log.lock().push(format!("books {account_id}"));
        "[{\"libreta\":1}]".into()
    }

    async fn put_contact(
        &self,
        account_id: &str,
        address_book_id: &str,
        payload: &str,
        if_match: &str,
    ) -> String {
        self.log.lock().push(format!(
            "put {account_id} {address_book_id} {payload} [{if_match}]"
        ));
        "{\"contact_id\":\"k1\"}".into()
    }

    async fn get_contact(&self, account_id: &str, contact_id: &str) -> String {
        self.log
            .lock()
            .push(format!("get {account_id} {contact_id}"));
        "{\"nombre\":\"Ana\"}".into()
    }
}

struct FakeOther;

#[zbus::interface(name = "ar.net.vasak.os.Otra")]
impl FakeOther {
    async fn nada(&self) -> String {
        String::new()
    }
}

async fn store() -> (ContactsSkill, Log, zbus::Connection) {
    let log = Log::default();
    let fake = FakeStore { log: log.clone() };
    let (server, client) = p2p_pair_as(":1.5", |builder| builder.serve_at(STORE_PATH, fake))
        .await
        .unwrap();
    (ContactsSkill::new(client), log, server)
}

fn ctx() -> SkillCtx {
    SkillCtx {
        conversation_id: "conv-1".into(),
    }
}

async fn run(skill: &ContactsSkill, name: &str, args: Value) -> Result<String, Error> {
    match skill.call(name, &args, &ctx()).await? {
        ToolOutcome::Result(text) => Ok(text),
        ToolOutcome::Pending { .. } => panic!("la herramienta no debía dejar una acción pendiente"),
    }
}

#[tokio::test]
async fn la_habilidad_ofrece_sus_herramientas_con_su_esquema() {
    let (skill, _log, _server) = store().await;
    assert_eq!(skill.domain(), DOMAIN_CONTACTS);
    assert_eq!(skill.permission(), "store.contacts");
    let tools = skill.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "list_address_books",
            "search_contacts",
            "read_contact",
            "create_contact"
        ]
    );
    assert_eq!(tools[0].parameters["required"], json!(["account_id"]));
    assert_eq!(
        tools[1].parameters["required"],
        json!(["account_id", "query"])
    );
    assert_eq!(
        tools[2].parameters["required"],
        json!(["account_id", "contact_id"])
    );
    assert_eq!(
        tools[3].parameters["required"],
        json!(["account_id", "address_book_id", "full_name"])
    );
}

#[tokio::test]
async fn el_probe_es_verdadero_si_el_almacen_responde_y_falso_si_no() {
    let (skill, _log, _server) = store().await;
    assert!(skill.probe().await);
    let (server, client) = p2p_pair_as(":1.5", |builder| {
        builder.serve_at("/otro/objeto", FakeOther)
    })
    .await
    .unwrap();
    assert!(!ContactsSkill::new(client).probe().await);
    drop(server);
}

#[tokio::test]
async fn search_contacts_busca_en_la_cuenta_con_la_primera_pagina_y_el_tope() {
    let (skill, log, _server) = store().await;
    let args = json!({"account_id": "a1", "query": "ana"});
    assert_eq!(
        run(&skill, "search_contacts", args).await.unwrap(),
        "[{\"contacto\":1}]"
    );
    assert_eq!(log.lock().as_slice(), ["search a1 ana [] 20"]);
}

#[tokio::test]
async fn read_contact_lee_el_contacto_de_la_cuenta() {
    let (skill, log, _server) = store().await;
    let args = json!({"account_id": "a1", "contact_id": "c9"});
    assert_eq!(
        run(&skill, "read_contact", args).await.unwrap(),
        "{\"nombre\":\"Ana\"}"
    );
    assert_eq!(log.lock().as_slice(), ["get a1 c9"]);
}

#[tokio::test]
async fn los_argumentos_invalidos_dan_invalid_args_y_no_tocan_el_almacen() {
    let (skill, log, _server) = store().await;
    for (name, args) in [
        ("search_contacts", json!({"query": "ana"})),
        ("search_contacts", json!({"account_id": "a1"})),
        ("search_contacts", json!({"account_id": "a1", "query": 5})),
        ("read_contact", json!({"account_id": "a1"})),
        ("read_contact", json!({"contact_id": "c9"})),
        ("read_contact", json!({"account_id": "a1", "contact_id": 7})),
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
    let result = run(&skill, "delete_contact", json!({})).await;
    assert!(matches!(result, Err(Error::InvalidArgs(m)) if m.contains("delete_contact")));
}

#[tokio::test]
async fn si_el_almacen_no_esta_la_lectura_da_error_de_backend() {
    let (server, client) = p2p_pair_as(":1.5", |builder| {
        builder.serve_at("/otro/objeto", FakeOther)
    })
    .await
    .unwrap();
    let skill = ContactsSkill::new(client);
    let args = json!({"account_id": "a1", "query": "ana"});
    let result = run(&skill, "search_contacts", args).await;
    assert!(matches!(result, Err(Error::Backend(_))), "{result:?}");
    drop(server);
}

/// Llama a `create_contact` y devuelve la acción pendiente con su ejecución.
async fn prepare(
    skill: &ContactsSkill,
    args: Value,
) -> Result<(PendingAction, crate::skills::SendAction), Error> {
    match skill.call("create_contact", &args, &ctx()).await? {
        ToolOutcome::Pending { action, execute } => Ok((action, execute)),
        ToolOutcome::Result(_) => panic!("create_contact no debe devolver un resultado directo"),
    }
}

#[tokio::test]
async fn list_address_books_consulta_las_libretas_de_la_cuenta() {
    let (skill, log, _server) = store().await;
    assert_eq!(
        run(&skill, "list_address_books", json!({"account_id": "a1"}))
            .await
            .unwrap(),
        "[{\"libreta\":1}]"
    );
    assert_eq!(log.lock().as_slice(), ["books a1"]);
    let result = run(&skill, "list_address_books", json!({})).await;
    assert!(matches!(result, Err(Error::InvalidArgs(_))));
    assert_eq!(log.lock().len(), 1);
}

#[tokio::test]
async fn create_contact_prepara_una_accion_y_no_escribe_hasta_confirmar() {
    let (skill, log, _server) = store().await;
    let args = json!({"account_id": "a1", "address_book_id": "b1", "full_name": "Ana Pérez"});
    let (action, execute) = prepare(&skill, args).await.unwrap();
    assert_eq!(action.kind, "create_contact");
    assert_eq!(action.conversation_id, "conv-1");
    assert!(action.summary.contains("Ana Pérez"), "{}", action.summary);
    assert!(
        log.lock().is_empty(),
        "preparar no debe llamar a PutContact"
    );
    execute().await.unwrap();
    // Sólo el nombre: los opcionales ausentes no entran; if_match vacío es crear.
    assert_eq!(
        log.lock().as_slice(),
        [format!(
            "put a1 b1 {} []",
            json!({"full_name": "Ana Pérez"})
        )]
    );
}

#[tokio::test]
async fn al_confirmar_el_contacto_lleva_correos_y_telefonos_como_value() {
    let (skill, log, _server) = store().await;
    let args = json!({
        "account_id": "a1", "address_book_id": "b1", "full_name": "Ana Pérez",
        "organization": "Vasak", "note": "Conocida del taller",
        "emails": ["ana@x.org", "ana@y.org"], "phones": ["+54 11 5555"]
    });
    let (_action, execute) = prepare(&skill, args).await.unwrap();
    execute().await.unwrap();
    let expected = json!({
        "full_name": "Ana Pérez",
        "organization": "Vasak",
        "note": "Conocida del taller",
        "emails": [{"value": "ana@x.org"}, {"value": "ana@y.org"}],
        "phones": [{"value": "+54 11 5555"}],
    });
    assert_eq!(log.lock().as_slice(), [format!("put a1 b1 {expected} []")]);
}

#[tokio::test]
async fn los_opcionales_vacios_no_van_al_payload_del_contacto() {
    let (skill, log, _server) = store().await;
    let args = json!({
        "account_id": "a1", "address_book_id": "b1", "full_name": "Ana",
        "organization": "", "note": "", "emails": [], "phones": []
    });
    let (_action, execute) = prepare(&skill, args).await.unwrap();
    execute().await.unwrap();
    assert_eq!(
        log.lock().as_slice(),
        [format!("put a1 b1 {} []", json!({"full_name": "Ana"}))]
    );
}

#[tokio::test]
async fn create_contact_con_argumentos_invalidos_es_invalid_args() {
    let (skill, log, _server) = store().await;
    let base = json!({"account_id": "a1", "address_book_id": "b1", "full_name": "Ana"});
    for campo in ["account_id", "address_book_id", "full_name"] {
        let mut args = base.clone();
        args.as_object_mut().unwrap().remove(campo);
        assert!(matches!(
            prepare(&skill, args).await,
            Err(Error::InvalidArgs(_))
        ));
    }
    let mut args = base;
    args["emails"] = json!("ana@x.org");
    assert!(matches!(
        prepare(&skill, args).await,
        Err(Error::InvalidArgs(_))
    ));
    assert!(log.lock().is_empty());
}
