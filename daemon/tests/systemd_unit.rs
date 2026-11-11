//! Guarda de la unidad de systemd: el reintento acotado del arranque (#47).

use std::fs;

/// Valor de `key` en la sección `section` del `.service`, o `None` si no está ahí.
/// systemd ignora en silencio `StartLimit*` fuera de `[Unit]`, por eso importa la sección.
fn value_in_section(section: &str, key: &str) -> Option<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../packaging/vasak-assistant.service"
    );
    let text = fs::read_to_string(path).expect("no se pudo leer el .service");
    let mut current = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = &line[1..line.len() - 1];
        } else if current == section {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

#[test]
fn la_unidad_acota_los_reintentos_de_arranque_en_la_seccion_unit() {
    let burst = value_in_section("Unit", "StartLimitBurst")
        .expect("StartLimitBurst debe estar en [Unit]: en [Service] systemd lo ignora");
    let burst: u32 = burst.parse().expect("StartLimitBurst debe ser un entero");
    assert!(
        (1..=10).contains(&burst),
        "StartLimitBurst fuera de rango: {burst}"
    );

    assert!(
        value_in_section("Unit", "StartLimitIntervalSec").is_some(),
        "StartLimitIntervalSec debe estar en [Unit]"
    );
    for key in ["StartLimitBurst", "StartLimitIntervalSec"] {
        assert!(
            value_in_section("Service", key).is_none(),
            "{key} en [Service] no tiene efecto"
        );
    }
}

/// Segundos de un valor como `20s` o `3`.
// shortcut: sólo segundos (sufijo `s` opcional); no cubre `min`, `ms` ni valores compuestos, ampliar si la unidad los usa.
fn seconds(section: &str, key: &str) -> u64 {
    let raw =
        value_in_section(section, key).unwrap_or_else(|| panic!("{key} debe estar en [{section}]"));
    raw.strip_suffix('s')
        .unwrap_or(&raw)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{key}={raw} debe ser un entero de segundos"))
}

#[test]
fn el_tope_de_reintentos_es_alcanzable_dentro_de_la_ventana() {
    let interval = seconds("Unit", "StartLimitIntervalSec");
    let burst = seconds("Unit", "StartLimitBurst");
    let restart = seconds("Service", "RestartSec");
    assert!(
        interval > 0,
        "StartLimitIntervalSec debe ser mayor que cero; en 0 systemd desactiva el tope"
    );
    assert!(
        interval >= burst * restart,
        "la ventana StartLimitIntervalSec={interval}s no cubre StartLimitBurst*RestartSec={}s: \
         los reintentos nunca llegan al tope, no se dispara y el demonio reintenta sin fin",
        burst * restart
    );
}
