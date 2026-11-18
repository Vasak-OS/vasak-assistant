//! Pruebas de la habilidad de archivos, contra un árbol real en un directorio
//! temporal: archivos, subcarpetas y symlinks que apuntan afuera del alcance.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// Un directorio temporal único que se borra al soltarlo.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vasak-files-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(dir).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Un árbol `root/` (el alcance) y `outside/` (fuera de él), hermanos.
struct Tree {
    _tmp: Tmp,
    root: PathBuf,
    outside: PathBuf,
}

fn tree() -> Tree {
    let tmp = Tmp::new();
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Tree {
        root,
        outside,
        _tmp: tmp,
    }
}

impl Tree {
    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        self._tmp.write(rel, bytes)
    }
}

fn skill(root: &Path) -> FilesSkill {
    FilesSkill {
        roots: vec![root.to_path_buf()],
        deny: vec![],
    }
}

fn ctx() -> SkillCtx {
    SkillCtx {
        conversation_id: "conv-1".into(),
    }
}

async fn run(skill: &FilesSkill, name: &str, args: Value) -> Result<Value, Error> {
    match skill.call(name, &args, &ctx()).await? {
        ToolOutcome::Result(text) => Ok(serde_json::from_str(&text).unwrap()),
        ToolOutcome::Pending { .. } => panic!("los archivos no dejan acciones pendientes"),
    }
}

async fn search(skill: &FilesSkill, query: &str) -> Value {
    run(skill, "search_files", json!({ "query": query }))
        .await
        .unwrap()
}

fn names(result: &Value) -> Vec<String> {
    let mut names: Vec<String> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

fn is_invalid(result: Result<Value, Error>) -> bool {
    matches!(result, Err(Error::InvalidArgs(_)))
}

// ── search_files ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn la_busqueda_encuentra_por_nombre_sin_distinguir_mayusculas_y_baja_a_subcarpetas() {
    let t = tree();
    t.write("root/Informe-Final.txt", "hola");
    t.write("root/a/b/otro_informe.md", "x");
    t.write("root/a/nada.txt", "x");
    let result = search(&skill(&t.root), "INFORME").await;
    assert_eq!(names(&result), ["Informe-Final.txt", "otro_informe.md"]);
    assert_eq!(result["truncated"], false);
    let item = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "Informe-Final.txt")
        .unwrap();
    assert_eq!(item["is_dir"], false);
    assert_eq!(item["size_bytes"], 4);
    assert_eq!(
        item["path"].as_str().unwrap(),
        t.root.join("Informe-Final.txt").to_str().unwrap()
    );
}

#[tokio::test]
async fn la_busqueda_encuentra_carpetas_y_lo_marca() {
    let t = tree();
    std::fs::create_dir_all(t.root.join("Fotos")).unwrap();
    let result = search(&skill(&t.root), "fotos").await;
    assert_eq!(result["items"][0]["is_dir"], true);
}

#[tokio::test]
async fn la_busqueda_saltea_los_archivos_y_carpetas_ocultos() {
    let t = tree();
    t.write("root/.nota-oculta.txt", "x");
    t.write("root/.cache/nota.txt", "x");
    t.write("root/visible/nota.txt", "x");
    let result = search(&skill(&t.root), "nota").await;
    assert_eq!(names(&result), ["nota.txt"]);
    assert!(result["items"][0]["path"]
        .as_str()
        .unwrap()
        .contains("visible"));
}

#[tokio::test]
async fn la_busqueda_corta_en_el_maximo_de_resultados_y_lo_marca() {
    let t = tree();
    for i in 0..35 {
        t.write(&format!("root/doc_{i}.txt"), "x");
    }
    let result = search(&skill(&t.root), "doc_").await;
    // El tope es parte del contrato con el modelo: 30, no «lo que valga la constante».
    assert_eq!(result["items"].as_array().unwrap().len(), 30);
    assert_eq!(result["truncated"], true);
}

