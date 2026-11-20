//! La habilidad de archivos: buscar y leer archivos del usuario dentro de un
//! **alcance acotado** que la persona elige, y moverlos (o renombrarlos) con su
//! confirmación. **Nunca borra**: `move_file` ni siquiera sobrescribe un destino
//! que ya existe (eso sería borrar lo que había), y no hay herramienta de borrado.
//! Mover es una acción con efecto, así que no la ejecuta el modelo: devuelve una
//! [`ToolOutcome::Pending`] que la persona confirma viendo el origen y el destino.
//!
//! A diferencia del correo, el calendario y los contactos, no hay un servicio
//! que sirva los archivos: Lince lee el filesystem directo, como el usuario. Por
//! eso el permiso `files` es **consentimiento y visibilidad, no una frontera**
//! (el usuario ya puede leer esos archivos con cualquier programa suyo). Lo que
//! de verdad contiene una lectura dentro del alcance es la verificación de
//! contención sobre la ruta canónica (que resuelve los symlinks): un symlink
//! dentro de una raíz que apunta afuera queda afuera del alcance y se rechaza.
//!
//! El alcance son las **raíces** que la persona configuró (`lince_files_roots`
//! en `vasak.conf`); por omisión ninguna, así que sin configurar nada la
//! capacidad se ve no disponible. Nunca todo el disco por defecto.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use super::dbus::string_arg;
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_FILES};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, PendingAction, DOMAIN_FILES};

/// Cuántos resultados trae una búsqueda. Acota el tamaño de la respuesta y el
/// tiempo del recorrido.
const MAX_RESULTS: usize = 30;

/// Hasta qué profundidad baja el recorrido desde cada raíz.
const MAX_DEPTH: usize = 8;

/// Cuántas entradas mira el recorrido antes de cortar, pase lo que pase: un tope
/// de tiempo duro para un árbol enorme.
const MAX_VISITED: usize = 50_000;

/// Cuánto de un archivo se lee y se le muestra al modelo. Texto, no binario.
const MAX_READ_BYTES: usize = 64 * 1024;

/// Subcarpetas sensibles que nunca se miran aunque caigan dentro de una raíz
/// permitida: credenciales, llaves y lo que guardan las cuentas y el llavero.
/// Defensa en profundidad para que el camino `files` no filtre lo que cubren los
/// recursos `Credentials` y `store.*`. Relativas a `$HOME`.
const DENY_UNDER_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".config",
    ".local/share/vasak",
    ".local/share/keyrings",
    ".pki",
    ".mozilla",
    ".thunderbird",
    ".bash_history",
    ".zsh_history",
    ".netrc",
    ".git-credentials",
    ".aws",
    ".kube",
    ".docker",
    ".password-store",
];

/// La habilidad de archivos. Guarda las raíces ya canonizadas y legibles; si la
/// lista queda vacía (ninguna configurada o ninguna existe), la capacidad se ve
/// no disponible.
pub struct FilesSkill {
    roots: Vec<PathBuf>,
    deny: Vec<PathBuf>,
}

