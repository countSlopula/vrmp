# Projection detection

VR video has no reliable universal metadata, so layout is taken from the file
name — `_LR_180`, `_SBS_180` and `_TB_180` are all recognised, as are fisheye
lens tags like `MKX200`. Untagged files fall back to the frame aspect ratio.

That fallback is genuinely ambiguous: a 2:1 frame is equally consistent with mono
360 and side-by-side 180. Real libraries are overwhelmingly the latter, so that
is the guess, and every guess records its confidence. `vrmp scan` reports how
many titles were guessed, titles show a `guessed` marker in the browser, and the
format selector fixes any of them. Corrections are stored by title id and
reapplied after every rescan, so detection can never overwrite them.
