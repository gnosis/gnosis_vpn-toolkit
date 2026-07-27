-- Gnosis VPN update loader applet.
--
-- Shown by gnosis_vpn-update while installer(8) replaces the client: the pkg
-- preinstall kills the "Gnosis VPN.app" UI early and the postinstall only
-- relaunches it at the very end, so without this window the user stares at a
-- dead desktop for the whole install. The updater writes "updating" to the
-- status file before launching this applet and "done" once the install
-- finished (success or failure); the applet polls the file and quits on
-- "done". A hard iteration cap (600 polls x 0.2 s = ~120 s) guarantees the
-- window can never linger if the updater dies without writing "done".
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

	repeat with i from 1 to 600 -- 600 x 0.2 s = ~120 s self-timeout
		try
			set statusContent to do shell script "/bin/cat " & quoted form of statusFile
			if statusContent contains "done" then exit repeat
		on error
			-- Status file missing or unreadable: keep waiting until the timeout.
		end try
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