impl FilesSkill {
    /// Arma la habilidad desde las raíces crudas de la config: expande un `~/`
    /// inicial, canoniza (resuelve symlinks), y se queda sólo con las que
    /// existen, son carpetas y no caen bajo una ruta de la denylist. Una raíz que
    /// no se puede resolver se descarta en silencio (la capacidad lo refleja).
    pub fn new(raw_roots: Vec<String>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let deny: Vec<PathBuf> = home
            .as_ref()
            .map(|h| DENY_UNDER_HOME.iter().map(|rel| h.join(rel)).collect())
            .unwrap_or_default();
        let mut roots = Vec::new();
        for raw in raw_roots {
            let expanded = expand_tilde(&raw, home.as_deref());
            let Ok(canonical) = std::fs::canonicalize(&expanded) else {
                continue;
            };
            if !canonical.is_dir() {
                continue;
            }
            if under_deny(&canonical, &deny) {
                continue;
            }
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        Self { roots, deny }
    }

    /// La ruta canónica de `requested` dentro del alcance (ver la función libre
    /// [`within_scope`]).
    fn within_scope(&self, requested: &str) -> Option<PathBuf> {
        within_scope(&self.roots, &self.deny, requested)
    }

    fn search_files(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let query = string_arg(args, "query")?;
        let needle = query.to_lowercase();
        if needle.is_empty() {
            return Err(Error::InvalidArgs("la búsqueda está vacía".into()));
        }
        let mut hits: Vec<Value> = Vec::new();
        let mut visited = 0usize;
        let mut truncated = false;
        // Pila de (ruta, profundidad); no sigue symlinks de carpeta (se saltan
        // al mirar el tipo de la entrada), así que el recorrido no escapa.
        for root in &self.roots {
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                if hits.len() >= MAX_RESULTS || visited >= MAX_VISITED {
                    truncated = true;
                    break;
                }
                // Una carpeta que no se puede leer no se omite en silencio: marca
                // el resultado como incompleto (`truncated`), para no dar un "no
                // hay nada" cuando en realidad no se pudo mirar.
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    truncated = true;
                    continue;
                };
                for entry in entries {
                    let Ok(entry) = entry else {
                        truncated = true;
                        continue;
                    };
                    visited += 1;
                    if hits.len() >= MAX_RESULTS || visited >= MAX_VISITED {
                        truncated = true;
                        break;
                    }
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    // Ocultos fuera: ni se miran ni se bajan.
                    if name.starts_with('.') {
                        continue;
                    }
                    let Ok(file_type) = entry.file_type() else {
                        truncated = true;
                        continue;
                    };
                    // Un symlink no se sigue (ni como carpeta ni como archivo):
                    // es el camino de escape del alcance.
                    if file_type.is_symlink() {
                        continue;
                    }
                    let path = entry.path();
                    if self.deny.iter().any(|d| path.starts_with(d)) {
                        continue;
                    }
                    if name.to_lowercase().contains(&needle) {
                        hits.push(json!({
                            "path": path.to_string_lossy(),
                            "name": name,
                            "is_dir": file_type.is_dir(),
                            "size_bytes": entry.metadata().map(|m| m.len()).unwrap_or(0),
                        }));
                    }
                    if file_type.is_dir() && depth + 1 < MAX_DEPTH {
                        stack.push((path, depth + 1));
                    }
                }
                if truncated {
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        let json = json!({ "items": hits, "truncated": truncated }).to_string();
        Ok(ToolOutcome::Result(json))
    }

    fn read_file(&self, args: &Value) -> Result<ToolOutcome, Error> {
        let requested = string_arg(args, "path")?;
        let Some(path) = self.within_scope(&requested) else {
            return Err(Error::InvalidArgs(
                "esa ruta está fuera de las carpetas que Lince puede leer".into(),
            ));
        };
        if !path.is_file() {
            return Err(Error::InvalidArgs("esa ruta no es un archivo".into()));
        }
        // Se leen a lo sumo MAX_READ_BYTES + 1: el +1 sólo para saber si había
        // más (marcar `truncated`). Nunca se carga el archivo entero a memoria,
        // así que un archivo enorme dentro del alcance (un .iso, un log de GB)
        // no voltea el demonio. `take` también acota un tamaño reportado
        // disparatado (p. ej. /proc/kcore).
        use std::io::Read;
        let file = std::fs::File::open(&path)
            .map_err(|e| Error::Backend(format!("no se pudo leer: {e}")))?;
        let mut bytes = Vec::new();
        file.take((MAX_READ_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Backend(format!("no se pudo leer: {e}")))?;
        let truncated = bytes.len() > MAX_READ_BYTES;
        let slice = &bytes[..bytes.len().min(MAX_READ_BYTES)];
        // Sólo texto: un archivo binario (o uno cortado en medio de un carácter)
        // no se le pasa al modelo.
        let content = match std::str::from_utf8(slice) {
            Ok(text) => text.to_owned(),
            Err(error) => {
                // Un carácter multibyte cortado por el tope (`error_len()` None)
                // se acepta por su parte válida. Un byte inválido en el medio
                // (`error_len()` Some) es un binario y se rechaza, aunque haya un
                // prefijo válido antes.
                if truncated && error.valid_up_to() > 0 && error.error_len().is_none() {
                    String::from_utf8_lossy(&slice[..error.valid_up_to()]).into_owned()
                } else {
                    return Err(Error::InvalidArgs(
                        "ese archivo no es texto: no lo puedo leer".into(),
                    ));
                }
            }
        };
        let json = json!({
            "path": path.to_string_lossy(),
            "content": content,
            "truncated": truncated,
        })
        .to_string();
        Ok(ToolOutcome::Result(json))
    }

    /// La ruta de destino de un movimiento, si es válida (ver la función libre
    /// [`resolve_destination`]).
    fn resolve_destination(&self, requested: &str) -> Option<PathBuf> {
        resolve_destination(&self.roots, &self.deny, requested)
    }

    /// Prepara un movimiento (o renombrado) para confirmar. No mueve nada acá: el
    /// origen tiene que estar en el alcance y ser un archivo, y el destino ser
    /// válido y libre; si todo cuadra, devuelve una acción pendiente cuyo `execute`
    /// corre el `rename` recién al confirmar.
    fn move_file(&self, args: &Value, ctx: &SkillCtx) -> Result<ToolOutcome, Error> {
        let from_raw = string_arg(args, "from")?;
        let to_raw = string_arg(args, "to")?;
        let Some(source) = self.within_scope(&from_raw) else {
            return Err(Error::InvalidArgs(
                "el origen está fuera de las carpetas que Lince puede tocar".into(),
            ));
        };
        // Sólo archivos: mover una carpeta arrastraría todo su árbol (incluso una
        // raíz configurada), que no es lo que ofrece la herramienta.
        if !source.is_file() {
            return Err(Error::InvalidArgs(
                "el origen no es un archivo: mover sólo mueve archivos".into(),
            ));
        }
        let Some(target) = self.resolve_destination(&to_raw) else {
            return Err(Error::InvalidArgs(
                "el destino no sirve: tiene que estar dentro del alcance, nombrar un archivo \
                 nuevo y no existir ya (mover nunca sobrescribe)"
                    .into(),
            ));
        };
        if source == target {
            return Err(Error::InvalidArgs(
                "el origen y el destino son el mismo archivo".into(),
            ));
        }
        let action = PendingAction::simple(
            uuid::Uuid::new_v4().to_string(),
            ctx.conversation_id.clone(),
            "move_file",
            format!(
                "Mover «{}» a «{}»",
                source.to_string_lossy(),
                target.to_string_lossy()
            ),
        );
        // El `execute` no se queda con las rutas canónicas de recién: las vuelve a
        // resolver desde las crudas al confirmar. Entre proponer y confirmar (hasta
        // el TTL) un componente del camino pudo cambiar —p. ej. un ancestro
        // reemplazado por un symlink que apunta afuera—; revalidar desde cero
        // rechaza un origen o un destino que se salió del alcance en vez de mover
        // afuera. Clona lo que la resolución necesita.
        let roots = self.roots.clone();
        let deny = self.deny.clone();
        let execute = Box::new(move || -> BoxFuture<'static, Result<(), Error>> {
            Box::pin(async move {
                // La parte de filesystem es bloqueante; va a un hilo de bloqueo
                // para no trabar el runtime.
                tokio::task::spawn_blocking(move || {
                    let Some(source) = within_scope(&roots, &deny, &from_raw) else {
                        return Err(Error::InvalidArgs(
                            "el origen ya no está dentro del alcance".into(),
                        ));
                    };
                    if !source.is_file() {
                        return Err(Error::InvalidArgs("el origen ya no es un archivo".into()));
                    }
                    let Some(target) = resolve_destination(&roots, &deny, &to_raw) else {
                        return Err(Error::InvalidArgs(
                            "el destino ya no sirve: apareció algo ahí o salió del alcance".into(),
                        ));
                    };
                    if source == target {
                        return Err(Error::InvalidArgs(
                            "el origen y el destino son el mismo archivo".into(),
                        ));
                    }
                    rename_no_replace(&source, &target)
                })
                .await
                .map_err(|e| Error::Backend(format!("no se pudo mover: {e}")))?
            })
        });
        Ok(ToolOutcome::Pending { action, execute })
    }
}

impl Skill for FilesSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_FILES
    }

