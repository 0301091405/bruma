# Changelog

## 0.7.2 - 2026-11-27

- `PrintScreen` answers at once on a busy machine. With every core taken by a
  game it could take seconds, or not answer at all.
- It works in full-screen games, including ones run as administrator (anti-
  cheat often starts them so): the selection opens over the game, and you're
  returned to the game afterwards. Setup asks Windows once to let Bruma run as
  administrator for this; say no and it runs as before.
- Where nothing can be shown over a game, the screen is copied at once and a
  shutter sound plays.
- While a game or a film fills the screen, Bruma gives back the memory it
  keeps for rewind and GIFs.
- `bruma quit` stops the running Bruma.

## 0.7.1 - 2026-11-26

- A Bruma that was ended without closing no longer leaves its icon behind in
  the notification area; the next start removes it.
- The tray icon is sharper at small sizes, with the dot in the middle.

## 0.7.0 - 2026-11-26

- Choosing an area copies it straight away. Click the preview card to open it
  again and draw on it, or hold Shift as you let go to stay open. Picking a
  tool before choosing also keeps it open. "Copy as soon as an area is
  chosen" in Settings turns this off.
- GIFs are recorded at thirty pictures a second, for up to ten seconds ("GIF
  length" in Settings). Only what changes on screen is kept.
- A GIF plays before it is saved: trim it with the bar under it, draw on it,
  and the drawings go on every frame. It is copied as a file, so it pastes
  into chats.
- Snap finds the larger parts of a window too: the page of a browser, the body
  of a site, the text of an editor. `Tab` steps out from the box under the
  pointer to the part around it and then the window; `Shift+Tab` steps back.
- The screen around a selection is no longer blurred, only a little darker.
- The intro is lighter: only the crop marks and the dot.

## 0.6.0 - 2026-11-26

- Settings have their own panel: right-click the tray icon, then Settings, or
  run `bruma settings`. Changes apply at once, the hotkey included.
- The tray menu is Bruma's own, in the same style as the rest.
- A short intro the first time Bruma starts.
- Clicks on the tray icon are no longer sometimes ignored.
- Uninstalling while Bruma is running now removes it completely.

## 0.5.3 - 2026-11-26

First public release.

- Region, window and full-screen capture from a frozen screen.
- Rewind: step back through the last few seconds, select what changed (`W`),
  save them as a GIF (`K`), or make a before/after picture (`J`).
- Snap to boxes, pictures and edges found in the pixels.
- Drawing tools: arrow, line, rectangle, ellipse, pen, highlighter, text,
  numbered steps, blur, focus, ruler.
- Blur private text (`M`) and copy text (`X`) using Windows text recognition.
- Scrolling capture, frame, pin, colour picker, preview card.
- History with text search.
- PNG files that keep their drawings and can be reopened with `bruma edit`.
- A setup program, `bruma-setup.exe`, that installs for the current user
  without administrator rights, and a one-line install from PowerShell.
