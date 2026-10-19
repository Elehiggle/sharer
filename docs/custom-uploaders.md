# Custom uploaders

ShareR can upload to any HTTP or HTTPS endpoint that follows this small contract. Choose **Custom** under **Settings → Upload**, or configure it from the command line:

```bash
ShareR --init https://uploads.example/upload
```

One optional request header can carry authentication:

```bash
ShareR --init https://uploads.example/upload --header "Authorization: Bearer token"
```

The header value is stored in ShareR's private per-user database. It is never written to logs.

Headers are bound to the endpoint where they were configured, including its scheme, host, port, path, and query parameters. Equivalent URL spellings (such as the default HTTPS port) share the same binding; fragments are ignored. Changing the endpoint clears the old header. Re-enter the header to authorize it for the new endpoint. Existing saved headers are bound to their existing endpoint when loaded after upgrading.

Endpoint URLs must not contain a username or password. Use the optional request header for authentication.

## Request

ShareR sends an HTTP `POST` with the file in a multipart field named `file`. The configured lifetime is added as a `time` query parameter in seconds. Existing unrelated query parameters are preserved.

Custom lifetimes can range from 1 second to 1 week.

Uploads do not follow HTTP redirects. Configure the final upload endpoint directly. This applies to built-in providers as well as Custom uploaders.

## Response

A successful upload returns JSON in this shape:

```json
{
  "data": {
    "originalName": "capture.png",
    "size": 12345,
    "link": "https://files.example/capture.png",
    "deleteLink": "https://files.example/delete/capture.png/token",
    "expiresAt": "2026-09-09T00:00:00Z"
  }
}
```

`link` and `deleteLink` must be absolute HTTP or HTTPS URLs. `deleteLink` is a sensitive capability and should be impossible to guess. ShareR stores both links in local history.

Response bodies larger than 64 KiB are rejected.

## One-off overrides

The saved endpoint and request header can be replaced for one upload:

```bash
ShareR archive.tar.zst --uploader-url https://other.example/upload
ShareR archive.tar.zst --header "Authorization: Bearer token"
ShareR archive.tar.zst --uploader-url https://other.example/upload --header "Authorization: Bearer other-token"
```

An endpoint override does not forward a saved header to a different endpoint. An explicit `--header` authorizes that header for the endpoint used by this command.

Use `ShareR --help` for the complete CLI reference.
