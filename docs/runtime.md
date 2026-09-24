# Runtime requirements

Any OpenXR runtime providing **`XR_KHR_opengl_enable`** will do — the decoder
renders through GL, so that one is required. In practice this means Monado-based
runtimes (WiVRn, Monado, Envision); SteamVR on Linux is effectively Vulkan-only
and will not work.

Two more extensions are used where present and degrade gracefully where not:
`XR_KHR_composition_layer_equirect2` (without it, 180/360 projection is
unavailable but flat playback still works) and
`XR_KHR_composition_layer_cylinder` (without it, the interface is shown on a flat
panel instead of a curved one).

Start a runtime and connect the headset before `vrmp play`. If the loader cannot
find one, point it at the manifest explicitly:

```sh
XR_RUNTIME_JSON=/path/to/runtime/share/openxr/1/runtime.json vrmp play
```

`vrmp doctor` reports whether the runtime is reachable and whether the required
extensions are present.

## What this has actually been tested on

NixOS, with WiVRn as the runtime, driving a standalone headset over the network.
That is the only combination that has seen real use, and everything above about
extension fallbacks and library requirements was measured there.

The requirements are written in terms of OpenXR extensions rather than a
particular runtime, so other Monado-based setups should work, but nobody has
confirmed that. Treat anything outside NixOS and WiVRn as untested rather than
supported. Reports of what does and does not work elsewhere are welcome.

## Building without Nix

There is no `build.rs`, no bindgen and no cmake in the tree, so the build is
`cargo build --release` once the libraries are present. Only two are linked:

```
libmpv.so.2
libopenxr_loader.so.1
```

Both need the unversioned `.so` symlink the linker resolves against, which
usually means the `-dev`/`-devel` package even though no headers are consumed.
Two more are opened at runtime by bare name — `libX11.so.6` and `libGL.so.1` —
and the `ffmpeg` binary must be on `PATH`, since cover extraction shells out to
it.

```sh
# Debian/Ubuntu
sudo apt install libmpv-dev libopenxr-dev libgl1 libx11-6 ffmpeg

# Fedora
sudo dnf install mpv-libs-devel openxr-devel mesa-libGL libX11 ffmpeg

# Arch
sudo pacman -S mpv openxr-loader mesa libx11 ffmpeg
```

Those package names are derived from what the binary links, not from having run
them — see the testing note above. If a distro ships no OpenXR loader package,
the alternative is switching the `openxr` crate to its `static` feature, which
builds the Khronos SDK and so adds cmake and a C++ toolchain to the requirements.
