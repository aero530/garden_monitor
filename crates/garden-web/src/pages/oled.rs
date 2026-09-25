//! What the counter display will actually show.
//!
//! A 128×64 white-on-black OLED, drawn at true coordinates and scaled up. Every
//! element here has an exact x/y in display pixels, so this page is both a preview and
//! the layout spec the firmware implements — a mock drawn "roughly like the screen"
//! would be worth very little, because the whole difficulty of a 128×64 panel is that
//! nothing fits and you have to decide what to drop.
//!
//! Rendered as SVG rather than scaled HTML so the coordinates in this file are the
//! coordinates in the firmware. The font is the one real approximation: a browser
//! cannot render Adafruit's 6×8 bitmap glyphs, so the widths are modelled
//! (`CHAR_W`) and the text is clipped where the real panel would clip it. If a string
//! overflows here, it overflows there.
//!
//! **Every state gets a frame, including the boring ones.** A display is mostly idle,
//! and "nothing to do" and "I have lost contact with the brain" are the two screens
//! that will be up the longest — and the two easiest to leave until last and get
//! wrong.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, State};
use axum::{Router, routing::get};
use garden_auth::Permission;
use garden_core::GardenId;
use maud::{Markup, PreEscaped, html};

pub fn routes() -> Router<AppState> {
    Router::new().route("/gardens/{id}/display/preview", get(page))
}

/// Panel geometry. The firmware's constants, and the reason this file is worth having.
const W: i32 = 128;
const H: i32 = 64;
/// Adafruit's built-in 5×7 glyph in a 6×8 cell — the default on every SSD1306 driver.
const CHAR_W: i32 = 6;
const LINE_H: i32 = 8;
/// Status bar depth, including its rule.
const BAR_H: i32 = 10;
/// Footer baseline, leaving the bottom row for the counter and the prompt.
const FOOT_Y: i32 = 62;
/// Icon box: square, left-aligned, vertically centred in the body.
const ICON: i32 = 28;
/// Body lines available for the rule's reasoning, beneath the title.
const BODY_LINES: usize = 3;
/// Left edge of the text column, clear of the icon box.
const TEXT_X: i32 = 34;

/// How many characters fit in `px` pixels.
const fn fits(px: i32) -> usize {
    (px / CHAR_W) as usize
}

/// Truncate the way the panel does — by dropping glyphs, not by wrapping.
///
/// An ellipsis costs a whole character cell out of twenty-one, which is a poor trade
/// on a line this short; the firmware simply stops drawing. Modelled exactly, so a
/// rationale that loses its last word here loses it there too.
fn clip(text: &str, cells: usize) -> String {
    text.chars().take(cells).collect()
}

