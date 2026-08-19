// arcade-telemetry event handler (SPEC §4.5).
//
// Emits one JSON object per line to the console/log stream, prefixed with
// the fixed sentinel "ARCADE_EVT " (see crates/protocol/src/event.rs — the
// authoritative wire format). The supervisor parses only lines beginning
// with the sentinel and ignores everything else, so the only hard rules
// here are:
//
//   * exactly one event per Console.Printf call (one line),
//   * the payload is valid version-1 JSON matching protocol::Event,
//   * no untrusted string can break out of its JSON string literal or
//     inject a newline (which would let it forge a fresh sentinel line).
//
// Registered via MAPINFO gameinfo { AddEventHandlers = "ArcadeTelemetry" }
// — ZScript event handlers are NOT auto-registered by class definition.

class ArcadeTelemetry : StaticEventHandler
{
	// True once run_start has been emitted. One gzdoom process hosts exactly
	// one run (the supervisor restarts the engine between runs), so this is
	// never reset. Together with the level.totaltime == 0 check it stops a
	// death-restart of the first map (whose totaltime is also 0) from
	// emitting a second run_start. NOTE: 4.14.2 does expose a NewGame()
	// virtual (gzdoom.pk3 zscript/events.zs), but it fires before any level
	// exists, so the first WorldLoaded of a new game is used instead — it
	// runs with LevelLocals live and needs nothing version-specific.
	private bool runStarted;

	// Set once the console player's death has been reported. Keeps
	// player_died single-shot, and suppresses the WorldUnloaded that a
	// post-death map reload (or the engine tearing down after the
	// supervisor's SIGTERM) would otherwise turn into a fake level_complete.
	private bool playerDead;

	// Sum of level.maptime across every completed map, in tics
	// (35 tics = 1 second). Becomes run_complete's total_maptime_tics.
	private int completedTics;

	// --- Hold-Start-to-quit state (SPEC §5 partial credit) ---
	//
	// The Start button is Enter, which is deliberately unbound in-game, so
	// the only observer is our InputProcess hook. InputProcess is ui scope
	// (gzdoom.pk3 zscript/events.zs: "virtual ui bool InputProcess") and ui
	// code may not write play-side fields, so key state crosses the fence
	// via EventHandler.SendNetworkEvent (clearscope static) and lands back
	// here in NetworkProcess (play scope). The netevent console command can
	// forge these events, but that grants nothing: it is exactly as powerful
	// as holding the (already player-reachable) Start button, and the
	// cabinet exposes no console anyway.

	// True while the Start button is believed down. Set/cleared by
	// NetworkProcess; consumed by WorldTick.
	private bool startHeld;

	// Consecutive WorldTick tics Start has been held. Release resets it.
	private int startHoldTics;

	// True once run_quit has been emitted. Single-shot for the process
	// lifetime, like runStarted: the supervisor SIGTERMs the engine on
	// receipt, so nothing after it matters. Do NOT quit from ZScript —
	// process control belongs to the supervisor (SPEC §4.4).
	private bool runQuitEmitted;

	// Hold-to-quit thresholds, in tics (35 tics = 1 second): the on-screen
	// warning appears after 1 s of holding, run_quit fires at 3 s.
	const QUIT_WARN_TICS = 35;
	const QUIT_HOLD_TICS = 105;

	// Progress heartbeat period: 70 tics = 2 s.
	const PROGRESS_INTERVAL_TICS = 70;

	// --- Walk-up HUD overlay state (SPEC §4.5) ---

	// Score banked from maps already completed, accumulated play-side in
	// WorldUnloaded and only *read* by the ui-scope RenderOverlay. The
	// overlay adds the open map's provisional points on top each frame.
	// DISPLAY-ONLY: the supervisor/server recompute the real score from
	// telemetry; if this drifts from crates/protocol/src/scoring.rs the
	// cabinet shows a wrong number but the leaderboard stays correct.
	private int bankedScore;

