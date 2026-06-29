-- Ticket-referenced paste events for the support-ticket link feature.
--
-- Append-only event log: one row per paste that happened while a
-- support ticket was the active context in the browser extension. The
-- ticket title is intentionally NOT stored - the dashboard drill-down
-- links out by reference, and analytics join ticket_ref against the
-- WHMCS datasource in Grafana, where the title already lives. Keeping
-- only the opaque reference keeps customer-adjacent text out of this
-- database.
--
-- snippet_id is intentionally NOT a foreign key (mirrors library_usage):
-- the event stays attributable even after the snippet is purged. Rows
-- are written only when the deployment opts in via
-- SNIPDESK_TICKET_LINK_ENABLED; otherwise the server ignores any ticket
-- events a client reports.

CREATE TABLE ticket_usage (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          INTEGER NOT NULL,
  -- The user who pasted. NULL after their account is deleted; the
  -- event itself stays for historical attribution.
  user_id     TEXT REFERENCES users(id) ON DELETE SET NULL,
  snippet_id  TEXT NOT NULL,
  -- Opaque ticket reference scraped from the support-tool URL (e.g. a
  -- WHMCS tid / id). An identifier only, never customer text.
  ticket_ref  TEXT NOT NULL
);

-- "Tickets a snippet was used in" (dashboard drill-down).
CREATE INDEX idx_ticket_usage_snippet ON ticket_usage(snippet_id, at DESC);
-- "Snippets used on a ticket" + the Grafana join key.
CREATE INDEX idx_ticket_usage_ref     ON ticket_usage(ticket_ref, at DESC);
CREATE INDEX idx_ticket_usage_at      ON ticket_usage(at DESC);