#[tokio::test]
async fn la_busqueda_no_baja_mas_alla_de_la_profundidad_maxima() {
    let t = tree();
    // n0/n1/…: `needle_k` vive dentro de `n{k}`. El nivel 7 todavía se lista;
    // el 8 ya no.
    let chain = |k: usize| {
        (0..=k)
            .map(|i| format!("n{i}"))
            .collect::<Vec<_>>()
            .join("/")
    };
    t.write(&format!("root/{}/needle_6.txt", chain(6)), "x");
    t.write(&format!("root/{}/needle_7.txt", chain(7)), "x");
    let result = search(&skill(&t.root), "needle").await;
    assert_eq!(names(&result), ["needle_6.txt"]);
}

#[tokio::test]
async fn la_busqueda_no_mira_las_rutas_de_la_denylist() {
    let t = tree();
    t.write("root/privado/token.txt", "x");
    t.write("root/publico/token.txt", "x");
    let skill = FilesSkill {
        roots: vec![t.root.clone()],
        deny: vec![t.root.join("privado")],
    };
    let result = search(&skill, "token").await;
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert!(result["items"][0]["path"]
        .as_str()
        .unwrap()
        .contains("publico"));
}

#[tokio::test]
async fn la_busqueda_no_sigue_un_symlink_de_carpeta_que_apunta_afuera() {
    let t = tree();
    t.write("outside/secreto.txt", "no");
    t.write("root/propio.txt", "si");
    std::os::unix::fs::symlink(&t.outside, t.root.join("enlace")).unwrap();
    std::os::unix::fs::symlink(t.outside.join("secreto.txt"), t.root.join("secreto-link")).unwrap();
    let result = search(&skill(&t.root), "secreto").await;
    assert!(result["items"].as_array().unwrap().is_empty(), "{result}");
    let result = search(&skill(&t.root), "propio").await;
    assert_eq!(names(&result), ["propio.txt"]);
}

// ── read_file ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn read_file_lee_un_archivo_dentro_de_la_raiz() {
    let t = tree();
    let file = t.write("root/sub/nota.txt", "contenido útil");
    let result = run(
        &skill(&t.root),
        "read_file",
        json!({ "path": file.to_str().unwrap() }),
    )
    .await
    .unwrap();
    assert_eq!(result["content"], "contenido útil");
    assert_eq!(result["truncated"], false);
    assert_eq!(result["path"].as_str().unwrap(), file.to_str().unwrap());
}

#[tokio::test]
async fn read_file_rechaza_lo_que_escapa_del_alcance() {
    let t = tree();
    let secret = t.write("outside/secreto.txt", "no");
    t.write("root/sub/dentro.txt", "si");
    std::os::unix::fs::symlink(&t.outside, t.root.join("enlace")).unwrap();
    std::os::unix::fs::symlink(&secret, t.root.join("archivo-link")).unwrap();
    let skill = skill(&t.root);
    let read = |p: String| run(&skill, "read_file", json!({ "path": p }));

    // Fuera de la raíz, tal cual.
    assert!(is_invalid(read(secret.to_str().unwrap().into()).await));
    // Un symlink de carpeta y uno de archivo que apuntan afuera.
    assert!(is_invalid(
        read(format!("{}/enlace/secreto.txt", t.root.display())).await
    ));
    assert!(is_invalid(
        read(format!("{}/archivo-link", t.root.display())).await
    ));
    // Subiendo con `..` hasta salir de la raíz.
    assert!(is_invalid(
        read(format!(
            "{}/sub/../../outside/secreto.txt",
            t.root.display()
        ))
        .await
    ));
    // Una ruta que no existe.
    assert!(is_invalid(
        read(format!("{}/no-existe.txt", t.root.display())).await
    ));
    // Y lo de adentro sigue andando, `..` incluido si no sale.
    assert!(read(format!("{}/sub/../sub/dentro.txt", t.root.display()))
        .await
        .is_ok());
}

