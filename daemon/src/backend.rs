//! La interfaz común de los backends de modelo.
//!
//! El motor de conversación y la interfaz D-Bus sólo conocen este rasgo. Lo
//! implementan Ollama ([`crate::ollama`]), cualquier servidor compatible con la
//! API de OpenAI ([`crate::openai`]) y [`RejectedBackend`], que es lo que queda
//! cuando la configuración no se puede usar. Las pruebas pueden poner uno de
//! mentira.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};

use vasak_assistant_protocol::{Message, ModelInfo};

/// Un futuro en caja: el rasgo se usa como `dyn`, y un `async fn` en un rasgo
/// no se puede.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Por qué el backend no respondió.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// No hay con quién hablar: el proveedor no corre, no tiene el modelo, pide
    /// una clave que no hay, o la configuración no se puede usar. El texto es
    /// para la persona.
    Unavailable(String),
    /// Respondió, pero algo que no sirve: un estado HTTP de error, un cuerpo
    /// ilegible o demasiado grande, o tardó más de la cuenta.
    Failed(String),
}

pub trait ModelBackend: Send + Sync {
    /// Lo que se sabe sin preguntarle nada a nadie: proveedor, host, si es
    /// local, y el modelo configurado (vacío si se elige solo).
    fn info(&self) -> ModelInfo;

    /// Los modelos que el proveedor tiene. Sirve también para saber si está.
    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>>;

    /// El modelo con el que se va a responder: el configurado o, si no hay, uno
    /// que el proveedor tenga.
    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>>;

    /// La respuesta del modelo a la conversación, que termina con el mensaje de
    /// la persona.
    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>>;
}

/// El backend que queda cuando la configuración no se puede usar (un remoto sin
/// permiso, una dirección inválida…): responde siempre «no disponible» con el
/// motivo, así el servicio sigue en pie y el estado lo explica.
pub struct RejectedBackend {
    pub info: ModelInfo,
    pub reason: String,
}

impl RejectedBackend {
    fn unavailable(&self) -> BackendError {
        BackendError::Unavailable(self.reason.clone())
    }
}

impl ModelBackend for RejectedBackend {
    fn info(&self) -> ModelInfo {
        self.info.clone()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }

    fn chat<'a>(&'a self, _: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }
}

/// Cuántos pedidos al backend puede haber en vuelo a la vez. Los demás
/// esperan su turno: varios clientes no pueden poner al modelo (ni a la
/// máquina) a atender decenas de pedidos juntos.
pub const MAX_IN_FLIGHT: usize = 4;

/// Cuánto vale la respuesta de `active_model` antes de volver a preguntar.
/// `Status` se puede consultar seguido (un panel abierto); el backend no
/// necesita enterarse de cada vez.
pub const ACTIVE_MODEL_TTL: Duration = Duration::from_secs(5);

/// Envuelve un backend con un tope de pedidos simultáneos y una caché corta del
/// modelo activo.
pub struct Limited {
    inner: Arc<dyn ModelBackend>,
    permits: Semaphore,
    active: parking_lot::Mutex<Option<(Instant, Result<ModelInfo, BackendError>)>>,
}

impl Limited {
    pub fn new(inner: Arc<dyn ModelBackend>) -> Self {
        Self {
            inner,
            permits: Semaphore::new(MAX_IN_FLIGHT),
            active: parking_lot::Mutex::new(None),
        }
    }

    async fn permit(&self) -> Result<SemaphorePermit<'_>, BackendError> {
        // El semáforo sólo se cierra a mano, y acá nadie lo cierra.
        self.permits
            .acquire()
            .await
            .map_err(|_| BackendError::Failed("el servicio se está cerrando".into()))
    }
}

