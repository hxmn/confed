# Data Center inline-comment captures

Requests the Confluence DC 9.5.4 (build 10000) page view sent to the private
`rest/inlinecomments/1.0` plugin API, captured from the browser's DevTools on
2026-10-02 and sanitized: cookies, session tokens, user agent and other
browser-only headers removed. Bodies are verbatim.

| File | Request |
|---|---|
| `create.request.json` | `POST /rest/inlinecomments/1.0/comments` — body |
| `create.response.json` | its `200` response |
| `replies.response.json` | `GET /rest/inlinecomments/1.0/comments/{id}/replies` right after create |
| `resolve.request.json` | `PUT /rest/inlinecomments/1.0/comments/{id}/resolve/true/dangling/false` — body (the whole comment, as the page view holds it) |
| `resolve.response.json` | its `200` response |
| `reply.request.json` | `POST /rest/inlinecomments/1.0/comments/{rootId}/replies?containerId={pageId}` — body |
| `reply.response.json` | its `200` response |

`serializedHighlights` is `[[text, "<node>:<n>", offset, length]]`, positions in
the page view's DOM that confed cannot reproduce; confed sends `"[]"`.

Not captured yet: reopening a resolved thread. confed does not offer it.

The browser authenticates with session cookies and `X-Requested-With:
XMLHttpRequest`; confed uses the same PAT or basic auth as REST v1 and adds
`X-Atlassian-Token: no-check`.