#[tokio::test]
async fn read_file_rechaza_lo_que_cae_bajo_la_denylist_aunque_este_en_la_raiz() {
    let t = tree();
    let key = t.write("root/.ssh/id_ed25519", "llave");
    let skill = FilesSkill {
        roots: vec![t.root.clone()],
        deny: vec![t.root.join(".ssh")],
    };
    assert!(is_invalid(
        run(
            &skill,
            "read_file",
            json!({ "path": key.to_str().unwrap() })
        )
        .await
    ));
}

#[tokio::test]
async fn read_file_rechaza_una_carpeta() {
    let t = tree();
    std::fs::create_dir_all(t.root.join("sub")).unwrap();
    assert!(is_invalid(
        run(
            &skill(&t.root),
            "read_file",
            json!({ "path": t.root.join("sub").to_str().unwrap() })
        )
        .await
    ));
}

#[tokio::test]
async fn read_file_rechaza_un_binario() {
    let t = tree();
    let file = t.write("root/img.bin", [0xffu8, 0xfe, 0x00, 0x80]);
    assert!(is_invalid(
        run(
            &skill(&t.root),
            "read_file",
            json!({ "path": file.to_str().unwrap() })
        )
        .await
    ));
}

#[tokio::test]
async fn read_file_corta_en_el_maximo_y_lo_marca() {
    let t = tree();
    let file = t.write("root/grande.txt", "a".repeat(64 * 1024 + 10));
    let result = run(
        &skill(&t.root),
        "read_file",
        json!({ "path": file.to_str().unwrap() }),
    )
    .await
    .unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["content"].as_str().unwrap().len(), 64 * 1024);
}

#[tokio::test]
async fn read_file_no_confunde_un_caracter_partido_por_el_tope_con_un_binario() {
    let t = tree();
    // El tope cae en el medio de una «é» de dos bytes.
    let file = t.write("root/acentos.txt", format!("a{}", "é".repeat(64 * 1024)));
    let result = run(
        &skill(&t.root),
        "read_file",
        json!({ "path": file.to_str().unwrap() }),
    )
    .await
    .unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["content"].as_str().unwrap().len(), 64 * 1024 - 1);
}

async fn read_path(skill: &FilesSkill, path: &Path) -> Result<Value, Error> {
    run(
        skill,
        "read_file",
        json!({ "path": path.to_str().unwrap() }),
    )
    .await
}

#[tokio::test]
async fn read_file_acotado_devuelve_el_tope_de_un_archivo_de_un_mib() {
    // Refuerzo: la salida es la misma que con una lectura entera. La cota de
    // memoria (take) no se afirma por salida, sólo que el contenido sale cortado.
    let t = tree();
    let file = t.write("root/mib.txt", "x".repeat(1024 * 1024));
    let result = read_path(&skill(&t.root), &file).await.unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["content"].as_str().unwrap().len(), MAX_READ_BYTES);
}

#[tokio::test]
async fn read_file_rechaza_una_entrada_de_la_denylist_que_es_un_symlink() {
    let t = tree();
    let key = t.write("root/keys/id_ed25519", "llave");
    std::os::unix::fs::symlink(t.root.join("keys"), t.root.join(".ssh")).unwrap();
    let skill = FilesSkill {
        roots: vec![t.root.clone()],
        deny: vec![t.root.join(".ssh")],
    };
    // Se pide por la ruta canónica (la de «keys»), no por el nombre del symlink.
    assert!(is_invalid(read_path(&skill, &key).await));
}

#[tokio::test]
async fn read_file_rechaza_una_entrada_de_la_denylist_que_es_un_archivo() {
    let t = tree();
    let history = t.write("root/.bash_history", "ls -la");
    let skill = FilesSkill {
        roots: vec![t.root.clone()],
        deny: vec![history.clone()],
    };
    assert!(is_invalid(read_path(&skill, &history).await));
}

#[tokio::test]
async fn read_file_rechaza_un_byte_invalido_en_el_medio_aunque_haya_prefijo_valido() {
    let t = tree();
    let mut bytes = vec![b'A', 0xff];
    bytes.resize(MAX_READ_BYTES + 100, b'b');
    let file = t.write("root/mixto.bin", bytes);
    assert!(is_invalid(read_path(&skill(&t.root), &file).await));
}