/// Break a rationale into panel lines, on word boundaries where it can.
fn wrap(text: &str, cells: usize, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if candidate.chars().count() <= cells {
            current = candidate;
        } else {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            // A single word longer than the line is cut rather than dropped: the
            // start of "conditioner" is still readable, its absence is not.
            current = clip(word, cells);
        }
        if out.len() == lines {
            return out;
        }
    }
    if !current.is_empty() && out.len() < lines {
        out.push(current);
    }
    out
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let garden_id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    actor.require(garden_id, Permission::ViewGarden)?;
    let garden = state.store.find_garden(garden_id).await?.ok_or(AppError::NotFound)?;

    let now = state.now();
    let snapshot = crate::state::build(&state.store, &garden, now).await?;
    let tasks: Vec<garden_store::tasks::TaskRecord> = state
        .store
        .tasks_for(garden.id)
        .await?
        .into_iter()
        .filter(|t| t.is_actionable(now))
        .filter(|t| t.target == garden_core::Target::Garden.to_string())
        .collect();

    let water = snapshot
        .capabilities
        .contains(garden_core::Capability::WaterLevel)
        .then(|| {
            (snapshot.tank_geometry.fill_fraction(snapshot.tank.volume_l) * 100.0).round() as i32
        });
    let online = !snapshot.capabilities.is_empty();
    let name = garden.name.clone();

    Ok(ui::page(
        "Display preview",
        Some(&actor),
        html! {
            p.muted.small { a href=(format!("/gardens/{}", garden.id)) { "← " (name) } }
            h1 { "Counter display" }
            p.small.muted {
                "128×64 monochrome OLED, drawn at true pixel coordinates and scaled ×4. "
                "The screen cycles through these frames; the button clears whichever one "
                "is showing."
            }

            @if tasks.is_empty() {
                h2 { "Nothing outstanding" }
                (frame(&idle_frame(&name, water, online)))
                p.small.muted {
                    "What is up almost all of the time. Worth it being pleasant to have "
                    "on a worktop rather than a blank panel that looks broken."
                }
            } @else {
                h2 { (tasks.len()) @if tasks.len() == 1 { " frame" } @else { " frames" } }
                @for (i, task) in tasks.iter().enumerate() {
                    (frame(&task_frame(&name, water, online, task, i + 1, tasks.len())))
                }
            }

            h2 { "The states that are easy to forget" }
            div.row style="flex-wrap:wrap; gap:1rem; align-items:flex-start" {
                div {
                    (frame(&idle_frame(&name, water, online)))
                    p.small.muted style="margin:0.3rem 0 0" { "Nothing to do" }
                }
                div {
                    (frame(&offline_frame(&name)))
                    p.small.muted style="margin:0.3rem 0 0" {
                        "No contact with the brain — distinct from " strong { "nothing to do" }
                        ", which otherwise looks identical and means the opposite"
                    }
                }
            }

            div.card {
                h3 style="margin-top:0" { "Layout, in display pixels" }
                ul.small.muted style="margin:0" {
                    li { "Status bar " code { "y 0–9" } " — name, tank %, a dot when the data is stale" }
                    li { "Icon box " code { "x 2–29, y 14–41" } " — " (ICON) "×" (ICON) " glyph per task kind" }
                    li { "Title " code { "x 34, y 22" } " — the short title, clipped at " (crate::pages::display::PANEL_CELLS) " characters" }
                    li { "Reason " code { "x 34, y 32/40/48" } " — " (BODY_LINES) " lines of " (fits(W - TEXT_X)) ", word-wrapped" }
                    li { "Footer " code { "y " (FOOT_Y) } " — position in the cycle, and the button prompt" }
                }
            }
        },
    ))
}

/// One rendered panel, as SVG at native size and scaled by CSS.
///
/// `shape-rendering: crispEdges` matters: without it the browser antialiases the
/// one-pixel rules into grey, and a monochrome panel has no grey.
fn frame(inner: &str) -> Markup {
    html! {
        div style="margin:0.75rem 0" {
            svg viewBox=(format!("0 0 {W} {H}"))
                width=(W * 4) height=(H * 4)
                style="background:#000; border-radius:6px; shape-rendering:crispEdges; \
                       image-rendering:pixelated; display:block"
                xmlns="http://www.w3.org/2000/svg" {
                (PreEscaped(inner))
            }
        }
    }
}

/// White text at a panel coordinate.
fn text(x: i32, y: i32, size: i32, body: &str) -> String {
    format!(
        "<text x='{x}' y='{y}' fill='#fff' font-family='monospace' font-size='{size}' \
         style='font-variant-ligatures:none'>{}</text>",
        escape(body)
    )
}

fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Name, tank percentage, and a staleness dot.
fn status_bar(name: &str, water: Option<i32>, online: bool) -> String {
    let right = match water {
        Some(pct) => format!("{pct}%"),
        None => "--".to_string(),
    };
    // The name yields to the reading, not the other way round: you know what your
    // garden is called, and the panel is the only place the tank level is shown.
    let reserved = (right.chars().count() as i32 + 2) * CHAR_W;
    let name_cells = fits(W - reserved);

    let mut svg = text(2, LINE_H - 1, 8, &clip(name, name_cells));
    svg += &text(W - reserved + CHAR_W, LINE_H - 1, 8, &right);
    if !online {
        // A filled square rather than a word. There is no room for a word, and the
        // absence of the tank reading already says most of it.
        svg += &format!("<rect x='{}' y='2' width='4' height='4' fill='#fff'/>", W - 6);
    }
    svg += &format!(
        "<rect x='0' y='{}' width='{W}' height='1' fill='#fff'/>",
        BAR_H - 1
    );
    svg
}

