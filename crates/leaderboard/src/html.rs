//! Server-rendered HTML leaderboard view (`GET /`).
//!
//! Rendered from the same board queries as `GET /v1/boards`, so the page is
//! complete with JavaScript disabled; a small inline script re-fetches
//! `/v1/boards` every 30 seconds and swaps the board rows in place,
//! flipping the header pip to OFFLINE when the fetch fails. Everything is
//! inline — no external requests of any kind: the Anta display face
//! (OFL 1.1, bundled at `assets/fonts/anta/`) is embedded as a base64 data
//! URI, and the header's DOOM fire is a ~50-line inline canvas effect that
//! degrades to a static ember glow without JS or under
//! `prefers-reduced-motion`.

use std::sync::OnceLock;

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

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Minimal standard base64 encoder (encode-only, padding included) so the
/// bundled font can become a data URI without pulling in a dependency.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        // 1 input byte -> 2 output chars, 2 -> 3, 3 -> 4; the rest is '='.
        let keep = chunk.len() + 1;
        for (i, &ix) in idx.iter().enumerate() {
            out.push(if i < keep {
                B64_ALPHABET[ix as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The bundled display face; the attract app loads the same file.
static ANTA_TTF: &[u8] = include_bytes!("../../../assets/fonts/anta/Anta-Regular.ttf");

/// `@font-face` rule with the font embedded as a data URI, built once.
fn font_face_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        format!(
            "@font-face{{font-family:'Anta';\
             src:url(data:font/ttf;base64,{}) format('truetype');\
             font-display:swap}}",
            base64(ANTA_TTF)
        )
    })
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
  --void:#080404;
  --steel-hi:#3d342a; --steel:#241d16; --steel-lo:#120d09;
  --blood:#a10e0e; --ember:#e8481c; --fire:#ff8c1a; --flare:#ffd23e;
  --bone:#e8dcc4; --dim:#8a7a64; --ochre:#c9862e;
  --gold:#ffb937; --silver:#b3bcc6; --bronze:#c47a3b;
  --mono:ui-monospace,"SF Mono",Menlo,Consolas,"Liberation Mono","DejaVu Sans Mono",monospace;
  --display:'Anta',var(--mono);
}
*{box-sizing:border-box;margin:0;padding:0}
html{background:var(--void)}
body{
  font-family:var(--mono);color:var(--bone);
  background:radial-gradient(120% 90% at 50% 0%,#160a05 0%,var(--void) 60%);
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
/* Header: bevelled steel marquee with the DOOM fire burning behind the title.
   Without JS (or with reduced motion) the fire canvas stays empty and the
   static ember gradient below carries the same read. */
.marquee{
  position:relative;overflow:hidden;
  max-width:76rem;margin:0 auto 1.4rem;
  background:
    linear-gradient(180deg,rgba(8,4,4,0) 35%,rgba(161,14,14,.28) 78%,rgba(232,72,28,.4) 100%),
    linear-gradient(180deg,#150c08,#0b0605);
  box-shadow:0 0 0 2px #000,inset 2px 2px 0 0 var(--steel-hi),inset -2px -2px 0 0 #0a0705;
}
.fire{
  position:absolute;left:0;right:0;bottom:0;width:100%;height:100%;
  image-rendering:pixelated;pointer-events:none;
}
.mq-inner{
  position:relative;z-index:2;display:flex;align-items:baseline;
  gap:.4rem 1.2rem;flex-wrap:wrap;padding:1.2rem 1.5rem 1.5rem;
}
.eyebrow{width:100%;font-size:.6rem;letter-spacing:.45em;color:var(--dim)}
h1{
  font-family:var(--display);font-weight:400;
  font-size:clamp(2rem,6vw,3.4rem);letter-spacing:.12em;line-height:1;
  background:linear-gradient(180deg,var(--flare) 0%,var(--fire) 34%,var(--ember) 58%,var(--blood) 100%);
  -webkit-background-clip:text;background-clip:text;color:transparent;
  filter:drop-shadow(0 2px 0 #000) drop-shadow(0 0 18px rgba(255,120,26,.35));
}
.tag{color:var(--ochre);letter-spacing:.28em;font-size:.7rem}
.pip{
  margin-left:auto;font-size:.65rem;letter-spacing:.25em;
  padding:.35rem .6rem;border:1px solid var(--steel-hi);color:var(--ochre);
  background:rgba(0,0,0,.45);
}
.pip.offline{color:var(--ember);border-color:var(--blood);animation:blink 1.1s steps(1) infinite}
/* Boards: chunky status-bar panels — hard black outline, raised bevel,
   recessed body well. */
.boards{
  max-width:76rem;margin:0 auto;display:grid;gap:1.2rem;
  grid-template-columns:repeat(auto-fill,minmax(19.5rem,1fr));
}
.card{
  background:linear-gradient(180deg,var(--steel),var(--steel-lo));
  box-shadow:0 0 0 2px #000,inset 2px 2px 0 0 var(--steel-hi),inset -2px -2px 0 0 #0a0705;
}
.card-head{
  display:flex;justify-content:space-between;align-items:baseline;
  padding:.75rem 1rem .55rem;border-bottom:2px solid var(--blood);
  background:linear-gradient(180deg,rgba(161,14,14,.18),rgba(161,14,14,.04));
}
.card-head h2{
  font-family:var(--display);font-weight:400;
  font-size:1.05rem;letter-spacing:.2em;color:var(--bone);
  text-shadow:0 0 12px rgba(232,72,28,.35);
}
.unit{font-size:.6rem;color:var(--dim);letter-spacing:.2em}
.card-body{
  padding:.5rem .45rem .7rem;
  box-shadow:inset 0 0 26px rgba(0,0,0,.55);
}
.rows{list-style:none}
.row{
  display:grid;grid-template-columns:2.2rem 4.4rem 1fr auto;gap:.5rem;
  align-items:baseline;padding:.46rem .55rem;font-size:.85rem;
  border-bottom:1px dotted rgba(61,52,42,.7);
}
.row:last-child{border-bottom:none}
.rank{color:var(--dim);font-size:.7rem}
.ini{font-family:var(--display);letter-spacing:.28em;font-size:1rem}
.val{
  font-family:var(--display);text-align:right;font-size:1.05rem;
  color:var(--fire);font-variant-numeric:tabular-nums;
  text-shadow:1px 1px 0 #000,0 0 10px rgba(255,140,26,.3);
}
.sub{color:var(--dim);font-size:.65rem;letter-spacing:.08em;text-align:right;min-width:5.2rem}
.top1 .rank,.top1 .ini{color:var(--gold)}
.top1 .val{color:var(--gold);text-shadow:1px 1px 0 #000,0 0 12px rgba(255,185,55,.45)}
.top1 .ini{text-shadow:0 0 10px rgba(255,185,55,.4)}
.top2 .rank,.top2 .ini,.top2 .val{color:var(--silver)}
.top3 .rank,.top3 .ini,.top3 .val{color:var(--bronze)}
.empty{padding:2rem .5rem;text-align:center;color:var(--dim);letter-spacing:.15em;font-size:.75rem}
.blink{color:var(--ember);animation:blink 1.1s steps(1) infinite}
@keyframes blink{50%{opacity:0}}
.footer{
  max-width:76rem;margin:1.75rem auto 0;display:flex;flex-wrap:wrap;
  gap:.5rem 1.5rem;justify-content:space-between;color:var(--dim);
  font-size:.65rem;letter-spacing:.15em;border-top:1px solid var(--steel-hi);
  padding-top:.8rem;
}
@media (prefers-reduced-motion:reduce){.blink,.pip.offline{animation:none}}
"#;

const SCRIPT: &str = r#"
/* DOOM fire (the classic PSX-era cellular automaton): heat rises from a
   permanently-hot bottom row, decaying and jittering sideways. Skipped
   entirely under prefers-reduced-motion; the static ember gradient in the
   marquee remains. */
(function () {
  var cv = document.getElementById('fire');
  if (!cv || !cv.getContext) return;
  if (window.matchMedia && matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  var W = 240, H = 64;
  cv.width = W; cv.height = H;
  var ctx = cv.getContext('2d');
  // 37-step heat ramp: void -> blood -> ember -> fire -> flare -> white.
  var stops = [
    [7, 7, 7], [31, 7, 7], [71, 15, 7], [103, 31, 7], [143, 39, 7],
    [175, 63, 7], [199, 71, 7], [223, 87, 7], [215, 103, 15], [207, 127, 15],
    [199, 151, 31], [191, 175, 47], [223, 207, 111], [255, 255, 255]
  ];
  var pal = [];
  for (var i = 0; i < 37; i++) {
    var t = i / 36 * (stops.length - 1);
    var a = Math.floor(t), b = Math.min(a + 1, stops.length - 1), f = t - a;
    pal.push([0, 1, 2].map(function (k) {
      return Math.round(stops[a][k] + (stops[b][k] - stops[a][k]) * f);
    }));
  }
  var heat = new Uint8Array(W * H);
  for (var x = 0; x < W; x++) heat[(H - 1) * W + x] = 36;
  var img = ctx.createImageData(W, H);
  function frame() {
    for (var y = 1; y < H; y++) {
      for (var x2 = 0; x2 < W; x2++) {
        var src = y * W + x2;
        var r = (Math.random() * 3) | 0;
        var dst = src - W - r + 1;
        if (dst < 0) dst = 0;
        var h = heat[src] - (r & 1);
        heat[dst] = h > 0 ? h : 0;
      }
    }
    var d = img.data;
    for (var p = 0; p < W * H; p++) {
      var c = pal[heat[p]], o = p * 4;
      d[o] = c[0]; d[o + 1] = c[1]; d[o + 2] = c[2];
      d[o + 3] = heat[p] === 0 ? 0 : 255;
    }
    ctx.putImageData(img, 0, 0);
  }
  setInterval(frame, 50);
})();
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

    let mut page = String::with_capacity(160 * 1024);
    page.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    page.push_str("<title>DOOM ARCADE &mdash; LEADERBOARD</title>");
    page.push_str("<style>");
    page.push_str(font_face_css());
    page.push_str(STYLE);
    page.push_str("</style></head><body>");
    page.push_str(
        "<header class=\"marquee\">\
         <canvas id=\"fire\" class=\"fire\" aria-hidden=\"true\"></canvas>\
         <div class=\"mq-inner\">\
         <span class=\"eyebrow\">CABINET LEADERBOARD</span>\
         <h1>DOOM ARCADE</h1>\
         <span class=\"tag\">ONE LIFE &middot; FIVE MAPS</span>\
         <span class=\"pip\" id=\"pip\">LIVE</span>\
         </div></header>",
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

    #[test]
    fn base64_known_vectors() {
        // RFC 4648 test vectors (padding in every phase).
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn page_embeds_font_and_fire_canvas() {
        let season = Season {
            iwad_sha256: String::new(),
            scoring_version: 1,
            map_rotation_id: "rot-v1".to_owned(),
        };
        let page = render(&season, &[]);
        assert!(page.contains("@font-face"), "font-face missing");
        assert!(
            page.contains("data:font/ttf;base64,"),
            "font not embedded as data URI"
        );
        assert!(page.contains("id=\"fire\""), "fire canvas missing");
        // The embedded font makes the page big but bounded: TTF is ~74 KiB,
        // so base64 lands near 100 KiB. Keep a ceiling so a future font swap
        // that balloons the page gets noticed.
        assert!(
            page.len() < 300 * 1024,
            "page unexpectedly huge: {}",
            page.len()
        );
    }
}
