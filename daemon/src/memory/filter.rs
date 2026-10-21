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

/// Si el texto tiene algo que parece un secreto. Se mira en NFKC, así los
/// dígitos y letras de ancho completo cuentan como los comunes (también para
/// Luhn).
pub fn contains_secret(text: &str) -> bool {
    let text = &nfkc(text);
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
    let text = &nfkc(text);
    USER_DATA.is_match(text)
        || PHONE
            .find_iter(text)
            .any(|found| found.as_str().bytes().filter(u8::is_ascii_digit).count() >= 8)
}

/// El texto en NFKC: compatibilidad (ancho completo, ligaduras, formas
/// estilizadas) llevada a la forma común.
pub fn nfkc(text: &str) -> String {
    text.nfkc().collect()
}

/// Caracteres que no tienen nada que hacer en una nota y sirven para esconder
/// o reordenar texto: las categorías Unicode Cf (formato, incluidos los de
/// dirección), Zl y Zp (separadores de línea y de párrafo), Co (uso privado) y
/// los de control.
pub fn has_forbidden_chars(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_control() || is_format_or_private(c))
}

fn is_format_or_private(c: char) -> bool {
    matches!(
        u32::from(c),
        // Cf
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2
            | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
            | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
            // Zl y Zp
            | 0x2028 | 0x2029
            // Co
            | 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
    )
}

/// Marcadores de rol o de instrucciones, para que una nota no se lea como un
/// mensaje del sistema o del asistente. Se buscan en minúsculas y sin tildes.
const INJECTION_MARKERS: &[&str] = &[
    "ignor",
    "instrucci",
    "sistema",
    "system",
    "prompt",
    "regla",
    "asistente:",
    "assistant",
    "user:",
    "<|",
    "[inst]",
    "```",
    "###",
    "</",
    "notas",
];

/// Si el texto trae marcadores de rol o de instrucciones.
pub fn has_injection_markers(text: &str) -> bool {
    let folded = normalize_terms(text);
    INJECTION_MARKERS
        .iter()
        .any(|marker| folded.contains(marker))
}

/// Palabras que hablan de credenciales o de cuentas: una nota que las nombra
/// se descarta aunque no traiga el valor.
const CREDENTIAL_WORDS: &[&str] = &[
    "contrasena",
    "contrasenas",
    "clave",
    "claves",
    "password",
    "passwords",
    "pin",
    "pines",
    "codigo",
    "codigos",
    "token",
    "tokens",
    "cvv",
    "cbu",
    "cvu",
];

