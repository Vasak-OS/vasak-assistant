# Changelog

Todas las novedades de este repositorio, por paquete.

## vasak-assistant 0.1.0

Primera versión: el servicio y el contrato (fase 1 de #1). Conversación con un
modelo local, sin ninguna capacidad sobre datos del usuario.

- **`protocol`** — el contrato D-Bus (`ar.net.vasak.os.Assistant`): las
  direcciones del bus, los tipos que viajan (`Message`, `Role`, `ModelInfo`,
  `ServiceStatus`, `Capability`), los dominios, los límites, los errores con
  nombre propio (`ar.net.vasak.os.Assistant.Error.*`) y el proxy `Assistant`.
- **`daemon`** — el servicio:
  - El motor de conversación (`conversations`): historial en memoria por
    conversación, con topes de tamaño de mensaje, de cantidad de conversaciones
    y de historial.
  - La interfaz común de los backends de modelo (`backend`) y el primero, Ollama
    por HTTP en la propia máquina (`ollama`), con tiempos máximos, tope de
    tamaño de respuesta, sin redirecciones y sin proxy.
  - La configuración mínima (`config`): la dirección de Ollama, que tiene que
    ser de la propia máquina, y el modelo, por variables de entorno.
  - La interfaz D-Bus (`service`): `Status`, `Capabilities` (todas no
    disponibles en esta fase), `Models`, `NewConversation`, `Conversations`,
    `History`, `Chat` y `DeleteConversation`.
- **`packaging`** — la unidad de usuario de systemd y la activación por D-Bus.