	// KEEP IN SYNC with crates/protocol/src/scoring.rs (SPEC §5) — the
	// authoritative constants live there; these mirror them for the HUD.
	const POINTS_PER_KILL = 10;
	const POINTS_PER_SECRET = 100;
	const POINTS_PER_ITEM = 5;
	const COMPLETION_BONUS = 500;
	const TIME_BONUS_PAR_SECONDS = 600;
	const TIME_BONUS_PER_SECOND = 2;
	const DEPTH_BONUS_PER_MAP = 200;

	// How long the controls block stays up after map entry: 525 tics = 15 s.
	const CONTROLS_HINT_TICS = 525;

	// Length of the fixed rotation (protocol::MAP_ROTATION). The map list
	// itself lives inside RotationIndex(): on 4.14.2 a class-scope
	// `static const` array is not visible as an identifier from method
	// bodies (verified: "Unknown identifier" at compile), so — like
	// gzdoom.pk3's own ui code — the arrays are declared in the functions
	// that use them.
	const ROTATION_LEN = 5;

	// ------------------------------------------------------------------
	// JSON helpers
	// ------------------------------------------------------------------

	// Escapes a string for embedding inside a JSON string literal.
	//
	// Applied to every string interpolated into an event: the arcade_session
	// / arcade_initials cvars are user cvars and therefore fully
	// attacker-controlled (any player at the console could set them to
	// arbitrary bytes), and level.MapName / level.LevelName come from the
	// WAD, which the spec says to treat as untrusted too.
	// LEXER QUIRK (verified empirically on 4.14.2, see pk3/NOTES.md): when
	// the ZScript tokenizer decides where a string literal ends, only \" is
	// honored as an escape pair — so a literal like "\\\\" swallows its own
	// closing quote ("...\\" lexes as backslash + escaped quote, and the
	// string runs on until the next bare quote in the file: "Unexpected
	// character: \" / "Unexpected identifier" errors far from the cause).
	// Therefore NO backslash characters appear in any string literal in
	// this file; every backslash in the output is produced with %c and
	// ASCII code 92.
	static String JsonEscape(String s)
	{
		String outStr = "";
		// s.Length() is unsigned; keep the loop variable signed for
		// ByteAt/Mid (both take int) without a signedness warning.
		int len = int(s.Length());
		for (int i = 0; i < len; i++)
		{
			int b = s.ByteAt(i);
			if (b == 0x5C || b == 0x22)
			{
				// Backslash (0x5C): must be doubled, otherwise a trailing
				// backslash in the input would escape our own closing
				// quote and everything after it would be parsed as string
				// data — classic string-literal breakout.
				//
				// Double quote (0x22): unescaped it terminates the JSON
				// string early, letting the rest of the input inject
				// arbitrary keys/values into the event object (e.g. a
				// forged "session" field).
				//
				// JSON escapes both the same way: backslash + the byte
				// itself.
				outStr.AppendFormat("%c%c", 92, b);
			}
			else if (b < 0x20)
			{
				// All control characters (0x00-0x1F) as backslash-u00XX:
				//  * JSON (RFC 8259) forbids them raw inside strings — a
				//    raw one would make the whole line unparseable;
				//  * \n / \r specifically would split the log line, letting
				//    hostile initials terminate this event and start a
				//    fresh line with a fake "ARCADE_EVT {...}" sentinel —
				//    the exact injection the protocol crate defends
				//    against; escaping makes it inert string data;
				//  * 0x1C (GZDoom's TEXTCOLOR escape) is also caught here,
				//    so console color codes cannot leak into the JSON.
				outStr.AppendFormat("%cu%04x", 92, b);
			}
			else
			{
				// Everything else passes through verbatim, copied via Mid()
				// (byte-based) rather than AppendCharacter() so multi-byte
				// UTF-8 sequences are not re-encoded byte-by-byte. If the
				// input contains invalid UTF-8, the resulting line simply
				// fails JSON parsing downstream and is dropped — fail-safe.
				outStr = outStr .. s.Mid(i, 1);
			}
		}
		return outStr;
	}

	// Reads a user cvar for the console player, empty string if unset.
	private String CvarString(Name cvarName)
	{
		CVar cv = CVar.GetCVar(cvarName, players[consoleplayer]);
		if (cv == null)
		{
			return "";
		}
		return cv.GetString();
	}

