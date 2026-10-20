//! Lo que nunca entra en la memoria, y cómo se normalizan los términos de
//! búsqueda. Funciones puras, sin la base.
//!
//! - [`contains_secret`]: claves, tokens, contraseñas, tarjetas. Si el mensaje
//!   de la persona tiene uno, ese turno no se aprende; si un recuerdo propuesto
//!   tiene uno, se descarta.
//! - [`is_user_data`]: datos de la persona que tienen su propio servicio o que
//!   la identifican (correos, teléfonos, rutas, cuentas bancarias, fechas
//!   absolutas). La memoria guarda cómo es la persona, no sus datos.
//!
//! Los dos se equivocan a propósito hacia el lado de descartar: un recuerdo de
//! menos no rompe nada, uno con un secreto sí.

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

fn regex(pattern: &str) -> Regex {
    // Los patrones son constantes de este archivo: si no compilan, es un error
    // de programación que salta en la primera prueba.
    Regex::new(pattern).expect("patrón válido")
}

/// Prefijos de claves conocidas, JWT, llaves privadas, `Bearer`, URL con
/// usuario y contraseña, y asignaciones o frases del tipo «mi clave es».
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?:\bsk-[A-Za-z0-9_-]{8,}",
        r"|\bgh[pousr]_[A-Za-z0-9]{16,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{16,}",
        r"|\bglpat-[A-Za-z0-9_-]{16,}",
        r"|\bxox[a-z]?-[A-Za-z0-9-]{8,}",
        r"|\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
        r"|\bAIza[0-9A-Za-z_-]{20,}",
        r"|\bhf_[A-Za-z0-9]{16,}",
        r"|\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}",
        r"|BEGIN [A-Z ]*PRIVATE KEY",
        r"|(?i:\bbearer\s+\S{6,})",
        r"|(?i:\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@)",
        r"|\b[0-9A-Fa-f]{32,}\b",
        r"|(?i:\b(?:password|passwd|contrase(?:ñ|n)a|clave|pin|token|secret|api[ _-]?key)\s*[:=]\s*\S+)",
        r"|(?i:\b(?:mi|la|el)\s+(?:contrase(?:ñ|n)a|clave|pin|c(?:ó|o)digo|token)\s+es\b))",
    ))
});

/// Candidatos a base64: se cuentan sólo si mezclan letras y dígitos (el
/// crate `regex` no tiene lookahead para decirlo en el patrón).
static BASE64: LazyLock<Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9+/_-]{24,}={0,2}"));

/// Candidatos a número de tarjeta: 13 a 19 dígitos con espacios o guiones.
static CARD: LazyLock<Regex> = LazyLock::new(|| regex(r"\b\d(?:[ -]?\d){12,18}\b"));

/// Si el texto tiene algo que parece un secreto.
pub fn contains_secret(text: &str) -> bool {
    SECRET.is_match(text)
        || BASE64.find_iter(text).any(|found| {
            let found = found.as_str();
            found.bytes().any(|b| b.is_ascii_alphabetic())
                && found.bytes().any(|b| b.is_ascii_digit())
        })
        || CARD.find_iter(text).any(|found| {
            let digits: Vec<u8> = found
                .as_str()
                .bytes()
                .filter(u8::is_ascii_digit)
                .map(|b| b - b'0')
                .collect();
            (13..=19).contains(&digits.len()) && luhn(&digits)
        })
}

/// El dígito verificador de Luhn, sobre dígitos ya separados (0–9).
pub fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            let d = u32::from(d);
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                d
            }
        })
        .sum();
    !digits.is_empty() && sum.is_multiple_of(10)
}

/// Correos, rutas absolutas o de la carpeta personal, `file://`, `C:\`, URLs
/// con ruta o consulta, CBU y CVU (22 dígitos), IBAN y fechas absolutas.
static USER_DATA: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?:[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
        r"|(?:^|[\s(\[«“])(?:~/|/)[A-Za-z0-9._-]",
        r"|(?i:\bfile://)",
        r"|\b[A-Za-z]:\\",
        r"|(?i:\b(?:https?|ftp)://[^\s/?#]+[/?#][^\s]*[^\s/?#])",
        r"|\b\d{22}\b",
        r"|\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,4})?\b",
        r"|\b\d{1,2}[/.-]\d{1,2}[/.-]\d{2,4}\b",
        r"|\b\d{4}-\d{2}-\d{2}\b",
        r"|(?i:\b\d{1,2}\s+de\s+(?:enero|febrero|marzo|abril|mayo|junio|julio|agosto|septiembre|setiembre|octubre|noviembre|diciembre)\b)",
        r"|(?i:\b(?:enero|febrero|marzo|abril|mayo|junio|julio|agosto|septiembre|setiembre|octubre|noviembre|diciembre)\s+(?:de\s+)?\d{4}\b))",
    ))
});

/// Candidatos a teléfono: se cuentan si tienen 8 dígitos o más.
static PHONE: LazyLock<Regex> = LazyLock::new(|| regex(r"\+?\d[\d\s().-]{6,}\d"));

/// Si el texto tiene datos de la persona que la memoria no guarda.
pub fn is_user_data(text: &str) -> bool {
    USER_DATA.is_match(text)
        || PHONE
            .find_iter(text)
            .any(|found| found.as_str().bytes().filter(u8::is_ascii_digit).count() >= 8)
}

/// Los términos con los que se indexa y se busca: descompuesto (NFD), sin
/// diacríticos, en minúsculas y con los espacios colapsados.
pub fn normalize_terms(text: &str) -> String {
    let folded: String = text
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}