#[cfg(unix)]
#[tokio::test]
async fn la_busqueda_marca_truncated_si_una_carpeta_no_se_puede_leer() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    t.write("root/visible.txt", "x");
    let closed = t.root.join("cerrada");
    std::fs::create_dir_all(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Como root los permisos no se aplican: no es determinista, se omite.
    let readable = std::fs::read_dir(&closed).is_ok();
    let result = search(&skill(&t.root), "visible").await;
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return;
    }
    assert_eq!(names(&result), vec!["visible.txt"]);
    assert_eq!(result["truncated"], true);
}

// ── argumentos y herramientas ───────────────────────────────────────────────────

#[tokio::test]
async fn los_argumentos_que_faltan_o_una_herramienta_desconocida_son_invalidos() {
    let t = tree();
    let skill = skill(&t.root);
    assert!(is_invalid(run(&skill, "search_files", json!({})).await));
    assert!(is_invalid(
        run(&skill, "search_files", json!({ "query": "" })).await
    ));
    assert!(is_invalid(run(&skill, "read_file", json!({})).await));
    assert!(is_invalid(run(&skill, "borrar_archivo", json!({})).await));
}

#[test]
fn la_habilidad_ofrece_solo_dos_herramientas_de_lectura_en_su_dominio() {
    let t = tree();
    let skill = skill(&t.root);
    let names: Vec<String> = skill.tools().into_iter().map(|t| t.name).collect();
    assert_eq!(names, ["search_files", "read_file"]);
    assert_eq!(skill.domain(), DOMAIN_FILES);
    assert_eq!(skill.permission(), PERMISSION_FILES);
    assert_eq!(PERMISSION_FILES, "files");
}

// ── new y probe ─────────────────────────────────────────────────────────────────

/// Serializa las pruebas que cambian `HOME` (es global del proceso).
static HOME_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn new_descarta_raices_inexistentes_archivos_y_denylist_y_deduplica() {
    let _guard = HOME_LOCK.lock();
    let t = tree();
    let home = t.root.join("home");
    t.write("root/home/.ssh/id", "llave");
    t.write("root/home/docs/a.txt", "hola");
    t.write("root/home/suelto.txt", "x");
    let old = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let skill = FilesSkill::new(vec![
        "~/docs".into(),                                  // se expande y entra
        home.join("docs/.").to_str().unwrap().into(),     // la misma, otra grafía
        home.join(".ssh").to_str().unwrap().into(),       // denylist
        home.join(".ssh/..").to_str().unwrap().into(),    // el home entero sí entra…
        home.join("suelto.txt").to_str().unwrap().into(), // un archivo
        home.join("no-existe").to_str().unwrap().into(),  // inexistente
    ]);
    // `read_file` expande `~` con el mismo HOME.
    let read = std::fs::canonicalize(home.join("docs/a.txt")).unwrap();
    let denied = home.join(".ssh/id");
    let tilde_read = skill.within_scope("~/docs/a.txt");
    let denied_read = skill.within_scope(denied.to_str().unwrap());
    match old {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let canon_home = std::fs::canonicalize(&home).unwrap();
    assert_eq!(skill.roots, [canon_home.join("docs"), canon_home]);
    assert_eq!(tilde_read, Some(read));
    // Aun con el home como raíz, `.ssh` queda afuera por la denylist.
    assert_eq!(denied_read, None);
}

#[tokio::test]
async fn probe_es_falso_sin_raices_legibles_y_verdadero_con_una() {
    let t = tree();
    assert!(!FilesSkill::new(vec![]).probe().await);
    assert!(
        !FilesSkill::new(vec![t.root.join("no-existe").to_str().unwrap().into()])
            .probe()
            .await
    );
    assert!(
        FilesSkill::new(vec![t.root.to_str().unwrap().into()])
            .probe()
            .await
    );
}
