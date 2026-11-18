//! La habilidad de archivos: buscar y leer archivos del usuario dentro de un
//! **alcance acotado** que la persona elige. **Sólo lectura**: ninguna
//! herramienta renombra, mueve ni borra nada —eso sería una acción confirmable,
//! para otra rebanada—.
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

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::dbus::string_arg;
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_FILES};
use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{Error, DOMAIN_FILES};

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

    /// La ruta canónica de `requested` si cae dentro de una raíz permitida y no
    /// bajo la denylist; `None` si escapa el alcance. Canoniza antes de decidir,
    /// así que un symlink que apunta afuera se resuelve y se rechaza.
    ///
    /// Queda una ventana TOCTOU entre canonizar y abrir (otro proceso podría
    /// intercambiar un componente por un symlink). No es un vector acá: el modelo
    /// sólo busca y lee, no crea symlinks, y un proceso del propio usuario que sí
    /// podría ya lee esos archivos igual (Lince corre como el usuario, sin
    /// confinar). Si esta ruta llegara a quedar expuesta a un confinado, el
    /// endurecimiento sería `openat2(RESOLVE_BENEATH)`.
    fn within_scope(&self, requested: &str) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let expanded = expand_tilde(requested, home.as_deref());
        let canonical = std::fs::canonicalize(expanded).ok()?;
        if under_deny(&canonical, &self.deny) {
            return None;
        }
        self.roots
            .iter()
            .any(|root| canonical.starts_with(root))
            .then_some(canonical)
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
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        Box::pin(async move {
            match name {
                "search_files" => self.search_files(args),
                "read_file" => self.read_file(args),
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
