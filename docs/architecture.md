# How it works

The player does **not** render video onto a sphere of its own. Where the runtime
supports `XR_KHR_composition_layer_equirect2`, the decoded frame is handed to it
as an equirect layer and the compositor performs the projection. This
matters for image quality: the compositor reprojects that layer at the headset's
own refresh rate, so the picture stays stable during head motion even though the
video itself runs at 60fps.

Stereo costs nothing extra. A side-by-side frame is a single swapchain image, and
the two eyes are two layers over that same image, distinguished only by
`eye_visibility` and an `image_rect` naming each half — so there is no per-eye
copy and no shader work.

Decoding is libmpv through its OpenGL render API. mpv handles demuxing, hardware
decode, audio output, A/V sync and seeking, and renders each frame directly into
the OpenXR swapchain image, so there is no intermediate copy on our side either.
Because mpv falls back to software decode automatically, files that exceed the
GPU's fixed-function decoder limits still play — which matters for 8K H.264,
since AMD's VCN caps H.264 at 4096×4096.

The interface is egui rendered to a texture and shown as a cylinder layer. A
controller ray is intersected with that cylinder and the hit point becomes a
mouse position, so the whole UI is ordinary 2D code driven by a 3D pointer.
