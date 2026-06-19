# Feature backlog (non-CSS)

See also docs/CSS-TODO.md for the CSS backlog.

## Web / JS APIs
- [x] **FetchData / FormData** — DONE: net::request (GET/POST/PUT/PATCH/DELETE), fetch() sends method/headers/body, FormData API + urlencoded serialization. (Multipart + File not yet.)
  - ~~original note~~ **FetchData** — (requested by user) form/data serialization for fetch. Likely `FormData`
      (`new FormData(form)`, `.append/.get/.getAll/.entries`, iterate) so forms can be submitted
      via `fetch(url, { method, body: formData })`; also wire `<form>` submit to build it.
      Confirm exact scope with user.