/// A task's icon, as a simple glyph the firmware can hold as a bitmap.
///
/// Deliberately crude shapes rather than detailed art: at 28×28 monochrome, detail
/// turns to noise, and these have to be distinguishable at a glance from across a
/// kitchen.
fn icon(kind: &str, x: i32, y: i32) -> String {
    let c = |dx: i32, dy: i32| (x + dx, y + dy);
    let (cx, cy) = c(ICON / 2, ICON / 2);
    match kind {
        // A droplet: triangle over a circle.
        "add water" => format!(
            "<path d='M {cx} {} L {} {} L {} {} Z' fill='#fff'/>\
             <circle cx='{cx}' cy='{}' r='8' fill='#fff'/>",
            y + 2,
            cx - 8,
            cy + 2,
            cx + 8,
            cy + 2,
            cy + 5
        ),
        // A beaker.
        "add plant food" => format!(
            "<path d='M {} {} L {} {} L {} {} L {} {} Z' fill='none' stroke='#fff' \
             stroke-width='2'/><rect x='{}' y='{}' width='14' height='7' fill='#fff'/>",
            cx - 5, y + 3, cx - 5, cy - 2, cx - 10, y + ICON - 3,
            cx + 10, y + ICON - 3,
            cx - 7, y + ICON - 11
        ),
        "add water conditioner" => format!(
            "<rect x='{}' y='{}' width='12' height='20' rx='2' fill='none' stroke='#fff' \
             stroke-width='2'/><rect x='{}' y='{}' width='12' height='8' fill='#fff'/>\
             <rect x='{}' y='{}' width='6' height='4' fill='#fff'/>",
            cx - 6, y + 6, cx - 6, y + ICON - 10, cx - 3, y + 2
        ),
        // Scissors: two crossed blades.
        "prune roots" => format!(
            "<line x1='{}' y1='{}' x2='{}' y2='{}' stroke='#fff' stroke-width='2'/>\
             <line x1='{}' y1='{}' x2='{}' y2='{}' stroke='#fff' stroke-width='2'/>\
             <circle cx='{}' cy='{}' r='4' fill='none' stroke='#fff' stroke-width='2'/>\
             <circle cx='{}' cy='{}' r='4' fill='none' stroke='#fff' stroke-width='2'/>",
            cx - 8, y + 3, cx + 7, cy + 6,
            cx + 8, y + 3, cx - 7, cy + 6,
            cx - 7, y + ICON - 5, cx + 7, y + ICON - 5
        ),
        // A tub with a waterline.
        "refresh tank" => format!(
            "<path d='M {} {} L {} {} L {} {} L {} {} Z' fill='none' stroke='#fff' \
             stroke-width='2'/><rect x='{}' y='{}' width='16' height='5' fill='#fff'/>",
            cx - 11, cy - 4, cx + 11, cy - 4, cx + 7, y + ICON - 3, cx - 7, y + ICON - 3,
            cx - 8, cy + 2
        ),
        // A sponge.
        "deep clean" => format!(
            "<rect x='{}' y='{}' width='22' height='15' rx='4' fill='#fff'/>\
             <circle cx='{}' cy='{}' r='2' fill='#000'/>\
             <circle cx='{cx}' cy='{}' r='2' fill='#000'/>\
             <circle cx='{}' cy='{}' r='2' fill='#000'/>",
            cx - 11, cy - 6, cx - 6, cy - 1, cy + 3, cx + 6, cy - 2
        ),
        // Anything unrecognised: an exclamation, which is honest and still actionable.
        _ => format!(
            "<rect x='{}' y='{}' width='4' height='14' fill='#fff'/>\
             <rect x='{}' y='{}' width='4' height='4' fill='#fff'/>",
            cx - 2, y + 3, cx - 2, y + ICON - 6
        ),
    }
}

