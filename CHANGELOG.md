# Changelog

Todas las novedades de este repositorio, por paquete.

## vasak-assistant 0.3.0

La memoria de Lince (fase 4 de #1): el almacén, apagado por omisión. Todavía
no guarda nada de las conversaciones.

- **`protocol`** — `MemoryStatus` (`(bbst)`: encendida, disponible, motivo y
  bytes) y tres métodos nuevos en el proxy: `MemoryStatus()`,
  `SetMemoryEnabled(enabled)` y `ClearMemory()`. `Capabilities` informa
  `memory` disponible cuando está encendida y abierta.
- **`daemon`** — la memoria (`memory`): una base de grafos Grafeo embebida,
  cifrada con AES-256-GCM (archivo y WAL) en `$STATE_DIRECTORY/memory/`, con la
  llave de 32 bytes en el llavero. Encender genera la llave sólo con el llavero
  desbloqueado, la relee y crea la base en `memory.tmp/` antes de renombrarla.
  Apagar borra primero la llave y después los archivos. Al arrancar nunca se
  genera ni se borra nada solo. El llavero (`keyring`) busca y guarda por un
  mapa de atributos y suma el borrado de ítems. El binario se compila con
  `lto = "fat"` y `codegen-units = 1`.
- **`packaging`** — la unidad tiene `StateDirectory=vasak-assistant` (modo
  0700), `UMask=0077` y `MemoryMax=512M`.

## vasak-assistant 0.2.0

La clave del proveedor se guarda por el servicio, para la pantalla «Asistente»
de Configuración (fase 3 de #1).

- **`protocol`** — dos métodos nuevos en el proxy: `SetProviderKey(url, key)` y
  `HasProviderKey(url)`.
- **`daemon`** — `SetProviderKey` valida la dirección (`normalize_url`, la misma
  forma con la que después se busca la clave; un remoto sólo por https) y la
  clave, y la guarda en el llavero con `CreateItem`, reemplazando la anterior.
  Con el llavero bloqueado contesta `Unavailable` y no abre ningún diálogo.
  `HasProviderKey` dice si hay una clave utilizable para esa dirección (con más
  de una, `false`, igual que la lectura). La clave nunca va al registro ni a un
  mensaje de error.

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
