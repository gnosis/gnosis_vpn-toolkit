-- Gnosis VPN update loader applet.
--
-- Shown by gnosis_vpn-update while installer(8) replaces the client: the pkg
-- preinstall kills the "Gnosis VPN.app" UI early and the postinstall only
-- relaunches it at the very end, so without this window the user stares at a
-- dead desktop for the whole install. The updater writes "updating <pid>" to
-- the status file before launching this applet and "done" once the install
-- finished (success or failure).
--
-- The applet must never depend on the updater surviving: when the update is
-- triggered from the app, the preinstall's pkill of the app can take the
-- updater down with it, so "done" may never arrive. It quits on the FIRST of:
--   1. "done" in the status file;
--   2. the client app observed gone and then running again — the postinstall
--      relaunched it (this fires while installer(8) may still be running,
--      and is the primary "update finished" signal for the user);
--   3. the status file missing for ~10 consecutive polls (~2 s) — the
--      updater already cleaned up before this applet got to read it;
--   4. the updater PID dead AND no installer(8) running, twice in a row
--      (checked every ~2 s) — orphaned by a failed install;
--   5. a hard iteration cap (600 polls x 0.2 s = ~120 s), the last resort.
--
-- The window is drawn directly with AppleScriptObjC (a titled, buttonless
-- NSWindow with a slowly filling progress bar) instead of AppleScript's
-- stock `progress` UI: the stock applet progress window always carries a
-- Stop button, which must not be offered mid-install.
--
-- IMPORTANT: neither the applet's bundle name nor any path it runs from may
-- contain the substring "Gnosis VPN" (with the space) — the pkg preinstall
-- runs `pkill -f "Gnosis VPN"` and would kill this loader too. The *window
-- title* below is exempt: pkill matches command lines, never window titles.
--
-- Compiled at build time by the crate's build.rs (osacompile + zip into
-- OUT_DIR); the zipped applet is `include_bytes!`-embedded by
-- src/update/loader.rs. See the crate README.

use framework "AppKit"
use framework "Foundation"
use scripting additions

-- Bundle id of the client app the pkg postinstall relaunches. Keep in sync
-- with the Tauri identifier in the gnosis_vpn-app repo (productName
-- "Gnosis VPN"). Presence is checked in-process via NSRunningApplication:
-- shelling out to `pgrep -f "Gnosis VPN"` would put the forbidden substring
-- on a child process's command line, inside the preinstall pkill's blast
-- radius (see the constraint above).
property appBundleId : "com.gnosisvpn.gnosisvpnclient"

on run
	set statusFile to "/Library/Logs/GnosisVPN/installer/update_status"
	set ca to current application

	-- Titled-only style mask: a title bar without close/minimize/zoom
	-- buttons, and no Stop button anywhere.
	set win to ca's NSWindow's alloc()'s initWithContentRect:(ca's NSMakeRect(0, 0, 430, 96)) styleMask:(ca's NSWindowStyleMaskTitled) backing:(ca's NSBackingStoreBuffered) defer:false
	(win's setTitle:"Gnosis VPN Update")
	(win's setReleasedWhenClosed:false)
	(win's setLevel:(ca's NSFloatingWindowLevel))

	set mainLabel to ca's NSTextField's labelWithString:"Updating Gnosis VPN…"
	(mainLabel's setFrame:(ca's NSMakeRect(20, 60, 390, 20)))
	(win's contentView()'s addSubview:mainLabel)

	-- Determinate bar advanced by hand: AppKit offers no control over the
	-- indeterminate style's animation speed, so a slow eased fill toward 95%
	-- (snapped to 100% on "done") stands in for real install progress.
	set bar to ca's NSProgressIndicator's alloc()'s initWithFrame:(ca's NSMakeRect(20, 38, 390, 16))
	(bar's setIndeterminate:false)
	(bar's setMinValue:0)
	(bar's setMaxValue:100)
	(bar's setDoubleValue:0)
	(win's contentView()'s addSubview:bar)

	set subLabel to ca's NSTextField's labelWithString:"The app will reopen when the update finishes."
	(subLabel's setFont:(ca's NSFont's systemFontOfSize:11))
	(subLabel's setTextColor:(ca's NSColor's secondaryLabelColor()))
	(subLabel's setFrame:(ca's NSMakeRect(20, 14, 390, 16)))
	(win's contentView()'s addSubview:subLabel)

	(win's |center|()) -- piped: bare `center` clashes with an AppleScript constant
	(win's makeKeyAndOrderFront:(missing value))
	(ca's NSApp's activateIgnoringOtherApps:true)

	set sawAppGone to false
	set missingReads to 0
	set orphanStrikes to 0
	set updaterPid to ""
	repeat with i from 1 to 600 -- 600 x 0.2 s = ~120 s self-timeout
		try
			set statusContent to do shell script "/bin/cat " & quoted form of statusFile
			set missingReads to 0
			if statusContent contains "done" then exit repeat
			if updaterPid is "" then
				try
					-- "updating <pid>"; the integer round-trip validates the
					-- pid before it is ever spliced into a shell command.
					set updaterPid to ((word 2 of statusContent) as integer) as text
				end try
			end if
		on error
			-- Status file missing/unreadable: the updater finished and
			-- cleaned up before this applet got to read "done". Tolerate
			-- brief glitches, then quit instead of lingering to the timeout.
			set missingReads to missingReads + 1
			if missingReads ≥ 10 then exit repeat -- ~2 s
		end try
		-- Quit as soon as the postinstall has relaunched the client app:
		-- gone-then-back is the user-visible "update finished" signal, and it
		-- fires while installer(8)/the updater may still be running. Fresh
		-- class-method query every tick — never cache NSRunningApplication
		-- instances, their properties only refresh with run-loop turns.
		set apps to (ca's NSRunningApplication's runningApplicationsWithBundleIdentifier:appBundleId)
		if ((apps's |count|()) as integer) is 0 then
			set sawAppGone to true
		else if sawAppGone then
			exit repeat
		end if
		-- Every ~2 s: quit when both the updater and installer(8) are gone —
		-- a failed install with nobody left to write "done" or relaunch the
		-- app. Two strikes so a momentary gap cannot close the window while
		-- an orphaned install is still finishing.
		if updaterPid is not "" and (i mod 10) is 0 then
			set orphaned to false
			try
				do shell script "/bin/ps -p " & updaterPid
			on error
				try
					do shell script "/usr/bin/pgrep -x installer"
				on error
					set orphaned to true
				end try
			end try
			if orphaned then
				set orphanStrikes to orphanStrikes + 1
				if orphanStrikes ≥ 2 then exit repeat
			else
				set orphanStrikes to 0
			end if
		end if
		-- Piecewise linear schedule (i is 0.2 s ticks): 0→40% over the first
		-- 10 s, →60% over the next 10 s, →80% over the next 40 s, →100% over
		-- the final 60 s, reaching full exactly at the self-timeout.
		if i ≤ 50 then
			set barValue to i * 0.8
		else if i ≤ 100 then
			set barValue to 40 + (i - 50) * 0.4
		else if i ≤ 300 then
			set barValue to 60 + (i - 100) * 0.1
		else
			set barValue to 80 + (i - 300) * (20 / 300)
		end if
		(bar's setDoubleValue:barValue)
		delay 0.2 -- in an applet, delay keeps pumping the event loop, so the window stays live
	end repeat

	-- Finish the bar before quitting so the install reads as completed.
	(bar's setDoubleValue:100)
	delay 0.3
end run
