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
	// directive.
	private void Emit(String payload)
	{
		Console.Printf("ARCADE_EVT %s", payload);
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

		// First WorldLoaded of a brand-new game: totaltime is 0 only on the
		// opening map of a fresh game (it accumulates monotonically across
		// the run). The runStarted latch keeps a death-restart of that same
		// opening map from double-firing run_start.
		if (!runStarted && level.totaltime == 0)
		{
			runStarted = true;
			playerDead = false;
			completedTics = 0;
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
}