	// Emits one complete telemetry line. The payload rides through a "%s"
	// so any '%' inside cvar-derived content cannot act as a format
	// directive. PRINT_LOG (verified live on 4.14.2, see pk3/NOTES.md):
	// the line reaches the +logfile FIFO exactly like plain Printf did,
	// but is NOT painted into the on-screen notify area — with progress
	// heartbeats every 2 s, plain Printf kept the top of the screen
	// permanently filled with raw JSON (and buried the HUD overlay).
	private void Emit(String payload)
	{
		Console.PrintfEx(PRINT_LOG, "ARCADE_EVT %s", payload);
	}

	// ------------------------------------------------------------------
	// Event emitters
	// ------------------------------------------------------------------

	private void EmitRunStart()
	{
		// SKILLP_ACSReturn is 0-based (Hurt Me Plenty == 2), while the
		// supervisor and SPEC speak the vanilla 1-based -skill convention
		// (HMP == 3, what the supervisor passes on the command line), so
		// shift by one. Verified empirically on 4.14.2: launching with
		// "-skill 3" makes ACSReturn read 2 (see pk3/NOTES.md).
		String payload = String.Format(
			"{\"v\":1,\"event\":\"run_start\",\"session\":\"%s\",\"initials\":\"%s\",\"skill\":%d,\"ts\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(CvarString('arcade_initials')),
			G_SkillPropertyInt(SKILLP_ACSReturn) + 1,
			level.totaltime);
		Emit(payload);
	}

