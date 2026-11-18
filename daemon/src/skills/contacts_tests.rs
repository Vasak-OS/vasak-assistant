//! Pruebas de la habilidad de contactos: los argumentos y el cable hacia
//! `AccountsStore`, contra un almacén de mentira servido en una conexión punto
//! a punto (sin bus).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::*;
use crate::skills::dbus::STORE_PATH;
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
        ToolOutcome::Pending { .. } => panic!("los contactos no dejan acciones pendientes"),
    }
}

#[tokio::test]
async fn la_habilidad_ofrece_las_dos_herramientas_de_solo_lectura_con_su_esquema() {
    let (skill, _log, _server) = store().await;
    assert_eq!(skill.domain(), DOMAIN_CONTACTS);
    assert_eq!(skill.permission(), "store.contacts");
    let tools = skill.tools();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search_contacts", "read_contact"]);
    assert_eq!(
        tools[0].parameters["required"],
        json!(["account_id", "query"])
    );
    assert_eq!(
        tools[1].parameters["required"],
        json!(["account_id", "contact_id"])
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
