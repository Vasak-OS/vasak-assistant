//! Garantías de seguridad del protocolo del puente de red.

use vasak_assistant_ipc::{BrokerEnvelope, BrokerRequest, ChatMessage, ToolDef};
use vasak_assistant_protocol::{Message, Role};

fn messages() -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: "hola".into(),
    }]
}

/// Una instancia de cada variante de `BrokerRequest`. El `match` sin comodín
/// obliga a sumar acá la variante nueva: así ninguna se escapa de la guarda.
fn every_variant() -> Vec<BrokerRequest> {
    let thread = vec![ChatMessage::User("hola".into())];
    let tools = vec![ToolDef {
        name: "t".into(),
        description: "d".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let all = vec![
        BrokerRequest::Info,
        BrokerRequest::Models,
        BrokerRequest::ActiveModel,
        BrokerRequest::SupportsTools,
        BrokerRequest::ActiveSupportsTools,
        BrokerRequest::Chat {
            messages: messages(),
        },
        BrokerRequest::ChatLocalOnly {
            messages: messages(),
        },
        BrokerRequest::ChatTools {
            messages: thread.clone(),
            tools: tools.clone(),
        },
        BrokerRequest::ChatToolsLocalOnly {
            messages: thread,
            tools,
        },
        BrokerRequest::WebFetch {
            url: "https://ejemplo.com/pagina".into(),
        },
    ];
    for request in &all {
        match request {
            BrokerRequest::Info
            | BrokerRequest::Models
            | BrokerRequest::ActiveModel
            | BrokerRequest::SupportsTools
            | BrokerRequest::ActiveSupportsTools
            | BrokerRequest::Chat { .. }
            | BrokerRequest::ChatLocalOnly { .. }
            | BrokerRequest::ChatTools { .. }
            | BrokerRequest::ChatToolsLocalOnly { .. }
            | BrokerRequest::WebFetch { .. } => {}
        }
    }
    all
}

/// Los pedidos de **modelo**: todas las variantes menos `WebFetch`, la excepción
/// sancionada. Ninguno de estos puede nombrar un destino.
fn model_requests() -> Vec<BrokerRequest> {
    every_variant()
        .into_iter()
        .filter(|request| !matches!(request, BrokerRequest::WebFetch { .. }))
        .collect()
}

/// Todas las claves de objeto de un JSON, a cualquier profundidad.
fn keys(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                out.push(key.to_ascii_lowercase());
                keys(inner, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| keys(item, out)),
        _ => {}
    }
}

const DESTINATION_KEYS: [&str; 7] = [
    "url", "uri", "host", "hostname", "endpoint", "addr", "address",
];

#[test]
fn ningun_pedido_de_modelo_transporta_un_destino() {
    for request in model_requests() {
        let json = serde_json::to_value(&request).unwrap();
        let mut found = Vec::new();
        keys(&json, &mut found);
        // Los campos de `ToolDef.parameters` son del esquema de la herramienta,
        // no de un destino: acá no hay ninguno con esos nombres.
        for key in DESTINATION_KEYS {
            assert!(
                !found.iter().any(|k| k == key),
                "{request:?} lleva el campo de destino «{key}»: un pedido de modelo no puede nombrar un host"
            );
        }
    }
}

/// `WebFetch` es la **única** excepción a la regla: es la habilidad web, sólo
/// lectura, y el broker valida la URL contra su allowlist + SSRF antes de salir. La
/// prueba la fija como la única que lleva un destino, para que ninguna otra se cuele
/// nombrando un host.
#[test]
fn solo_web_fetch_lleva_un_destino_y_es_la_excepcion_sancionada() {
    let web = BrokerRequest::WebFetch {
        url: "https://ejemplo.com/pagina".into(),
    };
    let json = serde_json::to_value(&web).unwrap();
    let mut found = Vec::new();
    keys(&json, &mut found);
    assert!(
        found.iter().any(|k| k == "url"),
        "WebFetch debería llevar la URL: es la excepción sancionada"
    );

    // Y es la única: ningún otro pedido lleva un destino.
    for request in model_requests() {
        let json = serde_json::to_value(&request).unwrap();
        let mut found = Vec::new();
        keys(&json, &mut found);
        assert!(
            !DESTINATION_KEYS
                .iter()
                .any(|key| found.iter().any(|k| k == key)),
            "{request:?} no puede nombrar un destino: sólo WebFetch puede"
        );
    }
}

#[test]
fn el_sobre_solo_agrega_la_clave_y_ningun_destino() {
    let envelope = BrokerEnvelope {
        api_key: Some("sk-x".into()),
        key_base_url: None,
        request: BrokerRequest::Models,
    };
    let json = serde_json::to_value(&envelope).unwrap();
    let mut top: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    top.sort();
    assert_eq!(top, ["api_key", "request"]);
}

#[test]
fn el_debug_del_sobre_no_imprime_la_clave() {
    let envelope = BrokerEnvelope {
        api_key: Some("sk-ultra-secreta-123".into()),
        key_base_url: None,
        request: BrokerRequest::Chat {
            messages: messages(),
        },
    };
    for text in [format!("{envelope:?}"), format!("{envelope:#?}")] {
        assert!(
            !text.contains("sk-ultra-secreta-123"),
            "se filtró la clave: {text}"
        );
        assert!(
            text.contains("<oculta>"),
            "falta la marca de redacción: {text}"
        );
    }
}

#[test]
fn el_debug_del_sobre_sin_clave_no_dice_oculta() {
    let envelope = BrokerEnvelope {
        api_key: None,
        key_base_url: None,
        request: BrokerRequest::Models,
    };
    assert!(!format!("{envelope:?}").contains("<oculta>"));
}

#[test]
fn el_sobre_con_clave_lleva_la_direccion_de_la_clave_y_ninguna_otra_cosa_de_destino() {
    let envelope = BrokerEnvelope {
        api_key: Some("sk-x".into()),
        key_base_url: Some("https://api.openai.com/v1/".into()),
        request: BrokerRequest::Models,
    };
    let json = serde_json::to_value(&envelope).unwrap();
    let mut top: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    top.sort();
    assert_eq!(top, ["api_key", "key_base_url", "request"]);
    // La dirección es sólo la marca de la clave: el pedido sigue sin destino.
    let mut found = Vec::new();
    keys(&json["request"], &mut found);
    assert!(found.is_empty(), "{found:?}");

    let back: BrokerEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
}

#[test]
fn el_debug_del_sobre_oculta_la_clave_aunque_lleve_su_direccion() {
    let envelope = BrokerEnvelope {
        api_key: Some("sk-ultra-secreta-123".into()),
        key_base_url: Some("https://api.openai.com/v1/".into()),
        request: BrokerRequest::Models,
    };
    let text = format!("{envelope:?}");
    assert!(!text.contains("sk-ultra-secreta-123"), "{text}");
    assert!(text.contains("<oculta>"));
}
