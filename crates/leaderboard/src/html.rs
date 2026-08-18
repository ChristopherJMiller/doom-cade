//! Server-rendered HTML leaderboard view (`GET /`).
//!
//! Rendered from the same board queries as `GET /v1/boards`, so the page is
//! complete with JavaScript disabled; a small inline script re-fetches
//! `/v1/boards` every 30 seconds and swaps the board rows in place,
//! flipping the header pip to OFFLINE when the fetch fails. Everything is
//! inline — no external fonts, stylesheets, scripts, or requests of any
//! kind.

use protocol::{Board, BoardCategory, BoardEntry, Season};

/// Escapes text for safe embedding in HTML content and attributes.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Small unit label shown in each card's header.
fn unit(category: BoardCategory) -> &'static str {
    match category {
        BoardCategory::HighScore => "PTS",
        BoardCategory::Deepest => "MAPS",
        BoardCategory::FastestClear => "TIME",
        BoardCategory::MostKills => "KILLS",
        BoardCategory::SecretHunter => "SECRETS",
    }
}

/// Secondary context per row: depth on the score board, score elsewhere.
fn subline(category: BoardCategory, e: &BoardEntry) -> String {
    match category {
        BoardCategory::HighScore => format!("{} MAPS", e.maps_completed),
        _ => format!("{} PTS", e.run_score),
    }
}

fn render_rows(board: &Board) -> String {
    if board.entries.is_empty() {
        return "<div class=\"empty\">NO RUNS YET &mdash; <span class=\"blink\">INSERT \
                COIN</span></div>"
            .to_owned();
    }
    let mut out = String::from("<ol class=\"rows\">");
    for e in &board.entries {
        let top = if (1..=3).contains(&e.rank) {
            format!(" top{}", e.rank)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "<li class=\"row{top}\"><span class=\"rank\">{:02}</span>\
             <span class=\"ini\">{}</span><span class=\"val\">{}</span>\
             <span class=\"sub\">{}</span></li>",
            e.rank,
            esc(&e.initials),
            esc(&e.value_display),
            esc(&subline(board.category, e)),
        ));
    }
    out.push_str("</ol>");
    out
}

fn render_card(board: &Board) -> String {
    format!(
        "<section class=\"card\" aria-label=\"{title}\">\
         <header class=\"card-head\"><h2>{title}</h2>\
         <span class=\"unit\">{unit}</span></header>\
         <div class=\"card-body\" id=\"board-{slug}\">{rows}</div>\
         </section>",
        title = esc(&board.title),
        unit = unit(board.category),
        slug = board.category.slug(),
        rows = render_rows(board),
    )
}

/// Short season fingerprint for the footer.
fn fingerprint(season: &Season) -> String {
    let sha = if season.iwad_sha256.is_empty() {
        "NO-IWAD".to_owned()
    } else {
        // Truncate first, THEN escape — the sha is client-supplied text
        // like every other dynamic field, and truncating after escaping
        // could split an entity.
        esc(&season.iwad_sha256.chars().take(8).collect::<String>())
    };
    format!(
        "{sha} &middot; v{} &middot; {}",
        season.scoring_version,
        esc(&season.map_rotation_id)
    )
}

