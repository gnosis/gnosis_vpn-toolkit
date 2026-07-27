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
-- IMPORTANT: neither the applet's bundle name nor any path it runs from may
-- contain the substring "Gnosis VPN" (with the space) — the pkg preinstall
-- runs `pkill -f "Gnosis VPN"` and would kill this loader too.
--
-- Compiled at build time by the crate's build.rs (osacompile + ditto into
-- OUT_DIR); the zipped applet is `include_bytes!`-embedded by
-- src/update/loader.rs. See the crate README.

set statusFile to "/Library/Logs/GnosisVPN/installer/update_status"

set progress total steps to -1
set progress description to "Updating Gnosis VPN…"
set progress additional description to "The app will reopen when the update finishes."

repeat 600 times -- 600 x 0.2 s = ~120 s self-timeout
	try
		set statusContent to do shell script "/bin/cat " & quoted form of statusFile
		if statusContent contains "done" then exit repeat
	on error
		-- Status file missing or unreadable: keep waiting until the timeout.
	end try
	delay 0.2
end repeat