    fn permission(&self) -> &'static str {
        PERMISSION_FILES
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "search_files".into(),
                description: "Busca archivos por nombre dentro de las carpetas que la persona \
                              dejó ver. Devuelve ruta, nombre, si es carpeta y tamaño."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Texto a buscar en el nombre del archivo."}
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "read_file".into(),
                description: "Lee el contenido de un archivo de texto por su ruta, si está dentro \
                              de las carpetas que la persona dejó ver."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "La ruta del archivo, como la devuelve search_files."}
                    },
                    "required": ["path"]
                }),
            },
            ToolDef {
                name: "move_file".into(),
                description: "Mueve o renombra un archivo dentro de las carpetas que la persona \
                              dejó ver. No lo mueve: la persona lo confirma antes. El destino es \
                              la ruta nueva completa (con el nombre del archivo) y no puede existir \
                              ya; nunca sobrescribe ni borra."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "from": {"type": "string", "description": "La ruta actual del archivo, como la devuelve search_files."},
                        "to": {"type": "string", "description": "La ruta nueva completa, incluido el nombre del archivo. No puede existir todavía."}
                    },
                    "required": ["from", "to"]
                }),
            },
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        Box::pin(async move {
            match name {
                "search_files" => self.search_files(args),
                "read_file" => self.read_file(args),
                "move_file" => self.move_file(args, ctx),
                other => Err(Error::InvalidArgs(format!(
                    "los archivos no tienen una herramienta «{other}»"
                ))),
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        // En pie si hay al menos una raíz configurada y legible. Sin raíces, la
        // capacidad se ve no disponible («sin carpetas configuradas»).
        let alive = self
            .roots
            .iter()
            .any(|root| std::fs::read_dir(root).is_ok());
        Box::pin(async move { alive })
    }
}

/// La ruta canónica de `requested` si cae dentro de una raíz permitida y no bajo
/// la denylist; `None` si escapa el alcance. Canoniza antes de decidir, así que un
/// symlink que apunta afuera se resuelve y se rechaza.
///
/// Queda una ventana TOCTOU entre canonizar y abrir/mover (otro proceso podría
/// intercambiar un componente por un symlink). No es un vector acá: el modelo sólo
/// busca, lee y mueve dentro del alcance, y un proceso del propio usuario que sí
/// podría ya toca esos archivos igual (Lince corre como el usuario, sin confinar).
/// Por eso `move_file` revalida con esta misma función **en el momento de mover**:
/// la contención se resuelve justo antes de la operación, no al proponerla. Si
/// esta ruta llegara a quedar expuesta a un confinado, el endurecimiento sería
/// `openat2(RESOLVE_BENEATH)`.
fn within_scope(roots: &[PathBuf], deny: &[PathBuf], requested: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let expanded = expand_tilde(requested, home.as_deref());
    let canonical = std::fs::canonicalize(expanded).ok()?;
    if under_deny(&canonical, deny) {
        return None;
    }
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

/// La ruta de destino de un movimiento, si es válida: su **carpeta padre** cae
/// dentro del alcance (canónica, resuelve symlinks, fuera de la denylist), el
/// último componente es un nombre normal (ni `.`, ni `..`, ni vacío) y el destino
/// **todavía no existe**. No se canoniza el destino en sí porque no existe aún; se
/// canoniza el padre y se le pega el nombre. Que no exista es parte de que mover
/// nunca borra; la otra parte es el `RENAME_NOREPLACE` atómico al mover.
fn resolve_destination(roots: &[PathBuf], deny: &[PathBuf], requested: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dest = expand_tilde(requested, home.as_deref());
    // El último componente tiene que ser un nombre de archivo normal: así se
    // descartan `…/`, `…/.` y `…/..`, que no nombran un archivo a crear.
    let Some(Component::Normal(name)) = dest.components().next_back() else {
        return None;
    };
    let name = name.to_owned();
    let parent = std::fs::canonicalize(dest.parent()?).ok()?;
    if under_deny(&parent, deny) {
        return None;
    }
    if !parent.is_dir() || !roots.iter().any(|root| parent.starts_with(root)) {
        return None;
    }
    let target = parent.join(&name);
    // No sobrescribir: cualquier cosa en el destino convertiría el movimiento en un
    // borrado de lo que había. `symlink_metadata` (lstat, no sigue el enlace)
    // rechaza también un symlink colgante, que `exists()` —que sí lo sigue— tomaría
    // por libre. Es un pre-chequeo; la garantía dura la da `RENAME_NOREPLACE`.
    if target.symlink_metadata().is_ok() {
        return None;
    }
    Some(target)
}

/// Mueve `source` a `target` **sin pisar** nada: `renameat2` con
/// `RENAME_NOREPLACE` falla con `EEXIST` si el destino apareció entre el chequeo y
/// el movimiento, en vez de reemplazarlo (lo que `std::fs::rename` sí haría). Así
/// un archivo que aparece en el destino en plena carrera no se borra. `rename`
/// entre discos distintos falla con `EXDEV` y se informa; no se copia-y-borra.
fn rename_no_replace(source: &Path, target: &Path) -> Result<(), Error> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidArgs("la ruta de origen tiene un NUL".into()))?;
    let dst = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidArgs("la ruta de destino tiene un NUL".into()))?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        return Err(Error::Backend(
            "ya existe algo en el destino: no se mueve para no sobrescribirlo".into(),
        ));
    }
    Err(Error::Backend(format!("no se pudo mover: {err}")))
}

/// Si `canonical` (una ruta ya canonizada) cae bajo una entrada de la denylist.
/// Resuelve cada entrada **en el momento de comparar**, en el mismo espacio
/// canónico que la ruta pedida: una entrada que es un symlink (p. ej. `~/.ssh`
/// que apunta a `~/keys`) se resuelve a su destino, así que no se la puede
/// esquivar pidiendo la ruta por su nombre canónico. Una entrada que no existe
/// no protege nada (no puede ser prefijo de una ruta que sí existe).
fn under_deny(canonical: &Path, deny: &[PathBuf]) -> bool {
    deny.iter()
        .any(|d| std::fs::canonicalize(d).is_ok_and(|resolved| canonical.starts_with(resolved)))
}

/// Expande un `~/` o `~` inicial a `$HOME`. Cualquier otra cosa queda igual.
fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), path, home) {
        (Some(rest), _, Some(home)) => home.join(rest),
        (None, "~", Some(home)) => home.to_path_buf(),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
