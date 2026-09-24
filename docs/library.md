# Library layout

No particular folder structure is required. The scanner handles three shapes and
decides between them per collection:

- **One folder per release**, split into discs — `Studio/TITLE-001/{_A,_B,_C}.mp4`
  becomes one title with three parts.
- **Nested quality folders** — `Studio/<title>/VR/VR 4K/<scene>.mp4` becomes one
  title per release. Releases that ship flat 2D cuts alongside VR ones are
  flagged as mixed, and playback starts on the VR part.
- **Loose files with quality suffixes** — `42. Scene Name … 2k.mp4` and
  `42. Scene Name … 4k h265.mp4` collapse into one title with two variants, and
  the highest resolution is played by default.

## Cover art

To set a cover by hand, drop a **`.cover`** file next to the media. It takes
precedence over everything else and needs no extension — the image format is read
from the file's contents, so any common image can just be renamed:

```
Studio/TITLE-001/.cover      # covers the whole title
Studio/scene.mp4
Studio/scene.cover           # covers just that file
```

`cover.jpg`, `folder.jpg`, `poster.jpg`, `thumb.*` and `fanart.*` are also
recognised. Failing all of those, a frame is extracted with ffmpeg, cropped to one
eye and to the centre of the forward hemisphere so the thumbnail is
recognisable — a raw grab from a 180 file is a pair of distorted circles and
unreadable at tile size.

Tiles are portrait, at the 0.735 ratio measured from real cover art. Generated
frame grabs are cropped to the same shape so a library mixing the two still lays
out as an even grid; artwork of any other ratio is letterboxed rather than
stretched, since a distorted cover reads as a bug while an inset one does not.

Any that cannot be generated are recorded in `data/cover-failures.log` with the
file path and the reason, and the library says so on screen with the log location
— a tile with no art is otherwise indistinguishable from one still waiting its
turn. The log is rewritten each run, so it always describes the current state,
and is not created at all when nothing fails.

Covers missing from the cache are generated in the background when the player
starts, so no separate step is needed and a newly added title gets art on its
own. On a first run that can be a minute or so of work, and since it reads from
the same share the video streams from it may make playback stutter while it
runs. The library says so on screen — "Generating covers 12/74" — rather than
having the worker quietly pause during playback, which would make its speed
depend on state you cannot see.

`vrmp covers` does that same work in the foreground instead, and is the way to
avoid the stutter entirely: run it once from the desk after adding titles and the
next launch has nothing left to generate. It needs no runtime and no headset, so
it is also what to reach for over SSH or from a script. It is the same worker the
player runs, waited on rather than backgrounded, so the results and the failure
log are identical either way — it only moves when the cost is paid.

Run `vrmp preview` to render the library view to a PNG and see the result without
putting the headset on.

**Library directories are only ever read from.** Covers, the probe cache, and
viewer corrections are all written to `./data` instead. Override that location
with `VRMP_DATA_DIR`.