fn task_frame(
    name: &str,
    water: Option<i32>,
    online: bool,
    task: &garden_store::tasks::TaskRecord,
    position: usize,
    total: usize,
) -> String {
    let mut svg = status_bar(name, water, online);
    svg += &icon(&task.kind, 2, 14);

    let text_x = TEXT_X;
    let cells = crate::pages::display::PANEL_CELLS;
    // The short title, not the stored label — see `display::panel_title`.
    svg += &text(text_x, 22, 8, &clip(crate::pages::display::panel_title(&task.kind), cells));
    // Three lines, not two. The mock showed "the tank has not been refreshed yet"
    // landing as "the tank has / not been", which reads as a sentence that broke
    // rather than one that was shortened. The space was already there: the body ends
    // at y 48 and the footer baseline is 62.
    for (i, line) in wrap(&task.rationale, cells, BODY_LINES).into_iter().enumerate() {
        svg += &text(text_x, 32 + (i as i32 * LINE_H), 7, &line);
    }

    // The prompt only appears when there is something to clear, so a press on an
    // idle screen is visibly not a thing rather than a thing that did nothing.
    svg += &text(2, FOOT_Y, 7, &format!("{position}/{total}"));
    svg += &text(W - (11 * CHAR_W), FOOT_Y, 7, "HOLD = DONE");
    svg
}

fn idle_frame(name: &str, water: Option<i32>, online: bool) -> String {
    let mut svg = status_bar(name, water, online);
    svg += &text(2, 34, 8, "Nothing to do.");
    svg += &text(2, 46, 7, "Everything is on track.");
    svg
}

