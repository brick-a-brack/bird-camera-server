# Toucan Camera Server

[![Official Website](docs/tags/website.svg)](https://brickfilms.com/) [![Discord](docs/tags/discord.svg)](https://discord.com/invite/mmU2sVAJUq)

**ToucanCameraServer** is an awesome, free, and open-source camera control REST API. The goal is to let users control cameras through a web API.

👉 _This project is supported by Brick à Brack, the non-profit organization that owns [Brickfilms.com](https://brickfilms.com/) - the biggest brickfilming community. You can join us; it's free and without ads!_ 🎥

- 📡 **Live view** - View the camera feed in real time (MJPEG Stream).
- 📸 **Take photos** - Take photos with any camera.
- ⚙️ **Change settings** - Update camera settings easily.

## Get started

TODO

## Authentication

Every request must be authenticated with a token.

**Generating the token**

By default, a random UUID v4 token is generated at startup. You can also provide your own:

```sh
# Auto-generated token (printed at startup)
./toucan-camera-server

# Custom token
./toucan-camera-server --token my-secret-token
```

At startup, the server prints the base URL including the token:

```
Listening on http://127.0.0.1:8080/?token=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

Opening this URL in a browser is enough to use the web UI — the token is read from the query string automatically.

**Authenticating requests**

Two methods are accepted:

| Method                  | Example                         |
| ----------------------- | ------------------------------- |
| `Authorization` header  | `Authorization: Bearer <token>` |
| `token` query parameter | `GET /cameras?token=<token>`    |

Requests with an invalid or missing token receive a `403 Forbidden` response.

## Contribute

Feel free to make pull-requests or report issues 😉

## Compatibility

| Backend                              | Windows | macOS | Linux |
| ------------------------------------ | ------- | ----- | ----- |
| Webcams macOS (AVFoundation / IOKit) | 🔴      | 🟢    | 🔴    |
| Webcams Windows (MediaFoundation)    | 🟢      | 🔴    | 🔴    |
| Webcams Linux (V4L2)                 | 🔴      | 🔴    | 🟠    |
| Canon EOS (EDSDK)                    | 🟢      | 🟢    | 🟠    |
| Nikon (Nikon SDKs)                   | 🟠      | 🟠    | 🔴    |
| Various cameras (libgphoto2)         | 🔴      | 🟠    | 🟠    |

🟢 - Supported  
🟠 - Planned  
🔴 - Not compatible / possible