const STYLE: &str = r#"
:root{
  --bg:#0a0503; --panel:#140b07; --panel2:#1c0f08;
  --red:#ff3f2f; --red-deep:#8c1c10;
  --ochre:#c9862e; --brown:#4a2d1a;
  --ink:#eadfc8; --dim:#93765a;
  --gold:#ffb937; --silver:#b3bcc6; --bronze:#c47a3b;
  --mono:ui-monospace,"SF Mono",Menlo,Consolas,"Liberation Mono","DejaVu Sans Mono",monospace;
}
*{box-sizing:border-box;margin:0;padding:0}
html{background:var(--bg)}
body{
  font-family:var(--mono);color:var(--ink);
  background:radial-gradient(120% 90% at 50% 0%,#1a0c06 0%,var(--bg) 60%);
  min-height:100vh;padding:1.5rem 1.25rem 2.5rem;letter-spacing:.02em;
}
body::before{
  content:"";position:fixed;inset:0;z-index:39;pointer-events:none;
  background:radial-gradient(130% 100% at 50% 45%,transparent 55%,rgba(0,0,0,.55) 100%);
}
body::after{
  content:"";position:fixed;inset:0;z-index:40;pointer-events:none;
  background:repeating-linear-gradient(0deg,rgba(0,0,0,.22) 0 1px,transparent 1px 3px);
}
.marquee{
  max-width:72rem;margin:0 auto 1.6rem;display:flex;align-items:baseline;
  gap:1rem;flex-wrap:wrap;border:2px solid var(--brown);
  border-bottom-color:var(--red-deep);
  background:linear-gradient(180deg,#200e07,#120804);padding:1rem 1.25rem;
}
h1{
  font-size:clamp(1.3rem,4vw,2.1rem);letter-spacing:.35em;color:var(--red);
  text-shadow:0 0 6px rgba(255,63,47,.9),0 0 28px rgba(255,63,47,.35);
  font-weight:800;
}
.tag{color:var(--ochre);letter-spacing:.25em;font-size:.7rem}
.pip{
  margin-left:auto;font-size:.65rem;letter-spacing:.25em;
  padding:.35rem .6rem;border:1px solid var(--brown);color:var(--ochre);
}
.pip.offline{color:var(--red);border-color:var(--red-deep);animation:blink 1.1s steps(1) infinite}
.boards{
  max-width:72rem;margin:0 auto;display:grid;gap:1.1rem;
  grid-template-columns:repeat(auto-fill,minmax(19.5rem,1fr));
}
.card{
  border:1px solid var(--brown);
  background:linear-gradient(180deg,var(--panel2),var(--panel));
  box-shadow:0 0 0 1px #000,inset 0 0 40px rgba(0,0,0,.5);
}
.card-head{
  display:flex;justify-content:space-between;align-items:center;
  padding:.7rem .9rem;border-bottom:2px solid var(--red-deep);
  background:rgba(140,28,16,.12);
}
.card-head h2{
  font-size:.85rem;letter-spacing:.3em;color:var(--ink);
  text-shadow:0 0 10px rgba(255,63,47,.25);
}
.unit{font-size:.6rem;color:var(--dim);letter-spacing:.2em}
.card-body{padding:.4rem .35rem .6rem}
.rows{list-style:none}
.row{
  display:grid;grid-template-columns:2.2rem 3.9rem 1fr auto;gap:.5rem;
  align-items:baseline;padding:.42rem .55rem;font-size:.85rem;
  border-bottom:1px dotted rgba(74,45,26,.6);
}
.row:last-child{border-bottom:none}
.rank{color:var(--dim);font-size:.7rem}
.ini{letter-spacing:.25em;font-weight:700}
.val{text-align:right;color:var(--ochre);font-variant-numeric:tabular-nums}
.sub{color:var(--dim);font-size:.65rem;letter-spacing:.08em;text-align:right;min-width:5.2rem}
.top1 .rank,.top1 .ini,.top1 .val{color:var(--gold)}
.top1 .ini{text-shadow:0 0 8px rgba(255,185,55,.35)}
.top2 .rank,.top2 .ini,.top2 .val{color:var(--silver)}
.top3 .rank,.top3 .ini,.top3 .val{color:var(--bronze)}
.empty{padding:2rem .5rem;text-align:center;color:var(--dim);letter-spacing:.15em;font-size:.75rem}
.blink{color:var(--red);animation:blink 1.1s steps(1) infinite}
@keyframes blink{50%{opacity:0}}
.footer{
  max-width:72rem;margin:1.75rem auto 0;display:flex;flex-wrap:wrap;
  gap:.5rem 1.5rem;justify-content:space-between;color:var(--dim);
  font-size:.65rem;letter-spacing:.15em;border-top:1px solid var(--brown);
  padding-top:.8rem;
}
@media (prefers-reduced-motion:reduce){.blink,.pip.offline{animation:none}}
"#;

const SCRIPT: &str = r#"
(function () {
  var pip = document.getElementById('pip');
  function subline(cat, e) {
    return cat === 'high-score' ? e.maps_completed + ' MAPS' : e.run_score + ' PTS';
  }
  function pad2(n) { return n < 10 ? '0' + n : String(n); }
  function row(cat, e) {
    var li = document.createElement('li');
    li.className = 'row' + (e.rank >= 1 && e.rank <= 3 ? ' top' + e.rank : '');
    [['rank', pad2(e.rank)], ['ini', e.initials], ['val', e.value_display],
     ['sub', subline(cat, e)]].forEach(function (c) {
      var s = document.createElement('span');
      s.className = c[0];
      s.textContent = c[1];
      li.appendChild(s);
    });
    return li;
  }
  function apply(data) {
    data.boards.forEach(function (b) {
      var body = document.getElementById('board-' + b.category);
      if (!body) return;
      body.textContent = '';
      if (!b.entries.length) {
        var d = document.createElement('div');
        d.className = 'empty';
        d.appendChild(document.createTextNode('NO RUNS YET — '));
        var bl = document.createElement('span');
        bl.className = 'blink';
        bl.textContent = 'INSERT COIN';
        d.appendChild(bl);
        body.appendChild(d);
      } else {
        var ol = document.createElement('ol');
        ol.className = 'rows';
        b.entries.forEach(function (e) { ol.appendChild(row(b.category, e)); });
        body.appendChild(ol);
      }
    });
    var fp = document.getElementById('fp-season');
    if (fp) {
      var sha = data.season.iwad_sha256 ? data.season.iwad_sha256.slice(0, 8) : 'NO-IWAD';
      fp.textContent = sha + ' · v' + data.season.scoring_version +
        ' · ' + data.season.map_rotation_id;
    }
  }
  function tick() {
    fetch('/v1/boards', { cache: 'no-store' })
      .then(function (r) { if (!r.ok) { throw new Error(String(r.status)); } return r.json(); })
      .then(function (d) {
        apply(d);
        pip.textContent = 'LIVE';
        pip.className = 'pip';
      })
      .catch(function () {
        pip.textContent = 'OFFLINE';
        pip.className = 'pip offline';
      });
  }
  setInterval(tick, 30000);
})();
"#;

/// Renders the full page from a season and its boards.
pub fn render(season: &Season, boards: &[Board]) -> String {
    let mut cards = String::new();
    for board in boards {
        cards.push_str(&render_card(board));
    }
    let fp = fingerprint(season);

    let mut page = String::with_capacity(16 * 1024);
    page.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    page.push_str("<title>DOOM ARCADE &mdash; LEADERBOARD</title>");
    page.push_str("<style>");
    page.push_str(STYLE);
    page.push_str("</style></head><body>");
    page.push_str(
        "<header class=\"marquee\"><h1>DOOM ARCADE</h1>\
         <span class=\"tag\">ONE LIFE &middot; FIVE MAPS</span>\
         <span class=\"pip\" id=\"pip\">LIVE</span></header>",
    );
    page.push_str("<main class=\"boards\">");
    page.push_str(&cards);
    page.push_str("</main>");
    page.push_str(&format!(
        "<footer class=\"footer\"><span>SEASON <span id=\"fp-season\">{fp}</span></span>\
         <span>NO SAVES &middot; 35 TICS/S &middot; HURT ME PLENTY</span></footer>"
    ));
    page.push_str("<script>");
    page.push_str(SCRIPT);
    page.push_str("</script></body></html>");
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_escapes_hostile_iwad_sha() {
        // iwad_sha256 is only length-checked on POST; a markup payload
        // that becomes the current season must not inject raw HTML into
        // the footer.
        let season = Season {
            iwad_sha256: "<script>".to_owned(),
            scoring_version: 1,
            map_rotation_id: "rot-v1".to_owned(),
        };
        let fp = fingerprint(&season);
        assert!(!fp.contains("<script>"), "raw markup leaked: {fp}");
        assert!(fp.contains("&lt;script&gt;"), "sha not escaped: {fp}");

        let page = render(&season, &[]);
        assert!(
            !page.contains("<span id=\"fp-season\"><script>"),
            "footer carries unescaped markup"
        );
    }

    #[test]
    fn fingerprint_keeps_hex_sha_and_no_iwad_fallback() {
        let season = Season {
            iwad_sha256: "feedface0011223344".to_owned(),
            scoring_version: 1,
            map_rotation_id: "rot-v1".to_owned(),
        };
        assert!(fingerprint(&season).starts_with("feedface &middot;"));
        let empty = Season {
            iwad_sha256: String::new(),
            scoring_version: 1,
            map_rotation_id: "rot-v1".to_owned(),
        };
        assert!(fingerprint(&empty).starts_with("NO-IWAD"));
    }
}