fn offline_frame(name: &str) -> String {
    let mut svg = status_bar(name, None, false);
    svg += &text(2, 34, 8, "No contact.");
    svg += &text(2, 46, 7, "Showing last known state.");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_and_the_panel_agree_on_how_much_fits() {
        // `display::panel_title` shortens titles to `PANEL_CELLS`, and this file draws
        // them. The two were equal by coincidence; a change to the icon box or the
        // text column would have silently made the API's idea of "short enough"
        // wrong, and the first symptom would have been clipped words on the hardware.
        assert_eq!(
            crate::pages::display::PANEL_CELLS,
            fits(W - TEXT_X),
            "the API shortens to a width the panel does not have"
        );
    }

    #[test]
    fn a_line_holds_twenty_one_characters() {
        // 128 / 6. The number every other decision on this panel is downstream of, so
        // it is worth pinning rather than rediscovering.
        assert_eq!(fits(W), 21);
        // And the body column, which is what the rationale actually gets.
        assert_eq!(fits(W - TEXT_X), 15);
    }

    #[test]
    fn text_is_clipped_rather_than_allowed_to_overflow() {
        // An overflowing string on an SSD1306 wraps into whatever is drawn below it
        // and corrupts the line under it. Better a cut word than a ruined frame.
        let long = "tank at 22% (3.4 L), using 0.50 L/day";
        assert_eq!(clip(long, 10).chars().count(), 10);
        assert_eq!(clip("short", 40), "short");
    }

    #[test]
    fn wrapping_breaks_on_words_and_stops_at_the_line_budget() {
        let lines = wrap("roots have never been checked at all", 15, 2);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(line.chars().count() <= 15, "{line:?} is too wide");
        }
        assert!(lines[0].starts_with("roots"), "{lines:?}");
    }

    #[test]
    fn a_word_too_long_for_the_line_is_cut_rather_than_dropped() {
        // The start of "conditioner" is readable; its absence is not.
        let lines = wrap("supercalifragilistic dose", 10, 2);
        assert_eq!(lines[0].chars().count(), 10);
        assert!(lines[0].starts_with("supercalif"), "{lines:?}");
    }

    #[test]
    fn the_body_does_not_overrun_the_footer() {
        // Three lines from y 32 at 8px each ends at 56; the footer baseline is 62.
        // A fourth would sit on top of it, and on a monochrome panel overlapping text
        // is not faint — it is solid white nonsense.
        let last_baseline = 32 + ((BODY_LINES as i32 - 1) * LINE_H);
        assert!(
            last_baseline + LINE_H <= FOOT_Y,
            "body reaches {} and the footer starts at {FOOT_Y}",
            last_baseline + LINE_H
        );
    }

    #[test]
    fn every_real_rationale_fits_the_frame() {
        // Two lines of fifteen is 30 characters, which is not much. These are verbatim
        // from a running garden, and the check is that wrapping loses nothing a person
        // needs — the icon and title already say *what*, so the body only has to carry
        // the number.
        for rationale in [
            "roots have never been checked",
            "the tank has not been refreshed yet",
            "no conditioner on record",
            "15.1 L added since the last dose",
        ] {
            let lines = wrap(rationale, fits(W - TEXT_X), BODY_LINES);
            assert!(!lines.is_empty(), "{rationale:?} produced nothing");
            assert!(
                lines.iter().all(|l| l.chars().count() <= 15),
                "{rationale:?} -> {lines:?}"
            );
            // Nothing lost. Two lines cut "refreshed yet" off the end of a sentence,
            // which reads as broken rather than abbreviated.
            let shown: String = lines.join(" ");
            assert_eq!(
                shown.split_whitespace().count(),
                rationale.split_whitespace().count(),
                "{rationale:?} lost words: {shown:?}"
            );
        }
    }

    #[test]
    fn the_status_bar_never_sacrifices_the_reading_to_the_name() {
        // A garden called something long must not push the tank percentage off the
        // panel — the name is known to the person, the number is not.
        let svg = status_bar("A Very Long Garden Name Indeed", Some(22), true);
        assert!(svg.contains("22%"), "{svg}");
        assert!(!svg.contains("Indeed"), "the name should have been clipped: {svg}");
    }

    #[test]
    fn nothing_to_do_and_no_contact_are_distinguishable() {
        // The two screens that are up longest, and they mean opposite things: one says
        // the garden is fine, the other says we have no idea whether it is.
        let idle = idle_frame("Kitchen", Some(80), true);
        let offline = offline_frame("Kitchen");
        assert_ne!(idle, offline);
        assert!(idle.contains("Nothing to do"));
        assert!(offline.contains("No contact"));
        // The staleness marker is on one and not the other.
        assert!(offline.contains("width='4' height='4'"));
    }

    #[test]
    fn markup_from_a_garden_name_cannot_reach_the_svg() {
        // Names are operator-chosen free text and this builds SVG by concatenation.
        let svg = status_bar("<script>x</script>", None, true);
        assert!(!svg.contains("<script>"), "{svg}");
        assert!(svg.contains("&lt;script&gt;"), "{svg}");
    }

    #[test]
    fn every_garden_level_kind_draws_something_distinct() {
        // A kind falling through to the fallback glyph is a silent regression: the
        // screen still works, it just stops telling you which job it means.
        let kinds = [
            "add water",
            "add plant food",
            "add water conditioner",
            "prune roots",
            "refresh tank",
            "deep clean",
        ];
        let drawn: Vec<String> = kinds.iter().map(|k| icon(k, 2, 14)).collect();
        let fallback = icon("something else", 2, 14);
        for (kind, svg) in kinds.iter().zip(&drawn) {
            assert_ne!(svg, &fallback, "'{kind}' fell through to the fallback glyph");
        }
        for i in 0..drawn.len() {
            for j in (i + 1)..drawn.len() {
                assert_ne!(drawn[i], drawn[j], "{} and {} draw alike", kinds[i], kinds[j]);
            }
        }
    }
}
