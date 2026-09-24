# Configuration

`vrmp config` prints every setting and writes them all to the file, so options
that were defaulting invisibly become editable.

| Setting | Default | What it does |
| --- | --- | --- |
| `roots` | none | Directories scanned for media; each immediate subdirectory is a collection |
| `ui_distance_m` | `2.0` | Metres from the viewer to the curved panel |
| `ui_angle_deg` | `70.0` | Horizontal arc the panel spans; height follows, so this is the one size control |
| `mirror_window` | `true` | Draw into the desktop window (the window exists either way) |
| `mirror_width` | `960` | Width of that window in pixels; height follows the source aspect |
| `cache_secs` | `60` | Seconds libmpv buffers ahead |
| `cache_max_mib` | `2048` | Upper bound on that buffer |

The last two are the ones to reach for when playback is streaming off a network
share. The defaults are deliberately generous, because a stall partway through a
scene is the most irritating way this can fail, and the cap exists so that an 8K
stream cannot buffer its way through all of RAM. On a slow link, raising
`cache_secs` costs only a longer wait before playback starts.

State — the config file itself, the probe cache, generated covers and viewer
corrections — lives in `./data`. Override that location with `VRMP_DATA_DIR`.
Library directories are only ever read from.
