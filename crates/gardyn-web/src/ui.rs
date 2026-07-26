//! Layout, styling, and shared markup.
//!
//! Server-rendered with maud. No build step, no bundler, no node_modules, and no
//! JavaScript at all — every interaction here is a form post, so the whole front end
//! ships inside the binary and keeps working after six months of neglect. If a page
//! later needs partial updates, HTMX can be vendored into the binary rather than
//! pulled from a CDN, which would put a third party in the runtime path.

use gardyn_auth::Actor;
use gardyn_core::Severity;
use maud::{DOCTYPE, Markup, PreEscaped, html};

pub fn page(title: &str, actor: Option<&Actor>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) " · Gardyn" }
                style { (PreEscaped(STYLES)) }
            }
            body {
                @if let Some(actor) = actor {
                    (nav(actor))
                }
                main { (body) }
                footer {
                    p { "Gardyn · self-hosted" }
                }
            }
        }
    }
}

fn nav(actor: &Actor) -> Markup {
    html! {
        nav {
            a.brand href="/" { "🌱 Gardyn" }
            div.spacer {}
            a href="/" { "Gardens" }
            @if actor.is_admin() {
                a href="/system" { "System" }
            }
            a href="/account" { (actor.user.label()) }
            form method="post" action="/logout" style="display:inline" {
                button.link type="submit" { "Sign out" }
            }
        }
    }
}

/// A page shown before sign-in: no nav, centred card.
pub fn plain_page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) " · Gardyn" }
                style { (PreEscaped(STYLES)) }
            }
            body.centred {
                main.card-narrow { (body) }
            }
        }
    }
}

pub fn error_page(heading: &str, message: &str) -> Markup {
    plain_page(
        heading,
        html! {
            h1 { (heading) }
            p.muted { (message) }
            p { a.button href="/" { "Back to your gardens" } }
        },
    )
}

pub fn severity_pill(severity: Severity) -> Markup {
    html! {
        span.pill class=(format!("sev-{}", severity.label())) { (severity.label()) }
    }
}

pub fn health_pill(health: gardyn_store::fleet::Health) -> Markup {
    html! {
        span.pill class=(format!("health-{}", health.label())) { (health.label()) }
    }
}

/// "3 minutes ago", for heartbeats and event logs.
pub fn relative(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

const STYLES: &str = r#"
:root {
  --bg: #fbfaf7; --panel: #ffffff; --ink: #1d1f1c; --muted: #6b7167;
  --line: #e3e2dc; --accent: #2f7d4f; --accent-ink: #ffffff;
  --info: #6b7167; --advisory: #7a7420; --important: #a2620f;
  --urgent: #b3401a; --critical: #96122a;
  --up: #2f7d4f; --down: #96122a; --degraded: #a2620f; --unknown: #6b7167;
  --radius: 10px;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #14160f; --panel: #1c1f18; --ink: #eceee6; --muted: #9aa093;
    --line: #2e332a; --accent: #6fbf8b; --accent-ink: #10130d;
    --info: #9aa093; --advisory: #d3c95f; --important: #e0a35c;
    --urgent: #ef8a63; --critical: #f4707f;
    --up: #6fbf8b; --down: #f4707f; --degraded: #e0a35c; --unknown: #9aa093;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--ink);
  font: 15px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
}
body.centred { display: grid; place-items: center; min-height: 100vh; padding: 1rem; }
main { max-width: 62rem; margin: 0 auto; padding: 1.5rem 1rem 4rem; }
main.card-narrow {
  width: min(26rem, 100%); background: var(--panel); border: 1px solid var(--line);
  border-radius: var(--radius); padding: 1.75rem;
}
nav {
  display: flex; gap: 1rem; align-items: center; flex-wrap: wrap;
  padding: 0.75rem 1rem; border-bottom: 1px solid var(--line); background: var(--panel);
}
nav .spacer { flex: 1; }
nav a { color: var(--muted); text-decoration: none; }
nav a:hover { color: var(--ink); }
nav a.brand { color: var(--ink); font-weight: 620; font-size: 1.05rem; }
footer { padding: 2rem 1rem; color: var(--muted); font-size: 0.85rem; text-align: center; }
h1 { font-size: 1.5rem; margin: 0 0 0.35rem; letter-spacing: -0.01em; }
h2 { font-size: 1.05rem; margin: 2rem 0 0.75rem; }
h3 { font-size: 0.95rem; margin: 0 0 0.35rem; }
p { margin: 0 0 0.75rem; }
.muted { color: var(--muted); }
.small { font-size: 0.85rem; }
a { color: var(--accent); }

.card {
  background: var(--panel); border: 1px solid var(--line);
  border-radius: var(--radius); padding: 1rem; margin-bottom: 0.75rem;
}
.grid { display: grid; gap: 0.75rem; grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr)); }
.row { display: flex; gap: 0.75rem; align-items: center; flex-wrap: wrap; }
.row .spacer { flex: 1; }

