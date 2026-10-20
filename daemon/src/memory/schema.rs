//! El esquema de la memoria, en el grafo de propiedades de Grafeo.
//!
//! Cada recuerdo es un nodo `:Memory` con una sola clase más ([`MemoryKind`]):
//! preferencias, estilo, hábitos, temas, correcciones, alias y resúmenes de
//! conversación. Las propiedades:
//!
//! | Propiedad | Tipo | Qué es |
//! |---|---|---|
//! | `id` | texto (uuid) | identificador estable; con índice de propiedad |
//! | `key` | texto | hash de la clase y el texto normalizado, para deduplicar; con índice |
//! | `text` | texto | el recuerdo, con el tope de su clase |
//! | `phrase` | texto | sólo `Alias`: la frase que usa la persona |
//! | `conversation` | texto | sólo `ConversationSummary`: el id de la conversación |
//! | `terms` | texto | lo que se busca: `text` (y `phrase`) normalizado; con índice de texto |
//! | `created_at`, `updated_at` | entero | segundos Unix |
//! | `weight` | real | 0,3 al nacer (0,5 `Correction` y `Style`); cada refuerzo suma `0.2*(1-w)` |
//! | `hits` | entero | cuántas veces se vio |
//!
//! Aristas: `(:Memory)-[:ABOUT]->(:Memory:Topic)`, hasta [`MAX_ABOUT`] por nodo.
//!
//! **No hay dónde guardar correos, eventos, contactos, archivos, cuentas ni
//! claves**: no hay clase ni propiedad para eso, y lo que entra pasa antes por
//! `filter`. Todo lo de acá corre fuera del runtime (llama a la base, que lee y
//! escribe archivos).

use std::collections::HashSet;

use grafeo::{GrafeoDB, NodeId, TextIndexOptions, TokenizerKind, Value};
use grafeo_core::graph::Direction;
use sha2::{Digest, Sha256};

use super::filter::normalize_terms;

/// La etiqueta común de todos los recuerdos.
pub const LABEL: &str = "Memory";
/// La arista de un recuerdo a un tema.
pub const ABOUT: &str = "ABOUT";
/// Cuántos temas puede tener un recuerdo.
pub const MAX_ABOUT: usize = 3;
/// El tope de `phrase` de un `Alias`.
pub const MAX_PHRASE_BYTES: usize = 64;
/// La vida media de un recuerdo en el puntaje, en días.
const HALF_LIFE_DAYS: f64 = 90.0;

/// Las clases de recuerdo. Cerrada: agregar una es cambiar el esquema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    Preference,
    Style,
    Habit,
    Topic,
    Correction,
    Alias,
    ConversationSummary,
}

impl MemoryKind {
    pub const ALL: [Self; 7] = [
        Self::Preference,
        Self::Style,
        Self::Habit,
        Self::Topic,
        Self::Correction,
        Self::Alias,
        Self::ConversationSummary,
    ];

    /// La etiqueta del nodo (además de `:Memory`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preference => "Preference",
            Self::Style => "Style",
            Self::Habit => "Habit",
            Self::Topic => "Topic",
            Self::Correction => "Correction",
            Self::Alias => "Alias",
            Self::ConversationSummary => "ConversationSummary",
        }
    }

    /// La clase por su nombre, sin distinguir mayúsculas
    /// (`preference`, `Preference`, `conversation_summary`).
    pub fn from_name(name: &str) -> Option<Self> {
        let name: String = name.chars().filter(|c| *c != '_').collect();
        Self::ALL
            .into_iter()
            .find(|kind| kind.label().eq_ignore_ascii_case(&name))
    }

    /// Cuántos nodos de esta clase puede haber.
    pub const fn max_nodes(self) -> usize {
        match self {
            Self::Style => 20,
            Self::Habit | Self::Correction | Self::ConversationSummary => 50,
            Self::Preference | Self::Topic | Self::Alias => 100,
        }
    }

    /// El tope de `text`, en bytes.
    pub const fn max_text(self) -> usize {
        match self {
            Self::Topic => 64,
            Self::Alias => 160,
            Self::ConversationSummary => 600,
            _ => 280,
        }
    }

    /// El peso con el que nace.
    pub const fn initial_weight(self) -> f64 {
        match self {
            Self::Correction | Self::Style => 0.5,
            _ => 0.3,
        }
    }
}

/// Los índices de la memoria: de propiedad sobre `id` y `key`, y de texto
/// sobre `:Memory(terms)`. Se crean con la base y se aseguran al abrirla.
pub fn ensure_indexes(db: &GrafeoDB) -> grafeo::Result<()> {
    db.create_property_index("id")?;
    db.create_property_index("key")?;
    if db.text_index_options(LABEL, "terms").is_none() {
        db.create_text_index_with(
            LABEL,
            "terms",
            TextIndexOptions::new()
                .with_tokenizer(TokenizerKind::Standard)
                .with_stop_words(SPANISH_STOP_WORDS),
        )?;
    }
    Ok(())
}

