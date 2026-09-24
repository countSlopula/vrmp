# vrmp

A VR media player for high-resolution 180/360 video, for OpenXR on Linux.

It scans a media library, shows it as a cover grid on a curved panel inside the
headset, and plays the selected title as a native OpenXR composition layer.

I ran into many problems with the players provided for Windows under Proton, so I
decided to give the whole vibe coding thing a try and sic Claude on it. This is,
at this point, a fully vibe coded project.

Feel free to suggest improvements and open issues if there are any, though I
won't make any guarantee I'll get to them. :P

## Building

```sh
nix develop      # dev shell
nix build        # build the package
```

Needs an OpenXR runtime providing `XR_KHR_opengl_enable`, which in practice means
a Monado-based one such as WiVRn. Start the runtime and connect the headset
before playing; `vrmp doctor` reports whether both are reachable.

Only NixOS with WiVRn has actually been tested. See
[docs/runtime.md](docs/runtime.md) for the full requirements, the extensions that
are optional, and how to build without Nix.

## Usage

```sh
vrmp add-root "/path/to/library"         # register a library
vrmp scan --verbose                      # see what was detected
vrmp covers                              # pre-generate cover art (optional)
vrmp doctor                              # check runtime and extensions
vrmp preview                             # render the library view to a PNG
vrmp config                              # show every setting, write them to file
vrmp play                                # start in the headset
```

Or just `./run`, which checks a runtime is available first and builds if needed.
It passes any other subcommand straight through — `./run scan -v`,
`./run preview` — and only insists on a headset for playback.

Any folder structure will do; the scanner recognises several, and cover art can
be supplied by dropping a `.cover` file beside the media. See
[docs/library.md](docs/library.md).

State is written to `./data` and never to the library, which is only ever read
from. Settings live in [docs/configuration.md](docs/configuration.md).

## Controls

| Input | Action |
| --- | --- |
| Point + trigger | Click the panel |
| B / menu button | Switch between the library and what is playing |
| A / X button | Show or hide the playback controls |
| Thumbstick click | Recentre everything in front of you |
| Thumbstick up/down | Scroll the library |
| Thumbstick left/right | Seek ±5s during playback |

The playback controls fade after about eight seconds of no input so they do not
sit over the video, and any controller activity brings them back. Recentring also
brings them back, since losing track of them is the usual reason to press it —
and it re-anchors the video and the panel to wherever you are now sitting and
facing, which matters because both are placed where you were when they appeared.

Recentring also follows how far back you are lying, so titles shot to be watched
lying down can be recentred while reclined and will tip to match. Small head
tilts are ignored — anything within 20 degrees of level counts as sitting up — so
glancing down at the controls never leaves the scene leaning.

Switching back to playback leaves the video unobstructed rather than showing the
transport bar, since returning to a title means wanting to watch it; A brings the
controls up when you do want them.

The library panel never hides: doing so would leave nothing on screen and no
obvious way to bring it back, so A only affects the playback controls.

Buttons vary by controller. A and X are the same action on opposite hands, since
Touch controllers only have A on the right. The simple and Vive profiles have no
spare buttons, so they get the trigger and menu only.

Every tile shows its cover, title, resolution and length. The format badge below
(`180° SBS`, or `180° SBS · guessed?` where detection was unsure) is a button:
click it and the format choices appear inline, so a wrong guess can be corrected
from the library without playing the title first. The same selector sits in the
playback controls, where a change applies to the picture immediately.

The desktop window mirrors what the player draws and takes mouse input, so the
whole thing can be driven from the desk — see
[docs/desktop-mirror.md](docs/desktop-mirror.md).

## Documentation

- [status.md](docs/status.md) — what works and what does not
- [runtime.md](docs/runtime.md) — OpenXR requirements, what has been tested, building without Nix
- [configuration.md](docs/configuration.md) — every setting and its default
- [library.md](docs/library.md) — folder layouts the scanner understands, and cover art
- [projection.md](docs/projection.md) — how 180/360 and stereo layout are detected
- [desktop-mirror.md](docs/desktop-mirror.md) — the X11 window, and what it does and does not show
- [architecture.md](docs/architecture.md) — composition layers, the mpv pipeline, the UI

## License

MIT. See [LICENSE](LICENSE).

This project is an independent piece of software and is **not affiliated with,
endorsed by, or supported by** the WiVRn, Monado, mpv, FFmpeg, egui or Khronos
projects. Those names appear here only to describe what the player talks to and
what it is built on. Each of those projects carries its own license and its own
maintainers; please direct questions about them to them, and questions about this
player here.