	private void EmitLevelEnter()
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"level_enter\",\"session\":\"%s\",\"map\":\"%s\",\"level_name\":\"%s\",\"ts\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(level.MapName),
			JsonEscape(level.LevelName),
			level.totaltime);
		Emit(payload);
	}

	private void EmitLevelComplete()
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"level_complete\",\"session\":\"%s\",\"map\":\"%s\","
			.. "\"kills\":%d,\"total_monsters\":%d,\"secrets\":%d,\"total_secrets\":%d,"
			.. "\"items\":%d,\"total_items\":%d,\"maptime_tics\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(level.MapName),
			level.killed_monsters,
			level.total_monsters,
			level.found_secrets,
			level.total_secrets,
			level.found_items,
			level.total_items,
			level.maptime);
		Emit(payload);
	}

	private void EmitPlayerDied()
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"player_died\",\"session\":\"%s\",\"map\":\"%s\","
			.. "\"kills\":%d,\"secrets\":%d,\"maptime_tics\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(level.MapName),
			level.killed_monsters,
			level.found_secrets,
			level.maptime);
		Emit(payload);
	}

	private void EmitRunComplete()
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"run_complete\",\"session\":\"%s\",\"total_maptime_tics\":%d}",
			JsonEscape(CvarString('arcade_session')),
			completedTics);
		Emit(payload);
	}

	// Heartbeat with provisional stats (protocol::Event::Progress), so a
	// run that ends without a clean map-exit event — walk-away, hold-quit,
	// engine death — still gets partial credit for the map in progress.
	// px/py feed the supervisor's walk-away detection only; whole map units
	// are plenty. The caller has already null-checked the player pawn.
	private void EmitProgress(int px, int py)
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"progress\",\"session\":\"%s\",\"map\":\"%s\","
			.. "\"kills\":%d,\"total_monsters\":%d,\"secrets\":%d,\"total_secrets\":%d,"
			.. "\"items\":%d,\"total_items\":%d,\"maptime_tics\":%d,\"px\":%d,\"py\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(level.MapName),
			level.killed_monsters,
			level.total_monsters,
			level.found_secrets,
			level.total_secrets,
			level.found_items,
			level.total_items,
			level.maptime,
			px,
			py);
		Emit(payload);
	}

	// The player deliberately ended the run by holding Start for 3 s
	// (protocol::Event::RunQuit). The map in progress is scored from the
	// last progress heartbeat; the supervisor SIGTERMs the engine.
	private void EmitRunQuit()
	{
		String payload = String.Format(
			"{\"v\":1,\"event\":\"run_quit\",\"session\":\"%s\",\"map\":\"%s\",\"maptime_tics\":%d}",
			JsonEscape(CvarString('arcade_session')),
			JsonEscape(level.MapName),
			level.maptime);
		Emit(payload);
	}

	// ------------------------------------------------------------------
	// Hooks
	// ------------------------------------------------------------------

	override void WorldLoaded(WorldEvent e)
	{
		// Save-game loads and hub re-entries are not fresh map entries.
		if (e.IsSaveGame || e.IsReopen)
		{
			return;
		}

		// Fresh map entry: drop any Start-hold state carried over from the
		// previous map (WorldTick does not run during intermissions, so a
		// hold spanning one never advanced anyway) or left latched by a
		// missed key-up (InputProcess is bypassed while a menu is open, so
		// releasing Start inside a menu is invisible to us — the player
		// must release and re-press). See pk3/NOTES.md.
		startHeld = false;
		startHoldTics = 0;

		// First WorldLoaded of a brand-new game: totaltime is 0 only on the
		// opening map of a fresh game (it accumulates monotonically across
		// the run). The runStarted latch keeps a death-restart of that same
		// opening map from double-firing run_start.
		if (!runStarted && level.totaltime == 0)
		{
			runStarted = true;
			playerDead = false;
			completedTics = 0;
			bankedScore = 0;
			EmitRunStart();
		}

		EmitLevelEnter();
	}

	override void WorldUnloaded(WorldEvent e)
	{
		// Unloads on behalf of a save-game load or hub traversal are not
		// completions.
		if (e.IsSaveGame || e.IsReopen)
		{
			return;
		}
		// No run in progress — nothing to report.
		if (!runStarted)
		{
			return;
		}
		// After death the only unloads are the post-death map reload or
		// engine teardown; neither is a completion.
		if (playerDead)
		{
			return;
		}

		EmitLevelComplete();
		completedTics += level.maptime;

		// Bank the completed map's score for the HUD overlay. KEEP IN SYNC
		// with map_score()/run_score() in crates/protocol/src/scoring.rs
		// (SPEC §5): kill/secret/item points, the completion bonus, the
		// time bonus (2/second under 600 s par, integer seconds, floored
		// at 0), and the per-map depth bonus. Display-only (see the
		// bankedScore comment).
		int secondsOnMap = level.maptime / 35;
		int timeBonus =
			max(0, TIME_BONUS_PAR_SECONDS - secondsOnMap) * TIME_BONUS_PER_SECOND;
		bankedScore += level.killed_monsters * POINTS_PER_KILL
			+ level.found_secrets * POINTS_PER_SECRET
			+ level.found_items * POINTS_PER_ITEM
			+ COMPLETION_BONUS
			+ timeBonus
			+ DEPTH_BONUS_PER_MAP;

		// MAP08 is the final map of the rotation (pk3/MAPINFO.txt): its
		// unload means the run ended in victory. Emitted after the
		// level_complete line, per the protocol's documented event order.
		// §13.3 resolved: verified live on 4.14.2 that WorldUnloaded DOES
		// fire on MAP08's real exit into the EndTitle end sequence, with
		// stats intact (see pk3/NOTES.md) — no MAPINFO end-sequence
		// fallback needed.
		if (level.MapName ~== "MAP08")
		{
			EmitRunComplete();
		}
	}

	override void WorldThingDied(WorldEvent e)
	{
		// The frozen design observes the console player's death through
		// WorldThingDied rather than the PlayerDied(PlayerEvent) virtual
		// (which does exist on 4.14.2 — gzdoom.pk3 zscript/events.zs); for
		// a single-player cabinet the two are equivalent, and this path
		// keeps the guard logic in one WorldEvent-shaped pipeline.
		if (!runStarted || playerDead)
		{
			return;
		}
		if (e.Thing == null || e.Thing.player == null)
		{
			return;
		}
		// Only the console player's own pawn ends the run (voodoo dolls and
		// other players have no business here; the cabinet is single-player).
		if (e.Thing != players[consoleplayer].mo)
		{
			return;
		}

		playerDead = true;
		EmitPlayerDied();
	}

	override void WorldTick()
	{
		// No run, or the run already ended in death: neither heartbeats nor
		// hold-to-quit have anything to add (the supervisor is already
		// tearing the process down after player_died).
		if (!runStarted || playerDead)
		{
			return;
		}

		// --- progress heartbeat, every PROGRESS_INTERVAL_TICS ---
		//
		// level.maptime increments once per tic before handlers run, so the
		// modulo hits exactly once per interval; maptime > 0 skips the very
		// first tic of a map (a zero-information event). No pawn, no
		// heartbeat: px/py are the point of the event (walk-away
		// detection), and the pawn can be null during teardown.
		let mo = players[consoleplayer].mo;
		if (mo != null
			&& level.maptime > 0
			&& level.maptime % PROGRESS_INTERVAL_TICS == 0)
		{
			EmitProgress(int(mo.pos.x), int(mo.pos.y));
		}

		// --- hold-Start-to-quit ---
		//
		// NOTE: WorldTick does not run during intermissions or while a menu
		// pauses a single-player game, so holding Start there does nothing
		// (documented behaviour, see pk3/NOTES.md).
		if (startHeld && !runQuitEmitted)
		{
			startHoldTics++;
			if (startHoldTics >= QUIT_HOLD_TICS)
			{
				// 3 s of continuous hold: report the deliberate quit once
				// and stop counting. The supervisor SIGTERMs the engine;
				// quitting from ZScript is forbidden (SPEC §4.4).
				runQuitEmitted = true;
				EmitRunQuit();
			}
			else if (startHoldTics >= QUIT_WARN_TICS
				&& (startHoldTics - QUIT_WARN_TICS) % 35 == 0)
			{
				// Shown at 1 s of hold and re-armed once per second while
				// the hold lasts (con_midtime defaults to 3 s, so the text
				// never blinks). Deliberately NOT re-armed every tic:
				// MidPrint also echoes into the logfile with separator
				// lines, and per-tic calls put ~210 junk lines/s onto the
				// telemetry FIFO (observed live, see pk3/NOTES.md).
				Console.MidPrint(smallfont, "KEEP HOLDING TO END RUN");
			}
		}
	}

	// Watches the raw input stream for the Start button (Enter — scancode
	// InputEvent.Key_Enter = 0x1c, gzdoom.pk3 zscript/engine/inputevents.zs;
	// deliberately unbound in-game so it reaches us and then falls through
	// to nothing). Returns false unconditionally: this hook only observes,
	// it must never eat input. ui scope, hence the SendNetworkEvent relay
	// to the play-side fields (see the state block up top).
	override bool InputProcess(InputEvent e)
	{
		if (e.KeyScan == InputEvent.Key_Enter)
		{
			if (e.Type == InputEvent.Type_KeyDown)
			{
				EventHandler.SendNetworkEvent("arcade_start_down");
			}
			else if (e.Type == InputEvent.Type_KeyUp)
			{
				EventHandler.SendNetworkEvent("arcade_start_up");
			}
		}
		return false;
	}

	// Play-scope receiver for the InputProcess relay above.
	override void NetworkProcess(ConsoleEvent e)
	{
		// Single-player cabinet: only the console player's events matter.
		if (e.Player != consoleplayer)
		{
			return;
		}
		if (e.Name ~== "arcade_start_down")
		{
			// Repeated key-down events (if the platform ever sends
			// autorepeat as raw events) are idempotent: the counter runs
			// off WorldTick, not off event arrival.
			startHeld = true;
		}
		else if (e.Name ~== "arcade_start_up")
		{
			// If the warning was up, blank it immediately rather than let
			// stale "KEEP HOLDING" text linger for the rest of con_midtime
			// after the player already let go.
			if (startHoldTics >= QUIT_WARN_TICS && !runQuitEmitted)
			{
				Console.MidPrint(smallfont, "");
			}
			startHeld = false;
			startHoldTics = 0;
		}
	}

	// ------------------------------------------------------------------
	// Walk-up HUD overlay (SPEC §4.5)
	// ------------------------------------------------------------------

	// Position of a map in the fixed rotation, -1 if not part of it.
	// Mirrors protocol::MAP_ROTATION (crates/protocol/src/scoring.rs);
	// keep ROTATION_LEN above in step with this list.
	// clearscope: pure function, and the class is play scope so an
	// unmarked method could not be called from the ui-scope RenderOverlay
	// ("Can't call play function from ui context", verified at compile).
	private clearscope static int RotationIndex(String mapName)
	{
		static const String rotationMaps[] =
		{
			"MAP01", "MAP02", "MAP03", "MAP07", "MAP08"
		};
		for (int i = 0; i < rotationMaps.Size(); i++)
		{
			if (mapName ~== rotationMaps[i])
			{
				return i;
			}
		}
		return -1;
	}

	// One right-aligned line of overlay text in the 1280x800 virtual space.
	private static ui void DrawRight(Font fnt, int color, int xRight, int y,
		String text)
	{
		Screen.DrawText(fnt, color, xRight - fnt.StringWidth(text), y, text,
			DTA_VirtualWidth, 1280, DTA_VirtualHeight, 800,
			DTA_KeepRatio, true);
	}

	// Top-right walk-up HUD: live score, rotation progress, a controls
	// crib that collapses after 15 s, and a permanent pointer at the
	// hold-Start-to-quit path. ui scope: reads play state (bankedScore,
	// LevelLocals), writes nothing, and must NEVER Console.Printf — this
	// runs every frame and the logfile is the telemetry FIFO.
	override void RenderOverlay(RenderEvent e)
	{
		if (!runStarted)
		{
			return;
		}
		// No pawn (brief teardown windows): nothing worth drawing over.
		if (players[consoleplayer].mo == null)
		{
			return;
		}

		Font fnt = NewSmallFont;
		int fh = fnt.GetHeight();
		int xRight = 1280 - 16;
		int y = 16;

		// 1. Live score: banked completed-map total plus the open map's
		// provisional points, read straight off the Level counters every
		// frame. KEEP IN SYNC with crates/protocol/src/scoring.rs
		// (kills/secrets/items terms of map_score; SPEC §5).
		int provisional = level.killed_monsters * POINTS_PER_KILL
			+ level.found_secrets * POINTS_PER_SECRET
			+ level.found_items * POINTS_PER_ITEM;
		DrawRight(fnt, Font.CR_GOLD, xRight, y,
			String.Format("SCORE %d", bankedScore + provisional));
		y += fh + 2;

		// 2. Where in the rotation this map sits.
		int idx = RotationIndex(level.MapName);
		String mapLine;
		if (idx >= 0)
		{
			mapLine = String.Format("MAP %d/%d · %s",
				idx + 1, ROTATION_LEN, level.LevelName);
		}
		else
		{
			mapLine = level.LevelName;
		}
		DrawRight(fnt, Font.CR_WHITE, xRight, y, mapLine);
		y += fh + 10;

		// 3. Controls crib, in full for the first 15 s of each map, then
		// collapsed away (the hold-Start line below slides up).
		if (level.maptime < CONTROLS_HINT_TICS)
		{
			// Cabinet control panel crib (SPEC §10). Declared in-function:
			// see the ROTATION_LEN comment up top.
			static const String controlLabels[] =
			{
				"STICK", "CTRL", "SPACE", ", .", "[ ]"
			};
			static const String controlDescs[] =
			{
				"MOVE + TURN", "FIRE", "OPEN / USE", "STRAFE", "WEAPON"
			};
			int descW = 0;
			for (int i = 0; i < controlDescs.Size(); i++)
			{
				descW = max(descW, fnt.StringWidth(controlDescs[i]));
			}
			int descX = xRight - descW;
			for (int i = 0; i < controlDescs.Size(); i++)
			{
				Screen.DrawText(fnt, Font.CR_GRAY,
					descX, y, controlDescs[i],
					DTA_VirtualWidth, 1280, DTA_VirtualHeight, 800,
					DTA_KeepRatio, true);
				Screen.DrawText(fnt, Font.CR_GRAY,
					descX - 14 - fnt.StringWidth(controlLabels[i]), y,
					controlLabels[i],
					DTA_VirtualWidth, 1280, DTA_VirtualHeight, 800,
					DTA_KeepRatio, true);
				y += fh;
			}
			y += 10;
		}

		// 4. Discoverability for the deliberate-quit path (always shown).
		DrawRight(fnt, Font.CR_WHITE, xRight, y,
			"HOLD START — END RUN + BANK SCORE");
	}
}
