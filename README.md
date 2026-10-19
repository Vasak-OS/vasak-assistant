# vasak-assistant

El asistente de IA residente de VasakOS — **Lince**. Un servicio local que
responde desde la máquina, con el modelo que elija cada persona —desde uno
totalmente local hasta el proveedor que prefiera— y **sin tocar ningún dato sin
permiso**: cada dominio que Lince puede leer —correo, calendario, contactos,
archivos, la web— es un permiso que se concede, igual que los tokens de
`vasak-accounts`.

El diseño completo, las fases y el porqué viven en el issue ancla **#1**.

## Lo que hay hoy (fase 1)

El servicio y el contrato:

- **`protocol/`** (`vasak-assistant-protocol`): el contrato D-Bus
  (`ar.net.vasak.os.Assistant` en `/ar/net/vasak/os/Assistant`), los tipos que
  viajan, los límites y el proxy de zbus 5.
- **`daemon/`** (`vasak-assistant-daemon`, binario `vasak-assistant`): conversa
  con un modelo **local** por [Ollama](https://ollama.com). Las conversaciones
  viven en memoria y se pierden al cerrar el servicio.

Todavía no tiene ninguna capacidad sobre datos del usuario: `Capabilities` las
informa todas como **no disponibles**. Si Ollama no corre o no tiene el modelo,
`Status` lo dice con un motivo legible y `Chat` devuelve el error
`ar.net.vasak.os.Assistant.Error.Unavailable`; el servicio sigue en pie.

## Correrlo

```bash
ollama pull llama3.2          # cualquier modelo sirve
cargo run -p vasak-assistant-daemon
```

En otra terminal:

```bash
d=ar.net.vasak.os.Assistant p=/ar/net/vasak/os/Assistant
busctl --user call $d $p $d Status
id=$(busctl --user call $d $p $d NewConversation | cut -d'"' -f2)
busctl --user call $d $p $d Chat ss "$id" "Hola, ¿quién sos?"
```

Dos variables de entorno, opcionales:

| Variable | Por omisión |
|---|---|
| `VASAK_ASSISTANT_OLLAMA_URL` | `http://127.0.0.1:11434` (sólo se acepta una dirección de la propia máquina) |
| `VASAK_ASSISTANT_MODEL` | el primer modelo que tenga Ollama |

Instalado, lo arranca systemd como servicio de usuario
(`packaging/vasak-assistant.service`, parte de `graphical-session.target`) o
D-Bus la primera vez que alguien le habla
(`packaging/ar.net.vasak.os.Assistant.service`).
