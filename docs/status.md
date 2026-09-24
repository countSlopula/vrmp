# Status

Working: library scanning, cover art, equirect 180/360 in mono, side-by-side and
top-bottom, flat 2D playback on a virtual screen, the in-headset browser, the
format selector, playback controls, resume points, and layout correction.

Not yet implemented: fisheye projections (`MKX200`, `RF52` and similar). These
are detected and labelled, but an equirect layer cannot describe a fisheye dome —
it needs a mesh of our own. The player says so during playback and offers the
format selector so such a title can still be watched as approximate equirect.
