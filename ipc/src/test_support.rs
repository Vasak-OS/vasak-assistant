//! Ayudantes de prueba compartidos por el demonio y el broker: la captura del
//! registro. Sólo se compila con la bandera `test-support` (las pruebas de los
//! dos crates la encienden); nunca entra en un binario.
//!
//! Cada crate llama a [`capture_logs`] con su propio filtro de bibliotecas
//! ([`crate::logging::library_filter`]); el resto —el suscriptor global, el
//! destino por hilo y el guardián— es el mismo para ambos.

use std::cell::RefCell;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing_subscriber::filter::Targets;

/// Lo que se escribió en el registro mientras dura la captura, de cualquier
/// módulo y en cualquier nivel (hasta `trace`), sin colores.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Todo el texto capturado hasta ahora.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

/// Captura el registro del hilo actual hasta que se suelta el guardián, con
/// `filter` como filtro de bibliotecas y todo lo del crate hasta `trace`. Con
/// `#[tokio::test]` (un solo hilo) cubre también las tareas lanzadas, y lo que
/// corre en `spawn_blocking` a través de [`with_capture`], que le pasa al hilo
/// de trabajo la captura de quien lo lanza. Con un runtime de varios hilos,
/// sólo lo del hilo que llama.
///
/// El suscriptor global se instala una sola vez por proceso, con el `filter` de
/// la primera llamada; cada crate pasa siempre el mismo, así es determinista.
/// Con un `set_default` por prueba, cada una que instala o suelta el suyo hacía
/// que `tracing` recalculara el interés mientras otras registraban, y a veces
/// una línea se perdía.
pub fn capture_logs(filter: Targets) -> (LogCapture, CaptureGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(|| CurrentCapture)
                .with_ansi(false)
                .with_filter(filter),
        );
        // Si otra cosa ya puso uno global, las capturas quedan vacías y la
        // prueba lo dice.
        let _ = subscriber.try_init();
    });
    let log = LogCapture::default();
    let previous = CAPTURE.with(|current| current.replace(Some(log.clone())));
    (log, CaptureGuard { previous })
}

thread_local! {
    /// La captura activa de este hilo, si hay.
    static CAPTURE: RefCell<Option<LogCapture>> = const { RefCell::new(None) };
}

/// Mientras vive, lo que se registra en este hilo va a su captura. Al soltarse
/// vuelve la que hubiera antes.
pub struct CaptureGuard {
    previous: Option<LogCapture>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CAPTURE.with(|current| *current.borrow_mut() = previous);
    }
}

/// La captura activa de este hilo, para pasarla a otro (`memory::blocking`).
pub fn current_capture() -> Option<LogCapture> {
    CAPTURE.with(|current| current.borrow().clone())
}

/// Corre `work` con `capture` como captura activa de este hilo.
pub fn with_capture<T>(capture: Option<LogCapture>, work: impl FnOnce() -> T) -> T {
    let previous = CAPTURE.with(|current| current.replace(capture));
    let _restore = CaptureGuard { previous };
    work()
}

/// El escritor del suscriptor global: va a la captura activa del hilo, o a
/// ningún lado.
struct CurrentCapture;

impl std::io::Write for CurrentCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|current| {
            if let Some(log) = &*current.borrow() {
                log.0.lock().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