/// Las palabras vacías del castellano, ya normalizadas (sin tildes), para que
/// no pesen en la búsqueda. Grafeo no trae una lista propia.
pub const SPANISH_STOP_WORDS: &[&str] = &[
    "a", "al", "algo", "algun", "alguna", "algunas", "alguno", "algunos", "ante", "antes", "aqui",
    "asi", "aun", "bien", "cada", "como", "con", "contra", "cual", "cuando", "de", "del", "desde",
    "donde", "durante", "e", "el", "ella", "ellas", "ellos", "en", "entre", "era", "eres", "es",
    "esa", "esas", "ese", "eso", "esos", "esta", "estan", "estas", "este", "esto", "estos",
    "estoy", "fue", "ha", "han", "hasta", "hay", "he", "la", "las", "le", "les", "lo", "los",
    "mas", "me", "mi", "mis", "mucho", "muy", "nada", "ni", "no", "nos", "o", "os", "otra",
    "otras", "otro", "otros", "para", "pero", "poco", "por", "porque", "que", "quien", "se", "sea",
    "ser", "si", "sin", "sobre", "son", "soy", "su", "sus", "tambien", "te", "ti", "todo", "todos",
    "tu", "tus", "un", "una", "unas", "uno", "unos", "vos", "y", "ya", "yo",
];

