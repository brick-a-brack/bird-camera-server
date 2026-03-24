# bird-camera-server
A Rust HTTP server that allows controlling webcams and DSLR cameras.

## Run

```bash
cargo run
```

The server binds to an available port automatically and prints the address, for example:

```text
bird-camera-server listening on http://0.0.0.0:49231
```

## Routes

- `GET /` returns JSON status.
- `GET /cameras` lists detected cameras via native Windows Media Foundation bindings.
- `GET /cameras/{camera_id}/photo` captures a real frame from the selected camera and returns `image/jpeg`.
- `PUT /` accepts JSON body: `{ "message": "hello" }`
- `PUT /update` accepts JSON body: `{ "message": "hello" }`

Examples:

```bash
curl http://127.0.0.1:49231/
curl http://127.0.0.1:49231/cameras
curl http://127.0.0.1:49231/cameras/0/photo --output camera.jpg
curl -X PUT http://127.0.0.1:49231/ -H "Content-Type: application/json" -d "{\"message\":\"hello\"}"
curl -X PUT http://127.0.0.1:49231/update -H "Content-Type: application/json" -d "{\"message\":\"hello\"}"
```

If your device does not expose MJPEG through Media Foundation, the endpoint returns an error message instead of a fake image.