/// Si el texto nombra una credencial o una cuenta (palabra entera).
pub fn mentions_credentials(text: &str) -> bool {
    normalize_terms(text)
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| CREDENTIAL_WORDS.contains(&word))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cada_patron_de_secreto_se_detecta() {
        // Armado en tiempo de ejecución: un literal con forma de token de
        // GitLab lo marca el análisis de secretos (S6690).
        let glpat = format!("glpat-{}", "abcdefghijklmnop1234");
        for secreto in [
            "usá sk-proj1234abcd5678 para eso",
            "mi token es ghp_abcdefghijklmnop1234",
            "github_pat_abcdefghijklmnop1234",
            glpat.as_str(),
            "xoxb-123456789-abcdef",
            "AKIAIOSFODNN7EXAMPLE",
            "ASIAIOSFODNN7EXAMPLE",
            "AIzaSyA-abcdefghijklmnopqrstu",
            "hf_abcdefghijklmnop1234",
            "eyJhbGciOiJI.eyJzdWIiOiIx.SflKxwRJSMeKKF2QT4",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----",
            "Authorization: Bearer abcdef123456",
            "bearer   tokensecreto",
            "postgres://usuario:clave@servidor/base",
            "0123456789abcdef0123456789abcdef",
            "password: hunter2",
            "contraseña = hunter2",
            "contrasena: hunter2",
            "api_key=abc",
            "API-KEY: abc",
            "pin: 1234",
            "token=xyz",
            "secret: xyz",
            "mi contraseña es hunter2",
            "la clave es tal",
            "el pin es cuatro",
            "mi código es uno",
            "mi token es este",
            "QWxhZGRpbjpvcGVuIHNlc2FtZTEyMw",
        ] {
            assert!(contains_secret(secreto), "{secreto}");
        }
    }

    #[test]
    fn una_tarjeta_con_luhn_valido_es_secreto_y_con_un_digito_cambiado_no() {
        for tarjeta in [
            "4111111111111111",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "5500 0000 0000 0004",
            "378282246310005",
            "mi tarjeta 4012888888881881 vence pronto",
        ] {
            assert!(contains_secret(tarjeta), "{tarjeta}");
        }
        for casi in ["4111111111111112", "4111 1111 1111 1110", "1234567890123"] {
            assert!(!contains_secret(casi), "{casi}");
        }
    }

    #[test]
    fn luhn_sobre_digitos_sueltos() {
        assert!(luhn(&[7, 9, 9, 2, 7, 3, 9, 8, 7, 1, 3]));
        assert!(!luhn(&[7, 9, 9, 2, 7, 3, 9, 8, 7, 1, 4]));
        assert!(!luhn(&[]));
        assert!(luhn(&[0]));
    }

    #[test]
    fn lo_que_parece_secreto_pero_es_una_frase_normal_pasa() {
        for frase in [
            "me gusta el sk8",
            "tengo 3 gatos",
            "prefiero Python",
            "prefiero respuestas cortas",
            "me llamo Ana y soy diseñadora",
            "trabajo de 9 a 17",
            "uso Arch Linux desde 2019",
            "mi clave de sol está desafinada",
            "el pin de la tabla",
            "Prefiere que lo trate de vos",
            "internacionalizacióndelossistemasoperativos",
        ] {
            assert!(!contains_secret(frase), "{frase}");
        }
    }

    #[test]
    fn cada_clase_de_dato_de_la_persona_se_detecta() {
        for dato in [
            "escribile a ana@ejemplo.com",
            "ana.perez+lince@sub.dominio.org",
            "llamame al +54 9 11 5555-1234",
            "mi número es 11 5555 1234",
            "tel (011) 4555-1234",
            "está en /home/ana/Documentos",
            "guardalo en ~/Descargas",
            "(/etc/hosts) es el archivo",
            "abrí file:///tmp/a.txt",
            "FILE://servidor/x",
            r"el archivo C:\Users\ana",
            "mirá https://ejemplo.com/ruta/privada",
            "http://ejemplo.com?id=7",
            "https://ejemplo.com/#seccion",
            "mi CBU es 0170099220000067797370",
            "el alias de la cuenta 1234567890123456789012",
            "IBAN ES9121000418450200051332",
            "nací el 12/03/1990",
            "el 5-6-24 fue",
            "el 3.4.2021",
            "2024-05-17",
            "el 15 de marzo",
            "Es el 3 de Septiembre",
            "en marzo de 2024",
            "enero 2025",
        ] {
            assert!(is_user_data(dato), "{dato}");
        }
    }

    #[test]
    fn los_falsos_positivos_de_datos_de_la_persona_pasan() {
        for frase in [
            "me gusta el sk8",
            "tengo 3 gatos",
            "prefiero Python",
            "prefiero las respuestas cortas",
            "le interesa el 3D y la impresión",
            "usa Linux y/o Windows",
            "prefiere 24 horas",
            "trabaja de lunes a viernes",
            "le gusta marzo",
            "https://ejemplo.com",
            "le gustan las rutas con 1/2 de pendiente",
            "dice que 2 + 2 es 4",
            "tiene 1234567 puntos",
        ] {
            assert!(!is_user_data(frase), "{frase}");
        }
    }

    #[test]
    fn los_terminos_van_sin_tildes_en_minusculas_y_con_los_espacios_colapsados() {
        assert_eq!(
            normalize_terms("  Canción   NUEVA\tÑandú "),
            "cancion nueva nandu"
        );
        assert_eq!(normalize_terms("canción"), normalize_terms("cancion"));
        assert_eq!(normalize_terms(""), "");
    }
}
