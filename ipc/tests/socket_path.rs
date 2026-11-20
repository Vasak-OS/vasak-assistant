//! La ruta del socket del broker. Un solo archivo con una sola prueba: toca el
//! entorno del proceso y no debe correr en paralelo con otras.

use vasak_assistant_ipc::{socket_path, RUNTIME_SUBDIR, SOCKET_NAME};

#[test]
fn el_socket_vive_en_el_subdirectorio_del_runtime_dir_y_sin_runtime_dir_no_hay_ruta() {
    std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
    let path = socket_path().expect("con XDG_RUNTIME_DIR hay ruta");
    assert_eq!(
        path,
        std::path::Path::new("/run/user/1000")
            .join(RUNTIME_SUBDIR)
            .join(SOCKET_NAME)
    );
    assert_eq!(RUNTIME_SUBDIR, "vasak-assistant");

    std::env::set_var("XDG_RUNTIME_DIR", "");
    assert_eq!(socket_path(), None);
    std::env::remove_var("XDG_RUNTIME_DIR");
    assert_eq!(socket_path(), None);
}
