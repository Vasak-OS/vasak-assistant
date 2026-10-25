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
    let key = match (new.kind, new.phrase) {
        // Dos alias con el mismo texto y distinta frase son dos recuerdos. El
        // NUL separa sin ambigüedad: `clean` no deja caracteres de control.
        (MemoryKind::Alias, Some(phrase)) => key_for(new.kind, &format!("{phrase}\0{}", new.text)),
        _ => key_for(new.kind, new.text),
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn db() -> GrafeoDB {
        let db = GrafeoDB::new_in_memory();
        ensure_indexes(&db).unwrap();
        db
    }

    fn add(db: &GrafeoDB, kind: MemoryKind, text: &str, now: i64) -> NodeId {
        upsert(
            db,
            &NewMemory {
                kind,
                text,
                phrase: None,
                conversation: None,
            },
            now,
        )
        .unwrap()
    }

    fn texts(db: &GrafeoDB, kind: MemoryKind) -> Vec<String> {
        let mut all: Vec<String> = entries(db, kind).into_iter().map(|e| e.text).collect();
        all.sort();
        all
    }

    fn search(db: &GrafeoDB, query: &str) -> Vec<String> {
        db.text_search(LABEL, "terms", &normalize_terms(query), 10, None)
            .unwrap()
            .into_iter()
            .filter_map(|(node, _)| entry(db, node))
            .map(|e| e.text)
            .collect()
    }

    #[test]
    fn las_clases_son_siete_con_sus_nombres_y_topes() {
        assert_eq!(MemoryKind::ALL.len(), 7);
        for (name, kind) in [
            ("preference", MemoryKind::Preference),
            ("Style", MemoryKind::Style),
            ("HABIT", MemoryKind::Habit),
            ("topic", MemoryKind::Topic),
            ("correction", MemoryKind::Correction),
            ("alias", MemoryKind::Alias),
            ("conversation_summary", MemoryKind::ConversationSummary),
            ("ConversationSummary", MemoryKind::ConversationSummary),
        ] {
            assert_eq!(MemoryKind::from_name(name), Some(kind), "{name}");
        }
        assert_eq!(MemoryKind::Style.max_nodes(), 20);
        assert_eq!(MemoryKind::Habit.max_nodes(), 50);
        assert_eq!(MemoryKind::Preference.max_nodes(), 100);
        assert_eq!(MemoryKind::Topic.max_text(), 64);
        assert_eq!(MemoryKind::ConversationSummary.max_text(), 600);
        assert_eq!(MemoryKind::Correction.initial_weight(), 0.5);
        assert_eq!(MemoryKind::Style.initial_weight(), 0.5);
        assert_eq!(MemoryKind::Habit.initial_weight(), 0.3);
    }

    #[test]
    fn no_hay_clase_para_correos_eventos_contactos_archivos_ni_cuentas() {
        for name in [
            "email",
            "mail",
            "event",
            "calendar",
            "contact",
            "file",
            "account",
            "secret",
            "key",
            "password",
            "",
            "memory",
            "LinceMeta",
        ] {
            assert_eq!(MemoryKind::from_name(name), None, "{name}");
        }
        let labels: Vec<_> = MemoryKind::ALL.iter().map(|k| k.label()).collect();
        assert_eq!(
            labels,
            [
                "Preference",
                "Style",
                "Habit",
                "Topic",
                "Correction",
                "Alias",
                "ConversationSummary"
            ]
        );
    }

    #[test]
    fn un_recuerdo_guarda_solo_las_propiedades_del_esquema() {
        let db = db();
        let node = upsert(
            &db,
            &NewMemory {
                kind: MemoryKind::Alias,
                text: "El bicho es su gato",
                phrase: Some("el bicho"),
                conversation: None,
            },
            NOW,
        )
        .unwrap();

        let stored = db.get_node(node).unwrap();
        let mut keys: Vec<String> = stored
            .properties
            .iter()
            .map(|(key, _)| key.as_str().to_owned())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "created_at",
                "hits",
                "id",
                "key",
                "phrase",
                "terms",
                "text",
                "updated_at",
                "weight"
            ]
        );
        assert!(stored.has_label(LABEL) && stored.has_label("Alias"));
    }

    #[test]
    fn la_clave_ignora_mayusculas_tildes_y_espacios_pero_no_la_clase() {
        let a = key_for(MemoryKind::Habit, "Escucha  CANCIÓN vieja");
        assert_eq!(a, key_for(MemoryKind::Habit, "escucha cancion vieja"));
        assert_ne!(a, key_for(MemoryKind::Preference, "escucha cancion vieja"));
        assert_ne!(a, key_for(MemoryKind::Habit, "escucha cancion nueva"));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn el_puntaje_se_reduce_a_la_mitad_cada_noventa_dias_y_el_refuerzo_suma_un_quinto_de_lo_que_falta(
    ) {
        assert!((score(0.8, NOW, NOW) - 0.8).abs() < 1e-9);
        assert!((score(0.8, NOW - 90 * DAY, NOW) - 0.4).abs() < 1e-9);
        assert!((score(0.8, NOW - 180 * DAY, NOW) - 0.2).abs() < 1e-9);
        // Un recuerdo «del futuro» no sube el puntaje.
        assert!((score(0.8, NOW + 10 * DAY, NOW) - 0.8).abs() < 1e-9);
        assert!((reinforced(0.3) - 0.44).abs() < 1e-9);
        assert!((reinforced(0.5) - 0.6).abs() < 1e-9);
        assert!(reinforced(0.99) < 1.0);
    }

    #[test]
    fn un_recuerdo_nace_con_el_peso_de_su_clase() {
        let db = db();
        add(&db, MemoryKind::Habit, "Madruga", NOW);
        add(&db, MemoryKind::Style, "Lo trata de vos", NOW);
        add(&db, MemoryKind::Correction, "No es Java", NOW);

        let weight = |kind| entries(&db, kind)[0].weight;
        assert_eq!(weight(MemoryKind::Habit), 0.3);
        assert_eq!(weight(MemoryKind::Style), 0.5);
        assert_eq!(weight(MemoryKind::Correction), 0.5);
    }

    #[test]
    fn el_mismo_recuerdo_dos_veces_refuerza_y_no_se_duplica() {
        let db = db();
        let first = add(&db, MemoryKind::Habit, "Escucha canción vieja", NOW);
        let again = add(&db, MemoryKind::Habit, "escucha CANCION vieja", NOW + DAY);

        assert_eq!(first, again);
        let all = entries(&db, MemoryKind::Habit);
        assert_eq!(all.len(), 1);
        assert!((all[0].weight - 0.44).abs() < 1e-9);
        assert_eq!(all[0].updated_at, NOW + DAY);
        let hits = db
            .get_node(first)
            .unwrap()
            .get_property("hits")
            .and_then(Value::as_int64);
        assert_eq!(hits, Some(2));
        // La misma frase en otra clase es otro recuerdo.
        add(&db, MemoryKind::Preference, "Escucha canción vieja", NOW);
        assert_eq!(entries(&db, MemoryKind::Preference).len(), 1);
        assert_eq!(entries(&db, MemoryKind::Habit).len(), 1);
    }

    #[test]
    fn el_tope_mas_uno_poda_el_de_menor_puntaje_en_cada_clase() {
        for kind in MemoryKind::ALL {
            let db = db();
            let max = kind.max_nodes();
            // El más viejo tiene el menor puntaje.
            add(&db, kind, "el más viejo", NOW - 400 * DAY);
            for n in 1..max {
                add(&db, kind, &format!("recuerdo {n}"), NOW - DAY);
            }
            assert_eq!(entries(&db, kind).len(), max, "{kind:?}");

            add(&db, kind, "el que llega", NOW);

            let all = texts(&db, kind);
            assert_eq!(all.len(), max, "{kind:?}");
            assert!(!all.contains(&"el más viejo".to_owned()), "{kind:?}");
            assert!(all.contains(&"el que llega".to_owned()), "{kind:?}");
        }
    }

    #[test]
    fn un_recuerdo_viejo_pero_reforzado_sobrevive_a_la_poda() {
        let db = db();
        let old = add(&db, MemoryKind::Style, "viejo y reforzado", NOW - 100 * DAY);
        for _ in 0..8 {
            reinforce(&db, old, NOW).unwrap();
        }
        for n in 1..MemoryKind::Style.max_nodes() {
            add(
                &db,
                MemoryKind::Style,
                &format!("flojo {n}"),
                NOW - 30 * DAY,
            );
        }

        add(&db, MemoryKind::Style, "nuevo", NOW);

        assert!(texts(&db, MemoryKind::Style).contains(&"viejo y reforzado".to_owned()));
    }

    #[test]
    fn un_recuerdo_no_apunta_a_mas_de_tres_temas_ni_repite_uno() {
        let db = db();
        let habit = add(&db, MemoryKind::Habit, "Riega a la mañana", NOW);

        for topic in ["jardinería", "plantas", "jardineria", "mañanas", "huerta"] {
            link_topic(&db, habit, topic, NOW).unwrap();
        }

        // «jardineria» es el mismo tema que «jardinería»; «huerta» ya no entra.
        assert_eq!(
            texts(&db, MemoryKind::Topic),
            ["jardinería", "mañanas", "plantas"]
        );
        let topics = linked(&db, habit, Direction::Outgoing);
        assert_eq!(topics.len(), MAX_ABOUT);
    }

    #[test]
    fn about_topic_da_los_recuerdos_del_tema_sin_repetir_y_hasta_el_limite() {
        let db = db();
        let mut topic = None;
        for n in 0..6 {
            let habit = add(&db, MemoryKind::Habit, &format!("hábito {n}"), NOW);
            link_topic(&db, habit, "jardinería", NOW).unwrap();
            topic = topic.or_else(|| linked(&db, habit, Direction::Outgoing).first().copied());
        }
        let topic = topic.unwrap();

        assert_eq!(about_topic(&db, topic, 4).len(), 4);
        assert_eq!(about_topic(&db, topic, 100).len(), 6);
        assert_eq!(entries(&db, MemoryKind::Topic).len(), 1);
    }

    #[test]
    fn la_poda_del_diez_por_ciento_saca_el_de_menor_puntaje_de_cada_clase_y_los_temas_huerfanos() {
        let db = db();
        let mut weakest = None;
        for n in 0..10 {
            let age = if n == 0 { 300 } else { 1 };
            let node = add(
                &db,
                MemoryKind::Habit,
                &format!("hábito {n}"),
                NOW - age * DAY,
            );
            if n == 0 {
                weakest = Some(node);
                link_topic(&db, node, "solo de este", NOW).unwrap();
            }
        }
        add(&db, MemoryKind::Preference, "única", NOW);
        assert_eq!(entries(&db, MemoryKind::Topic).len(), 1);

        prune_tenth(&db, NOW).unwrap();

        assert_eq!(entries(&db, MemoryKind::Habit).len(), 9);
        assert!(entry(&db, weakest.unwrap()).is_none());
        // Al menos uno por clase que tenga: la única preferencia también cae.
        assert!(entries(&db, MemoryKind::Preference).is_empty());
        // El tema se quedó sin recuerdos y se va con ellos.
        assert!(entries(&db, MemoryKind::Topic).is_empty());
    }

    #[test]
    fn la_busqueda_encuentra_con_y_sin_tildes_en_los_dos_sentidos() {
        let db = db();
        add(
            &db,
            MemoryKind::Preference,
            "Le gusta esa canción de cuna",
            NOW,
        );
        add(
            &db,
            MemoryKind::Preference,
            "Prefiere la cancion sin tildes",
            NOW,
        );

        for query in ["canción", "cancion", "CANCIÓN", "una cancion cualquiera"] {
            let found = search(&db, query);
            assert_eq!(found.len(), 2, "{query}: {found:?}");
        }
        assert!(search(&db, "zanahoria").is_empty());
    }

    #[test]
    fn las_palabras_vacias_no_hacen_que_un_recuerdo_aparezca() {
        let db = db();
        add(&db, MemoryKind::Preference, "Prefiere el mate amargo", NOW);
        assert!(search(&db, "el de la que").is_empty());
        assert_eq!(search(&db, "el mate").len(), 1);
    }

    #[test]
    fn asegurar_los_indices_dos_veces_no_falla_y_un_alias_se_busca_por_su_frase() {
        let db = db();
        ensure_indexes(&db).unwrap();
        upsert(
            &db,
            &NewMemory {
                kind: MemoryKind::Alias,
                text: "Es su gato",
                phrase: Some("El Bicho"),
                conversation: None,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(search(&db, "bicho"), ["Es su gato"]);
    }

    #[test]
    fn un_nodo_que_no_es_un_recuerdo_no_se_lee_como_tal() {
        let db = db();
        let meta = db
            .create_node_with_props(&["LinceMeta"], [("format", 1i64)])
            .unwrap();
        let other = db
            .create_node_with_props(&[LABEL, "Desconocida"], [("text", "x")])
            .unwrap();
        assert!(entry(&db, meta).is_none());
        assert!(entry(&db, other).is_none());
        assert!(find_by_key(&db, "no existe").is_none());
    }

    #[test]
    fn el_resumen_de_una_conversacion_lleva_su_id() {
        let db = db();
        let node = upsert(
            &db,
            &NewMemory {
                kind: MemoryKind::ConversationSummary,
                text: "Hablaron de gatos",
                phrase: None,
                conversation: Some("c-1"),
            },
            NOW,
        )
        .unwrap();
        let conversation = db
            .get_node(node)
            .unwrap()
            .get_property("conversation")
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(conversation.as_deref(), Some("c-1"));
    }
}

/// La clave de un alias incluye su frase.
#[cfg(test)]
mod alias_tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn alias(db: &GrafeoDB, phrase: &str, text: &str) -> NodeId {
        upsert(
            db,
            &NewMemory {
                kind: MemoryKind::Alias,
                text,
                phrase: Some(phrase),
                conversation: None,
            },
            NOW,
        )
        .unwrap()
    }

    fn db() -> GrafeoDB {
        let db = GrafeoDB::new_in_memory();
        ensure_indexes(&db).unwrap();
        db
    }

    #[test]
    fn dos_alias_con_el_mismo_texto_y_distinta_frase_son_dos_recuerdos() {
        let db = db();

        let first = alias(&db, "el bicho", "Es su gato");
        let second = alias(&db, "la bestia", "Es su gato");

        assert_ne!(first, second);
        let mut phrases: Vec<_> = entries(&db, MemoryKind::Alias)
            .into_iter()
            .filter_map(|e| e.phrase)
            .collect();
        phrases.sort();
        assert_eq!(phrases, ["el bicho", "la bestia"]);
    }

    #[test]
    fn el_mismo_alias_dos_veces_se_refuerza_y_no_se_duplica() {
        let db = db();

        let first = alias(&db, "el bicho", "Es su gato");
        let again = alias(&db, "El  Bicho", "es su gato");

        assert_eq!(first, again);
        assert_eq!(entries(&db, MemoryKind::Alias).len(), 1);
    }

    #[test]
    fn la_frase_no_se_confunde_con_el_texto_en_la_clave() {
        let db = db();
        // «a b» + «c» no es lo mismo que «a» + «b c».
        let first = alias(&db, "a b", "c");
        let second = alias(&db, "a", "b c");
        assert_ne!(first, second);
        // Y un alias no choca con una preferencia del mismo texto.
        upsert(
            &db,
            &NewMemory {
                kind: MemoryKind::Preference,
                text: "c",
                phrase: None,
                conversation: None,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(entries(&db, MemoryKind::Alias).len(), 2);
    }
}
