# Changelog

Todas las novedades de este repositorio, por paquete.

## vasak-assistant 0.1.0

Primera versión: el servicio y el contrato (fase 1 de #1), con parte de la
fase 2. Conversación con un modelo —local por omisión, o un proveedor remoto si
la persona lo permite— sin ninguna capacidad sobre datos del usuario.

- **`protocol`** — el contrato D-Bus (`ar.net.vasak.os.Assistant`): las
  direcciones del bus, los tipos que viajan (`Message`, `Role`, `ModelInfo`,
  `ServiceStatus`, `Capability`), los proveedores, los dominios, los límites,
  los errores con nombre propio (`ar.net.vasak.os.Assistant.Error.*`) y el
  proxy `Assistant`.
- **`daemon`** — el servicio:
  - El motor de conversación (`conversations`): historial en memoria por
    conversación, con topes de tamaño de mensaje, de cantidad de conversaciones
    y de historial.
  - La interfaz común de los backends de modelo (`backend`) y dos backends:
    Ollama (`ollama`) y uno genérico compatible con la API de OpenAI
    (`openai`). Lo común de HTTP (`http`): tiempos máximos, tope de tamaño de
    respuesta, sin redirecciones y sin proxy; https con rustls.
  - La configuración (`config`), en la sección `assistant` de `vasak.conf`:
    backend, dirección, modelo y `allow_remote`. Local o remoto sale de la
    dirección; un remoto exige permiso explícito, https y modelo.
  - La clave del proveedor (`keyring`), leída de `vasak-keyring` por la API del
    Secret Service y atada a la dirección del backend. Nunca en disco, en el
    entorno ni en el registro.
  - La interfaz D-Bus (`service`): `Status` (con `model.local` y
    `model.host` para el indicador de «esto sale de la caja»), `Capabilities`
    (las de datos del usuario, no disponibles; `remote_backends` según
    `allow_remote`), `Models`, `NewConversation`, `Conversations`,
    `History`, `Chat` y `DeleteConversation`.
- **`packaging`** — la unidad de usuario de systemd y la activación por D-Bus.
