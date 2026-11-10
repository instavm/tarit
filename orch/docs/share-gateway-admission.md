# Share gateway admission

Each gateway node admits at most 256 concurrent share requests, including
unknown-slug lookups. After authorization, that same slot remains held while
stored share ownership is used to enforce 32 requests per tenant and 8 per
share. These are new fixed admission budgets. Saturation returns HTTP 429
without queuing more gateway work. No deployed configuration is changed.

Public lookup authorization has a 10-second response deadline (HTTP 503).
Cancellation and timeout retain the global slot until outstanding lookup work
actually finishes, including a blocked SQLite lookup. Unknown or unauthorized
slugs never consume a tenant or share slot. A stuck database can exhaust the
finite global capacity until its work completes.

HTTP response bodies retain admission until dropped. WebSocket bridges retain
it through the actual bridge lifetime, including after upgrade headers are
sent. Owner-side proxying independently applies the same node budgets before
activation or upstream connection; a cross-node request uses capacity on both
nodes. An owner saturation response remains 429 through the forwarding node.

These bounds limit concurrent gateway work; they are not DDoS protection or a
request-rate policy. They do not provide per-client fairness, change existing
WebSocket message limits, or bound the external HTTP server's pre-handler
connection/header work. Internal peer authentication and route admission remain
separate from the public pre-lookup budget.
