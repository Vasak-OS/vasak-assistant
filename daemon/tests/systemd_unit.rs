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

// ── Aislamiento de red (#6): el demonio sin AF_INET, el broker con ───────────

/// Las líneas con efecto de un `.service` de `packaging/` (sin comentarios ni
/// vacías), recortadas.
fn directives(file: &str) -> Vec<String> {
    let path = format!("{}/../packaging/{file}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no se pudo leer {file}"))
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Cada asignación de `RestrictAddressFamilies=` de la unit, en orden. systemd
/// acumula las asignaciones y una vacía **reinicia** la lista: por eso no basta
/// con mirar la primera.
fn address_family_assignments(file: &str) -> Vec<Vec<String>> {
    directives(file)
        .into_iter()
        .filter_map(|line| {
            line.strip_prefix("RestrictAddressFamilies=")
                .map(|v| v.split_whitespace().map(str::to_owned).collect())
        })
        .collect()
}

/// Las familias efectivas: se aplica cada asignación en orden (vacía = reinicia).
fn effective_families(assignments: Vec<Vec<String>>) -> Vec<String> {
    let mut effective: Vec<String> = Vec::new();
    for assignment in assignments {
        if assignment.is_empty() {
            effective.clear();
        } else {
            effective.extend(assignment);
        }
    }
    effective
}

fn address_families(file: &str) -> Vec<String> {
    effective_families(address_family_assignments(file))
}

#[test]
fn el_demonio_solo_puede_abrir_sockets_unix() {
    let assignments = address_family_assignments("vasak-assistant.service");
    assert_eq!(
        assignments,
        [["AF_UNIX"]],
        "debe haber una sola asignación, AF_UNIX: otra (vacía o con AF_INET) la anularía"
    );
    assert_eq!(address_families("vasak-assistant.service"), ["AF_UNIX"]);
}

#[test]
fn una_asignacion_vacia_reinicia_las_familias_efectivas() {
    // Guarda del ayudante: un unit con `RestrictAddressFamilies=` vacío después
    // de AF_UNIX queda sin restricción, y eso no debe leerse como «AF_UNIX».
    let unix = vec!["AF_UNIX".to_owned()];
    assert!(effective_families(vec![unix.clone(), vec![]]).is_empty());
    assert_eq!(
        effective_families(vec![unix.clone(), vec![], unix.clone()]),
        ["AF_UNIX"]
    );
    assert_eq!(
        effective_families(vec![unix, vec!["AF_INET".to_owned()]]),
        ["AF_UNIX", "AF_INET"]
    );
}

#[test]
fn el_demonio_no_puede_usar_io_uring() {
    let lines = directives("vasak-assistant.service");
    assert!(lines
        .iter()
        .any(|l| l == "SystemCallFilter=@system-service"));
    assert!(
        lines.iter().any(|l| l == "SystemCallFilter=~@io_uring"),
        "falta SystemCallFilter=~@io_uring en el demonio"
    );
}

#[test]
fn el_broker_crea_su_subdirectorio_de_runtime_donde_vive_el_socket() {
    let lines = directives("vasak-assistant-broker.service");
    let expected = format!("RuntimeDirectory={}", vasak_assistant_ipc::RUNTIME_SUBDIR);
    assert!(
        lines.contains(&expected),
        "el broker debe llevar {expected}: el socket vive en ese subdirectorio"
    );
}

#[test]
fn el_broker_puede_abrir_sockets_ip() {
    let families = address_families("vasak-assistant-broker.service");
    for family in ["AF_UNIX", "AF_INET", "AF_INET6"] {
        assert!(
            families.iter().any(|f| f == family),
            "al broker le falta {family}"
        );
    }
}

#[test]
fn el_demonio_quiere_al_broker_pero_no_depende_de_el() {
    let lines = directives("vasak-assistant.service");
    assert!(
        lines
            .iter()
            .any(|l| l == "Wants=vasak-assistant-broker.service"),
        "el demonio debe llevar Wants= del broker"
    );
    for strong in ["Requires=", "BindsTo=", "Requisite="] {
        assert!(
            !lines.iter().any(|l| l.starts_with(strong)),
            "{strong} arrastraría al demonio a caerse con el broker"
        );
    }
}