.stat { font-variant-numeric: tabular-nums; font-size: 1.5rem; font-weight: 600; }
.stat-label { color: var(--muted); font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.04em; }

.pill {
  display: inline-block; padding: 0.1rem 0.5rem; border-radius: 999px;
  font-size: 0.75rem; font-weight: 600; border: 1px solid currentColor;
}
.sev-info { color: var(--info); } .sev-advisory { color: var(--advisory); }
.sev-important { color: var(--important); } .sev-urgent { color: var(--urgent); }
.sev-critical { color: var(--critical); }
.health-up { color: var(--up); } .health-down { color: var(--down); }
.health-degraded { color: var(--degraded); } .health-unknown { color: var(--unknown); }

label { display: block; margin: 0.75rem 0 0.25rem; font-size: 0.85rem; color: var(--muted); }
input, select, textarea {
  width: 100%; padding: 0.5rem 0.6rem; border: 1px solid var(--line);
  border-radius: 8px; background: var(--bg); color: var(--ink); font: inherit;
}
button, .button {
  display: inline-block; padding: 0.45rem 0.85rem; border-radius: 8px; border: 1px solid var(--line);
  background: var(--panel); color: var(--ink); font: inherit; cursor: pointer; text-decoration: none;
}
button.primary, .button.primary { background: var(--accent); color: var(--accent-ink); border-color: transparent; font-weight: 600; }
button.link { border: none; background: none; color: var(--muted); padding: 0; }
button.link:hover { color: var(--ink); text-decoration: underline; }
button.danger { color: var(--critical); }
button:hover, .button:hover { border-color: var(--muted); }

table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 0.55rem 0.5rem; border-bottom: 1px solid var(--line); }
th { color: var(--muted); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; font-weight: 600; }
.table-wrap { overflow-x: auto; }

.flash { padding: 0.6rem 0.8rem; border-radius: 8px; background: var(--panel); border: 1px solid var(--accent); }
.error { color: var(--critical); }
.slotgrid { display: grid; gap: 0.4rem; grid-template-columns: repeat(auto-fill, minmax(6.5rem, 1fr)); }

/* The physical tower: one CSS column per real column, slots top to bottom inside it,
   so what is on screen matches what the operator is looking at. */
.tower { display: grid; gap: 0.9rem; align-items: start; }
.tower-column { display: flex; flex-direction: column; gap: 0.4rem; min-width: 0; }
.tower-head {
  font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.06em;
  color: var(--muted); text-align: center; padding-bottom: 0.2rem;
  border-bottom: 1px solid var(--line);
}
.zone-strip { width: 3px; border-radius: 2px; flex: none; }
.zone-high { background: var(--advisory); }
.zone-medium { background: var(--accent); }
.zone-low { background: var(--line); }
.slot-row { display: flex; gap: 0.5rem; align-items: stretch; }
.slot-row > .card, .slot-row > .slot { flex: 1; min-width: 0; margin-bottom: 0; }
@media (max-width: 34rem) {
  /* One column at a time on a phone; a two-up tower is unreadable at that width. */
  .tower { grid-template-columns: 1fr !important; }
}
.slot { border: 1px solid var(--line); border-radius: 8px; padding: 0.45rem 0.5rem; background: var(--panel); font-size: 0.8rem; }
.slot.empty { color: var(--muted); border-style: dashed; }
code { font-family: ui-monospace, "Cascadia Code", monospace; font-size: 0.85em; }
.token { word-break: break-all; background: var(--bg); padding: 0.5rem; border-radius: 8px; border: 1px solid var(--line); }
"#;
