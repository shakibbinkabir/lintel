# Roadmap: help wanted

This is the honest list of what Lintel 2.0.0 leaves open. Nothing here is promised for a date. Each line is something a contributor can pick up. Comment on the matching issue, or open one, before you start anything medium or large, so that two people do not do the same work.

[CONTRIBUTING.md](../CONTRIBUTING.md) explains how to build, the rules and how to propose a change.

## Small: good first contributions

- **Translations of new strings.** The interface has ten languages, but some newer lines are English only. The lines of the Features page live in `src/settings/main.ts` and are not translated at all. The other strings are in `src/features/<id>/strings.json` and `src/i18n/`.
- **Fix a translation.** If a line reads badly in your language, change it. Native speakers see what nobody else can.
- **Testing on other machines.** Lintel was built and checked on one laptop: one display at 125 % scaling, Windows 11. Reports from Windows 10, from desktops, from other scalings and from several displays are useful even when everything works. Say what you tried and what you saw.
- **Tooltips in Settings and in feature windows.** They were checked on the island only. Look for hints that are cut off, late or repeat what the control already says.
- **The installer as an MSI.** It is built but was never installed. It installs for the whole machine and asks for administrator rights. Try it and report.
- **Agent installers on real installs.** The hooks for Antigravity, Cursor, Muse and Hermes were never confirmed against a real install, and the Linear pill was never tried against a real account. Connect one, read the diff that Lintel shows, and report whether sessions appear.
- **A real update, from one release to the next.** Updating from the island downloads the installer, checks it against GitHub's SHA-256 and runs it. The installer's restart flags have not been tried on a real update. Try it when the next release is out, and report.
- **A quieter log.** While another app holds the same global shortcuts, six "not registered" lines are written at every registration. Once is enough.
- **A list of recent recordings.** A short list of the last recordings, like the one the island keeps for captures, is not built.

## Medium

- **A Windows notification when a permission card waits behind a full-screen app.** The island steps aside for full-screen apps, so a waiting card can go unseen. A notification that only informs, and never answers the card, is not built.
- **A separate control for a session's permission mode.** A control on the island to choose a session's permission mode, apart from the Allow button, is not built.
- **The wake phrase standing down for other microphone users.** While dictation or the screen recorder has the microphone, the wake phrase should stop listening and come back afterwards.
- **The wake phrase measured with real voices.** The list of words the speech engine may write for "Lintel" is a guess. It needs recordings of many voices and a measured table.
- **A smoother mascot at rest that still costs little.** With "The small island stays" on, the mascot is drawn about fourteen times a second and the resting island uses about 10 % of one processor core (about 1 % with the mascot hidden). The aim is a mascot that still looks alive at a fraction of that.
- **A session that stays "working".** When the Coding agents feature is switched off in the middle of a turn, the session goes on showing as working.
- **More for WSL and SSH.** Claude Code runs in WSL and on SSH hosts. The plan-usage status line in WSL, other agents on SSH hosts, a host marker on the pill and the live state of the tunnel are not built.
- **Start with the agent, for more agents.** Lintel can be started when Claude Code starts working. The same opt-in for other agents is not built.
- **"Start automatically" for the hardware helper.** The design is one scheduled task created during a consented elevation. It is not built.

## Large

- **The wait before a spoken answer from the local model.** An open question waits 9 to 15 seconds for its first word on a laptop, and about half a minute the first time. Warming the model earlier costs more than 4 GB of memory for every command, so the answer is not obvious.
- **Code signing.** The installer and the programs are not signed, so SmartScreen warns. The SignPath Foundation declined the application in October 2026 for now (the project is young), and may be asked again later. The path is the Microsoft Store version, which Microsoft signs; it is being prepared.
- **Fan reading on more laptops.** On some laptops the sensor library cannot read the embedded controller, so fans and some temperatures are missing. This is work in or beside LibreHardwareMonitor.
- **A second display for the island.** The island lives on one display at a time. Showing it on several, or following the pointer, is not built.
- **Voice with a live microphone.** The assistant was built and checked mostly with recorded clips. It needs long sessions with real voices, real rooms and real interruptions, and fixes for what they show.

## Not planned

Two things were considered and dropped: a replacement for Alt+Tab, and an editor for recordings. Voice is English only.