/// La clave de deduplicación: SHA-256 de la clase y el texto normalizado,
/// los primeros 16 bytes en hexadecimal.
pub fn key_for(kind: MemoryKind, text: &str) -> String {
    let digest = Sha256::new()
        .chain_update(kind.label().as_bytes())
        .chain_update([0])
        .chain_update(normalize_terms(text).as_bytes())
        .finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// El puntaje con el que se elige qué recordar y qué podar: el peso, que se
/// reduce a la mitad cada 90 días sin refuerzo.
pub fn score(weight: f64, updated_at: i64, now: i64) -> f64 {
    let days = (now - updated_at).max(0) as f64 / 86_400.0;
    weight * 0.5f64.powf(days / HALF_LIFE_DAYS)
}

/// El peso después de un refuerzo.
pub fn reinforced(weight: f64) -> f64 {
    weight + 0.2 * (1.0 - weight)
}

/// Ahora, en segundos Unix.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Un recuerdo leído de la base.
#[derive(Debug, Clone)]
pub struct Entry {
    pub node: NodeId,
    pub kind: MemoryKind,
    pub text: String,
    pub phrase: Option<String>,
    pub weight: f64,
    pub updated_at: i64,
}

impl Entry {
    pub fn score(&self, now: i64) -> f64 {
        score(self.weight, self.updated_at, now)
    }
}

/// El recuerdo de ese nodo, si existe y es un `:Memory` de una clase conocida.
pub fn entry(db: &GrafeoDB, node: NodeId) -> Option<Entry> {
    let found = db.get_node(node)?;
    if !found.has_label(LABEL) {
        return None;
    }
    let kind = MemoryKind::ALL
        .into_iter()
        .find(|kind| found.has_label(kind.label()))?;
    let text = |key: &str| {
        found
            .get_property(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let int = |key: &str| {
        found
            .get_property(key)
            .and_then(Value::as_int64)
            .unwrap_or(0)
    };
    Some(Entry {
        node,
        kind,
        text: text("text")?,
        phrase: text("phrase"),
        weight: found
            .get_property("weight")
            .and_then(Value::as_float64)
            .unwrap_or_else(|| kind.initial_weight()),
        updated_at: int("updated_at"),
    })
}

/// Todos los recuerdos de una clase.
pub fn entries(db: &GrafeoDB, kind: MemoryKind) -> Vec<Entry> {
    db.store()
        .nodes_by_label(kind.label())
        .into_iter()
        .filter_map(|node| entry(db, node))
        .filter(|found| found.kind == kind)
        .collect()
}

/// El nodo con esa clave de deduplicación, si hay.
pub fn find_by_key(db: &GrafeoDB, key: &str) -> Option<NodeId> {
    db.find_nodes_by_property("key", &Value::from(key))
        .into_iter()
        .find(|node| db.get_node(*node).is_some())
}

/// Los nodos al otro lado de las aristas visibles de `node` en esa dirección.
fn linked(db: &GrafeoDB, node: NodeId, direction: Direction) -> Vec<NodeId> {
    db.store()
        .edges_from(node, direction)
        .filter(|(_, edge)| db.get_edge(*edge).is_some())
        .map(|(other, _)| other)
        .collect()
}

/// Los recuerdos que apuntan a un tema, hasta `limit`.
pub fn about_topic(db: &GrafeoDB, topic: NodeId, limit: usize) -> Vec<NodeId> {
    let mut seen = HashSet::new();
    linked(db, topic, Direction::Incoming)
        .into_iter()
        .filter(|node| seen.insert(*node))
        .take(limit)
        .collect()
}

/// Refuerza un recuerdo que se volvió a ver.
pub fn reinforce(db: &GrafeoDB, node: NodeId, now: i64) -> grafeo::Result<()> {
    let Some(found) = db.get_node(node) else {
        return Ok(());
    };
    let weight = found
        .get_property("weight")
        .and_then(Value::as_float64)
        .unwrap_or(0.3);
    let hits = found
        .get_property("hits")
        .and_then(Value::as_int64)
        .unwrap_or(0);
    db.set_node_property(node, "weight", Value::from(reinforced(weight)))?;
    db.set_node_property(node, "hits", Value::from(hits.saturating_add(1)))?;
    db.set_node_property(node, "updated_at", Value::from(now))
}

/// Lo que se va a crear: el texto ya validado y recortado.
pub struct NewMemory<'a> {
    pub kind: MemoryKind,
    pub text: &'a str,
    pub phrase: Option<&'a str>,
    pub conversation: Option<&'a str>,
}

/// Crea un recuerdo, o refuerza el que tenga la misma clave. Antes de crear,
/// poda la clase para que quepa.
pub fn upsert(db: &GrafeoDB, new: &NewMemory<'_>, now: i64) -> grafeo::Result<NodeId> {
    let key = key_for(new.kind, new.text);
    if let Some(node) = find_by_key(db, &key) {
        reinforce(db, node, now)?;
        return Ok(node);
    }
    prune_kind(db, new.kind, new.kind.max_nodes().saturating_sub(1), now)?;
    let terms = match new.phrase {
        Some(phrase) => normalize_terms(&format!("{phrase} {}", new.text)),
        None => normalize_terms(new.text),
    };
    let mut properties: Vec<(&str, Value)> = vec![
        ("id", Value::from(uuid::Uuid::new_v4().to_string())),
        ("key", Value::from(key)),
        ("text", Value::from(new.text)),
        ("terms", Value::from(terms)),
        ("created_at", Value::from(now)),
        ("updated_at", Value::from(now)),
        ("weight", Value::from(new.kind.initial_weight())),
        ("hits", Value::from(1i64)),
    ];
    if let Some(phrase) = new.phrase {
        properties.push(("phrase", Value::from(phrase)));
    }
    if let Some(conversation) = new.conversation {
        properties.push(("conversation", Value::from(conversation)));
    }
    db.create_node_with_props(&[LABEL, new.kind.label()], properties)
}

/// Une un recuerdo con un tema (`ABOUT`), creando o reforzando el tema. No
/// pasa de [`MAX_ABOUT`] aristas.
pub fn link_topic(db: &GrafeoDB, node: NodeId, topic: &str, now: i64) -> grafeo::Result<()> {
    let current = linked(db, node, Direction::Outgoing);
    if current.len() >= MAX_ABOUT {
        return Ok(());
    }
    let topic = upsert(
        db,
        &NewMemory {
            kind: MemoryKind::Topic,
            text: topic,
            phrase: None,
            conversation: None,
        },
        now,
    )?;
    if topic == node || current.contains(&topic) {
        return Ok(());
    }
    db.create_edge(node, topic, ABOUT).map(drop)
}

/// Borra un nodo con sus aristas.
fn delete(db: &GrafeoDB, node: NodeId) -> grafeo::Result<()> {
    for (_, edge) in db.store().edges_from(node, Direction::Both) {
        if db.get_edge(edge).is_some() {
            db.delete_edge(edge)?;
        }
    }
    db.delete_node(node).map(drop)
}

/// Deja como mucho `keep` nodos de la clase, sacando los de menor puntaje.
pub fn prune_kind(db: &GrafeoDB, kind: MemoryKind, keep: usize, now: i64) -> grafeo::Result<usize> {
    let mut all = entries(db, kind);
    if all.len() <= keep {
        return Ok(0);
    }
    all.sort_by(|a, b| a.score(now).total_cmp(&b.score(now)));
    let excess = all.len() - keep;
    for found in &all[..excess] {
        delete(db, found.node)?;
    }
    Ok(excess)
}

/// Borra los temas a los que no apunta ningún recuerdo.
pub fn remove_orphan_topics(db: &GrafeoDB) -> grafeo::Result<()> {
    for topic in entries(db, MemoryKind::Topic) {
        if linked(db, topic.node, Direction::Incoming).is_empty() {
            delete(db, topic.node)?;
        }
    }
    Ok(())
}

/// La poda por espacio: el 10 % de cada clase (al menos uno si hay), los
/// temas huérfanos, y después `gc` y `wal_checkpoint`.
pub fn prune_tenth(db: &GrafeoDB, now: i64) -> grafeo::Result<()> {
    for kind in MemoryKind::ALL {
        let count = entries(db, kind).len();
        if count > 0 {
            let drop = count.div_ceil(10);
            prune_kind(db, kind, count - drop, now)?;
        }
    }
    remove_orphan_topics(db)?;
    db.gc();
    db.wal_checkpoint()
}
