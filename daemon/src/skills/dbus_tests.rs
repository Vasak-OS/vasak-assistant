//! Pruebas de los helpers compartidos de las habilidades: el parseo de los
//! argumentos del modelo, el plazo de las llamadas y el armado de errores.

use serde_json::json;

use super::*;

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
fn el_texto_opcional_es_vacio_si_falta_y_error_si_no_es_texto() {
    let args = json!({"s": "x", "n": 5, "nulo": null});
    assert_eq!(opt_string_arg(&args, "s").unwrap(), "x");
    assert_eq!(opt_string_arg(&args, "falta").unwrap(), "");
    assert_eq!(opt_string_arg(&args, "nulo").unwrap(), "");
    assert!(matches!(
        opt_string_arg(&args, "n"),
        Err(Error::InvalidArgs(m)) if m.contains("texto")
    ));
}

#[test]
fn el_booleano_opcional_usa_el_default_si_falta_y_error_si_no_es_booleano() {
    let args = json!({"b": true, "f": false, "s": "true", "nulo": null});
    assert!(bool_arg(&args, "b", false).unwrap());
    assert!(!bool_arg(&args, "f", true).unwrap());
    assert!(bool_arg(&args, "falta", true).unwrap());
    assert!(bool_arg(&args, "nulo", true).unwrap());
    assert!(matches!(
        bool_arg(&args, "s", false),
        Err(Error::InvalidArgs(m)) if m.contains("booleano")
    ));
}

#[test]
fn la_lista_de_textos_opcional_rechaza_lo_que_no_es_lista_de_textos() {
    let args = json!({"ids": ["a", "b"], "mal": [1], "no_lista": "a", "nulo": null});
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
    // El plazo externo hace que, sin el timeout, el test falle en vez de colgarse.
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
    // Una respuesta a tiempo pasa tal cual.
    let ok = call_with_timeout(Duration::from_secs(5), async { Ok::<_, zbus::Error>(7) }).await;
    assert_eq!(ok.unwrap(), 7);
}

#[test]
fn el_error_del_bus_dice_que_el_servicio_de_cuentas_no_respondio() {
    let error = backend_error("boom");
    assert!(matches!(&error, Error::Backend(m) if m.contains("boom") && m.contains("cuentas")));
}