impl ModelBackend for Limited {
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async move {
            let _permit = self.permit().await?;
            self.inner.models().await
        })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            if let Some((at, cached)) = &*self.active.lock() {
                if at.elapsed() < ACTIVE_MODEL_TTL {
                    return cached.clone();
                }
            }
            let result = {
                let _permit = self.permit().await?;
                self.inner.active_model().await
            };
            *self.active.lock() = Some((Instant::now(), result.clone()));
            result
        })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let _permit = self.permit().await?;
            self.inner.chat(messages).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::model_info;

    fn rejected() -> RejectedBackend {
        RejectedBackend {
            info: model_info("gpt"),
            reason: "No se puede usar esta configuración.".into(),
        }
    }

    #[tokio::test]
    async fn el_backend_rechazado_contesta_no_disponible_con_el_motivo_a_todo() {
        let backend = rejected();
        let esperado = BackendError::Unavailable("No se puede usar esta configuración.".into());
        assert_eq!(backend.models().await.unwrap_err(), esperado);
        assert_eq!(backend.active_model().await.unwrap_err(), esperado);
        let mensaje = Message {
            role: vasak_assistant_protocol::Role::User,
            content: "hola".into(),
        };
        assert_eq!(backend.chat(&[mensaje]).await.unwrap_err(), esperado);
    }

    #[test]
    fn el_backend_rechazado_igual_dice_que_modelo_y_host_eran() {
        assert_eq!(rejected().info(), model_info("gpt"));
    }

    // ── Limited: tope de pedidos simultáneos y caché del modelo activo ────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{mpsc, Semaphore};

    /// Un backend que cuenta cuántos `chat` hay a la vez y no contesta hasta
    /// que se lo libera.
    struct Counting {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        entered: mpsc::UnboundedSender<()>,
        release: Semaphore,
        active_calls: AtomicUsize,
    }

    impl ModelBackend for Counting {
        fn info(&self) -> ModelInfo {
            model_info("")
        }

        fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
            Box::pin(async move { Ok(vec![]) })
        }

        fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
            Box::pin(async move {
                self.active_calls.fetch_add(1, Ordering::SeqCst);
                Ok(model_info("modelo"))
            })
        }

        fn chat<'a>(&'a self, _: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
            Box::pin(async move {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                self.entered.send(()).unwrap();
                self.release.acquire().await.unwrap().forget();
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok("listo".into())
            })
        }
    }

    fn counting() -> (Arc<Counting>, mpsc::UnboundedReceiver<()>) {
        let (entered, receiver) = mpsc::unbounded_channel();
        let backend = Arc::new(Counting {
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            entered,
            release: Semaphore::new(0),
            active_calls: AtomicUsize::new(0),
        });
        (backend, receiver)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn no_hay_mas_pedidos_al_backend_a_la_vez_que_el_tope_y_los_demas_esperan_su_turno() {
        let (inner, mut entered) = counting();
        let limited = Arc::new(Limited::new(inner.clone()));
        let total = MAX_IN_FLIGHT + 3;
        let tasks: Vec<_> = (0..total)
            .map(|_| {
                let limited = limited.clone();
                tokio::spawn(async move { limited.chat(&[]).await })
            })
            .collect();

        settle().await;
        for _ in 0..MAX_IN_FLIGHT {
            entered
                .try_recv()
                .expect("debían haber entrado los primeros");
        }
        assert!(entered.try_recv().is_err(), "entró uno de más");
        assert_eq!(inner.in_flight.load(Ordering::SeqCst), MAX_IN_FLIGHT);

        // Al liberar dos, entran dos de los que esperaban, no más.
        inner.release.add_permits(2);
        settle().await;
        assert!(entered.try_recv().is_ok());
        assert!(entered.try_recv().is_ok());
        assert!(entered.try_recv().is_err());

        inner.release.add_permits(total);
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), "listo");
        }
        assert_eq!(inner.peak.load(Ordering::SeqCst), MAX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn el_modelo_activo_se_pregunta_una_vez_dentro_del_plazo_de_la_cache() {
        let (inner, _entered) = counting();
        let limited = Limited::new(inner.clone());
        let primero = limited.active_model().await.unwrap();
        let segundo = limited.active_model().await.unwrap();
        assert_eq!(primero, segundo);
        assert_eq!(inner.active_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn limited_deja_pasar_la_info_y_los_errores_del_backend() {
        let rejected = Limited::new(Arc::new(rejected()));
        assert_eq!(rejected.info(), model_info("gpt"));
        let esperado = BackendError::Unavailable("No se puede usar esta configuración.".into());
        assert_eq!(rejected.models().await.unwrap_err(), esperado);
        assert_eq!(rejected.active_model().await.unwrap_err(), esperado);
        assert_eq!(rejected.chat(&[]).await.unwrap_err(), esperado);
    }
}
