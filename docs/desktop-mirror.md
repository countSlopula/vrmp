# Desktop mirror

The player keeps a small X11 window open — it is the GLX drawable, and the only
route keyboard input can reach the app. By default it mirrors what the player is
drawing, so you can see what is playing, or what the interface looks like,
without putting the headset on.

It is not a view through an eye. The window receives the same images the player
hands the compositor — the left half of the video frame, and the interface
texture — drawn flat, one over the other. Everything the compositor does with
those images afterwards happens downstream of the mirror: a 180 title appears as
the raw equirect rectangle rather than projected onto a sphere, and the interface
appears as a flat panel rather than curved. That is enough to see what is
playing and to drive the player from the desk, which is all it is for. It
letterboxes rather than stretches, since a square 180 frame and a 16:9 interface
do not share the window's shape.

During playback the window shows the video with a control bar composited along
the bottom, and that bar does not hide — the window exists to run the player from
the desk, so fading it there is pure friction. In the headset the controls still
fade over the video as before.

The window also takes mouse input. Whichever was used most recently — mouse or
controller — owns the pointer, so picking either up simply takes over, with no
mode to switch. Clicks land on the interface, so while the panel is up the window
shows the panel rather than the video; video takes the window back the moment the
panel hides. Clicking a letterbox bar does nothing.

Turn it off with `mirror_window` or resize it with `mirror_width`; the window
itself remains either way, because keyboard input depends on it. Both are
described in [configuration.md](configuration.md).
