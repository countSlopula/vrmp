{
  description = "vrmp - a VR media player for high-resolution 180/360 video";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Everything the process needs found for it, and nothing more.
        #
        # This list is deliberately short. mpv and the OpenXR runtime pull in a
        # great deal — Vulkan, Wayland, xkbcommon, VA-API — but every one of
        # those arrives through their own RUNPATH, baked in when nixpkgs built
        # them, so naming them here changes nothing. What does have to be here
        # is what this binary resolves itself: libmpv and the OpenXR loader,
        # which it links, plus libX11 and libGL, which x11-dl dlopens by bare
        # SONAME with no RUNPATH to fall back on.
        #
        # Verified by running with exactly these four and watching the library
        # scan, GLX context and runtime all come up. Before adding to this list,
        # check that the library is genuinely unreachable without it.
        runtimeLibs = with pkgs; [
          mpv-unwrapped
          openxr-loader
          libGL
          libx11
        ];

        buildDeps = runtimeLibs;
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            ffmpeg
            # Only for the version banner below; nothing in the crate builds
            # against pkg-config.
            pkg-config
          ];
          buildInputs = buildDeps;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

          shellHook = ''
            echo "vrmp dev shell - rustc $(rustc --version | cut -d' ' -f2), mpv $(pkg-config --modversion mpv)"
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "vrmp";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # There is no build.rs, no bindgen and no cmake in the tree, so the
          # build needs nothing beyond cargo and the wrapper.
          nativeBuildInputs = [ pkgs.makeWrapper ];
          buildInputs = buildDeps;

          # ffmpeg is invoked as a subprocess for thumbnail extraction, so it has
          # to be on PATH rather than merely linked.
          postInstall = ''
            wrapProgram $out/bin/vrmp \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]} \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath runtimeLibs}
          '';

          meta = with pkgs.lib; {
            description = "A VR media player for high-resolution 180/360 video";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "vrmp";
          };
        };

        apps.default = flake-utils.lib.mkApp { drv = self.packages.${system}.default; };
      });
}
